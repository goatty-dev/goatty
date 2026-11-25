//! This computer's goattyd: is the running daemon the build goatty would
//! start now, and stopping it (whatever protocol it speaks).
//!
//! After an update (or a `cargo build`) the daemon that's still running is
//! the old build. It keeps the sessions, so nothing replaces it silently:
//! [`outdated`] tells the GUI (see `goatty-app/src/connect.rs`), which
//! restarts it right away when that keeps the sessions ([`restart_daemon`],
//! protocol v10 on Unix) or there are none, and asks otherwise.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use crate::ipc::LocalStream;
use crate::{Control, VersionMismatch, service};

/// The goattyd a fresh start of the local daemon runs
/// ([`crate::connect_or_spawn_daemon`]): the user service's (systemd / launchd /
/// Scheduled Task) when it serves this endpoint, else `goattyd` (`goattyd.exe`
/// on Windows) next to the current binary.
pub fn installed_daemon() -> Option<PathBuf> {
    let path = goatty_proto::socket_path();
    if service::unit_socket_path().is_some_and(|p| p == path) && service::installed() {
        return service::service_program();
    }
    let name = format!("goattyd{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe().ok().map(|e| e.with_file_name(name)).filter(|d| d.exists())
}

/// The running local daemon is an older build than [`installed_daemon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outdated {
    pub pid: u32,
    /// What it runs (on Linux, `/proc/<pid>/exe`, with ` (deleted)` once replaced).
    pub running: PathBuf,
    pub installed: PathBuf,
    /// Identifies the installed build, for [`decline`].
    installed_ino: u64,
}

/// A file's identity and age (Unix).
#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Build {
    dev: u64,
    ino: u64,
    modified: SystemTime,
}

#[cfg(unix)]
impl Build {
    fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        Some(Self { dev: m.dev(), ino: m.ino(), modified: m.modified().ok()? })
    }
}

/// `installed` is another file than `running`, and not older: a rebuild or
/// an update. (An older one, e.g. after checking out an older branch, isn't
/// worth restarting for.)
#[cfg(any(unix, test))]
fn is_newer(running: Build, installed: Build) -> bool {
    (running.dev, running.ino) != (installed.dev, installed.ino) && installed.modified >= running.modified
}

/// `Some` when the local daemon with `pid` (from its `Hello`) runs an older
/// build than a restart would start.
pub fn outdated(pid: u32) -> Option<Outdated> {
    outdated_than(pid, installed_daemon()?)
}

/// [`outdated`] against a given `installed` goattyd.
///
/// * **Linux:** stats the running binary through `/proc/<pid>/exe`.
/// * **macOS:** inspects the process's mapped executable vnode via
///   `PROC_PIDREGIONPATHINFO` (which stays accurate even across in-place
///   `execve` restarts), falling back to comparing the process start time
///   (`PROC_PIDTBSDINFO`) with `installed`'s mtime.
/// * **Windows:** inspects the process start time (`GetProcessTimes`) and
///   executable path (`QueryFullProcessImageNameW`) against `installed`'s
///   mtime and size.
pub fn outdated_than(pid: u32, installed: PathBuf) -> Option<Outdated> {
    #[cfg(target_os = "linux")]
    {
        let exe = PathBuf::from(format!("/proc/{pid}/exe"));
        let running = std::fs::read_link(&exe).ok()?;
        // stat follows /proc's link to the file the process runs, even deleted.
        let (old, new) = (Build::of(&exe)?, Build::of(&installed)?);
        is_newer(old, new).then_some(Outdated { pid, running, installed, installed_ino: new.ino })
    }
    #[cfg(target_os = "macos")]
    {
        let new = Build::of(&installed)?;
        if let Some((old, running)) = macos_running_build(pid) {
            return is_newer(old, new).then_some(Outdated { pid, running, installed, installed_ino: new.ino });
        }
        let started = macos_process_start(pid)?;
        let running = macos_pid_path(pid).unwrap_or_else(|| installed.clone());
        (new.modified > started).then_some(Outdated { pid, running, installed, installed_ino: new.ino })
    }
    #[cfg(windows)]
    {
        windows_outdated_than(pid, installed)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (pid, installed);
        None
    }
}

