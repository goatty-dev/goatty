//! One client connection: decode requests, manage its subscriptions, and
//! write replies and stream frames through a dedicated writer thread.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use goatty_proto::{
    ClientMsg, Frame, HISTORY_SINCE, PROTOCOL_VERSION, REMOTE_FILES_SINCE, SIZE_CONTROL_SINCE, ServerMsg, SessionId, encode, encode_server,
    read_msg,
};
use goatty_term::Palette;

use crate::server::Daemon;
use crate::session::{Session, SubFeatures, Subscriber};

/// Max concurrent `StatPath` worker threads per connection (bounds thread
/// leaks if a remote mount stalls `stat`).
const MAX_STAT_WORKERS: usize = 4;
/// Max concurrent `ReadFile` worker threads per connection (bounds memory and
/// stream head-of-line delay).
const MAX_FILE_WORKERS: usize = 2;
/// Max queued `FileData` bytes per connection waiting in the writer queue.
const MAX_QUEUED_FILE_BYTES: usize = goatty_proto::MAX_FRAME;
/// A connection that pings (remote clients do, every 5 s) and then sends
/// nothing for this long is gone: a dead TCP link behind ssh never closes
/// the proxy's socket, so EOF alone would keep it subscribed (#13).
pub(crate) const SILENT_AFTER: Duration = Duration::from_secs(60);

/// When a connection was last heard from, for [`reap_silent`].
pub(crate) struct Liveness {
    born: Instant,
    /// Milliseconds after `born` of the last message.
    last_ms: AtomicU64,
    /// It has sent a `Ping`, so silence means it's gone. Local clients
    /// don't ping: an idle one is just idle, and a dead one closes the
    /// socket.
    pings: AtomicBool,
    /// Already shut down for silence (log it once).
    dropped: AtomicBool,
    stream: UnixStream,
}

impl Liveness {
    fn new(stream: UnixStream) -> Self {
        Self { born: Instant::now(), last_ms: AtomicU64::new(0), pings: AtomicBool::new(false), dropped: AtomicBool::new(false), stream }
    }

    fn heard(&self, ping: bool) {
        let ms = u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
        if ping {
            self.pings.store(true, Ordering::Relaxed);
        }
    }

    /// How long a pinging connection has been silent at `now`; `None` for
    /// one that never pinged.
    fn silent_for(&self, now: Instant) -> Option<Duration> {
        if !self.pings.load(Ordering::Relaxed) {
            return None;
        }
        let last = self.born + Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
        Some(now.saturating_duration_since(last))
    }
}

/// Shut down every pinging connection silent for longer than `after`. Its
/// reader then sees EOF and ends its subscriptions, which keeps the
/// `attached` count and size control right. Run every 2 s by `serve`.
pub(crate) fn reap_silent(conns: &parking_lot::Mutex<HashMap<u64, Arc<Liveness>>>, now: Instant, after: Duration) {
    for (conn, live) in conns.lock().iter() {
        let Some(silent) = live.silent_for(now).filter(|s| *s > after) else { continue };
        if !live.dropped.swap(true, Ordering::Relaxed) {
            log::info!("connection {conn}: nothing heard for {}s; dropping it", silent.as_secs());
        }
        let _ = live.stream.shutdown(std::net::Shutdown::Both);
    }
}

pub(crate) struct Outbound {
    pub(crate) frame: Frame,
    _file_bytes: Option<FileByteGuard>,
}

impl From<Frame> for Outbound {
    fn from(frame: Frame) -> Self {
        Self { frame, _file_bytes: None }
    }
}

pub(crate) struct WorkerGuard(Arc<AtomicUsize>);

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct FileByteGuard {
    counter: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for FileByteGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub(crate) fn try_acquire_worker(counter: &Arc<AtomicUsize>, limit: usize) -> Option<WorkerGuard> {
    let mut cur = counter.load(Ordering::Acquire);
    while cur < limit {
        match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(WorkerGuard(counter.clone())),
            Err(next) => cur = next,
        }
    }
    None
}

