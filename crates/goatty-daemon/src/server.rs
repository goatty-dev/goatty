//! The daemon process: socket setup, the accept loop, and the session
//! registry.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use goatty_client::LocalListener;
#[cfg(unix)]
use goatty_client::LocalStream;
use goatty_proto::{HostInfo, SessionId, SessionInfo, WinSize};
use goatty_shell::HostService;
use goatty_term::pty::{Pty, PtyReader, ShellCommand};
use goatty_term::{Palette, Terminal, TerminalChannels};
use parking_lot::{Mutex, RwLock};

#[cfg(unix)]
use crate::session::Pause;
use crate::session::{CwdCoordinator, Session, pump_output, to_size};
use crate::workspaces::{Changes, Workspaces};

/// The session registry shared by every connection.
pub(crate) struct Daemon {
    /// Host settings, generations, and each session's shell. The settings
    /// file is re-read for every new session, so edits apply without
    /// restarting the daemon.
    pub(crate) host: HostService,
    /// Removed on shutdown.
    pub(crate) socket: PathBuf,
    pub(crate) sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    /// The tabs and splits client windows share (#215).
    pub(crate) workspaces: Mutex<Workspaces>,
    pub(crate) next_session: AtomicU64,
    pub(crate) next_conn: AtomicU64,
    /// The listening socket the accept loop serves (a restart passes it on).
    #[cfg(unix)]
    pub(crate) listener_fd: Mutex<Option<RawFd>>,
    /// The goattyd file this process was started as: what a restart runs by
    /// default, so after an update it's the new build.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) exe: Option<PathBuf>,
    /// A restart is under way: no new sessions.
    pub(crate) restarting: AtomicBool,
    /// Held (shared) while a session is created, from the `restarting`
    /// check to its registration; a restart takes it exclusively to set
    /// `restarting`, so it never misses a session being started.
    pub(crate) admission: RwLock<()>,
    /// This host on the user's tailnet (`crate::net`), once used.
    pub(crate) net: Mutex<Option<Arc<goatty_net::host::Host>>>,
    /// Every open connection, for dropping the silent ones (#13).
    pub(crate) conns: Mutex<HashMap<u64, Arc<crate::client::Liveness>>>,
    /// The listening socket is owned by systemd or launchd (socket activation),
    /// so `shutdown` must not unlink the socket path.
    #[cfg(unix)]
    pub(crate) socket_activated: AtomicBool,
    /// What comes back after a reboot or crash (#344).
    #[cfg(unix)]
    pub(crate) resurrect: crate::resurrect::Keeper,
}

/// What [`Daemon::start`] makes a session of: a PTY with its process, and
/// the terminal that mirrors it.
pub(crate) struct NewSession {
    pub id: SessionId,
    pub pty: Pty,
    pub reader: PtyReader,
    pub term: Terminal,
    pub chans: TerminalChannels,
    pub created_unix: u64,
    pub managed: bool,
    /// The connection that asked for it (`Create`, `Run`); `None` when restored.
    pub creator: Option<u64>,
    /// Launch directory passed to `Pty::spawn` (used to seed initial CWD when
    /// the platform lacks `/proc/<pid>/cwd`, such as Windows before OSC 7).
    pub initial_cwd: Option<PathBuf>,
}

/// The program this process runs, as a path a restart can run again (not
/// `/proc/self/exe`, which stays the old file after an update).
fn own_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let s = exe.to_string_lossy();
    Some(s.strip_suffix(" (deleted)").map(PathBuf::from).unwrap_or(exe))
}

impl Daemon {
    pub(crate) fn new(socket: PathBuf) -> Self {
        // `goatty-setup` in a managed shell runs this goattyd's `shell install`.
        let host = HostService::from_env(std::env::current_exe().ok());
        #[cfg(unix)]
        let resurrect = crate::resurrect::Keeper::new(host.launcher.dirs().map(|d| d.state.as_path()), &socket);
        Self {
            host,
            socket,
            sessions: Mutex::default(),
            workspaces: Mutex::default(),
            next_session: AtomicU64::new(0),
            next_conn: AtomicU64::new(0),
            #[cfg(unix)]
            listener_fd: Mutex::new(None),
            exe: own_exe(),
            restarting: AtomicBool::new(false),
            admission: RwLock::new(()),
            net: Mutex::new(None),
            conns: Mutex::default(),
            #[cfg(unix)]
            socket_activated: AtomicBool::new(false),
            #[cfg(unix)]
            resurrect,
        }
    }

