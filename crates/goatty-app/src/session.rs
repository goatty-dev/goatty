//! Where a pane's bytes come from: an in-process PTY (`--local`) or a
//! goattyd session over a [`Connection`], local or remote. Both feed a
//! client-side [`Terminal`] that the renderer reads; the rest of the UI
//! doesn't care which backend is in use.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use goatty_client::{AttachRequest, Connection, History, PaneEvent, PaneSink, TopLineCause};
use goatty_config::LocalEcho;
use goatty_proto::WinSize;
use goatty_shell::HostService;
use goatty_term::predict::Predictor;
use goatty_term::process::ForegroundWatch;
use goatty_term::pty::{Pty, PtyInput, ShellCommand};
use goatty_term::{Frame, HistoryRows, Palette, TermMode, TermSize, Terminal, UiEvent};
use parking_lot::Mutex;

/// One view's wake-ups: many output chunks → one UI notification per frame.
pub struct WakeListener {
    pending: AtomicBool,
    tx: flume::Sender<()>,
}

impl WakeListener {
    /// Re-arm the wake-up. Call when handling a wake, before reading state.
    pub fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

/// Wakes every view of a session (two windows of this process can show the
/// same one: a workspace shown in both, #215), each at most once per frame.
#[derive(Default)]
pub(crate) struct Waker {
    listeners: Mutex<Vec<std::sync::Weak<WakeListener>>>,
}

impl Waker {
    pub(crate) fn wake(&self) {
        let mut listeners = self.listeners.lock();
        listeners.retain(|l| match l.upgrade() {
            Some(l) => {
                if !l.pending.swap(true, Ordering::AcqRel) {
                    let _ = l.tx.send(());
                }
                true
            }
            None => false,
        });
    }

    /// A new view's wake-ups. It's woken once right away, for what
    /// happened before it existed (the session may even have ended).
    pub(crate) fn subscribe(&self) -> (Arc<WakeListener>, flume::Receiver<()>) {
        let (tx, rx) = flume::unbounded();
        let _ = tx.send(());
        let l = Arc::new(WakeListener { pending: AtomicBool::new(true), tx });
        self.listeners.lock().push(Arc::downgrade(&l));
        (l, rx)
    }
}

/// The terminal's UI events, kept for every view to read at its own pace.
#[derive(Default)]
struct UiLog {
    events: std::collections::VecDeque<(u64, UiEvent)>,
    /// The number the next event gets.
    next: u64,
    /// The newest title (a view that comes later starts with it).
    title: Option<String>,
}

/// Events kept for views that haven't read them yet.
const UI_LOG: usize = 512;

/// The sessions panes show, by connection and session: a window that shows
/// a session another window of this process shows already gets the same
/// one (one connection carries one subscription per session). Claiming a
/// view of one ([`shared`]) and letting go of the last ([`Session::release_view`])
/// both happen under this lock, so a session is never detached while it's
/// being handed to another window.
static SHARED: Mutex<Vec<(usize, u64, std::sync::Weak<Session>)>> = Mutex::new(Vec::new());

/// The session `id` on `conn` a window of this process shows, with a view
/// claimed for the caller (a pane takes it over, or the caller lets go).
fn shared(conn: &Connection, id: u64) -> Option<Arc<Session>> {
    let key = conn.key();
    let mut shared = SHARED.lock();
    shared.retain(|(_, _, w)| w.strong_count() > 0);
    let s = shared
        .iter()
        .find(|(c, s, _)| *c == key && *s == id)
        .and_then(|(_, _, w)| w.upgrade())
        .filter(|s| !s.exited.load(Ordering::Acquire) && s.views.load(Ordering::Acquire) > 0)?;
    s.views.fetch_add(1, Ordering::AcqRel);
    Some(s)
}

/// [`Session::daemon_attach_many`]'s lock for connection `conn`.
fn attach_lock(conn: &Connection) -> Arc<Mutex<()>> {
    static LOCKS: Mutex<Vec<(usize, std::sync::Weak<Mutex<()>>)>> = Mutex::new(Vec::new());
    let key = conn.key();
    let mut locks = LOCKS.lock();
    locks.retain(|(_, w)| w.strong_count() > 0);
    if let Some(l) = locks.iter().find(|(k, _)| *k == key).and_then(|(_, w)| w.upgrade()) {
        return l;
    }
    let l = Arc::new(Mutex::new(()));
    locks.push((key, Arc::downgrade(&l)));
    l
}

fn share(s: &Arc<Session>) {
    if let (Some(conn), Some(id)) = (s.connection(), s.daemon_id()) {
        let key = conn.key();
        let mut shared = SHARED.lock();
        shared.retain(|(c, i, w)| w.strong_count() > 0 && !(*c == key && *i == id));
        shared.push((key, id, Arc::downgrade(s)));
    }
}

/// What a window shows of a host it connects to (#215): it always joins one
/// of the host's workspaces; these say which, and what it adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachMode {
    /// Workspace `Some(id)` while it exists, else the most recently active
    /// one; a fresh tab when it has none.
    Join(Option<goatty_proto::WorkspaceId>),
    /// A window reopened from the last run: like `Join`, but nothing left
    /// to show is `saved::Startup::Nothing` rather than a fresh tab.
    Restore(goatty_proto::WorkspaceId),
    /// The most recently active workspace, with a fresh tab added
    /// (`--new`, `workspace.on_launch = new`, a new tab on a host).
    New,
    /// The workspace that shows session `id` (else the most recently
    /// active one), with that session shown.
    Id(u64),
    /// The most recently active workspace, with every detached session no
    /// workspace shows added as a tab each.
    AllDetached,
    /// Workspace `id` with its detached tabs brought back.
    Undetach(goatty_proto::WorkspaceId),
}

