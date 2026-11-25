//! What the next `goatty` reopens (#252): every terminal window that was
//! open when goatty quit, with its tabs on every host, not only this
//! computer's.
//!
//! Each goattyd keeps its workspaces' tabs (#215), but only goatty knows
//! which goattyds a window had tabs on. So each window records here which
//! hosts it has tabs on, in tab order, with the workspace it shows on each
//! and the tab it had active there, its size and the host of its active
//! tab: what's this window's alone and not the workspace's. The next
//! `goatty` (without `--ssh`, `--new` or `--attach`, with
//! `workspace.on_launch` = `reattach`) opens one window per record whose
//! process is gone, connects to each host and joins that workspace there
//! (`AttachMode::Restore`; the most recently active one when it's gone).
//!
//! Like a browser's session restore: closing one window while others stay
//! open forgets it (its workspaces stay on their hosts); closing the last
//! window (quitting) keeps every window that was still open. A window whose
//! tabs were all closed or detached has nothing to record.
//!
//! State, not a setting: `$XDG_STATE_HOME/goatty/windows.json`. Several
//! goatty processes share the file: each one rewrites only its own records
//! (by pid, under a lock on `windows.json.lock`), and a record is reopened
//! only once its process is gone. Writes happen off the UI thread; quitting
//! waits (briefly) for the last one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use goatty_client::Endpoint;
use goatty_proto::WorkspaceId;
use gpui::{App, Global, WindowId};
use serde::{Deserialize, Serialize};

use super::saved::SavedWindow;
use crate::hosts::HostRef;

/// v1 recorded window layouts from before workspaces; such a file is
/// dropped (#215: no migration).
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenWindows {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub windows: Vec<WindowRecord>,
}

/// One terminal window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowRecord {
    /// The goatty process that has (or had) it open.
    pub pid: u32,
    /// Which of that process's windows (random).
    pub key: u64,
    /// The hosts it has tabs on, in the order of their tabs.
    pub hosts: Vec<HostRecord>,
    /// Index into `hosts` of the active tab's host.
    #[serde(default)]
    pub active: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<SavedWindow>,
}

/// A host a window has tabs on: the workspace it shows there, and the tab
/// it had active there (each window has its own).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub host: SavedHost,
    pub workspace: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<u64>,
}

/// A host as [`HostRef`] names it, in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedHost {
    Local,
    Ssh { host: String },
    Tailnet { dns: String, name: String },
}

impl SavedHost {
    /// The host behind `endpoint`, when goatty can connect to it again by
    /// name (`--via` commands and a custom remote goattyd can't).
    pub fn of(endpoint: &Endpoint) -> Option<Self> {
        match endpoint {
            Endpoint::Local => Some(SavedHost::Local),
            Endpoint::Ssh(t) if t.remote_daemon.is_none() => Some(SavedHost::Ssh { host: t.host.clone() }),
            Endpoint::Tailnet(t) => Some(SavedHost::Tailnet { dns: t.dns.clone(), name: t.name.clone() }),
            _ => None,
        }
    }

    pub fn host_ref(&self) -> HostRef {
        match self {
            SavedHost::Local => HostRef::Local,
            SavedHost::Ssh { host } => HostRef::Ssh(host.clone()),
            SavedHost::Tailnet { dns, name } => HostRef::Tailnet { dns: dns.clone(), name: name.clone() },
        }
    }
}

impl OpenWindows {
    /// The file as it is (a newer or damaged one reads as empty).
    #[cfg(test)]
    pub fn load(path: &Path) -> Self {
        let loaded: Self = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if loaded.version != VERSION {
            log::info!("{} has format {} (this goatty reads {VERSION}); ignoring it", path.display(), loaded.version);
            return Self::default();
        }
        loaded
    }

