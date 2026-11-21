//! What the next `goatty` reopens (#252): every terminal window that was
//! open when goatty quit, with its tabs on every host, not only this
//! computer's.
//!
//! Each goattyd keeps a window's layout of the tabs on it (`saved`), but
//! only goatty knows which goattyds a window had tabs on. So each window
//! records here which hosts it has tabs on, in tab order, with the id of
//! its layout on each, its size and the host of its active tab. The next
//! `goatty` (without `--ssh`, `--new` or `--attach`, with
//! `workspace.on_launch` = `reattach`) opens one window per record whose
//! process is gone, connects to each host and claims exactly that layout
//! there (`AttachMode::Layout`).
//!
//! Like a browser's session restore: closing one window while others stay
//! open forgets it (its sessions stay detached on their hosts, and the
//! sessions picker offers to restore its tabs); closing the last window
//! (quitting) keeps every window that was still open. A window whose tabs
//! were all closed or detached has nothing to record.
//!
//! State, not a setting: `$XDG_STATE_HOME/goatty/windows.json`. Several
//! goatty processes share the file: each one rewrites only its own records
//! (by pid), and a record is reopened only once its process is gone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use goatty_client::Endpoint;
use goatty_proto::LayoutId;
use gpui::{App, Global, WindowId};
use serde::{Deserialize, Serialize};

use super::saved::SavedWindow;
use crate::hosts::HostRef;

pub const VERSION: u32 = 1;

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

/// A host a window has tabs on, and the id of the layout of those tabs on
/// that host's goattyd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub host: SavedHost,
    pub layout: LayoutId,
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
    pub fn load(path: &Path) -> Self {
        let loaded: Self = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if loaded.version > VERSION {
            log::info!("{} has format {} (this goatty reads {VERSION}); ignoring it", path.display(), loaded.version);
            return Self::default();
        }
        loaded
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        goatty_config::store::write_text(path, &text).map(drop)
    }

    /// Take out the records of processes `alive` says are gone; the rest
    /// stay. Records without hosts are dropped.
    fn take_dead(&mut self, alive: impl Fn(u32) -> bool) -> Vec<WindowRecord> {
        let (dead, live): (Vec<_>, Vec<_>) = std::mem::take(&mut self.windows).into_iter().partition(|w| !alive(w.pid));
        self.windows = live;
        dead.into_iter().filter(|w| !w.hosts.is_empty()).collect()
    }

    /// Replace process `pid`'s records with `mine`.
    fn replace_own(&mut self, pid: u32, mine: impl IntoIterator<Item = WindowRecord>) {
        self.version = VERSION;
        self.windows.retain(|w| w.pid != pid);
        self.windows.extend(mine.into_iter().filter(|w| !w.hosts.is_empty()));
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
    let Ok(raw) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: signal 0 sends nothing; kill only checks that the process
    // exists and may be signalled.
    let exists = unsafe { libc::kill(raw, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists {
        return false;
    }
    // A reused pid is some other program.
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm.trim() == "goatty",
        Err(_) => !Path::new("/proc/self").exists(),
    }
}

/// The windows to reopen at launch: the records of goatty processes that
/// are gone, taken out of the file (so a second `goatty` doesn't reopen
/// them too).
pub fn take_for_launch() -> Vec<WindowRecord> {
    let Some(path) = state_file() else { return Vec::new() };
    let mut file = OpenWindows::load(&path);
    let taken = file.take_dead(alive);
    if !taken.is_empty() {
        file.version = VERSION;
        if let Err(e) = file.save(&path) {
            log::warn!("saving {}: {e}", path.display());
        }
        log::info!("reopening {} window(s) from the last run", taken.len());
    }
    taken
}

/// This process's terminal windows and their records.
#[derive(Default)]
pub struct Windows {
    records: HashMap<WindowId, WindowRecord>,
    /// Terminal windows that never had a record yet.
    open: Vec<WindowId>,
    writer: Option<mpsc::Sender<Vec<WindowRecord>>>,
}

impl Global for Windows {}

impl Windows {
    /// Write this process's records, off the UI thread, in order.
    fn write(&mut self) {
        let mine: Vec<WindowRecord> = self.records.values().cloned().collect();
        let writer = self.writer.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<Vec<WindowRecord>>();
            let _ = std::thread::Builder::new().name("reopen-writer".into()).spawn(move || {
                let Some(path) = state_file() else { return };
                while let Ok(mut mine) = rx.recv() {
                    // Only the newest matters.
                    while let Ok(newer) = rx.try_recv() {
                        mine = newer;
                    }
                    let mut file = OpenWindows::load(&path);
                    file.replace_own(std::process::id(), mine);
                    if let Err(e) = file.save(&path) {
                        log::warn!("saving {}: {e}", path.display());
                    }
                }
            });
            tx
        });
        let _ = writer.send(mine);
    }
}

/// A terminal window opened.
pub fn window_opened(cx: &mut App, id: WindowId) {
    let w = cx.default_global::<Windows>();
    if !w.open.contains(&id) {
        w.open.push(id);
    }
}

/// Window `id` has tabs on these hosts now (`None`: none it could reopen).
/// Written when it changed.
pub fn update(cx: &mut App, id: WindowId, record: Option<WindowRecord>) {
    let w = cx.default_global::<Windows>();
    let changed = match record {
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
    if !w.open.is_empty() && w.records.remove(&id).is_some() {
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

    fn rec(pid: u32, hosts: &[(SavedHost, LayoutId)]) -> WindowRecord {
        WindowRecord {
            pid,
            key: pid as u64 * 10,
            hosts: hosts.iter().map(|(h, l)| HostRecord { host: h.clone(), layout: *l }).collect(),
            active: 0,
            window: None,
        }
    }

    #[test]
    fn json_format() {
        let mut r = rec(7, &[(SavedHost::Local, 1), (SavedHost::Ssh { host: "prod".into() }, 2)]);
        r.hosts.push(HostRecord { host: SavedHost::Tailnet { dns: "pi.ts.net".into(), name: "pi".into() }, layout: 3 });
        r.active = 1;
        let file = OpenWindows { version: VERSION, windows: vec![r] };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains(r#"{"host":{"kind":"ssh","host":"prod"},"layout":2}"#), "{json}");
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
        assert_eq!(file.windows.iter().map(|w| (w.pid, w.hosts[0].layout)).collect::<Vec<_>>(), [(2, 6), (1, 8)]);
        // Process 2 is still running: only 1's window is reopened.
        let taken = file.take_dead(|pid| pid == 2);
        assert_eq!(taken.iter().map(|w| w.pid).collect::<Vec<_>>(), [1]);
        assert_eq!(file.windows.iter().map(|w| w.pid).collect::<Vec<_>>(), [2]);
        assert!(file.take_dead(|pid| pid == 2).is_empty());
    }

    #[test]
    fn unreadable_or_newer_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("windows.json");
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(OpenWindows::load(&path), OpenWindows::default());
        std::fs::write(&path, r#"{"version":99,"windows":[{"pid":1,"key":1,"hosts":[{"host":{"kind":"local"},"layout":1}]}]}"#).unwrap();
        assert_eq!(OpenWindows::load(&path), OpenWindows::default());
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