#[cfg(windows)]
fn windows_file_fingerprint(meta: &std::fs::Metadata) -> u64 {
    let (secs, nanos) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), u64::from(d.subsec_nanos())))
        .unwrap_or((0, 0));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [secs, nanos, meta.len()] {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(windows)]
fn windows_process_info(pid: u32) -> Option<(PathBuf, SystemTime)> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    const WINDOWS_TO_UNIX_SECS: u64 = 11_644_473_600;

    // SAFETY: plain Win32 call; handle is validated and closed before return.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if handle.is_null() {
        return None;
    }

    let path = {
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `handle` is open and `buf` has space for `len` UTF-16 code units.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &raw mut len) };
        if ok != 0 && len > 0 { Some(PathBuf::from(OsString::from_wide(&buf[..len as usize]))) } else { None }
    };

    let started = {
        let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut exit = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut kernel = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut user = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: `handle` is open and all four `FILETIME` pointers are valid writable locals.
        let ok = unsafe { GetProcessTimes(handle, &raw mut created, &raw mut exit, &raw mut kernel, &raw mut user) };
        if ok != 0 {
            let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            let secs = ticks / 10_000_000;
            let nanos = ((ticks % 10_000_000) * 100) as u32;
            secs.checked_sub(WINDOWS_TO_UNIX_SECS).map(|s| SystemTime::UNIX_EPOCH + Duration::new(s, nanos))
        } else {
            None
        }
    };

    // SAFETY: `handle` is owned exclusively by this scope.
    unsafe {
        CloseHandle(handle);
    }

    Some((path?, started?))
}

#[cfg(windows)]
fn windows_outdated_than(pid: u32, installed: PathBuf) -> Option<Outdated> {
    let (running, started) = windows_process_info(pid)?;
    let new_meta = std::fs::metadata(&installed).ok()?;
    let new_mtime = new_meta.modified().ok()?;
    let new_key = windows_file_fingerprint(&new_meta);

    let same_path = match (std::fs::canonicalize(&running), std::fs::canonicalize(&installed)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => running.to_string_lossy().eq_ignore_ascii_case(&installed.to_string_lossy()),
    };

    let outdated = if new_mtime > started {
        true
    } else if !same_path {
        match std::fs::metadata(&running) {
            Ok(old_meta) => {
                let old_mtime = old_meta.modified().ok()?;
                new_mtime > old_mtime || (new_mtime == old_mtime && new_meta.len() != old_meta.len())
            }
            Err(_) => true,
        }
    } else {
        false
    };

    outdated.then_some(Outdated { pid, running, installed, installed_ino: new_key })
}

#[cfg(target_os = "macos")]
fn macos_pid_path(pid: u32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
    let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let len = (n as usize).min(buf.len());
    let s = std::str::from_utf8(&buf[..len]).ok()?;
    (!s.is_empty()).then(|| PathBuf::from(s))
}

#[cfg(target_os = "macos")]
fn macos_process_start(pid: u32) -> Option<SystemTime> {
    // SAFETY: `proc_bsdinfo` is plain data; zeroed bytes are valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` points to `size` writable bytes.
    let ret = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if ret != size {
        return None;
    }
    let nanos = (info.pbi_start_tvusec.min(999_999) as u32) * 1_000;
    Some(SystemTime::UNIX_EPOCH + Duration::new(info.pbi_start_tvsec, nanos))
}