fn try_reserve_file_bytes(counter: &Arc<AtomicUsize>, bytes: usize, limit: usize) -> Option<FileByteGuard> {
    let mut cur = counter.load(Ordering::Acquire);
    // Always admit a single valid frame when nothing else is queued so a
    // HARD_MAX_FILE_BYTES reply cannot be starved by metadata overhead.
    while cur == 0 || cur.saturating_add(bytes) <= limit {
        match counter.compare_exchange_weak(cur, cur.saturating_add(bytes), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(FileByteGuard { counter: counter.clone(), bytes }),
            Err(next) => cur = next,
        }
    }
    None
}

/// Serve one client connection until it closes. Its subscriptions end with
/// it; the sessions keep running.
pub(crate) fn handle(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let conn = daemon.next_conn.fetch_add(1, Ordering::Relaxed);
    // Unbounded for stream/control: stream frames are limited by each
    // subscriber's window, control replies are tiny, and `FileData` frames
    // carry a `FileByteGuard` bounded by `MAX_QUEUED_FILE_BYTES`.
    let (tx, rx) = flume::unbounded::<Outbound>();
    // An empty frame (real ones never are) asks the writer to flush and say
    // so here: everything queued before it has been sent.
    let (flushed_tx, flushed_rx) = flume::bounded::<()>(1);
    let wstream = stream.try_clone()?;
    let live = Arc::new(Liveness::new(stream.try_clone()?));
    let writer = std::thread::Builder::new().name(format!("c{conn}-writer")).spawn(move || {
        let mut w = BufWriter::with_capacity(128 << 10, &wstream);
        while let Ok(out) = rx.recv() {
            if out.frame.is_empty() {
                if w.flush().is_err() {
                    break;
                }
                let _ = flushed_tx.try_send(());
                continue;
            }
            if w.write_all(&out.frame).is_err() {
                break;
            }
            // Drop `out` (and any `FileByteGuard`) as soon as it is written.
            drop(out);
            // Batch under load; flush the moment the queue is idle.
            if rx.is_empty() && w.flush().is_err() {
                break;
            }
        }
        let _ = wstream.shutdown(std::net::Shutdown::Both);
    })?;
    // Registered only once nothing can fail before the cleanup below.
    daemon.conns.lock().insert(conn, live.clone());
    let reply = |msg: ServerMsg| {
        if let Ok(f) = encode(&msg) {
            let _ = tx.send(Outbound::from(f));
        }
    };

    let mut subs: HashMap<SessionId, (Arc<Session>, Arc<Subscriber>)> = HashMap::new();
    // The protocol version agreed in the handshake.
    let mut agreed_version = 0;
    let mut features = SubFeatures::default();
    let stat_workers = Arc::new(AtomicUsize::new(0));
    let file_workers = Arc::new(AtomicUsize::new(0));
    let file_queued_bytes = Arc::new(AtomicUsize::new(0));
    let mut rstream = BufReader::with_capacity(64 << 10, stream);
    let result = (|| -> Result<()> {
        loop {
            let msg: ClientMsg = match read_msg(&mut rstream) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            live.heard(matches!(msg, ClientMsg::Ping { .. }));
            match msg {
                ClientMsg::Hello { version, client } => {
                    let host = crate::server::host_info();
                    let daemon_pid = std::process::id();
                    let Some(agreed) = goatty_proto::negotiate(version) else {
                        // Our newest version tells the client which side is older.
                        log::info!("connection {conn}: {client} speaks protocol v{version}, which this goattyd doesn't");
                        reply(ServerMsg::Hello { version: PROTOCOL_VERSION, daemon_pid, host });
                        return Ok(());
                    };
                    log::debug!("connection {conn}: {client} (protocol v{agreed})");
                    agreed_version = agreed;
                    features.lines = agreed >= HISTORY_SINCE;
                    features.cwd = agreed >= REMOTE_FILES_SINCE;
                    features.size_ctl = agreed >= SIZE_CONTROL_SINCE;
                    reply(ServerMsg::Hello { version: agreed, daemon_pid, host });
                }
                ClientMsg::ListSessions { req } => reply(ServerMsg::Sessions { req, list: daemon.list() }),
                ClientMsg::Create { size, req, .. } | ClientMsg::Run { size, req, .. } | ClientMsg::Attach { size, req, .. }
                    if !size.is_sane() =>
                {
                    reply(ServerMsg::Error { req: Some(req), message: format!("window size {}×{} is too large", size.cols, size.rows) });
                }
                ClientMsg::Resize { size, .. } if !size.is_sane() => {
                    log::warn!("connection {conn}: refused a {}×{} resize", size.cols, size.rows);
                    reply(ServerMsg::Error { req: None, message: format!("window size {}×{} is too large", size.cols, size.rows) });
                }
                ClientMsg::Create { req, size, cwd, opts } => {
                    match daemon.create(conn, size, cwd, agreed_version >= goatty_proto::SHELL_SINCE) {
                        Ok(s) => {
                            let sub = s.subscribe(conn, req, size, opts, features, &tx);
                            subs.insert(s.id, (s, sub));
                        }
                        Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                    }
                }
                ClientMsg::Run { req, size, cwd, argv, opts } => match daemon.run(conn, size, cwd, argv) {
                    Ok(s) => {
                        let sub = s.subscribe(conn, req, size, opts, features, &tx);
                        subs.insert(s.id, (s, sub));
                    }
                    Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                },
                ClientMsg::Attach { req, session, size, opts } => match daemon.get(session) {
                    Some(s) => {
                        let sub = s.subscribe(conn, req, size, opts, features, &tx);
                        subs.insert(s.id, (s, sub));
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::Detach { session } => {
                    if let Some((s, _)) = subs.remove(&session) {
                        s.unsubscribe(conn);
                    }
                }
                ClientMsg::Input { session, data } => {
                    if let Some((s, _)) = subs.get(&session) {
                        let _ = s.pty.lock().write(&data);
                        // Enter may have started a program that prints nothing.
                        s.poke_foreground();
                    }
                }
                ClientMsg::Resize { session, size } => {
                    if let Some((s, _)) = subs.get(&session) {
                        s.resize(size, conn);
                    }
                }
                ClientMsg::Ack { session, consumed } => {
                    if let Some((s, sub)) = subs.get(&session) {
                        s.ack(sub, consumed);
                    }
                }
                ClientMsg::Ping { nonce } => reply(ServerMsg::Pong { nonce }),
                ClientMsg::Kill { req, session } => match daemon.get(session) {
                    Some(s) => {
                        s.pty.lock().kill();
                        reply(ServerMsg::Ok { req });
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::SendInput { req, session, data } => match daemon.get(session) {
                    Some(s) => {
                        let _ = s.pty.lock().write(&data);
                        reply(ServerMsg::Ok { req });
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::Shutdown { req } => {
                    reply(ServerMsg::Ok { req });
                    daemon.shutdown();
                }
                ClientMsg::Restart { req, exe } => match crate::restore::successor(&daemon, exe) {
                    Ok(exe) => {
                        reply(ServerMsg::Ok { req });
                        // The exec closes the connection: let the reply out first.
                        let _ = tx.send(Outbound::from(Frame::from(&[][..])));
                        if flushed_rx.recv_timeout(std::time::Duration::from_secs(2)).is_err() {
                            log::warn!("connection {conn}: the restart reply may not have gone out");
                        }
                        if let Err(e) = crate::restore::restart(&daemon, &exe) {
                            log::error!("restart failed, going on as before: {e:#}");
                        }
                    }
                    Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                },
                ClientMsg::GetHostSettings { req } => reply(crate::settings::get(&daemon, req)),
                ClientMsg::SetHostSettings { req, json, apply } => reply(crate::settings::set(&daemon, req, &json, apply)),
                ClientMsg::ListGenerations { req } => reply(crate::settings::generations(&daemon, req)),
                ClientMsg::Rollback { req, generation } => reply(crate::settings::rollback(&daemon, req, generation)),
                ClientMsg::ReloadShells { req } => {
                    reply(ServerMsg::ShellsReloaded { req, count: u32::try_from(daemon.host.reload_shells()).unwrap_or(u32::MAX) })
                }
                ClientMsg::Plugin { req, request } => {
                    // Installs download and build a generation: don't hold
                    // up this connection's input (keystrokes) meanwhile.
                    let (daemon, tx) = (daemon.clone(), tx.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-plugin")).spawn(move || {
                        let msg = crate::settings::plugin(&daemon, req, &request);
                        if let Ok(f) = encode(&msg) {
                            let _ = tx.send(Outbound::from(f));
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't start the plugin operation: {e}") });
                    }
                }
                ClientMsg::Shell { req, request } => {
                    // The status starts a shell, installs download: off the reader thread.
                    let (daemon, tx) = (daemon.clone(), tx.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-shell")).spawn(move || {
                        let msg = crate::settings::shell(&daemon, req, &request);
                        if let Ok(f) = encode(&msg) {
                            let _ = tx.send(Outbound::from(f));
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't start the shell operation: {e}") });
                    }
                }
                ClientMsg::Net { req, request } => {
                    // Runs `tailscale`: off the reader thread.
                    let (daemon, tx) = (daemon.clone(), tx.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-net")).spawn(move || {
                        let msg = crate::net::request(&daemon, req, &request);
                        if let Ok(f) = encode(&msg) {
                            let _ = tx.send(Outbound::from(f));
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't start the tailnet operation: {e}") });
                    }
                }
                ClientMsg::SaveLayout { id, sessions, json } => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    daemon.layouts.lock().save(conn, id, sessions, json, now);
                }
                ClientMsg::ClaimLayout { req } => reply(ServerMsg::Layout { req, layout: daemon.claim_layout(conn) }),
                ClientMsg::FetchHistory { req, session, before, count } => match daemon.get(session) {
                    Some(s) => {
                        let rows = s.term.history_rows(before, count as usize);
                        let msg = ServerMsg::History { req, session, before, first: rows.first, oldest: rows.oldest, data: rows.data };
                        if let Ok(f) = encode_server(&msg, features.zstd) {
                            let _ = tx.send(Outbound::from(f));
                        }
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::Compress { zstd } => {
                    features.zstd = zstd;
                    for (_, sub) in subs.values() {
                        sub.zstd.store(zstd, Ordering::Relaxed);
                    }
                }
                ClientMsg::SetPalette { session, colors } => {
                    // Any client of the session may set it (last one wins);
                    // it only affects answers to color queries.
                    match (daemon.get(session), Palette::from_rgb_list(&colors, true)) {
                        (Some(s), Some(p)) => s.term.set_palette(Arc::new(p)),
                        (_, None) => log::debug!("connection {conn}: bad palette ({} colors)", colors.len()),
                        (None, _) => {}
                    }
                }
                ClientMsg::StatPath { req, session, cwd, candidates } => {
                    // Stat off the reader thread in case the host has a slow/hung mount.
                    let Some(guard) = try_acquire_worker(&stat_workers, MAX_STAT_WORKERS) else {
                        reply(ServerMsg::PathStat { req, info: None });
                        continue;
                    };
                    let (daemon, tx) = (daemon.clone(), tx.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-stat")).spawn(move || {
                        let _guard = guard;
                        let info = crate::files::stat_path(&daemon, session, cwd.as_deref(), &candidates);
                        if let Ok(f) = encode(&ServerMsg::PathStat { req, info }) {
                            let _ = tx.send(Outbound::from(f));
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't stat path: {e}") });
                    }
                }
                ClientMsg::TakeSize { session } => {
                    if let Some((s, _)) = subs.get(&session) {
                        s.take_size(conn);
                    }
                }
                ClientMsg::ReadFile { req, path, max_bytes } => {
                    if file_queued_bytes.load(Ordering::Acquire) >= MAX_QUEUED_FILE_BYTES {
                        reply(ServerMsg::Error { req: Some(req), message: "file transfer queue is full; try again shortly".into() });
                        continue;
                    }
                    let Some(guard) = try_acquire_worker(&file_workers, MAX_FILE_WORKERS) else {
                        reply(ServerMsg::Error { req: Some(req), message: "too many concurrent file reads".into() });
                        continue;
                    };
                    let (tx, zstd, file_queued_bytes) = (tx.clone(), features.zstd, file_queued_bytes.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-file")).spawn(move || {
                        let _guard = guard;
                        let msg = crate::files::read_file(req, &path, max_bytes);
                        let is_data = matches!(msg, ServerMsg::FileData { .. });
                        if let Ok(f) = encode_server(&msg, zstd) {
                            if is_data {
                                if let Some(byte_guard) = try_reserve_file_bytes(&file_queued_bytes, f.len(), MAX_QUEUED_FILE_BYTES) {
                                    let _ = tx.send(Outbound { frame: f, _file_bytes: Some(byte_guard) });
                                } else if let Ok(err) = encode(&ServerMsg::Error {
                                    req: Some(req),
                                    message: "file transfer queue is full; try again shortly".into(),
                                }) {
                                    let _ = tx.send(Outbound::from(err));
                                }
                            } else {
                                let _ = tx.send(Outbound::from(f));
                            }
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't read file: {e}") });
                    }
                }
            }
        }
    })();

    daemon.layouts.lock().release(conn);
    for (id, (s, _)) in subs {
        s.unsubscribe(conn);
        log::info!("connection {conn} detached from session {id}");
    }
    drop(tx);
    // A worker stuck on a hung mount keeps the writer waiting: stay
    // registered meanwhile, so silence still closes the socket.
    let _ = writer.join();
    daemon.conns.lock().remove(&conn);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the peer of a reaped stream sees it closed.
    fn closed(peer: &UnixStream) -> bool {
        use std::io::Read;
        peer.set_nonblocking(true).unwrap();
        matches!((&*peer).read(&mut [0u8; 1]), Ok(0))
    }

    #[test]
    fn reaps_only_silent_pinging_connections() {
        let (quiet, quiet_peer) = UnixStream::pair().unwrap();
        let (pinging, pinging_peer) = UnixStream::pair().unwrap();
        let conns = parking_lot::Mutex::new(HashMap::new());
        let quiet = Arc::new(Liveness::new(quiet));
        let pinging = Arc::new(Liveness::new(pinging));
        quiet.heard(false);
        pinging.heard(true);
        conns.lock().insert(1, quiet);
        conns.lock().insert(2, pinging.clone());

        // Recently heard from: both stay.
        reap_silent(&conns, Instant::now(), SILENT_AFTER);
        assert!(!closed(&quiet_peer) && !closed(&pinging_peer));

        // Long silent: only the one that pings goes; a local client that
        // never pings may idle forever.
        let later = Instant::now() + SILENT_AFTER + Duration::from_secs(1);
        reap_silent(&conns, later, SILENT_AFTER);
        assert!(!closed(&quiet_peer), "a connection that never pinged is kept");
        assert!(closed(&pinging_peer), "a silent pinging connection is shut down");
        assert!(pinging.dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn queued_file_byte_budget_bounds_and_releases() {
        let queued = Arc::new(AtomicUsize::new(0));
        let limit = 100;
        let g1 = try_reserve_file_bytes(&queued, 60, limit).expect("first fits");
        assert_eq!(queued.load(Ordering::Acquire), 60);
        assert!(try_reserve_file_bytes(&queued, 50, limit).is_none(), "second exceeds remaining budget");
        let g2 = try_reserve_file_bytes(&queued, 40, limit).expect("exact remaining fits");
        assert_eq!(queued.load(Ordering::Acquire), 100);
        drop(g1);
        assert_eq!(queued.load(Ordering::Acquire), 40);
        drop(g2);
        assert_eq!(queued.load(Ordering::Acquire), 0);

        // Single oversized frame is still admitted when the queue is empty.
        let big = try_reserve_file_bytes(&queued, 120, limit).expect("single frame when empty");
        assert_eq!(queued.load(Ordering::Acquire), 120);
        assert!(try_reserve_file_bytes(&queued, 1, limit).is_none(), "blocks while oversized frame queued");
        drop(big);
        assert_eq!(queued.load(Ordering::Acquire), 0);
    }
}