impl AttachMode {
    /// Restoring something that was there (the connect screen says so).
    pub fn restores(&self) -> bool {
        matches!(self, AttachMode::Restore(_) | AttachMode::AllDetached | AttachMode::Undetach(_))
    }
}

/// Link status shown in the title bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    Connected,
    Reconnecting,
    /// Automatic reconnects stopped: ssh needs the user (Enter reconnects).
    NeedsReconnect,
    Lost(String),
}

enum Backend {
    /// Input has its own handle, so a write blocked on a program that
    /// doesn't read never holds up `kill` and the rest (#233).
    Local {
        pty: Mutex<Pty>,
        input: Mutex<PtyInput>,
    },
    Daemon {
        conn: Arc<Connection>,
        id: AtomicU64,
        sink: Arc<dyn PaneSink>,
    },
}

pub struct Session {
    pub terminal: Terminal,
    backend: Backend,
    ui_events: flume::Receiver<UiEvent>,
    ui_log: Mutex<UiLog>,
    /// Views of it (panes in any window of this process, or ones being
    /// handed to a pane): one from the start, for whoever made or attached
    /// it; the last one to let go detaches it ([`Self::release_view`]).
    views: std::sync::atomic::AtomicUsize,
    /// A view read its UI events already (a later one starts at the end).
    ui_started: AtomicBool,
    /// The view whose pane size the session follows (0: whichever lays
    /// out first), so two views of different sizes don't take turns.
    size_owner: AtomicU64,
    pub exited: AtomicBool,
    /// The connection came back after a drop (the view exchanges the shared
    /// shell config again, #313).
    pub reconnected: AtomicBool,
    exit_code: Mutex<Option<u32>>,
    pub bytes_in: AtomicU64,
    pub status: Mutex<LinkStatus>,
    /// `user@host` for remote daemons, `None` locally.
    pub host_label: Option<String>,
    /// Connectable SSH host destination (`SshTarget::host` or Tailnet MagicDNS), `None` locally or for `--via`.
    pub ssh_host: Option<String>,
    /// Authoritative shell directory from `ServerMsg::Cwd` (daemon v13+).
    remote_cwd: Mutex<Option<String>>,
    /// Bumped whenever `PaneEvent::Cwd` updates `remote_cwd` (each view
    /// keeps the last one it saw).
    cwd_gen: AtomicU64,
    /// The foreground program (`zsh`, `nvim`, …), when known.
    pub process: Mutex<Option<String>>,
    /// Local sessions watch their own PTY; daemon sessions hear it from goattyd.
    fg: std::sync::OnceLock<ForegroundWatch>,
    waker: Waker,
    /// A `fetch_history` is on its way.
    fetching: AtomicBool,
    /// Local echo prediction (#16). Lock order: `view`, then
    /// `predictor`, then the terminal.
    predictor: Mutex<Predictor>,
    /// `ssh.local_echo` as `LocalEcho as u8`.
    local_echo: AtomicU8,
    /// The pane's size in cells, as last given to [`Self::resize`]. The
    /// terminal has it too unless another client controls the session's size.
    view: Mutex<TermSize>,
    /// Another client controls the session's size (#12, protocol v14): the
    /// terminal stays at the session's size and the pane letterboxes it.
    watching: AtomicBool,
}

/// Makes the sessions for new tabs and panes: on a goattyd connection, as
/// local PTYs (`--local`), or `ssh host` again for plain SSH tabs.
#[derive(Clone)]
pub enum Spawner {
    Local(Arc<HostService>),
    Daemon(Arc<Connection>),
    /// Plain SSH: `ssh host` in a session of `base` (the local goattyd, or a
    /// local PTY when it can't).
    Plain {
        host: String,
        base: Box<Spawner>,
    },
}

impl Spawner {
    /// Where this tab's host settings live (plain SSH: this computer's).
    pub fn host_link(&self) -> crate::settings_window::HostLink {
        match self {
            Spawner::Local(host) => crate::settings_window::HostLink::Local(host.clone()),
            Spawner::Daemon(conn) => crate::settings_window::HostLink::Daemon(conn.clone()),
            Spawner::Plain { base, .. } => base.host_link(),
        }
    }

    /// Whether sessions from `self` and `other` live in the same place, so
    /// their panes can share a tab.
    pub fn same_host(&self, other: &Spawner) -> bool {
        match (self, other) {
            (Spawner::Local(a), Spawner::Local(b)) => Arc::ptr_eq(a, b),
            (Spawner::Daemon(a), Spawner::Daemon(b)) => Arc::ptr_eq(a, b),
            (Spawner::Plain { host: a, base: x }, Spawner::Plain { host: b, base: y }) => a == b && x.same_host(y),
            _ => false,
        }
    }

    /// The goattyd connection its sessions live on, if any.
    pub fn connection(&self) -> Option<&Arc<Connection>> {
        match self {
            Spawner::Local(_) => None,
            Spawner::Daemon(conn) => Some(conn),
            Spawner::Plain { base, .. } => base.connection(),
        }
    }