    pub(crate) fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().get(&id).cloned()
    }

    /// A session running the user's shell (goatty's managed zsh by default).
    /// `gui_setup`: the client offers the shell setup itself (protocol v9),
    /// so the shell's own missing-tools hint stays quiet.
    pub(crate) fn create(self: &Arc<Self>, conn: u64, ws: WinSize, cwd: Option<String>, gui_setup: bool) -> Result<Arc<Session>> {
        let host = self.host.settings();
        for issue in &host.file.issues {
            log::warn!("host settings: {issue}");
        }
        let mut launch = self.host.launcher.launch(&host);
        if gui_setup {
            goatty_shell::setup::gui_offers_setup(&mut launch);
        }
        let managed = launch.managed;
        self.spawn(None, Some(conn), ShellCommand::new(launch.program, launch.args, launch.env), managed, ws, cwd)
    }

    /// A new shell for session `id`, whose shell ended with the previous
    /// goattyd (`crate::resurrect`).
    #[cfg(unix)]
    pub(crate) fn resurrect_session(self: &Arc<Self>, id: SessionId, ws: WinSize, cwd: Option<String>) -> Result<Arc<Session>> {
        let launch = self.host.launcher.launch(&self.host.settings());
        let managed = launch.managed;
        self.spawn(Some(id), None, ShellCommand::new(launch.program, launch.args, launch.env), managed, ws, cwd)
    }

    /// A session running `argv` instead of a shell, using the host's managed
    /// `PATH` and environment so tools installed by goatty plugins or listed in
    /// `shell.path` are found (#241).
    pub(crate) fn run(self: &Arc<Self>, conn: u64, ws: WinSize, cwd: Option<String>, argv: Vec<String>) -> Result<Arc<Session>> {
        let host = self.host.settings();
        let launch = self.host.launcher.launch_command(&host, argv, cwd.as_deref())?;
        self.spawn(None, Some(conn), ShellCommand::new(launch.program, launch.args, launch.env), false, ws, cwd)
    }

    /// Start `cmd` as session `id` (a new one when `None`) for connection `creator`.
    fn spawn(
        self: &Arc<Self>,
        id: Option<SessionId>,
        creator: Option<u64>,
        mut cmd: ShellCommand,
        managed: bool,
        ws: WinSize,
        cwd: Option<String>,
    ) -> Result<Arc<Session>> {
        let _admitted = self.admission.read();
        if self.restarting.load(Ordering::SeqCst) {
            bail!("goattyd is restarting; try again in a moment");
        }
        let size = to_size(ws);
        if let Some(cwd) = cwd.filter(|c| std::path::Path::new(c).is_dir()) {
            cmd.cwd = Some(cwd.into());
        }
        let initial_cwd = cmd.cwd.clone();
        let (pty, reader) = Pty::spawn(&cmd, size).with_context(|| format!("couldn't start {}", cmd.program))?;
        // The daemon's terminal is authoritative and answers queries
        // (DA, DSR, OSC 4/10/11...). Clients run with answering disabled.
        // Over a remote link this also means query replies never pay the RTT.
        let (term, chans) = Terminal::new(size, true, Arc::new(Palette::default()));
        let id = id.unwrap_or_else(|| self.next_session.fetch_add(1, Ordering::Relaxed) + 1);
        let created_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let pid = pty.pid();
        log::info!("session {id}: spawned {}{} (pid {pid:?})", cmd.program, if managed { " with goatty's managed config" } else { "" });
        let session = self.start(NewSession { id, pty, reader, term, chans, created_unix, managed, creator, initial_cwd })?;
        #[cfg(unix)]
        if crate::systemd::has_fd_store() {
            crate::systemd::store(&session.stored(), session.master_fd);
        }
        Ok(session)
    }

    /// Register a session and start its threads: query replies → PTY, and
    /// PTY → terminal + subscribers until the process exits.
    pub(crate) fn start(self: &Arc<Self>, n: NewSession) -> Result<Arc<Session>> {
        let NewSession { id, pty, reader, term, chans, created_unix, managed, creator, initial_cwd } = n;
        let pid = pty.pid();
        #[cfg(unix)]
        let master_fd = pty.master_fd().as_raw_fd();
        #[cfg(windows)]
        let (resizer, exit_watch) = (pty.resizer(), pty.exit_watcher());
        let proc_cwd = pty.cwd();
        let input = pty.input().context("duplicate the PTY for input")?;
        let init_cwd = term
            .current_directory()
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| proc_cwd.as_ref().map(|p| p.to_string_lossy().into_owned()))
            .or_else(|| initial_cwd.as_ref().map(|p| p.to_string_lossy().into_owned()));
        for _ in chans.ui.try_iter() {}
        let subs = Arc::new(Mutex::new(Vec::new()));
        let cwd = Arc::new(CwdCoordinator::new(id, subs.clone(), init_cwd, proc_cwd));
        let session = Arc::new(Session {
            id,
            term,
            pty: Mutex::new(pty),
            input: Mutex::new(input),
            subs,
            created_unix,
            process: Mutex::new(None),
            cwd,
            fg: OnceLock::new(),
            managed,
            #[cfg(unix)]
            pause: Pause::new()?,
            pid,
            #[cfg(unix)]
            master_fd,
            #[cfg(windows)]
            resizer,
            ui_events: chans.ui.clone(),
            replies: chans.replies.clone(),
            replies_taken: AtomicU64::new(0),
            replies_written: AtomicU64::new(0),
            controller: Mutex::new(None),
            creator: Mutex::new(creator),
            exited: Mutex::new(None),
        });
        if let Err(e) = session.watch_foreground() {
            log::warn!("session {id}: can't watch the foreground program: {e}");
        }
        // Only goatty's zsh handles the reload request.
        let managed_pid = pid.filter(|_| managed);
        if let Some(pid) = managed_pid {
            self.host.track_shell(pid);
        }

        // Register before spawning the reader/exit threads so a fast-exiting
        // command cannot run `sessions.remove(&id)` before `insert(id, ...)`
        // and leave a finished session stranded in `ListSessions`.
        self.sessions.lock().insert(id, session.clone());
        let rollback = || {
            self.sessions.lock().remove(&id);
            if let Some(pid) = managed_pid {
                self.host.untrack_shell(pid);
            }
            session.pty.lock().kill();
        };

        // Query replies → PTY.
        {
            let s = Arc::downgrade(&session);
            if let Err(e) = std::thread::Builder::new().name(format!("s{id}-replies")).spawn(move || {
                while let Ok(bytes) = chans.replies.recv() {
                    let Some(s) = s.upgrade() else { break };
                    s.replies_taken.fetch_add(1, Ordering::SeqCst);
                    let _ = s.input.lock().write(&bytes);
                    s.replies_written.fetch_add(1, Ordering::SeqCst);
                }
            }) {
                rollback();
                return Err(e.into());
            }
        }
        // Windows ConPTY keeps the output pipe open until `ClosePseudoConsole` is
        // called. Start the exit watcher BEFORE `s{id}-reader` so a failure to
        // spawn `s{id}-exit` never leaves `s{id}-reader` blocked on an unclosed
        // pseudoconsole, and a failure to spawn `s{id}-reader` still wakes
        // `s{id}-exit` once `rollback()` kills the child.
        #[cfg(windows)]
        {
            if let Err(e) = std::thread::Builder::new().name(format!("s{id}-exit")).spawn(move || {
                exit_watch.wait_and_close_console();
            }) {
                rollback();
                return Err(e.into());
            }
        }
        // PTY → terminal + subscribers.
        {
            let s = session.clone();
            let daemon = self.clone();
            if let Err(e) = std::thread::Builder::new().name(format!("s{id}-reader")).spawn(move || {
                pump_output(&s, reader);
                // The PTY master can hit EOF/EIO slightly before the child is
                // reapable via WNOHANG; retry briefly so short-lived `Run`
                // sessions report their exit code reliably.
                let mut code = None;
                for attempt in 0..40 {
                    if let Some(status) = s.pty.lock().try_wait() {
                        code = Some(status);
                        break;
                    }
                    if attempt + 1 < 40 {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                log::info!("session {}: exited ({code:?})", s.id);
                daemon.sessions.lock().remove(&s.id);
                #[cfg(unix)]
                if crate::systemd::has_fd_store() {
                    crate::systemd::unstore(&s.stored());
                }
                if let Some(pid) = managed_pid {
                    daemon.host.untrack_shell(pid);
                }
                s.finish(code);
                daemon.session_ended(s.id);
            }) {
                rollback();
                return Err(e.into());
            }
        }
        Ok(session)
    }

    /// The running sessions' ids.
    pub(crate) fn live(&self) -> std::collections::HashSet<SessionId> {
        self.sessions.lock().keys().copied().collect()
    }

    /// Run `f` on the workspaces with what's running now. The running
    /// sessions are read under the workspaces' lock (always in that order),
    /// so a session made and saved into a workspace meanwhile can't look
    /// ended to a decision taken with an older snapshot.
    pub(crate) fn with_workspaces<T>(&self, f: impl FnOnce(&mut Workspaces, &dyn Fn(SessionId) -> bool) -> T) -> T {
        let mut w = self.workspaces.lock();
        let live = self.live();
        f(&mut w, &|s| live.contains(&s))
    }

    /// Session `id` ended: the workspaces that listed it changed (and one no
    /// window shows may have ended with it).
    fn session_ended(&self, id: SessionId) {
        if self.with_workspaces(|w, alive| w.session_ended(id, alive)) {
            self.announce(Changes { list: true, layout: None });
        }
    }

    /// Tell the connections what changed in the workspaces: a saved layout
    /// to the ones that show it (not to the window that saved it), the list
    /// to every one that watches workspaces. Each frame is encoded once.
    pub(crate) fn announce(&self, changes: Changes) {
        let send = |frame: &goatty_proto::Frame, to: &dyn Fn(u64, &crate::client::Liveness) -> bool| {
            for (&conn, live) in self.conns.lock().iter() {
                if to(conn, live)
                    && let Some(tx) = live.writer.upgrade()
                {
                    let _ = tx.send(crate::client::Outbound::from(frame.clone()));
                }
            }
        };
        if let Some((by, layout, conns)) = changes.layout
            && let Ok(frame) = goatty_proto::encode(&goatty_proto::ServerMsg::LayoutChanged { by, layout })
        {
            send(&frame, &|conn, _| conns.contains(&conn));
        }
        if changes.list {
            let (list, seq) = self.with_workspaces(|w, alive| (w.list(alive), w.version()));
            if let Ok(frame) = goatty_proto::encode(&goatty_proto::ServerMsg::WorkspacesChanged { seq, list }) {
                send(&frame, &|_, live| live.watches_workspaces());
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<_> = self.sessions.lock().values().map(|s| s.info()).collect();
        list.sort_by_key(|s| s.id);
        list
    }

    /// Kill every session, remove the socket (unless socket-activated) and exit
    /// the process.
    pub(crate) fn shutdown(&self) -> ! {
        log::info!("shutdown requested");
        #[cfg(unix)]
        self.resurrect.forget();
        for s in self.sessions.lock().values() {
            s.pty.lock().kill();
        }
        // Give the connection writers a moment to flush the `Ok` reply.
        std::thread::sleep(Duration::from_millis(100));
        // Under systemd/launchd socket activation the supervisor owns the
        // socket file and keeps listening on it for the next connection.
        // On Windows, Named Pipes vanish automatically when closed.
        #[cfg(unix)]
        if !self.socket_activated.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&self.socket);
        }
        // The Serve route stays (the next goattyd serves it again); the socket goes.
        #[cfg(unix)]
        if let Some(net) = self.net.lock().as_ref() {
            let _ = std::fs::remove_file(net.socket());
        }
        std::process::exit(0);
    }
}

