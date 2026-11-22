//! The daemon process: socket setup, the accept loop, and the session
//! registry.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use goatty_proto::{HostInfo, SessionId, SessionInfo, WinSize};
use goatty_shell::HostService;
use goatty_term::pty::{Pty, ShellCommand};
use goatty_term::{Palette, Terminal, TerminalChannels};
use parking_lot::{Mutex, RwLock};

use crate::layouts::Layouts;
use crate::session::{CwdCoordinator, Pause, Session, pump_output, to_size};

/// The session registry shared by every connection.
pub(crate) struct Daemon {
    /// Host settings, generations, and each session's shell. The settings
    /// file is re-read for every new session, so edits apply without
    /// restarting the daemon.
    pub(crate) host: HostService,
    /// Removed on shutdown.
    pub(crate) socket: PathBuf,
    pub(crate) sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    /// The windows' tabs and splits.
    pub(crate) layouts: Mutex<Layouts>,
    pub(crate) next_session: AtomicU64,
    pub(crate) next_conn: AtomicU64,
    /// The listening socket the accept loop serves (a restart passes it on).
    pub(crate) listener_fd: Mutex<Option<RawFd>>,
    /// The goattyd file this process was started as: what a restart runs by
    /// default, so after an update it's the new build.
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
    pub(crate) socket_activated: AtomicBool,
}

/// What [`Daemon::start`] makes a session of: a PTY with its process, and
/// the terminal that mirrors it.
pub(crate) struct NewSession {
    pub id: SessionId,
    pub pty: Pty,
    pub reader: File,
    pub term: Terminal,
    pub chans: TerminalChannels,
    pub created_unix: u64,
    pub managed: bool,
    /// The connection that asked for it (`Create`, `Run`); `None` when restored.
    pub creator: Option<u64>,
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
        Self {
            // `goatty-setup` in a managed shell runs this goattyd's `shell install`.
            host: HostService::from_env(std::env::current_exe().ok()),
            socket,
            sessions: Mutex::default(),
            layouts: Mutex::default(),
            next_session: AtomicU64::new(0),
            next_conn: AtomicU64::new(0),
            listener_fd: Mutex::new(None),
            exe: own_exe(),
            restarting: AtomicBool::new(false),
            admission: RwLock::new(()),
            net: Mutex::new(None),
            conns: Mutex::default(),
            socket_activated: AtomicBool::new(false),
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
        self.spawn(conn, ShellCommand::new(launch.program, launch.args, launch.env), managed, ws, cwd)
    }

    /// A session running `argv` instead of a shell, using the host's managed
    /// `PATH` and environment so tools installed by goatty plugins or listed in
    /// `shell.path` are found (#241).
    pub(crate) fn run(self: &Arc<Self>, conn: u64, ws: WinSize, cwd: Option<String>, argv: Vec<String>) -> Result<Arc<Session>> {
        let host = self.host.settings();
        let launch = self.host.launcher.launch_command(&host, argv, cwd.as_deref())?;
        self.spawn(conn, ShellCommand::new(launch.program, launch.args, launch.env), false, ws, cwd)
    }

    fn spawn(self: &Arc<Self>, conn: u64, mut cmd: ShellCommand, managed: bool, ws: WinSize, cwd: Option<String>) -> Result<Arc<Session>> {
        let _admitted = self.admission.read();
        if self.restarting.load(Ordering::SeqCst) {
            bail!("goattyd is restarting; try again in a moment");
        }
        let size = to_size(ws);
        if let Some(cwd) = cwd.filter(|c| std::path::Path::new(c).is_dir()) {
            cmd.cwd = Some(cwd.into());
        }
        let (pty, reader) = Pty::spawn(&cmd, size).with_context(|| format!("couldn't start {}", cmd.program))?;
        // The daemon's terminal is authoritative and answers queries
        // (DA, DSR, OSC 4/10/11...). Clients run with answering disabled.
        // Over a remote link this also means query replies never pay the RTT.
        let (term, chans) = Terminal::new(size, true, Arc::new(Palette::default()));
        let id = self.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        let created_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let pid = pty.pid();
        log::info!("session {id}: spawned {}{} (pid {pid:?})", cmd.program, if managed { " with goatty's managed config" } else { "" });
        let session = self.start(NewSession { id, pty, reader, term, chans, created_unix, managed, creator: Some(conn) })?;
        if crate::systemd::has_fd_store() {
            crate::systemd::store(&session.stored(), session.master_fd);
        }
        Ok(session)
    }

