//! Sessions outlive goattyd itself (#131): a restart, an upgrade, and under
//! the systemd service a crash or `systemctl --user restart`.
//!
//! What has to carry over is each session's PTY master (the shell keeps
//! running as long as one is open) and what the daemon knows about it: the
//! screens and scrollback (`Terminal::restore_snapshot`), size, ids, and the
//! workspaces.
//!
//! * **Restart** (protocol `Restart`, `goattyd restart`): every output pump
//!   pauses between two reads, the state goes to `<socket>.restore/`, the
//!   PTYs and the listening socket lose close-on-exec, and the process
//!   `execve`s the new goattyd as `goattyd serve --restore FILE` ([`restart`]).
//!   The pid stays, so the shells are still its children and systemd (if it
//!   supervises the daemon) notices nothing. The new process takes over in
//!   [`take_over`]. Clients lose their connection and reconnect.
//! * **systemd**: every PTY also sits in the service's fd store (see
//!   `crate::systemd`). On SIGTERM the state is saved without the fds
//!   ([`save_on_sigterm`]); after a crash there's none. The next start gets
//!   the PTYs from systemd and [`adopt_stored`] rebuilds the sessions, with
//!   their screens when the state is there, else empty screens that the
//!   programs are asked to redraw (SIGWINCH). The shells are no longer our
//!   children then: a session ends at PTY EOF, without an exit code.
//!   `systemctl --user stop` empties the fd store, which closes the PTYs: the
//!   shells get SIGHUP, as when a terminal window closes.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use goatty_proto::SessionId;
use goatty_term::pty::{self, Pty};
use goatty_term::{Palette, TermSize, Terminal};
use serde::{Deserialize, Serialize};

use crate::server::{Daemon, NewSession};
use crate::session::Session;
use crate::systemd::StoredPty;
use crate::workspaces::{SavedWorkspace, Workspaces};

/// The newest state format this build reads; `goattyd restore-version`
/// prints it, and a restart only runs a goattyd that reads ours.
/// 3: workspaces (#215) instead of window layouts. A build that reads 2
/// would take the sessions and drop the workspaces, so a restart refuses it.
pub const STATE_VERSION: u32 = 3;

/// How long a restart waits for a session's output pump to pause.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    next_session: u64,
    /// The listening socket, inherited (restart only).
    listener_fd: Option<RawFd>,
    /// Whether the listening socket belongs to systemd/launchd (socket activation).
    #[serde(default)]
    socket_activated: bool,
    sessions: Vec<SavedSession>,
    /// Always empty: builds before v17 read their window layouts here (a
    /// state without it fails to load there), and a v17 build drops what an
    /// older one wrote (#215: old layouts aren't migrated).
    #[serde(default, skip_deserializing)]
    layouts: [(); 0],
    #[serde(default)]
    workspaces: Vec<SavedWorkspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedSession {
    id: SessionId,
    /// The PTY master, inherited (restart only; systemd passes it otherwise).
    fd: Option<RawFd>,
    pid: u32,
    created_unix: u64,
    managed: bool,
    /// The screen as VT bytes, in `s<id>.vt` next to the state.
    snapshot: String,
}