    /// A new shell, started in `cwd` when given (a path on the session's
    /// host). Blocks on the daemon's reply: call it off the UI thread.
    pub fn spawn(&self, size: TermSize, cwd: Option<String>) -> Result<Arc<Session>> {
        match self {
            Spawner::Local(host) => Session::local(size, host, cwd),
            Spawner::Daemon(conn) => Session::daemon(conn.clone(), size, None, cwd),
            Spawner::Plain { host, base } => {
                let argv = plain_ssh_argv(host);
                match &**base {
                    Spawner::Daemon(conn) => Session::daemon_run(conn.clone(), size, argv, None),
                    _ => Session::local_command(size, None, argv, None),
                }
            }
        }
    }

    /// Launch `argv` directly (no shell) on this host in `cwd` (#241).
    /// Blocks on the daemon's reply: call it off the UI thread.
    pub fn spawn_command(&self, size: TermSize, argv: Vec<String>, cwd: Option<String>) -> Result<Arc<Session>> {
        match self {
            Spawner::Local(host) => Session::local_command(size, Some(host), argv, cwd),
            Spawner::Daemon(conn) => Session::daemon_run(conn.clone(), size, argv, cwd),
            Spawner::Plain { host, .. } => {
                anyhow::bail!("Can't launch programs directly on plain SSH ({host}); connect with goattyd instead.")
            }
        }
    }

    /// This host's home directory, for expanding `~` in direct-launch arguments
    /// off the UI thread.
    pub fn resolve_home(&self) -> Option<String> {
        match self {
            Spawner::Local(_) => std::env::var("HOME").ok().filter(|h| !h.is_empty()),
            Spawner::Daemon(conn) => {
                if conn.endpoint().is_local() {
                    std::env::var("HOME").ok().filter(|h| !h.is_empty())
                } else if conn.supports_remote_files() {
                    conn.stat_path(None, None, vec!["~".into()]).ok().flatten().map(|info| info.path)
                } else {
                    None
                }
            }
            Spawner::Plain { .. } => None,
        }
    }
}

/// `ssh host`, for plain SSH sessions.
pub fn plain_ssh_argv(host: &str) -> Vec<String> {
    vec![goatty_client::ssh_program(), "--".into(), host.to_string()]
}

/// Input that is an Enter press: a CR, or the kitty protocol's Enter
/// (`CSI 13 u`, with modifiers or as a repeat, not a release) when the
/// program asked for every key as an escape.
fn is_enter(bytes: &[u8]) -> bool {
    if bytes.contains(&b'\r') {
        return true;
    }
    let Some(rest) = bytes.strip_prefix(b"\x1b[13").and_then(|r| r.strip_suffix(b"u")) else { return false };
    rest.is_empty() || (rest.starts_with(b";") && !rest.ends_with(b":3"))
}

fn palette_list(p: &Palette) -> Vec<u32> {
    let mut v: Vec<u32> = p.ansi.iter().map(|c| c.to_u32()).collect();
    v.extend([p.foreground, p.background, p.cursor, p.selection_fg, p.selection_bg].map(|c| c.to_u32()));
    v
}

fn win(size: TermSize) -> WinSize {
    WinSize { cols: size.cols, rows: size.rows, cell_width: size.cell_width, cell_height: size.cell_height }
}

/// Adapter: the connection's reader thread feeds our mirror terminal.
struct Sink(std::sync::Weak<Session>);

impl PaneSink for Sink {
    fn feed(&self, bytes: &[u8]) {
        if let Some(s) = self.0.upgrade() {
            s.ingest(bytes);
        }
    }

    fn event(&self, ev: PaneEvent) {
        let Some(s) = self.0.upgrade() else { return };
        match ev {
            PaneEvent::Resynced { skipped } => log::debug!("resynced, {skipped} bytes skipped"),
            PaneEvent::Disconnected(why) => {
                *s.status.lock() = LinkStatus::Reconnecting;
                s.predictor.lock().reset();
                // Shown locally only: the reattach snapshot starts with RIS
                // and wipes this line again.
                s.terminal.feed(format!("\r\n\x1b[0;2m[goatty: connection lost ({why}); reconnecting…]\x1b[0m").as_bytes());
            }
            PaneEvent::NeedsReconnect(why) => {
                *s.status.lock() = LinkStatus::NeedsReconnect;
                s.terminal.feed(
                    format!("\r\n\x1b[0;33m[goatty: can't reconnect without asking ({why}); press Enter to reconnect]\x1b[0m").as_bytes(),
                );
            }
            PaneEvent::Reconnected => {
                s.predictor.lock().reset();
                *s.status.lock() = LinkStatus::Connected;
                s.reconnected.store(true, Ordering::Release);
                // A daemon from before v14 says nothing about control: the
                // pane sets the size again.
                if s.connection().is_some_and(|c| !c.supports_size_control()) {
                    s.set_controlling();
                }
            }
            PaneEvent::SizeControl { cols, rows, controlling } => {
                if controlling {
                    s.set_controlling();
                } else {
                    let view = s.view.lock();
                    s.watching.store(true, Ordering::Release);
                    if s.terminal.size() != (cols, rows) {
                        s.predictor.lock().reset();
                        s.terminal.resize(TermSize { cols, rows, ..*view });
                    }
                }
            }
            PaneEvent::Lost(why) => {
                s.terminal.feed(format!("\r\n\x1b[0;31m[goatty: session lost: {why}]\x1b[0m\r\n").as_bytes());
                *s.status.lock() = LinkStatus::Lost(why);
            }
            PaneEvent::Exited(code) => {
                *s.exit_code.lock() = code;
                s.exited.store(true, Ordering::Release);
            }
            PaneEvent::Process(name) => *s.process.lock() = name,
            PaneEvent::Cwd(cwd) => {
                *s.remote_cwd.lock() = cwd;
                s.cwd_gen.fetch_add(1, Ordering::AcqRel);
            }
        }
        s.waker.wake();
    }