    /// Register a session and start its threads: query replies → PTY, and
    /// PTY → terminal + subscribers until the process exits.
    pub(crate) fn start(self: &Arc<Self>, n: NewSession) -> Result<Arc<Session>> {
        let NewSession { id, pty, reader, term, chans, created_unix, managed, creator } = n;
        let (pid, master_fd) = (pty.pid(), pty.master_fd().as_raw_fd());
        let proc_cwd = pty.cwd();
        let init_cwd = term
            .current_directory()
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| proc_cwd.as_ref().map(|p| p.to_string_lossy().into_owned()));
        for _ in chans.ui.try_iter() {}
        let subs = Arc::new(Mutex::new(Vec::new()));
        let cwd = Arc::new(CwdCoordinator::new(id, subs.clone(), init_cwd, proc_cwd));
        let session = Arc::new(Session {
            id,
            term,
            pty: Mutex::new(pty),
            subs,
            created_unix,
            process: Mutex::new(None),
            cwd,
            fg: OnceLock::new(),
            managed,
            pause: Pause::new()?,
            pid,
            master_fd,
            ui_events: chans.ui.clone(),
            replies: chans.replies.clone(),
            replies_taken: AtomicU64::new(0),
            replies_written: AtomicU64::new(0),
            controller: Mutex::new(None),
            creator: Mutex::new(creator),
        });
        if let Err(e) = session.watch_foreground() {
            log::warn!("session {id}: can't watch the foreground program: {e}");
        }
        // Only goatty's zsh handles the reload request.
        let managed_pid = pid.filter(|_| managed);
        if let Some(pid) = managed_pid {
            self.host.track_shell(pid);
        }

        // Query replies → PTY.
        {
            let s = Arc::downgrade(&session);
            std::thread::Builder::new().name(format!("s{id}-replies")).spawn(move || {
                while let Ok(bytes) = chans.replies.recv() {
                    let Some(s) = s.upgrade() else { break };
                    s.replies_taken.fetch_add(1, Ordering::SeqCst);
                    let _ = s.pty.lock().write(&bytes);
                    s.replies_written.fetch_add(1, Ordering::SeqCst);
                }
            })?;
        }
        // PTY → terminal + subscribers.
        {
            let s = session.clone();
            let daemon = self.clone();
            std::thread::Builder::new().name(format!("s{id}-reader")).spawn(move || {
                pump_output(&s, reader);
                let code = s.pty.lock().try_wait();
                log::info!("session {}: exited ({code:?})", s.id);
                daemon.sessions.lock().remove(&s.id);
                if crate::systemd::has_fd_store() {
                    crate::systemd::unstore(&s.stored());
                }
                if let Some(pid) = managed_pid {
                    daemon.host.untrack_shell(pid);
                }
                s.finish(code);
            })?;
        }
        self.sessions.lock().insert(id, session.clone());
        Ok(session)
    }

    /// Give connection `conn` the newest free window layout (see `Layouts`).
    pub(crate) fn claim_layout(&self, conn: u64) -> Option<goatty_proto::LayoutInfo> {
        let live: std::collections::HashSet<SessionId> = self.sessions.lock().keys().copied().collect();
        self.layouts.lock().claim(conn, |s| live.contains(&s))
    }

    /// Give connection `conn` free layout `id` (see `Layouts::claim_id`).
    pub(crate) fn claim_layout_id(&self, conn: u64, id: goatty_proto::LayoutId) -> Option<goatty_proto::LayoutInfo> {
        let live: std::collections::HashSet<SessionId> = self.sessions.lock().keys().copied().collect();
        self.layouts.lock().claim_id(conn, id, |s| live.contains(&s))
    }

    /// The free layouts (see `Layouts::list_free`).
    pub(crate) fn free_layouts(&self) -> Vec<goatty_proto::LayoutInfo> {
        let live: std::collections::HashSet<SessionId> = self.sessions.lock().keys().copied().collect();
        self.layouts.lock().list_free(|s| live.contains(&s))
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
        for s in self.sessions.lock().values() {
            s.pty.lock().kill();
        }
        // Give the connection writers a moment to flush the `Ok` reply.
        std::thread::sleep(Duration::from_millis(100));
        // Under systemd/launchd socket activation the supervisor owns the
        // socket file and keeps listening on it for the next connection.
        if !self.socket_activated.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&self.socket);
        }
        // The Serve route stays (the next goattyd serves it again); the socket goes.
        if let Some(net) = self.net.lock().as_ref() {
            let _ = std::fs::remove_file(net.socket());
        }
        std::process::exit(0);
    }
}