#[cfg(target_os = "macos")]
fn macos_running_build(pid: u32) -> Option<(Build, PathBuf)> {
    // From Apple's `<sys/proc_info.h>` (stable public Darwin ABI since 10.5).
    const PROC_PIDREGIONPATHINFO: libc::c_int = 8;
    const VM_PROT_EXECUTE: u32 = 4;

    #[repr(C)]
    #[allow(dead_code)]
    struct ProcRegionInfo {
        pri_protection: u32,
        pri_max_protection: u32,
        pri_inheritance: u32,
        pri_flags: u32,
        pri_offset: u64,
        pri_behavior: u32,
        pri_user_wired_count: u32,
        pri_user_tag: u32,
        pri_pages_resident: u32,
        pri_pages_shared_now_private: u32,
        pri_pages_swapped_out: u32,
        pri_pages_dirtied: u32,
        pri_ref_count: u32,
        pri_shadow_depth: u32,
        pri_share_mode: u32,
        pri_private_pages_resident: u32,
        pri_shared_pages_resident: u32,
        pri_obj_id: u32,
        pri_depth: u32,
        pri_address: u64,
        pri_size: u64,
    }

    #[repr(C)]
    struct ProcRegionWithPathInfo {
        prp_prinfo: ProcRegionInfo,
        prp_vip: libc::vnode_info_path,
    }

    let expected_path = macos_pid_path(pid);
    let size = std::mem::size_of::<ProcRegionWithPathInfo>() as libc::c_int;
    let mut addr: u64 = 0;
    let mut fallback: Option<(Build, PathBuf)> = None;

    for _ in 0..4096 {
        // SAFETY: `ProcRegionWithPathInfo` is plain C data; all zeroes is valid.
        let mut info: ProcRegionWithPathInfo = unsafe { std::mem::zeroed() };
        // SAFETY: `info` points to `size` writable bytes.
        let ret = unsafe { libc::proc_pidinfo(pid as libc::c_int, PROC_PIDREGIONPATHINFO, addr, (&raw mut info).cast(), size) };
        if ret != size {
            break;
        }
        let st = &info.prp_vip.vip_vi.vi_stat;
        if st.vst_ino != 0 && (info.prp_prinfo.pri_protection & VM_PROT_EXECUTE) != 0 {
            let raw_bytes: &[u8] =
                // SAFETY: `vip_path` is a contiguous `[c_char; 1024]` array inside `info`.
                unsafe { std::slice::from_raw_parts(info.prp_vip.vip_path.as_ptr().cast::<u8>(), std::mem::size_of_val(&info.prp_vip.vip_path)) };
            let nul = raw_bytes.iter().position(|&b| b == 0).unwrap_or(raw_bytes.len());
            let vpath = String::from_utf8_lossy(&raw_bytes[..nul]);
            let is_runtime_lib = vpath == "/usr/lib/dyld" || vpath.contains("dyld_shared_cache") || vpath.ends_with(".dylib");
            if !is_runtime_lib {
                let secs = u64::try_from(st.vst_mtime).unwrap_or(0);
                let nanos = u32::try_from(st.vst_mtimensec.clamp(0, 999_999_999)).unwrap_or(0);
                // Match `MetadataExt::dev()` on macOS (`stat.st_dev: i32` widened to `u64`).
                let build = Build {
                    dev: (st.vst_dev as i32) as u64,
                    ino: st.vst_ino,
                    modified: SystemTime::UNIX_EPOCH + Duration::new(secs, nanos),
                };
                let running = expected_path.clone().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from(vpath.as_ref()));
                if expected_path.as_deref().is_some_and(|p| p == Path::new(vpath.as_ref())) || st.vst_nlink == 0 {
                    return Some((build, running));
                }
                if fallback.is_none() {
                    fallback = Some((build, running));
                }
            }
        }
        let Some(next) = info.prp_prinfo.pri_address.checked_add(info.prp_prinfo.pri_size) else { break };
        if next <= addr {
            break;
        }
        addr = next;
    }
    fallback
}

fn declined_file() -> PathBuf {
    goatty_proto::runtime_dir().join("restart-declined")
}

fn decline_key(o: &Outdated) -> String {
    format!("{} {}", o.pid, o.installed_ino)
}

/// The user chose to keep this daemon running: don't ask again until it or
/// the installed build changes (or the next login: the runtime dir).
pub fn decline(o: &Outdated) {
    let _ = std::fs::create_dir_all(goatty_proto::runtime_dir());
    if let Err(e) = std::fs::write(declined_file(), decline_key(o)) {
        log::warn!("couldn't remember keeping goattyd running: {e}");
    }
}

/// See [`decline`].
pub fn declined(o: &Outdated) -> bool {
    std::fs::read_to_string(declined_file()).is_ok_and(|s| s.trim() == decline_key(o))
}

