//! This computer's goattyd: is the running daemon the build goatty would
//! start now, and stopping it (whatever protocol it speaks).
//!
//! After an update (or a `cargo build`) the daemon that's still running is
//! the old build. It keeps the sessions, so nothing replaces it silently:
//! [`outdated`] tells the GUI (see `goatty-app/src/connect.rs`), which
//! restarts it right away when that keeps the sessions ([`restart_daemon`],
//! protocol v10 on) or there are none, and asks otherwise.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use crate::{Control, VersionMismatch, service};

/// The goattyd a fresh start of the local daemon runs
/// ([`crate::connect_or_spawn_daemon`]): the user service's (systemd/launchd)
/// when it serves this socket, else `goattyd` next to the current binary.
pub fn installed_daemon() -> Option<PathBuf> {
    let path = goatty_proto::socket_path();
    if service::unit_socket_path().is_some_and(|p| p == path) && service::installed() {
        return service::service_program();
    }
    std::env::current_exe().ok().map(|e| e.with_file_name("goattyd")).filter(|d| d.exists())
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

/// A file's identity and age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Build {
    dev: u64,
    ino: u64,
    modified: SystemTime,
}

impl Build {
    fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        Some(Self { dev: m.dev(), ino: m.ino(), modified: m.modified().ok()? })
    }
}

/// `installed` is another file than `running`, and not older: a rebuild or
/// an update. (An older one, e.g. after checking out an older branch, isn't
/// worth restarting for.)
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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, installed);
        None
    }
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
    /// It was too old to keep its sessions over a restart (or spoke no
    /// protocol this build does) and was stopped: they ended. The next
    /// connection starts a new one.
    Stopped(Stopped),
}

/// The running local daemon can restart without ending its sessions.
pub fn can_keep_sessions(protocol: u32) -> bool {
    protocol >= goatty_proto::RESTART_SINCE
}

/// Restart the local daemon as `exe` (default: the goattyd it was started
/// as) and wait until it answers again. Sessions carry over when the daemon
/// can do that (protocol v10 on). One that can't is stopped only with
/// `force`, which ends its sessions; without it that's an error.
pub fn restart_daemon(exe: Option<&Path>, force: bool) -> Result<Restarted> {
    let primary = goatty_proto::socket_path();
    let mut sock = primary.clone();
    // Any daemon a restart keeps the sessions of, also one older than this
    // build speaks (`ADMIN_MIN_PROTOCOL_VERSION`): the new one takes them over.
    let admin = |p: &Path| Control::connect_admin(p, "goatty-restart", None);
    let mut control = match admin(&primary) {
        Ok(c) => c,
        Err(e) => {
            if UnixStream::connect(&primary).is_err()
                && let Some(legacy) = goatty_proto::legacy_socket_path()
                && UnixStream::connect(&legacy).is_ok()
            {
                sock = legacy;
                match admin(&sock) {
                    Ok(c) => c,
                    Err(le) => {
                        if UnixStream::connect(&sock).is_err() {
                            return Ok(Restarted::NotRunning);
                        }
                        anyhow::ensure!(force, "{le:#}");
                        return stop_daemon(true).map(Restarted::Stopped);
                    }
                }
            } else {
                if UnixStream::connect(&primary).is_err() {
                    return Ok(Restarted::NotRunning);
                }
                anyhow::ensure!(force, "{e:#}");
                return stop_daemon(true).map(Restarted::Stopped);
            }
        }
    };
    if !can_keep_sessions(control.protocol_version()) {
        let n = control.list_sessions()?.len();
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
    /// It didn't understand the request (`why`); systemd stopped the service.
    Service {
        why: String,
    },
    /// It didn't understand the request (`why`); it was signalled.
    Killed {
        pid: u32,
        why: String,
    },
}

/// The process exists and isn't a zombie. Reaps it if it was our child
/// (goatty started the daemon it now stops).
fn alive(pid: u32) -> bool {
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
        libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) == size && info.pbi_status != libc::SZOMB
    };
    // Elsewhere: whether it can still be signalled.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    // SAFETY: plain syscall; signal 0 only checks.
    return unsafe { libc::kill(pid as i32, 0) } == 0;
}

/// The name of process `pid` (`/proc/<pid>/comm`; `proc_name` on macOS);
/// empty when it can't be read.
fn process_name(pid: u32) -> String {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is valid for writes of the size passed.
        let n = unsafe { libc::proc_name(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
        String::from_utf8_lossy(&buf[..(n.max(0) as usize).min(buf.len())]).into_owned()
    }
    #[cfg(not(target_os = "macos"))]
    std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default()
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

/// Stop the local daemon and wait until it's gone. This ends its sessions:
/// only after the user agreed (or when it has none).
///
/// It's asked first (`Shutdown`). With `force`, one that doesn't speak our
/// protocol is stopped anyway: through systemd/launchd when the user service
/// runs it, else SIGTERM (SIGKILL after 5 s) to the socket's owner once it's
/// checked to be a goattyd (`SO_PEERCRED` / `LOCAL_PEERPID`).
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
                && UnixStream::connect(&legacy).is_ok()
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
            if UnixStream::connect(&path).is_err()
                && let Some(legacy) = goatty_proto::legacy_socket_path()
                && legacy != path
                && UnixStream::connect(&legacy).is_ok()
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
    if UnixStream::connect(&path).is_err() {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return Ok(Stopped::NotRunning);
    }
    let pid = crate::control::socket_owner(&path)?;
    // Under socket activation the listener belongs to systemd/launchd, never kill that.
    let comm = process_name(pid);
    anyhow::ensure!(comm.starts_with("goattyd"), "{} belongs to pid {pid} ({}), not a goattyd", path.display(), comm.trim());
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_is_not_outdated_against_itself() {
        // Our own exe vs itself: same file.
        let exe = std::env::current_exe().unwrap();
        assert_eq!(outdated_than(std::process::id(), exe), None);
        assert!(alive(std::process::id()));
        assert!(!alive(u32::MAX >> 1));
    }
}