/// Where the state goes: next to the socket (in its 0700 directory), so
/// each daemon has its own.
fn state_dir(socket: &Path) -> PathBuf {
    PathBuf::from(format!("{}.restore", socket.display()))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

/// Pause every session's output pump. False when one didn't pause in time
/// (it may hold output it read but didn't feed yet).
fn pause_all(sessions: &HashMap<SessionId, Arc<Session>>) -> bool {
    let mut all = true;
    for s in sessions.values() {
        if !s.pause.pause(PAUSE_TIMEOUT) {
            log::warn!("session {}: output didn't pause", s.id);
            all = false;
        }
    }
    all
}

/// Wait until the query answers the paused output produced reached the
/// PTYs (a program may wait for one). False when one is still pending after
/// [`PAUSE_TIMEOUT`]: the write blocks, or the queue isn't empty.
fn settle_replies(sessions: &HashMap<SessionId, Arc<Session>>) -> bool {
    let deadline = Instant::now() + PAUSE_TIMEOUT;
    for s in sessions.values() {
        loop {
            // Twice, a moment apart: an answer the thread just took off the
            // queue is counted as taken right after.
            let taken = s.replies_taken.load(Ordering::SeqCst);
            if s.replies_idle() {
                std::thread::sleep(Duration::from_millis(5));
                if s.replies_idle() && s.replies_taken.load(Ordering::SeqCst) == taken {
                    break;
                }
            }
            if Instant::now() > deadline {
                log::warn!("session {}: a query answer is still on its way to the program", s.id);
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    true
}

/// Stop new sessions from starting, waiting for the ones being started to
/// be registered. False when a restart is already under way.
fn close_admission(daemon: &Daemon) -> bool {
    let _exclusive = daemon.admission.write();
    !daemon.restarting.swap(true, Ordering::SeqCst)
}

fn resume_all(sessions: &HashMap<SessionId, Arc<Session>>) {
    for s in sessions.values() {
        s.pause.resume();
    }
}

/// Write the state of `sessions` (paused) to the state dir; `with_fds`
/// records the fds for an `execve`. Returns the state file.
fn save(daemon: &Daemon, sessions: &HashMap<SessionId, Arc<Session>>, with_fds: bool) -> Result<PathBuf> {
    let dir = state_dir(&daemon.socket);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut saved = Vec::new();
    for s in sessions.values() {
        let Some(pid) = s.pid else { continue };
        let snapshot = format!("s{}.vt", s.id);
        write_private(&dir.join(&snapshot), &s.term.restore_snapshot())?;
        saved.push(SavedSession {
            id: s.id,
            fd: with_fds.then_some(s.master_fd),
            pid,
            created_unix: s.created_unix,
            managed: s.managed,
            snapshot,
        });
    }
    saved.sort_by_key(|s| s.id);
    let state = State {
        version: STATE_VERSION,
        next_session: daemon.next_session.load(Ordering::SeqCst),
        listener_fd: if with_fds { *daemon.listener_fd.lock() } else { None },
        socket_activated: daemon.socket_activated.load(Ordering::SeqCst),
        sessions: saved,
        layouts: [],
        workspaces: daemon.workspaces.lock().export(),
    };
    let file = dir.join("state.json");
    write_private(&file, &serde_json::to_vec(&state)?)?;
    Ok(file)
}

fn load(file: &Path) -> Result<State> {
    let state: State = serde_json::from_slice(&std::fs::read(file).with_context(|| format!("read {}", file.display()))?)
        .with_context(|| format!("parse {}", file.display()))?;
    if state.version > STATE_VERSION {
        bail!("{} has state format {}, this goattyd reads up to {STATE_VERSION}", file.display(), state.version);
    }
    Ok(state)
}

/// Run `exe restore-version` and check it can take over from this build.
fn check_successor(exe: &Path) -> Result<()> {
    let mut child = Command::new(exe)
        .arg("restore-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("couldn't run {}", exe.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} didn't answer", exe.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read as _;
        let _ = o.read_to_string(&mut out);
    }
    match out.trim().parse::<u32>() {
        Ok(v) if status.success() && v >= STATE_VERSION => Ok(()),
        Ok(v) => bail!("{} reads restore format {v}, this goattyd writes {STATE_VERSION}", exe.display()),
        Err(_) => bail!("{} is a goattyd that can't take over sessions (too old?)", exe.display()),
    }
}

/// The goattyd a restart runs: `exe`, or the one this daemon was started as.
/// Checked to be one that can take over.
pub(crate) fn successor(daemon: &Daemon, exe: Option<String>) -> Result<PathBuf> {
    let exe = match exe {
        Some(e) => PathBuf::from(e),
        None => daemon.exe.clone().context("don't know which goattyd to run")?,
    };
    if !exe.is_absolute() {
        bail!("{} is not an absolute path", exe.display());
    }
    check_successor(&exe)?;
    Ok(exe)
}

/// Replace this process with `exe`, handing it every session. Returns only
/// when that failed; the daemon then goes on as before.
pub(crate) fn restart(daemon: &Arc<Daemon>, exe: &Path) -> Result<()> {
    if !close_admission(daemon) {
        bail!("a restart is already under way");
    }
    // Held until the exec: no session starts or ends meanwhile.
    let sessions = daemon.sessions.lock();
    let result = (|| -> Result<std::io::Error> {
        // Whatever a pump read and didn't feed, or an answer not written
        // yet, would be lost with this process: rather not restart.
        if !pause_all(&sessions) {
            bail!("a session's output didn't pause; not restarting");
        }
        if !settle_replies(&sessions) {
            bail!("a session's program doesn't take its input; not restarting");
        }
        let file = save(daemon, &sessions, true)?;
        let listener = daemon.listener_fd.lock().context("no listening socket")?;
        let mut fds = vec![listener];
        fds.extend(sessions.values().map(|s| s.master_fd));
        for &fd in &fds {
            pty::set_cloexec(fd, false)?;
        }
        log::info!("restarting as {} with {} sessions", exe.display(), sessions.len());
        let mut cmd = Command::new(exe);
        cmd.arg("serve").arg("--restore").arg(&file);
        if let Some(n) = crate::systemd::notify_socket() {
            cmd.env("NOTIFY_SOCKET", n);
        }
        use std::os::unix::process::CommandExt as _;
        let err = cmd.exec();
        for &fd in &fds {
            let _ = pty::set_cloexec(fd, true);
        }
        let _ = std::fs::remove_dir_all(state_dir(&daemon.socket));
        Ok(err)
    })();
    resume_all(&sessions);
    drop(sessions);
    daemon.restarting.store(false, Ordering::SeqCst);
    match result {
        Ok(err) => Err(err).with_context(|| format!("couldn't run {}", exe.display())),
        Err(e) => Err(e),
    }
}

/// Make a session of an inherited or stored PTY, its screen rebuilt from
/// `snapshot` (or empty).
fn adopt(daemon: &Arc<Daemon>, s: &SavedSession, master: OwnedFd, snapshot: Option<&[u8]>) -> Result<Arc<Session>> {
    // The PTY knows its size (the daemon sets both on every resize).
    let size = pty::window_size(master.as_fd()).unwrap_or(TermSize::new(80, 24));
    let size = TermSize { cols: size.cols.max(2), rows: size.rows.max(1), ..size };
    let (pty, reader) = Pty::adopt(master, s.pid)?;
    let (term, chans) = Terminal::new(size, true, Arc::new(Palette::default()));
    if let Some(bytes) = snapshot {
        term.feed(bytes);
    }
    // Answers to anything in the replay would go to the shell, which
    // didn't ask.
    for _ in chans.replies.try_iter() {}
    let n = NewSession { id: s.id, pty, reader, term, chans, created_unix: s.created_unix, managed: s.managed, creator: None };
    let session = daemon.start(n)?;
    log::info!("session {}: taken over (pid {}{})", s.id, s.pid, if snapshot.is_some() { "" } else { ", no saved screen" });
    Ok(session)
}

fn bump_next_session(daemon: &Daemon, next: u64) {
    daemon.next_session.fetch_max(next, Ordering::SeqCst);
}

/// `goattyd serve --restore FILE`: take the sessions and the socket over
/// from the goattyd that ran before in this process.
pub(crate) fn take_over(daemon: &Arc<Daemon>, file: &Path) -> Result<Option<UnixListener>> {
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let state = load(file);
    let state = match state {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.context("couldn't take over the previous goattyd's sessions"));
        }
    };
    for s in &state.sessions {
        let Some(fd) = s.fd else { continue };
        // SAFETY: the previous goattyd passed this fd for this session, and
        // nothing in this process has adopted it before.
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        let snapshot = std::fs::read(dir.join(&s.snapshot)).ok();
        if let Err(e) = adopt(daemon, s, master, snapshot.as_deref()) {
            log::error!("session {}: couldn't take it over: {e:#}", s.id);
        }
        bump_next_session(daemon, s.id);
    }
    bump_next_session(daemon, state.next_session);
    daemon.socket_activated.store(state.socket_activated, Ordering::SeqCst);
    let live = daemon.live();
    *daemon.workspaces.lock() = Workspaces::import(state.workspaces, |s| live.contains(&s));
    let _ = std::fs::remove_dir_all(&dir);
    state.listener_fd.map(crate::server::listener_from_fd).transpose()
}

/// Rebuild the sessions whose PTYs systemd kept in the fd store (after a
/// crash or `systemctl --user restart`), with the screens saved on SIGTERM
/// when they're there.
pub(crate) fn adopt_stored(daemon: &Arc<Daemon>, ptys: Vec<(StoredPty, OwnedFd)>) {
    let dir = state_dir(&daemon.socket);
    let state = load(&dir.join("state.json")).ok();
    let saved: HashMap<SessionId, SavedSession> =
        state.as_ref().map(|s| s.sessions.iter().map(|x| (x.id, x.clone())).collect()).unwrap_or_default();
    let mut adopted = Vec::new();
    for (stored, master) in ptys {
        // The shell leads the terminal's session; none means it's gone.
        let Some(pid) = pty::session_leader(master.as_fd()) else {
            log::info!("session {}: its shell is gone", stored.id);
            crate::systemd::unstore(&stored);
            continue;
        };
        let same = saved.get(&stored.id).filter(|s| s.pid == pid);
        let s = SavedSession {
            id: stored.id,
            fd: None,
            pid,
            created_unix: stored.created_unix,
            managed: stored.managed,
            snapshot: String::new(),
        };
        let snapshot = same.and_then(|x| std::fs::read(dir.join(&x.snapshot)).ok());
        match adopt(daemon, &s, master, snapshot.as_deref()) {
            Ok(session) => adopted.push((session, snapshot.is_none())),
            Err(e) => log::error!("session {}: couldn't take it over: {e:#}", s.id),
        }
        bump_next_session(daemon, s.id);
    }
    if let Some(state) = state {
        bump_next_session(daemon, state.next_session);
        if !adopted.is_empty() {
            let live = daemon.live();
            *daemon.workspaces.lock() = Workspaces::import(state.workspaces, |s| live.contains(&s));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if !adopted.is_empty() {
        log::info!("took over {} sessions kept by systemd", adopted.len());
    }
    // Without a saved screen, ask the programs to draw theirs again.
    for (session, _) in adopted.iter().filter(|(_, blank)| *blank) {
        if let Some(pgrp) = session.pty.lock().foreground_pid() {
            // SAFETY: plain syscall; signals the terminal's foreground group.
            unsafe { libc::kill(-(pgrp as libc::pid_t), libc::SIGWINCH) };
        }
    }
}

/// The write end of the SIGTERM pipe, for the signal handler.
static SIGTERM_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_sigterm(_: libc::c_int) {
    let fd = SIGTERM_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write(2) is async-signal-safe; one byte from a static buffer.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// Under the systemd service: on SIGTERM (`systemctl --user restart` or
/// `stop`) save the screens and exit without ending the sessions. Their
/// PTYs stay in the fd store for the next start; a stop empties it.
pub(crate) fn save_on_sigterm(daemon: Arc<Daemon>) -> Result<()> {
    let (rx, tx) = std::io::pipe().context("pipe")?;
    let (rx, tx) = (OwnedFd::from(rx), OwnedFd::from(tx));
    SIGTERM_PIPE.store(tx.as_raw_fd(), Ordering::SeqCst);
    std::mem::forget(tx); // the handler writes to it for the life of the process
    // SAFETY: installs a handler that only calls write(2); sigaction is
    // zeroed plain data apart from the fields set.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigterm as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&raw mut sa.sa_mask);
        if libc::sigaction(libc::SIGTERM, &raw const sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("sigaction");
        }
    }
    std::thread::Builder::new().name("sigterm".into()).spawn(move || {
        let mut b = [0u8; 1];
        // SAFETY: reading one byte into a valid buffer from our own pipe.
        while unsafe { libc::read(rx.as_raw_fd(), b.as_mut_ptr().cast(), 1) } != 1 {}
        close_admission(&daemon);
        let sessions = daemon.sessions.lock();
        // Best effort: the process ends either way.
        if !(pause_all(&sessions) && settle_replies(&sessions)) {
            log::warn!("SIGTERM: saving the screens anyway");
        }
        match save(&daemon, &sessions, false) {
            Ok(_) => log::info!("SIGTERM: saved {} sessions; systemd keeps their PTYs", sessions.len()),
            Err(e) => log::error!("SIGTERM: couldn't save the sessions' screens: {e:#}"),
        }
        std::process::exit(0);
    })?;
    Ok(())
}