pub(crate) fn host_info() -> HostInfo {
    let hostname = goatty_client::hostname();
    #[cfg(windows)]
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
    #[cfg(not(windows))]
    let user = std::env::var("USER").unwrap_or_default();
    HostInfo { hostname, user, os: std::env::consts::OS.into(), daemon_version: env!("CARGO_PKG_VERSION").into() }
}

#[cfg(unix)]
fn bind(path: &Path) -> Result<(LocalListener, u64)> {
    let dir = path.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        if LocalStream::connect(path).is_ok() {
            bail!("goattyd already running at {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = LocalListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let ino = std::fs::metadata(path)?.ino();
    Ok((listener, ino))
}

#[cfg(windows)]
fn bind(path: &Path) -> Result<LocalListener> {
    LocalListener::bind(path).with_context(|| format!("bind {}", path.display()))
}

fn accept_loop(daemon: Arc<Daemon>, listener: LocalListener) {
    #[cfg(unix)]
    {
        *daemon.listener_fd.lock() = Some(listener.as_raw_fd());
    }
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        let d = daemon.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::client::handle(d, stream) {
                log::debug!("client error: {e:#}");
            }
        });
    }
}

/// How `serve` starts.
pub enum Start {
    /// Fresh, or with the socket passed by systemd (`LISTEN_FDS`, plus any
    /// PTYs from the fd store after a crash or `systemctl --user restart`) or
    /// macOS launchd (`launch_activate_socket`).
    Normal,
    /// Taking over from the previous goattyd (`goattyd serve --restore
    /// FILE`, run by `Restart`): its socket and PTYs are inherited fds.
    Restore(PathBuf),
}