pub(crate) fn host_info() -> HostInfo {
    let hostname = goatty_client::hostname();
    HostInfo {
        hostname,
        user: std::env::var("USER").unwrap_or_default(),
        os: std::env::consts::OS.into(),
        daemon_version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn bind(path: &Path) -> Result<(UnixListener, u64)> {
    let dir = path.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("goattyd already running at {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let ino = std::fs::metadata(path)?.ino();
    Ok((listener, ino))
}

fn accept_loop(daemon: Arc<Daemon>, listener: UnixListener) {
    *daemon.listener_fd.lock() = Some(listener.as_raw_fd());
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

/// Run the daemon on the socket at `path` until `goattyd stop`. Never returns
/// `Ok`; startup errors (socket in use, permissions) are returned. Under
/// systemd or launchd socket activation it serves the socket it was given.
pub fn serve(path: &Path, start: Start) -> Result<()> {
    crate::systemd::capture_env();
    let path = path.to_path_buf();
    let daemon = Arc::new(Daemon::new(path.clone()));
    let listener = match start {
        Start::Restore(state) => {
            let listener = crate::restore::take_over(&daemon, &state)?;
            log::info!("took over from the previous goattyd ({} sessions)", daemon.sessions.lock().len());
            listener
        }
        Start::Normal => {
            let passed = crate::systemd::take_passed();
            crate::restore::adopt_stored(&daemon, passed.ptys);
            let activated = passed
                .listener
                .map(|fd| {
                    log::info!("using the socket from systemd");
                    UnixListener::from(fd)
                })
                .or_else(|| {
                    crate::launchd::activate_socket().map(|fd| {
                        log::info!("using the socket from launchd");
                        UnixListener::from(fd)
                    })
                });
            if activated.is_some() {
                daemon.socket_activated.store(true, Ordering::SeqCst);
            }
            activated
        }
    };
    let (listener, mut ino) = match listener {
        Some(l) => (l, std::fs::metadata(&path).map(|m| m.ino()).unwrap_or(0)),
        None => bind(&path)?,
    };
    log::info!("goattyd {} listening on {} (pid {})", env!("CARGO_PKG_VERSION"), path.display(), std::process::id());
    if crate::systemd::has_fd_store() {
        crate::restore::save_on_sigterm(daemon.clone())?;
    }
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
        if current.is_some() && UnixStream::connect(&path).is_ok() {
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

/// An inherited listening socket, close-on-exec from now on.
pub(crate) fn listener_from_fd(fd: RawFd) -> Result<UnixListener> {
    goatty_term::pty::set_cloexec(fd, true).with_context(|| format!("the inherited socket (fd {fd}) isn't open"))?;
    // SAFETY: the previous goattyd passed this fd for exactly this, and
    // nothing in this process has adopted it before.
    Ok(UnixListener::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}