/// What the local daemon is wrong about; the GUI asks before restarting.
#[derive(Debug, Clone)]
pub enum LocalProblem {
    /// It speaks no protocol this goatty does.
    Incompatible(VersionMismatch),
    /// It works, but an older build than the installed one is running.
    Outdated { outdated: Outdated, sessions: usize },
}

impl std::fmt::Display for LocalProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalProblem::Incompatible(m) => write!(f, "the local {m}"),
            LocalProblem::Outdated { outdated, .. } => {
                write!(f, "the local goattyd (pid {}) runs an older build than {}", outdated.pid, outdated.installed.display())
            }
        }
    }
}

impl std::error::Error for LocalProblem {}

/// A connect error, as a [`LocalProblem`] when it is one.
pub fn diagnose(err: anyhow::Error) -> anyhow::Error {
    match err.downcast_ref::<VersionMismatch>() {
        Some(m) => LocalProblem::Incompatible(m.clone()).into(),
        None => err,
    }
}

/// How [`restart_daemon`] went.
#[derive(Debug)]
pub enum Restarted {
    NotRunning,
    /// The daemon runs `exe` now, with every session it had.
    Kept {
        sessions: usize,
    },
    /// It couldn't keep its sessions over a restart (too old, Windows lacks
    /// `execve`/ConPTY handoff, or spoke no protocol this build does) and was
    /// stopped: they ended. The next connection starts a new one.
    Stopped(Stopped),
}

/// The running local daemon can restart without ending its sessions.
///
/// Always `false` on Windows: Windows has no `execve`, and ConPTY pseudo-console
/// handles (`HPCON`) are tied to their owning process's `conhost.exe`.
pub fn can_keep_sessions(protocol: u32) -> bool {
    #[cfg(windows)]
    {
        let _ = protocol;
        false
    }
    #[cfg(not(windows))]
    {
        protocol >= goatty_proto::RESTART_SINCE
    }
}