/// Run the daemon on the socket/pipe at `path` until `goattyd stop`. Never
/// returns `Ok`; startup errors (endpoint in use, permissions) are returned.
/// Under systemd or launchd socket activation it serves the socket it was given.
pub fn serve(path: &Path, start: Start) -> Result<()> {
    crate::systemd::capture_env();
    let path = path.to_path_buf();
    let daemon = Arc::new(Daemon::new(path.clone()));
    #[cfg(unix)]
    let (listener, mut ino) = {
        let listener = match start {
            Start::Restore(state) => {
                let listener = crate::restore::take_over(&daemon, &state)?;
                log::info!("took over from the previous goattyd ({} sessions)", daemon.sessions.lock().len());
                listener
            }
            Start::Normal => {
                let passed = crate::systemd::take_passed();
                crate::restore::adopt_stored(&daemon, passed.ptys);
                crate::resurrect::resurrect(&daemon);
                let activated = passed
                    .listener
                    .map(|fd| {
                        log::info!("using the socket from systemd");
                        LocalListener::from(fd)
                    })
                    .or_else(|| {
                        crate::launchd::activate_socket().map(|fd| {
                            log::info!("using the socket from launchd");
                            LocalListener::from(fd)
                        })
                    });
                if activated.is_some() {
                    daemon.socket_activated.store(true, Ordering::SeqCst);
                }
                activated
            }
        };
        match listener {
            Some(l) => (l, std::fs::metadata(&path).map(|m| m.ino()).unwrap_or(0)),
            None => bind(&path)?,
        }
    };
    #[cfg(windows)]
    let listener = match start {
        Start::Restore(state) => {
            let _ = crate::restore::take_over(&daemon, &state)?;
            bind(&path)?
        }
        Start::Normal => bind(&path)?,
    };
    log::info!("goattyd {} listening on {} (pid {})", env!("CARGO_PKG_VERSION"), path.display(), std::process::id());
    #[cfg(unix)]
    if crate::systemd::has_fd_store() {
        crate::restore::save_on_sigterm(daemon.clone())?;
    }
    #[cfg(unix)]
    crate::resurrect::Keeper::start(&daemon)?;
    {
        let d = daemon.clone();
        std::thread::Builder::new().name("accept".into()).spawn(move || accept_loop(d, listener))?;
    }
    // Serve the tailnet again if this host was made available.
    crate::net::start(&daemon);
    // The first tab shouldn't wait for the import of the user's shell setup.
    {
        let host = daemon.host.clone();
        std::thread::Builder::new().name("shell-import".into()).spawn(move || host.launcher.prepare_import(&host.settings().value))?;
    }
    // Plugin updates are found without the Plugins page open (#95): fetch
    // registries older than a day, but only when a plugin from one is
    // installed. Clients ask with `Updates`; nothing is installed here.
    {
        let host = daemon.host.clone();
        std::thread::Builder::new().name("plugin-registries".into()).spawn(move || {
            std::thread::sleep(Duration::from_secs(120));
            loop {
                host.refresh_stale_registries();
                std::thread::sleep(Duration::from_secs(3600));
            }
        })?;
    }
    // Drop clients that went silent (#13). Its own thread: the socket
    // watchdog below can block probing another listener.
    {
        let d = daemon.clone();
        std::thread::Builder::new().name("reaper".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                crate::client::reap_silent(&d.conns, std::time::Instant::now(), crate::client::SILENT_AFTER);
            }
        })?;
    }
    #[cfg(unix)]
    {
        // Socket watchdog. On a server, systemd-logind deletes
        // /run/user/$UID when the user's last login session ends (unless
        // lingering is enabled). The daemon and its shells survive that, but the
        // socket path vanishes, and the next `ssh host goattyd proxy` would start
        // a second, empty daemon. Re-create the socket when it disappears.
        let mut taken_over = false;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            crate::net::check(&daemon);
            let current = std::fs::metadata(&path).ok().map(|m| m.ino());
            if current == Some(ino) || daemon.restarting.load(Ordering::SeqCst) {
                continue;
            }
            // Someone else listens there now (another daemon, or a systemd
            // socket installed meanwhile): leave it alone, say so once.
            if current.is_some() && LocalStream::connect(&path).is_ok() {
                if !taken_over {
                    log::warn!("another listener took over {}; this daemon keeps serving its connected clients", path.display());
                    taken_over = true;
                }
                continue;
            }
            log::warn!("socket {} disappeared; re-binding", path.display());
            match bind(&path) {
                Ok((l, i)) => {
                    ino = i;
                    taken_over = false;
                    daemon.socket_activated.store(false, Ordering::SeqCst);
                    let d = daemon.clone();
                    std::thread::Builder::new().name("accept".into()).spawn(move || accept_loop(d, l))?;
                }
                Err(e) => log::error!("re-bind failed: {e:#}"),
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Named Pipes exist in the kernel object manager for as long as the
        // listener holds a handle; no stale socket file or inode watchdog is needed.
        loop {
            std::thread::sleep(Duration::from_secs(2));
            crate::net::check(&daemon);
        }
    }
}