    /// Written to a temporary file and renamed over it (no backups: it's
    /// rewritten on every change of a window's hosts).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        // The new directory entry too, so a power loss keeps the new file.
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Load, change and save the file at `path`, holding the lock that
    /// keeps other goatty processes from doing the same meanwhile.
    fn edit<T>(path: &Path, f: impl FnOnce(&mut Self) -> T) -> std::io::Result<T> {
        let _lock = FileLock::new(path)?;
        // Read once, under the lock: a file that can't be read is never
        // replaced (it'd lose the other processes' windows).
        let mut file = match std::fs::read(path) {
            Ok(bytes) => {
                let version = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|j| j["version"].as_u64());
                if let Some(v) = version.filter(|&v| v > u64::from(VERSION)) {
                    // A newer goatty's file: leave it alone.
                    return Err(std::io::Error::other(format!("{} has format {v}, newer than this goatty's", path.display())));
                }
                // Damaged, or from before workspaces (v1): start over.
                serde_json::from_slice::<Self>(&bytes).ok().filter(|f| f.version == VERSION).unwrap_or_default()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e),
        };
        let out = f(&mut file);
        file.version = VERSION;
        file.save(path)?;
        Ok(out)
    }

    /// Take out the records of processes `alive` says are gone; the rest
    /// stay. Records without hosts are dropped.
    fn take_dead(&mut self, alive: impl Fn(u32) -> bool) -> Vec<WindowRecord> {
        let (dead, live): (Vec<_>, Vec<_>) = std::mem::take(&mut self.windows).into_iter().partition(|w| !alive(w.pid));
        self.windows = live;
        dead.into_iter().filter(|w| !w.hosts.is_empty()).collect()
    }

    /// Reserve the records of processes `alive` says are gone for process
    /// `pid` (they become its records, so no other goatty reopens them, and
    /// they survive if it exits before reopening them). Returns them.
    fn reserve_dead(&mut self, pid: u32, alive: impl Fn(u32) -> bool) -> Vec<WindowRecord> {
        let mut taken = self.take_dead(alive);
        for w in &mut taken {
            w.pid = pid;
        }
        self.windows.extend(taken.iter().cloned());
        taken
    }

    /// Replace process `pid`'s records with `mine`.
    fn replace_own(&mut self, pid: u32, mine: impl IntoIterator<Item = WindowRecord>) {
        self.version = VERSION;
        self.windows.retain(|w| w.pid != pid);
        self.windows.extend(mine.into_iter().filter(|w| !w.hosts.is_empty()));
    }
}

/// An exclusive advisory lock on `<path>.lock`, released when dropped.
struct FileLock(std::fs::File);