/// Restart the local daemon as `exe` (default: the goattyd it was started
/// as) and wait until it answers again. Sessions carry over when the daemon
/// can do that (protocol v10 on Unix). One that can't (or on Windows, where
/// ConPTY handles cannot survive a process image replacement) is stopped only
/// with `force`, which ends its sessions; without it that's an error.
pub fn restart_daemon(exe: Option<&Path>, force: bool) -> Result<Restarted> {
    let primary = goatty_proto::socket_path();
    let mut sock = primary.clone();
    // Any daemon a restart keeps the sessions of, also one older than this
    // build speaks (`ADMIN_MIN_PROTOCOL_VERSION`): the new one takes them over.
    let admin = |p: &Path| Control::connect_admin(p, "goatty-restart", None);
    let mut control = match admin(&primary) {
        Ok(c) => c,
        Err(e) => {
            if LocalStream::connect(&primary).is_err()
                && let Some(legacy) = goatty_proto::legacy_socket_path()
                && LocalStream::connect(&legacy).is_ok()
            {
                sock = legacy;
                match admin(&sock) {
                    Ok(c) => c,
                    Err(le) => {
                        if LocalStream::connect(&sock).is_err() {
                            return Ok(Restarted::NotRunning);
                        }
                        anyhow::ensure!(force, "{le:#}");
                        return stop_daemon(true).map(Restarted::Stopped);
                    }
                }
            } else {
                if LocalStream::connect(&primary).is_err() {
                    return Ok(Restarted::NotRunning);
                }
                anyhow::ensure!(force, "{e:#}");
                return stop_daemon(true).map(Restarted::Stopped);
            }
        }
    };
    if !can_keep_sessions(control.protocol_version()) {
        let n = control.list_sessions()?.len();
        #[cfg(windows)]
        anyhow::ensure!(
            force,
            "goattyd on Windows (pid {}) cannot keep its {n} session(s) across a restart; pass --force to stop it and end them",
            control.pid()
        );
        #[cfg(not(windows))]
        anyhow::ensure!(
            force,
            "goattyd (pid {}, protocol v{}) is too old to keep its {n} session(s) over a restart; stopping it ends them",
            control.pid(),
            control.protocol_version()
        );
        drop(control);
        return stop_daemon(true).map(Restarted::Stopped);
    }
    let pid = control.pid();
    let sessions = control.list_sessions()?.len();
    control.restart(exe)?;
    // The new image accepts on the same socket once it took the sessions over
    // (and may also bind `primary` if upgrading off the legacy macOS path).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(50));
        let attempt = Control::connect_within(&sock, "goatty-restart", Some(left)).or_else(|e| {
            if sock != primary { Control::connect_within(&primary, "goatty-restart", Some(left)).map_err(|_| e) } else { Err(e) }
        });
        match attempt {
            Ok(c) if c.pid() == pid => return Ok(Restarted::Kept { sessions }),
            Ok(c) => anyhow::bail!("another goattyd (pid {}) answers now", c.pid()),
            Err(e) if Instant::now() > deadline => return Err(e.context("goattyd didn't come back after restarting")),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// How [`stop_daemon`] stopped it.
#[derive(Debug)]
pub enum Stopped {
    NotRunning,
    /// It shut down when asked.
    Asked,
    /// It didn't understand the request (`why`); the user service manager stopped it.
    Service {
        why: String,
    },
    /// It didn't understand the request (`why`); it was terminated.
    Killed {
        pid: u32,
        why: String,
    },
}

/// The process exists and isn't a zombie. Reaps it if it was our child
/// (goatty started the daemon it now stops).
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: plain syscall; for a pid that isn't our child it fails with
        // ECHILD and changes nothing.
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
        #[cfg(target_os = "linux")]
        return std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next()) != Some('Z'));
        #[cfg(target_os = "macos")]
        // SAFETY: `info` is plain data valid for writes of the size passed.
        return unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) == size
                && info.pbi_status != libc::SZOMB
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        // SAFETY: plain syscall; signal 0 only checks.
        return unsafe { libc::kill(pid as i32, 0) } == 0;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, FALSE, GetLastError, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

        // SAFETY: plain Win32 call; returned handle is validated and closed.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if handle.is_null() {
            // SAFETY: `GetLastError` has no preconditions.
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        }
        let mut exit_code = 0u32;
        // SAFETY: `handle` is open and `exit_code` is a valid out-pointer.
        let ok = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) };
        // SAFETY: `handle` is owned exclusively by this scope.
        unsafe {
            CloseHandle(handle);
        }
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// The name of process `pid` (`/proc/<pid>/comm` on Linux, `proc_name` on macOS);
/// empty when unavailable. Windows verifies the basename directly on the retained
/// process handle inside [`windows_force_stop`].
#[cfg(unix)]
fn process_name(pid: u32) -> String {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is valid for writes of the size passed.
        let n = unsafe { libc::proc_name(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
        String::from_utf8_lossy(&buf[..(n.max(0) as usize).min(buf.len())]).into_owned()
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        String::new()
    }
}

fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while alive(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn is_goattyd_process_name_for(name: &str, windows: bool) -> bool {
    let trimmed = name.trim();
    if windows { trimmed.eq_ignore_ascii_case("goattyd.exe") } else { trimmed.starts_with("goattyd") }
}

fn is_goattyd_process_name(name: &str) -> bool {
    is_goattyd_process_name_for(name, cfg!(windows))
}

#[cfg(windows)]
fn windows_force_stop(path: &Path, why: String) -> Result<Stopped> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use anyhow::Context;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FALSE, GetLastError, HANDLE, STILL_ACTIVE, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };

    struct OwnedProcess(HANDLE);
    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid process handle owned exclusively by `self`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let Ok(stream) = LocalStream::connect(path) else {
        return Ok(Stopped::NotRunning);
    };
    let pid = stream.server_pid()?;

    // Open a single handle with query + terminate + wait rights while `stream`
    // is still connected so the PID cannot be reused between verification and
    // termination.
    // SAFETY: plain Win32 call; returned handle is validated and closed on drop.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE, FALSE, pid) };
    // Capture `GetLastError` immediately before `drop(stream)` calls `CloseHandle`,
    // which could overwrite the thread's last-error code.
    // SAFETY: `GetLastError` has no preconditions.
    let open_err = if raw.is_null() { unsafe { GetLastError() } } else { 0 };
    drop(stream);
    if raw.is_null() {
        if open_err == ERROR_INVALID_PARAMETER {
            return Ok(Stopped::NotRunning);
        }
        anyhow::bail!("OpenProcess(pid {pid}): {}", std::io::Error::from_raw_os_error(open_err as i32));
    }
    let proc = OwnedProcess(raw);

    let mut exit_code = 0u32;
    // SAFETY: `proc.0` is open and `exit_code` is a valid out-pointer.
    if unsafe { GetExitCodeProcess(proc.0, &raw mut exit_code) } != 0 && exit_code != STILL_ACTIVE as u32 {
        return Ok(Stopped::NotRunning);
    }

    // Verify the retained handle belongs to the current user SID.
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle.
    let my_sid = crate::ipc::TokenUserBuf::for_process(unsafe { GetCurrentProcess() }).context("read current user SID")?;
    let peer_sid = crate::ipc::TokenUserBuf::for_process(proc.0)
        .with_context(|| format!("{} belongs to another user or inaccessible process (pid {pid})", path.display()))?;
    // SAFETY: both `TokenUserBuf`s live across `EqualSid`.
    let same_user = unsafe { EqualSid(my_sid.sid(), peer_sid.sid()) != 0 };
    anyhow::ensure!(same_user, "{} belongs to another user (pid {pid})", path.display());

    // Verify the retained handle's exact executable basename is `goattyd.exe`.
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `proc.0` is open and `buf` has space for `len` UTF-16 code units.
    let ok = unsafe { QueryFullProcessImageNameW(proc.0, 0, buf.as_mut_ptr(), &raw mut len) };
    anyhow::ensure!(ok != 0 && len > 0, "QueryFullProcessImageNameW(pid {pid}): {}", std::io::Error::last_os_error());
    let exe_path = PathBuf::from(OsString::from_wide(&buf[..len as usize]));
    let comm = exe_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    anyhow::ensure!(is_goattyd_process_name(&comm), "{} belongs to pid {pid} ({}), not a goattyd", path.display(), comm.trim());

    log::info!("stopping goattyd (pid {pid}): {why}");
    // SAFETY: `proc.0` was opened with `PROCESS_TERMINATE` and just verified above.
    if unsafe { TerminateProcess(proc.0, 1) } == 0 {
        // SAFETY: `GetLastError` has no preconditions.
        let err = unsafe { GetLastError() };
        // If the process exited between our check and `TerminateProcess`, report it gone.
        let mut code = 0u32;
        // SAFETY: `proc.0` is open and `code` is a valid out-pointer.
        if unsafe { GetExitCodeProcess(proc.0, &raw mut code) } != 0 && code != STILL_ACTIVE as u32 {
            return Ok(Stopped::NotRunning);
        }
        anyhow::bail!("TerminateProcess(pid {pid}): {}", std::io::Error::from_raw_os_error(err as i32));
    }

    // Confirm exit on the same handle before reporting `Stopped::Killed`.
    // SAFETY: `proc.0` was opened with `SYNCHRONIZE`.
    let wait = unsafe { WaitForSingleObject(proc.0, 5_000) };
    anyhow::ensure!(wait == WAIT_OBJECT_0, "goattyd (pid {pid}) did not exit within 5 s after TerminateProcess");
    Ok(Stopped::Killed { pid, why })
}