    fn top_line(&self, line: u64, cause: TopLineCause) {
        if let Some(s) = self.0.upgrade() {
            let cause = match cause {
                TopLineCause::Attach => goatty_term::TopLineCause::Attach,
                TopLineCause::Resync => goatty_term::TopLineCause::Resync,
                TopLineCause::Resize => goatty_term::TopLineCause::Resize,
            };
            s.terminal.set_top_line(line, cause);
        }
    }

    fn history(&self, history: Option<History>) {
        let Some(s) = self.0.upgrade() else { return };
        if let Some(h) = history {
            let rows = HistoryRows { first: h.first, oldest: h.oldest, data: h.data };
            if !s.terminal.insert_history(h.before, &rows) {
                log::debug!("history before line {} no longer fits; asking again", h.before);
            }
        }
        s.fetching.store(false, Ordering::Release);
        // Redraw, which also asks for more if the viewport is still near a gap.
        s.waker.wake();
    }
}

impl Session {
    fn new(
        terminal: Terminal,
        ui: flume::Receiver<UiEvent>,
        backend: Backend,
        host_label: Option<String>,
        ssh_host: Option<String>,
    ) -> Self {
        let (cols, rows) = terminal.size();
        Self {
            view: Mutex::new(TermSize { cols, rows, cell_width: 0, cell_height: 0 }),
            watching: AtomicBool::new(false),
            terminal,
            backend,
            ui_events: ui,
            ui_log: Mutex::default(),
            views: std::sync::atomic::AtomicUsize::new(1),
            ui_started: AtomicBool::new(false),
            size_owner: AtomicU64::new(0),
            exited: AtomicBool::new(false),
            reconnected: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            bytes_in: AtomicU64::new(0),
            status: Mutex::new(LinkStatus::Connected),
            host_label,
            ssh_host,
            remote_cwd: Mutex::new(None),
            cwd_gen: AtomicU64::new(0),
            process: Mutex::new(None),
            fg: std::sync::OnceLock::new(),
            waker: Waker::default(),
            fetching: AtomicBool::new(false),
            predictor: Mutex::new(Predictor::new()),
            local_echo: AtomicU8::new(LocalEcho::default() as u8),
        }
    }

    /// A shell owned by this process (`--local`): no daemon, so it ends
    /// with the window. Same shell choice and managed zsh as goattyd.
    pub fn local(size: TermSize, host: &HostService, cwd: Option<String>) -> Result<Arc<Self>> {
        let mut launch = host.launcher.launch(&host.settings());
        // This window offers the shell setup: the shell's own hint stays quiet.
        goatty_shell::setup::gui_offers_setup(&mut launch);
        let mut cmd = ShellCommand::new(launch.program, launch.args, launch.env);
        if let Some(cwd) = cwd.filter(|c| std::path::Path::new(c).is_dir()) {
            cmd.cwd = Some(cwd.into());
        }
        Self::local_pty(size, cmd, launch.managed.then_some(host))
    }

    /// `argv` in a PTY owned by this process (when no daemon can run it, or
    /// `--local`). When `host` is given, resolves the executable and sets the
    /// host's managed `PATH` and plugin/imported environment (#241).
    pub fn local_command(size: TermSize, host: Option<&HostService>, mut argv: Vec<String>, cwd: Option<String>) -> Result<Arc<Self>> {
        let mut cmd = match host {
            Some(host) => {
                let launch = host.launcher.launch_command(&host.settings(), argv, cwd.as_deref())?;
                ShellCommand::new(launch.program, launch.args, launch.env)
            }
            None => {
                anyhow::ensure!(!argv.is_empty(), "nothing to run");
                let program = argv.remove(0);
                ShellCommand::new(program, argv, Vec::new())
            }
        };
        if let Some(cwd) = cwd.filter(|c| std::path::Path::new(c).is_dir()) {
            cmd.cwd = Some(cwd.into());
        }
        Self::local_pty(size, cmd, None)
    }