/// An inherited listening socket, close-on-exec from now on.
#[cfg(unix)]
pub(crate) fn listener_from_fd(fd: RawFd) -> Result<LocalListener> {
    goatty_term::pty::set_cloexec(fd, true).with_context(|| format!("the inherited socket (fd {fd}) isn't open"))?;
    // SAFETY: the previous goattyd passed this fd for exactly this, and
    // nothing in this process has adopted it before.
    Ok(LocalListener::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use goatty_proto::{ServerMsg, StreamOpts, WinSize, read_msg};

    use super::Daemon;
    use crate::flow::FlowControl;
    use crate::session::{SubConn, SubFeatures};

    const SIZE: WinSize = WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

    fn exit_cmd(code: u32) -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd.exe".into(), "/C".into(), format!("exit {code}")]
        }
        #[cfg(not(windows))]
        {
            vec!["/bin/sh".into(), "-c".into(), format!("exit {code}")]
        }
    }

    #[test]
    fn subscribe_after_finish_still_delivers_exited_and_clears_session_list() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Arc::new(Daemon::new(dir.path().join("d.sock")));
        let session = daemon.run(1, SIZE, None, exit_cmd(42)).unwrap();

        // Wait until the fast-exiting child has been removed from `sessions`
        // AND `Session::finish` has run BEFORE subscribing, deterministically
        // exercising both ordering races (registration vs removal and finish vs subscribe).
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.get(session.id).is_some() || session.exited.lock().is_none() {
            assert!(Instant::now() < deadline, "session did not finish and leave sessions map");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(daemon.list().is_empty(), "finished session must not remain in list()");

        // Subscribe AFTER `finish` has already completed: must still receive
        // `Attached` followed by `Exited { code: Some(42) }`, and must not
        // leave a dangling subscriber behind.
        let (tx, rx) = flume::unbounded();
        let flow = Arc::new(FlowControl::new());
        let _sub = session.subscribe(SubConn { id: 1, features: SubFeatures::default(), flow: &flow, tx: &tx }, 7, SIZE, StreamOpts::LOCAL);
        assert!(session.subs.lock().is_empty(), "late subscriber on finished session must not stay registered");

        let mut saw_attached = false;
        let mut exit_code = None;
        for out in rx.try_iter() {
            if out.stop {
                continue;
            }
            let mut slice: &[u8] = &out.frame;
            match read_msg::<_, ServerMsg>(&mut slice).unwrap() {
                ServerMsg::Attached { req, session: sid, .. } => {
                    assert_eq!(req, 7);
                    assert_eq!(sid, session.id);
                    saw_attached = true;
                }
                ServerMsg::Exited { session: sid, code } => {
                    assert_eq!(sid, session.id);
                    exit_code = Some(code);
                }
                _ => {}
            }
        }
        assert!(saw_attached, "late subscriber must receive Attached snapshot");
        assert_eq!(exit_code, Some(Some(42)), "late subscriber must receive Exited with the child exit code");
    }

    #[test]
    fn subscribe_before_finish_delivers_exited_once_and_clears_session_list() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Arc::new(Daemon::new(dir.path().join("d.sock")));
        // Command waits for a line on stdin before exiting with code 7 so
        // `subscribe` deterministically runs while the session is still alive.
        #[cfg(windows)]
        let wait_cmd = vec!["cmd.exe".into(), "/C".into(), "set /p _= & exit 7".into()];
        #[cfg(not(windows))]
        let wait_cmd = vec!["/bin/sh".into(), "-c".into(), "read line; exit 7".into()];
        let session = daemon.run(1, SIZE, None, wait_cmd).unwrap();

        let (tx, rx) = flume::unbounded();
        let flow = Arc::new(FlowControl::new());
        let _sub = session.subscribe(SubConn { id: 1, features: SubFeatures::default(), flow: &flow, tx: &tx }, 9, SIZE, StreamOpts::LOCAL);
        assert!(session.exited.lock().is_none(), "session exited before subscribe completed");
        assert!(daemon.get(session.id).is_some(), "live session must be registered in sessions map");

        // Release the waiting child and wait until exit cleanup has completed.
        session.input.lock().write(b"go\r\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.get(session.id).is_some() || session.exited.lock().is_none() {
            assert!(Instant::now() < deadline, "session did not exit and clean up");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(daemon.list().is_empty(), "finished session remained in list()");

        // Drain EVERY message delivered to the subscriber and assert `Exited`
        // arrived exactly once with exit code 7 (no duplicates).
        let mut exited = Vec::new();
        for out in rx.try_iter() {
            if out.stop {
                continue;
            }
            let mut slice: &[u8] = &out.frame;
            if let ServerMsg::Exited { session: sid, code } = read_msg::<_, ServerMsg>(&mut slice).unwrap() {
                assert_eq!(sid, session.id);
                exited.push(code);
            }
        }
        assert_eq!(exited, vec![Some(7)]);
    }
}