/// Stop the local daemon and wait until it's gone. This ends its sessions:
/// only after the user agreed (or when it has none).
///
/// It's asked first (`Shutdown`). With `force`, one that doesn't speak our
/// protocol is stopped anyway: through the user service manager when that
/// runs it, else SIGTERM/SIGKILL (Unix) or `TerminateProcess` (Windows) to
/// the endpoint's owner once verified to be a `goattyd`.
pub fn stop_daemon(force: bool) -> Result<Stopped> {
    let mut path = goatty_proto::socket_path();
    let mut is_service_socket = service::unit_socket_path().is_some_and(|p| p == path);
    // Under socket activation, connecting to an idle listener would wake
    // goattyd just to shut it down. If only the supervisor's socket is active,
    // no daemon is running there (still check any pre-upgrade macOS socket).
    if is_service_socket && service::installed() {
        let st = service::status();
        if st.socket_active && !st.service_active {
            if let Some(legacy) = goatty_proto::legacy_socket_path()
                && LocalStream::connect(&legacy).is_ok()
            {
                path = legacy;
                is_service_socket = false;
            } else {
                return Ok(Stopped::NotRunning);
            }
        }
    }
    let connect_and_shutdown = |p: &Path| Control::connect_admin(p, "goatty-stop", None).and_then(|mut c| c.shutdown().map(|()| c.pid()));
    let err = match connect_and_shutdown(&path) {
        Ok(pid) => {
            if !wait_gone(pid, Duration::from_secs(5)) {
                anyhow::bail!("goattyd (pid {pid}) is still running 5 s after it was asked to stop");
            }
            return Ok(Stopped::Asked);
        }
        Err(e) => {
            if LocalStream::connect(&path).is_err()
                && let Some(legacy) = goatty_proto::legacy_socket_path()
                && legacy != path
                && LocalStream::connect(&legacy).is_ok()
            {
                path = legacy;
                is_service_socket = false;
                match connect_and_shutdown(&path) {
                    Ok(pid) => {
                        if !wait_gone(pid, Duration::from_secs(5)) {
                            anyhow::bail!("goattyd (pid {pid}) is still running 5 s after it was asked to stop");
                        }
                        return Ok(Stopped::Asked);
                    }
                    Err(le) if force => le,
                    Err(le) => return Err(le),
                }
            } else if force {
                e
            } else {
                return Err(e);
            }
        }
    };
    let why = format!("{err:#}");
    if is_service_socket && service::service_active() {
        service::stop_service()?;
        return Ok(Stopped::Service { why });
    }
    // Nothing listening: nothing to stop (remote updates run this blindly).
    #[cfg(windows)]
    {
        windows_force_stop(&path, why)
    }
    #[cfg(unix)]
    {
        if LocalStream::connect(&path).is_err() {
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            return Ok(Stopped::NotRunning);
        }
        let pid = crate::control::socket_owner(&path)?;
        // Under socket activation the listener belongs to systemd/launchd, never kill that.
        let comm = process_name(pid);
        anyhow::ensure!(is_goattyd_process_name(&comm), "{} belongs to pid {pid} ({}), not a goattyd", path.display(), comm.trim());
        log::info!("stopping goattyd (pid {pid}): {why}");
        // SAFETY: plain syscall on a pid we just checked is our goattyd.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if !wait_gone(pid, Duration::from_secs(5)) {
            // SAFETY: as above.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            wait_gone(pid, Duration::from_secs(2));
        }
        // The old daemon didn't get to remove its socket.
        if crate::control::socket_owner(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
        Ok(Stopped::Killed { pid, why })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, why);
        Ok(Stopped::NotRunning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(ino: u64, secs: u64) -> Build {
        Build { dev: 1, ino, modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs) }
    }

    #[test]
    fn only_a_different_newer_file_counts() {
        assert!(is_newer(build(1, 10), build(2, 20)));
        assert!(is_newer(build(1, 10), build(2, 10)));
        // The very file it runs.
        assert!(!is_newer(build(1, 10), build(1, 10)));
        // An older build (an older branch checked out).
        assert!(!is_newer(build(1, 20), build(2, 10)));
    }

    #[test]
    fn goattyd_process_name_check_handles_unix_and_windows() {
        assert!(is_goattyd_process_name_for("goattyd", false));
        assert!(is_goattyd_process_name_for("goattyd\n", false));
        assert!(is_goattyd_process_name_for("goattyd-0.3.0-p", false));
        assert!(!is_goattyd_process_name_for("systemd", false));
        assert!(!is_goattyd_process_name_for("launchd", false));

        assert!(is_goattyd_process_name_for("goattyd.exe", true));
        assert!(is_goattyd_process_name_for("GOATTYD.EXE", true));
        assert!(!is_goattyd_process_name_for("goattyd-diagnostics.exe", true));
        assert!(!is_goattyd_process_name_for("goattyd", true));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn this_process_is_not_outdated_against_itself() {
        // Our own exe vs itself: same file.
        let exe = std::env::current_exe().unwrap();
        assert_eq!(outdated_than(std::process::id(), exe), None);
        assert!(alive(std::process::id()));
        assert!(!alive(u32::MAX >> 1));
    }
}