    /// `managed`: goatty's zsh, tracked by `host` for reloads.
    fn local_pty(size: TermSize, cmd: ShellCommand, managed: Option<&HostService>) -> Result<Arc<Self>> {
        let (pty, reader) = Pty::spawn(&cmd, size).with_context(|| format!("couldn't start {}", cmd.program))?;
        let managed_pid = pty.pid().filter(|_| managed.is_some());
        let host = managed.cloned();
        if let (Some(pid), Some(host)) = (managed_pid, &host) {
            host.track_shell(pid);
        }
        let input = Mutex::new(pty.input().context("duplicate the PTY for input")?);
        let (terminal, chans) = Terminal::new(size, true, Arc::new(Palette::default()));
        let s = Arc::new(Self::new(terminal, chans.ui, Backend::Local { pty: Mutex::new(pty), input }, None, None));
        let probe = {
            let weak = Arc::downgrade(&s);
            move || match &weak.upgrade()?.backend {
                Backend::Local { pty, .. } => pty.lock().foreground_name(),
                Backend::Daemon { .. } => None,
            }
        };
        let on_change = {
            let weak = Arc::downgrade(&s);
            move |name| {
                if let Some(s) = weak.upgrade() {
                    *s.process.lock() = name;
                    s.waker.wake();
                }
            }
        };
        let _ = s.fg.set(ForegroundWatch::spawn("pty-fg".into(), probe, on_change)?);
        let weak = Arc::downgrade(&s);
        std::thread::Builder::new().name("pty-replies".into()).spawn(move || {
            while let Ok(bytes) = chans.replies.recv() {
                let Some(s) = weak.upgrade() else { break };
                s.write_raw(&bytes);
            }
        })?;
        let s2 = s.clone();
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            let mut reader = reader;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        s2.ingest(&buf[..n]);
                        if let Some(fg) = s2.fg.get() {
                            fg.poke();
                        }
                    }
                }
            }
            if let (Some(pid), Some(host)) = (managed_pid, &host) {
                host.untrack_shell(pid);
            }
            if let Backend::Local { pty, .. } = &s2.backend {
                // On Unix, PTY EOF can arrive slightly before the child is
                // reapable; poll briefly so immediate non-zero exits still
                // report their status before `exited` is published.
                let mut code = None;
                for i in 0..40 {
                    if let Some(c) = pty.lock().try_wait() {
                        code = Some(c);
                        break;
                    }
                    if i + 1 < 40 {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                *s2.exit_code.lock() = code;
            }
            s2.exited.store(true, Ordering::Release);
            s2.waker.wake();
        })?;
        Ok(s)
    }

    fn daemon_host_meta(conn: &Connection) -> (Option<String>, Option<String>) {
        if conn.endpoint().is_local() {
            return (None, None);
        }
        let h = conn.host();
        let host_label = Some(format!("{}@{}", h.user, h.hostname));
        let ssh_host = conn.ssh_host();
        (host_label, ssh_host)
    }

    fn new_daemon(
        conn: &Arc<Connection>,
        size: TermSize,
        id: u64,
        host_label: Option<String>,
        ssh_host: Option<String>,
    ) -> (Arc<Self>, Arc<dyn PaneSink>) {
        // Client-side terminal never answers queries; the daemon's does.
        let (terminal, chans) = Terminal::new(size, false, Arc::new(Palette::default()));
        let mut sink_out: Option<Arc<dyn PaneSink>> = None;
        let s = Arc::new_cyclic(|weak| {
            let sink: Arc<dyn PaneSink> = Arc::new(Sink(weak.clone()));
            sink_out = Some(sink.clone());
            Self::new(terminal, chans.ui, Backend::Daemon { conn: conn.clone(), id: AtomicU64::new(id), sink }, host_label, ssh_host)
        });
        *s.view.lock() = size;
        (s, sink_out.expect("initialized in new_cyclic"))
    }

    /// Attach to session `attach` on the daemon behind `conn`, or create
    /// one (`None`). New sessions start in `cwd` (a path on the daemon's
    /// host); without one, a local daemon uses this process's directory.
    pub fn daemon(conn: Arc<Connection>, size: TermSize, attach: Option<u64>, cwd: Option<String>) -> Result<Arc<Self>> {
        if let Some(id) = attach {
            return Self::daemon_attach_many(&conn, size, &[id]).pop().map(|(_, r)| r).expect("one id, one result");
        }
        let (host_label, ssh_host) = Self::daemon_host_meta(&conn);
        let (s, sink) = Self::new_daemon(&conn, size, 0, host_label, ssh_host);
        // A local cwd means nothing on another host.
        let cwd = cwd.or_else(|| {
            conn.endpoint().is_local().then(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())).flatten()
        });
        let id = conn.create(win(size), cwd, sink)?;
        log::info!("started session {id}");
        if let Backend::Daemon { id: slot, .. } = &s.backend {
            slot.store(id, Ordering::Relaxed);
        }
        share(&s);
        Ok(s)
    }

    /// Attach to every session in `ids` on `conn` in one pipelined burst
    /// (used when restoring a saved window layout). Returns one `(id, result)`
    /// per requested id, in order.
    pub fn daemon_attach_many(conn: &Arc<Connection>, size: TermSize, ids: &[u64]) -> Vec<(u64, Result<Arc<Self>>)> {
        if ids.is_empty() {
            return Vec::new();
        }
        let (host_label, ssh_host) = Self::daemon_host_meta(conn);
        let ws = win(size);
        // One attach at a time per connection: two windows restoring the
        // same session at once share it rather than both subscribing (a
        // connection keeps one subscription per session).
        let lock = attach_lock(conn);
        let _attaching = lock.lock();
        // Ones a window of this process shows already are shared.
        let mut out: Vec<(u64, Option<Result<Arc<Self>>>)> = ids.iter().map(|&id| (id, shared(conn, id).map(Ok))).collect();
        let mut sessions = Vec::with_capacity(ids.len());
        let mut requests = Vec::with_capacity(ids.len());
        for &(id, _) in out.iter().filter(|(_, s)| s.is_none()) {
            let (s, sink) = Self::new_daemon(conn, size, id, host_label.clone(), ssh_host.clone());
            sessions.push((id, s));
            requests.push(AttachRequest { session: id, size: ws, sink });
        }
        let replies = conn.attach_many(requests);
        let mut fresh = sessions.into_iter().zip(replies);
        for (id, slot) in out.iter_mut().filter(|(_, s)| s.is_none()) {
            let ((_, s), res) = fresh.next().expect("one reply per request");
            *slot = Some(res.map(|()| {
                log::info!("attached to session {id}");
                share(&s);
                s
            }));
        }
        out.into_iter().map(|(id, r)| (id, r.expect("filled"))).collect()
    }

    /// Run `argv` in a new session on the daemon behind `conn` (in `cwd` when
    /// given, else this process's directory when `conn` is local).
    pub fn daemon_run(conn: Arc<Connection>, size: TermSize, argv: Vec<String>, cwd: Option<String>) -> Result<Arc<Self>> {
        let (host_label, ssh_host) = Self::daemon_host_meta(&conn);
        let (s, sink) = Self::new_daemon(&conn, size, 0, host_label, ssh_host);
        let cwd = cwd.or_else(|| {
            conn.endpoint().is_local().then(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())).flatten()
        });
        let id = conn.run(win(size), cwd, argv, sink)?;
        log::info!("running in session {id}");
        if let Backend::Daemon { id: slot, .. } = &s.backend {
            slot.store(id, Ordering::Relaxed);
        }
        share(&s);
        Ok(s)
    }

    /// Exit code of the session's process once it has exited, if known.
    pub fn exit_code(&self) -> Option<u32> {
        *self.exit_code.lock()
    }

    /// Wake-ups for one view: fires (coalesced) when the terminal changed
    /// or the link state did.
    pub fn subscribe_wake(&self) -> (Arc<WakeListener>, flume::Receiver<()>) {
        self.waker.subscribe()
    }

    /// Move what the terminal emitted into the log every view reads.
    fn drain_ui(&self, log: &mut UiLog) {
        for ev in self.ui_events.try_iter() {
            match &ev {
                UiEvent::Title(t) => log.title = Some(t.clone()),
                UiEvent::ResetTitle => log.title = None,
                _ => {}
            }
            log.events.push_back((log.next, ev));
            log.next += 1;
            if log.events.len() > UI_LOG {
                log.events.pop_front();
            }
        }
    }

    /// Where a new view starts reading UI events, and the title so far.
    /// The first view reads everything; a later one only what comes next.
    pub fn ui_cursor(&self) -> (u64, Option<String>) {
        let mut log = self.ui_log.lock();
        self.drain_ui(&mut log);
        let first = !self.ui_started.swap(true, Ordering::AcqRel);
        (if first { log.events.front().map_or(log.next, |(n, _)| *n) } else { log.next }, log.title.clone())
    }

    /// The UI events after `cursor` (moved past them). A view that fell
    /// further behind than the log reaches gets the current title first.
    pub fn ui_events(&self, cursor: &mut u64) -> Vec<UiEvent> {
        let mut log = self.ui_log.lock();
        self.drain_ui(&mut log);
        let mut out = Vec::new();
        if log.events.front().is_some_and(|(n, _)| *n > *cursor) {
            out.push(match &log.title {
                Some(t) => UiEvent::Title(t.clone()),
                None => UiEvent::ResetTitle,
            });
        }
        out.extend(log.events.iter().filter(|(n, _)| *n >= *cursor).map(|(_, e)| e.clone()));
        *cursor = log.next;
        out
    }

    /// A view of it went away (a pane detached, its window closed, or it
    /// was attached for a tab that's gone): the last one detaches it from
    /// goattyd, without ending its shell. True when it did.
    pub fn release_view(&self) -> bool {
        let Backend::Daemon { conn, id, sink } = &self.backend else { return false };
        {
            let mut shared = SHARED.lock();
            let before = self.views.try_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(1))).unwrap_or(0);
            if before > 1 {
                return false;
            }
            // Nobody can claim it from here on.
            let (key, sid) = (conn.key(), id.load(Ordering::Relaxed));
            shared.retain(|(c, i, w)| w.strong_count() > 0 && !(*c == key && *i == sid));
        }
        conn.detach_sink(id.load(Ordering::Relaxed), sink);
        true
    }

    /// Whether view `token` may set the session's size: the one that last
    /// had focus, or any while none has claimed it.
    pub fn may_resize(&self, token: u64) -> bool {
        let owner = self.size_owner.load(Ordering::Acquire);
        owner == 0 || owner == token
    }

    /// View `token` sets the session's size from now on (it got focus).
    pub fn own_size(&self, token: u64) {
        self.size_owner.store(token, Ordering::Release);
    }

    /// View `token` went away.
    pub fn disown_size(&self, token: u64) {
        let _ = self.size_owner.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    fn ingest(&self, bytes: &[u8]) {
        self.bytes_in.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let redraw = self.terminal.feed(bytes);
        self.predictor.lock().output(&self.terminal, Instant::now());
        if redraw {
            self.waker.wake();
        }
    }

    /// Input from the user (keys, pastes, mouse reports). The client the
    /// user types into sizes the session (#361).
    pub fn write(&self, bytes: &[u8]) {
        self.claim_size();
        self.predict(bytes);
        self.write_raw(bytes);
    }

    fn write_raw(&self, bytes: &[u8]) {
        match &self.backend {
            Backend::Local { input, .. } => {
                let _ = input.lock().write(bytes);
                if let Some(fg) = self.fg.get() {
                    fg.poke();
                }
            }
            Backend::Daemon { conn, id, .. } => {
                if *self.status.lock() == LinkStatus::NeedsReconnect {
                    // The link is down: input has nowhere to go. Enter asks ssh
                    // to reconnect, with prompts.
                    if is_enter(bytes) {
                        *self.status.lock() = LinkStatus::Reconnecting;
                        conn.reconnect();
                    }
                    return;
                }
                conn.input(id.load(Ordering::Relaxed), bytes)
            }
        }
    }

    /// Guess the echo of input on its way to the shell (#16), not in the
    /// alt screen and while connected. The guesses are drawn when
    /// `ssh.local_echo` wants it for the link's round trip or the echo
    /// delay measured from earlier guesses (a slow host, `ssh` inside a
    /// session), whichever is longer.
    fn predict(&self, bytes: &[u8]) {
        let setting = match self.local_echo.load(Ordering::Relaxed) {
            x if x == LocalEcho::Always as u8 => LocalEcho::Always,
            x if x == LocalEcho::Never as u8 => LocalEcho::Never,
            _ => LocalEcho::Auto,
        };
        let mut p = self.predictor.lock();
        // Programs in the alternate screen or speaking the kitty keyboard
        // protocol (key releases, CSI u keys) aren't a prompt to echo at.
        let mode = self.terminal.mode();
        let on = setting != LocalEcho::Never
            && *self.status.lock() == LinkStatus::Connected
            && !mode.intersects(TermMode::ALT_SCREEN | TermMode::KITTY_KEYBOARD);
        if !on {
            if p.deadline().is_some() {
                p.reset();
            }
            return;
        }
        let rtt = match &self.backend {
            Backend::Daemon { conn, .. } => conn.rtt(),
            Backend::Local { .. } => None,
        };
        let slow = rtt.max(p.echo_delay());
        p.set_visible(setting.wanted(slow));
        // A few round trips, plus room for a busy host.
        let timeout = slow.unwrap_or_default() * 3 + Duration::from_millis(400);
        p.input(&self.terminal, bytes, Instant::now(), timeout);
    }

    pub fn set_local_echo(&self, setting: LocalEcho) {
        self.local_echo.store(setting as u8, Ordering::Relaxed);
        if setting == LocalEcho::Never {
            self.predictor.lock().reset();
        }
    }

    /// Draw the predicted echo over a frame of the terminal.
    pub fn overlay_predictions(&self, frame: &mut Frame) {
        // Where `predict` doesn't guess: guesses from before the program
        // switched aren't drawn.
        if !frame.mode.intersects(TermMode::ALT_SCREEN | TermMode::KITTY_KEYBOARD) {
            self.predictor.lock().overlay(frame);
        }
    }

    /// When predicted echo times out; call [`Self::expire_predictions`] then.
    pub fn prediction_deadline(&self) -> Option<Instant> {
        self.predictor.lock().deadline()
    }

    /// Drop predictions the host didn't confirm in time. Returns whether
    /// the screen changed.
    pub fn expire_predictions(&self) -> bool {
        self.predictor.lock().expire(Instant::now())
    }

    /// Fetch scrollback the daemon has and this terminal lacks, when the
    /// viewport is near it (older lines, or what a resync skipped). Cheap;
    /// call it whenever the view is scrolled up. Never blocks.
    pub fn fill_history(&self) {
        let Backend::Daemon { conn, id, .. } = &self.backend else { return };
        if self.fetching.swap(true, Ordering::AcqRel) {
            return;
        }
        let margin = self.terminal.size().1 as usize * 2;
        let sent = self.terminal.wanted_history(margin).is_some_and(|w| conn.fetch_history(id.load(Ordering::Relaxed), w.before, w.count));
        if !sent {
            self.fetching.store(false, Ordering::Release);
        }
    }

    /// The pane's size (cells and cell pixels) differs from what [`Self::resize`]
    /// was last told.
    pub fn view_differs(&self, size: TermSize) -> bool {
        *self.view.lock() != size
    }

    /// Another client controls the session's size: the terminal has the
    /// session's size, not the pane's (#12).
    pub fn watching(&self) -> bool {
        self.watching.load(Ordering::Acquire)
    }

    /// Take control of the session's size from the client that has it.
    /// False when the request couldn't be sent (the link is down).
    pub fn take_size(&self) -> bool {
        match &self.backend {
            Backend::Daemon { conn, id, .. } => conn.take_size(id.load(Ordering::Relaxed)),
            Backend::Local { .. } => false,
        }
    }

    /// The user is using the session here (typing, or its pane came into
    /// view): take its size from the client that has it, if another one
    /// does (#361). That client keeps the "Take over" chip.
    pub fn claim_size(&self) {
        if self.watching() {
            let _ = self.take_size();
        }
    }

    /// This client sets the session's size (again): the terminal follows
    /// the pane, and the daemon hears the pane's size.
    fn set_controlling(&self) {
        let view = self.view.lock();
        if !self.watching.swap(false, Ordering::AcqRel) {
            return;
        }
        let view = *view;
        if self.terminal.size() != (view.cols, view.rows) {
            self.predictor.lock().reset();
            self.terminal.resize(view);
        }
        if let Backend::Daemon { conn, id, .. } = &self.backend {
            conn.resize(id.load(Ordering::Relaxed), win(view));
        }
    }

    /// The pane is now `size`. The terminal follows unless another client
    /// controls the session's size; the daemon always hears it (it's the
    /// size the session takes when this client controls it).
    pub fn resize(&self, size: TermSize) {
        if matches!(self.backend, Backend::Daemon { .. }) && !win(size).is_sane() {
            // goattyd would refuse it: keep the mirror at the session's size.
            return;
        }
        // `view` is held while the terminal is resized, so a `SizeControl`
        // on the reader thread can't interleave with it.
        let mut view = self.view.lock();
        *view = size;
        if !self.watching() {
            self.predictor.lock().reset();
            self.terminal.resize(size);
        }
        drop(view);
        match &self.backend {
            Backend::Local { pty, .. } => {
                let _ = pty.lock().resize(size);
            }
            Backend::Daemon { conn, id, .. } => conn.resize(id.load(Ordering::Relaxed), win(size)),
        }
    }

    /// Show the session with `palette`: our terminal renders with it, and
    /// the daemon answers color queries (OSC 4/10/11) with it.
    pub fn set_palette(&self, palette: Palette) {
        let colors = palette_list(&palette);
        self.terminal.set_palette(Arc::new(palette));
        if let Backend::Daemon { conn, id, .. } = &self.backend {
            conn.set_palette(id.load(Ordering::Relaxed), colors);
        }
    }

    /// End the shell (closing its pane). The daemon's reply is awaited:
    /// call it off the UI thread.
    pub fn kill(&self) {
        match &self.backend {
            Backend::Local { pty, .. } => pty.lock().kill(),
            Backend::Daemon { conn, id, .. } => {
                if let Err(e) = conn.kill(id.load(Ordering::Relaxed)) {
                    log::warn!("closing session: {e:#}");
                }
            }
        }
    }

    /// Whether this session is kept by a `goattyd` and can be detached
    /// without ending its shell.
    pub fn can_detach(&self) -> bool {
        self.daemon_id().is_some()
    }

    /// The goattyd session id; `None` for local PTYs.
    pub fn daemon_id(&self) -> Option<u64> {
        match &self.backend {
            Backend::Local { .. } => None,
            Backend::Daemon { id, .. } => Some(id.load(Ordering::Relaxed)),
        }
    }

    /// The goattyd connection behind this session, if any.
    pub fn connection(&self) -> Option<&Arc<Connection>> {
        match &self.backend {
            Backend::Local { .. } => None,
            Backend::Daemon { conn, .. } => Some(conn),
        }
    }

    /// Whether this session runs on a remote host.
    pub fn is_remote(&self) -> bool {
        self.host_label.is_some()
    }

    /// The shell's working directory on the session's host: daemon-tracked
    /// CWD (OSC 7 with `/proc` fallback) when available, else client OSC 7,
    /// else local PTY's `/proc` cwd.
    pub fn cwd(&self) -> Option<PathBuf> {
        self.remote_cwd.lock().as_deref().map(PathBuf::from).or_else(|| self.terminal.current_directory()).or_else(|| match &self.backend {
            Backend::Local { pty, .. } => pty.lock().cwd(),
            Backend::Daemon { .. } => None,
        })
    }

    /// String form of [`Self::cwd`], used to key cached remote path lookups.
    pub fn remote_cwd_str(&self) -> Option<String> {
        self.remote_cwd.lock().clone().or_else(|| self.terminal.current_directory().map(|p| p.to_string_lossy().into_owned()))
    }

    /// How many CWD changes `PaneEvent::Cwd` pushed so far (a view compares
    /// it with the last one it saw).
    pub fn cwd_generation(&self) -> u64 {
        self.cwd_gen.load(Ordering::Acquire)
    }

    /// Window title: the app's title plus host and link state when remote.
    pub fn decorate_title(&self, title: &str) -> String {
        let mut t = title.to_string();
        if let Some(h) = &self.host_label {
            t = format!("{t} — {h}");
        }
        match &*self.status.lock() {
            LinkStatus::Connected => t,
            LinkStatus::Reconnecting => format!("{t} (reconnecting…)"),
            LinkStatus::NeedsReconnect => format!("{t} (disconnected — press Enter)"),
            LinkStatus::Lost(_) => format!("{t} (disconnected)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_enter;

    #[test]
    fn enter_in_any_encoding_reconnects() {
        assert!(is_enter(b"\r"));
        assert!(is_enter(b"\x1b\r"));
        assert!(is_enter(b"\x1b[13u"));
        assert!(is_enter(b"\x1b[13;5u"));
        assert!(is_enter(b"\x1b[13;1:2u"));
        assert!(!is_enter(b"\x1b[13;1:3u"), "a release");
        assert!(!is_enter(b"\x1b[13~"), "F3");
        assert!(!is_enter(b"\x1b[131u"));
        assert!(!is_enter(b"a"));
    }
}