impl FileLock {
    fn new(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path.with_extension("json.lock"))?;
        f.lock()?;
        Ok(Self(f))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The state file for this user. `None` without `$HOME`.
pub fn state_file() -> Option<PathBuf> {
    goatty_config::color_history::state_dir_from(|k| std::env::var(k).ok()).map(|d| d.join("windows.json"))
}

/// Is goatty process `pid` still running?
fn alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        let Ok(raw) = libc::pid_t::try_from(pid) else { return false };
        // SAFETY: signal 0 sends nothing; kill only checks that the process
        // exists and may be signalled.
        let exists = unsafe { libc::kill(raw, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !exists {
            return false;
        }
        // A reused pid is some other program.
        if Path::new("/proc/self").exists() {
            return std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim() == "goatty");
        }
        // No /proc (macOS): ask ps for the program's name.
        std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().rsplit('/').next() == Some("goatty"))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FALSE, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        };

        // SAFETY: plain Win32 call; handle is validated and closed before return.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0u32;
        // SAFETY: `handle` is open and `exit_code` is a valid out-pointer.
        let still_running = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) } != 0 && exit_code == STILL_ACTIVE as u32;
        let is_goatty = if still_running {
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            // SAFETY: `handle` is open and `buf` has space for `len` u16 units.
            if unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &raw mut len) } != 0 && len > 0 {
                let image = String::from_utf16_lossy(&buf[..len as usize]);
                image.rsplit(['\\', '/']).next().is_some_and(|name| name.eq_ignore_ascii_case("goatty.exe"))
            } else {
                false
            }
        } else {
            false
        };
        // SAFETY: `handle` is owned exclusively by this scope.
        unsafe {
            CloseHandle(handle);
        }
        is_goatty
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// The windows to reopen at launch: the records of goatty processes that
/// are gone, reserved for this one (so a second `goatty` doesn't reopen
/// them too; each stays in the file until its window records itself).
pub fn take_for_launch() -> Vec<WindowRecord> {
    let Some(path) = state_file() else { return Vec::new() };
    match OpenWindows::edit(&path, |file| file.reserve_dead(std::process::id(), alive)) {
        Ok(taken) => {
            if !taken.is_empty() {
                log::info!("reopening {} window(s) from the last run", taken.len());
            }
            taken
        }
        Err(e) => {
            log::warn!("reading {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Drop the records of goatty processes that are gone, unread: goatty was
/// set not to reopen windows (`workspace.on_launch`), so they'd only pile up.
pub fn forget_dead() {
    let Some(path) = state_file() else { return };
    if path.exists()
        && let Err(e) = OpenWindows::edit(&path, |file| drop(file.take_dead(alive)))
    {
        log::warn!("saving {}: {e}", path.display());
    }
}

/// What the writer thread is asked to do.
enum Write {
    Records(Vec<WindowRecord>),
    /// Answer once everything before it is written.
    Flush(flume::Sender<()>),
}

/// This process's terminal windows and their records.
#[derive(Default)]
pub struct Windows {
    records: HashMap<WindowId, WindowRecord>,
    /// Records of the last run's windows being reopened (by key), written
    /// until their new window records itself.
    reserved: Vec<WindowRecord>,
    /// The terminal windows open now.
    open: Vec<WindowId>,
    /// goatty is quitting: windows closing now stay recorded.
    quitting: bool,
    writer: Option<mpsc::Sender<Write>>,
}

impl Global for Windows {}

impl Windows {
    fn writer(&mut self) -> &mpsc::Sender<Write> {
        self.writer.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<Write>();
            let _ = std::thread::Builder::new().name("reopen-writer".into()).spawn(move || {
                let path = state_file();
                let mut pending: Option<Vec<WindowRecord>> = None;
                while let Ok(first) = rx.recv() {
                    // Only the newest records matter; flushes wait for them.
                    let mut flushes = Vec::new();
                    for w in std::iter::once(first).chain(rx.try_iter()) {
                        match w {
                            Write::Records(r) => pending = Some(r),
                            Write::Flush(done) => flushes.push(done),
                        }
                    }
                    if let (Some(mine), Some(path)) = (pending.take(), &path)
                        && let Err(e) = OpenWindows::edit(path, |file| file.replace_own(std::process::id(), mine))
                    {
                        log::warn!("saving {}: {e}", path.display());
                    }
                    for done in flushes {
                        let _ = done.send(());
                    }
                }
            });
            tx
        })
    }

    /// Write this process's records, off the UI thread, in order.
    fn write(&mut self) {
        let mine: Vec<WindowRecord> = self.records.values().chain(self.reserved.iter()).cloned().collect();
        let _ = self.writer().send(Write::Records(mine));
    }
}

/// Before goatty quits (the windows still open stay recorded): wait, at
/// most until GPUI's quit timeout, for the last write.
pub fn init(cx: &mut App) {
    cx.on_app_quit(|cx| {
        let w = cx.default_global::<Windows>();
        w.quitting = true;
        let done = w.writer.as_ref().map(|writer| {
            let (tx, rx) = flume::bounded(1);
            let _ = writer.send(Write::Flush(tx));
            rx
        });
        async move {
            if let Some(rx) = done {
                let _ = rx.recv_async().await;
            }
        }
    })
    .detach();
}

/// A terminal window opened.
pub fn window_opened(cx: &mut App, id: WindowId) {
    let w = cx.default_global::<Windows>();
    if !w.open.contains(&id) {
        w.open.push(id);
    }
    // A window that closed as the last one (the settings window kept goatty
    // running) isn't the last one any more.
    let before = w.records.len();
    let open = w.open.clone();
    w.records.retain(|id, _| open.contains(id));
    if w.records.len() != before {
        w.write();
    }
}

/// Window `record` is being reopened: it stays recorded until that window
/// records itself (with the same key).
pub fn reserve(cx: &mut App, record: WindowRecord) {
    let w = cx.default_global::<Windows>();
    w.reserved.retain(|r| r.key != record.key);
    w.reserved.push(record);
}

/// Window `id` (record key `key`) has tabs on these hosts now (`None`: none
/// it could reopen). Written when it changed.
pub fn update(cx: &mut App, id: WindowId, key: u64, record: Option<WindowRecord>) {
    let w = cx.default_global::<Windows>();
    if w.quitting || !w.open.contains(&id) {
        return;
    }
    let reserved = w.reserved.len();
    w.reserved.retain(|r| r.key != key);
    let changed = w.reserved.len() != reserved;
    let changed = changed
        | match record {
            Some(r) => w.records.insert(id, r.clone()).as_ref() != Some(&r),
            None => w.records.remove(&id).is_some(),
        };
    if changed {
        w.write();
    }
}

/// Terminal window `id` closed: forgotten unless it was the last one (then
/// goatty quits, and the next one reopens it).
pub fn window_closed(cx: &mut App, id: WindowId) {
    let Some(w) = cx.try_global::<Windows>() else { return };
    if !w.open.contains(&id) {
        return;
    }
    let w = cx.global_mut::<Windows>();
    w.open.retain(|&o| o != id);
    if !w.quitting && !w.open.is_empty() && w.records.remove(&id).is_some() {
        w.write();
    }
}

/// A record's process key: random per window.
pub fn new_key() -> u64 {
    super::saved::new_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, hosts: &[(SavedHost, WorkspaceId)]) -> WindowRecord {
        WindowRecord {
            pid,
            key: pid as u64 * 10,
            hosts: hosts.iter().map(|(h, w)| HostRecord { host: h.clone(), workspace: *w, active_tab: None }).collect(),
            active: 0,
            window: None,
        }
    }

    #[test]
    fn json_format() {
        let mut r = rec(7, &[(SavedHost::Local, 1), (SavedHost::Ssh { host: "prod".into() }, 2)]);
        r.hosts.push(HostRecord {
            host: SavedHost::Tailnet { dns: "pi.ts.net".into(), name: "pi".into() },
            workspace: 3,
            active_tab: Some(9),
        });
        r.active = 1;
        let file = OpenWindows { version: VERSION, windows: vec![r] };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains(r#"{"host":{"kind":"ssh","host":"prod"},"workspace":2}"#), "{json}");
        assert!(json.contains(r#""workspace":3,"active_tab":9"#), "{json}");
        assert!(json.contains(r#"{"kind":"local"}"#) && json.contains(r#""kind":"tailnet","dns":"pi.ts.net","name":"pi""#), "{json}");
        assert_eq!(serde_json::from_str::<OpenWindows>(&json).unwrap(), file);
    }

    #[test]
    fn processes_keep_their_own_records_and_dead_ones_are_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/windows.json");
        let mut file = OpenWindows::load(&path);
        assert_eq!(file, OpenWindows::default());
        file.replace_own(1, [rec(1, &[(SavedHost::Local, 5)]), rec(1, &[])]);
        file.replace_own(2, [rec(2, &[(SavedHost::Ssh { host: "box".into() }, 6)])]);
        file.save(&path).unwrap();
        let mut file = OpenWindows::load(&path);
        assert_eq!(file.windows.len(), 2, "a window without hosts isn't recorded");
        // Process 1 updates its windows; 2's stay.
        file.replace_own(1, [rec(1, &[(SavedHost::Local, 8)])]);
        assert_eq!(file.windows.iter().map(|w| (w.pid, w.hosts[0].workspace)).collect::<Vec<_>>(), [(2, 6), (1, 8)]);
        // Process 2 is still running: only 1's window is reopened, by 3,
        // which holds it now (so nobody else reopens it, and it's kept if 3
        // exits before its window records itself).
        let taken = file.reserve_dead(3, |pid| pid == 2 || pid == 3);
        assert_eq!(taken.iter().map(|w| (w.pid, w.key)).collect::<Vec<_>>(), [(3, 10)]);
        assert_eq!(file.windows.iter().map(|w| w.pid).collect::<Vec<_>>(), [2, 3]);
        assert!(file.reserve_dead(4, |pid| pid == 2 || pid == 3).is_empty());
        assert_eq!(file.reserve_dead(4, |pid| pid == 2).len(), 1, "3 exited first");
        assert!(file.take_dead(|_| true).is_empty());
    }

    #[test]
    fn unreadable_or_newer_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("windows.json");
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(OpenWindows::load(&path), OpenWindows::default());
        std::fs::write(&path, r#"{"version":99,"windows":[{"pid":1,"key":1,"hosts":[{"host":{"kind":"local"},"workspace":1}]}]}"#).unwrap();
        assert_eq!(OpenWindows::load(&path), OpenWindows::default());
        // ... and not overwritten.
        assert!(OpenWindows::edit(&path, |f| f.replace_own(1, [])).is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains(r#""version":99"#));
        // A file from before workspaces (window layouts) is dropped.
        std::fs::write(&path, r#"{"version":1,"windows":[{"pid":1,"key":1,"hosts":[{"host":{"kind":"local"},"layout":1}]}]}"#).unwrap();
        assert_eq!(OpenWindows::load(&path), OpenWindows::default());
        assert!(OpenWindows::edit(&path, |f| f.take_dead(|_| false)).unwrap().is_empty());
    }

    #[test]
    fn hosts_from_endpoints() {
        assert_eq!(SavedHost::of(&Endpoint::Local), Some(SavedHost::Local));
        let mut t = goatty_client::SshTarget::new("prod");
        assert_eq!(SavedHost::of(&Endpoint::Ssh(t.clone())), Some(SavedHost::Ssh { host: "prod".into() }));
        assert_eq!(SavedHost::Ssh { host: "prod".into() }.host_ref(), HostRef::Ssh("prod".into()));
        t.remote_daemon = Some("/opt/goattyd".into());
        assert_eq!(SavedHost::of(&Endpoint::Ssh(t)), None);
        assert!(alive(std::process::id()));
    }
}
