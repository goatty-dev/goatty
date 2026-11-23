//! One client connection: decode requests, manage its subscriptions, and
//! write replies and stream frames through a dedicated writer thread.

use std::collections::HashMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use goatty_proto::{
    ClientMsg, FLOW_CONTROL_SINCE, Frame, HISTORY_SINCE, PROTOCOL_VERSION, REMOTE_FILES_SINCE, SIZE_CONTROL_SINCE, ServerMsg, SessionId,
    encode, encode_server, read_msg,
};
use goatty_term::Palette;

use crate::flow::FlowControl;
use crate::server::Daemon;
use crate::session::{Session, SubConn, SubFeatures, Subscriber};

/// Max concurrent `StatPath` worker threads per connection (bounds thread
/// leaks if a remote mount stalls `stat`).
const MAX_STAT_WORKERS: usize = 4;
/// Max concurrent `ReadFile` worker threads per connection (bounds memory and
/// stream head-of-line delay).
const MAX_FILE_WORKERS: usize = 2;
/// Max queued `FileData` bytes per connection waiting in the writer queue.
const MAX_QUEUED_FILE_BYTES: usize = goatty_proto::MAX_FRAME;
/// Max input bytes per connection waiting for a PTY that doesn't read. Past
/// it the reader waits (the client feels backpressure) until the queue
/// drains or the connection is dropped.
const MAX_QUEUED_INPUT_BYTES: usize = 4 << 20;
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
    /// Already shut down for silence.
    dropped: AtomicBool,
    stream: UnixStream,
    /// The connection's writer queue, to stop the writer even while workers
    /// (a stat on a hung mount) still hold senders.
    writer: flume::WeakSender<Outbound>,
}

impl Liveness {
    fn new(stream: UnixStream, writer: flume::WeakSender<Outbound>) -> Self {
        Self {
            born: Instant::now(),
            last_ms: AtomicU64::new(0),
            pings: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            stream,
            writer,
        }
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
/// `attached` count and size control right, and its writer stops. Run every
/// 2 s by `serve`.
pub(crate) fn reap_silent(conns: &parking_lot::Mutex<HashMap<u64, Arc<Liveness>>>, now: Instant, after: Duration) {
    for (conn, live) in conns.lock().iter() {
        let Some(silent) = live.silent_for(now).filter(|s| *s > after) else { continue };
        if !live.dropped.swap(true, Ordering::AcqRel) {
            log::info!("connection {conn}: nothing heard for {}s; dropping it", silent.as_secs());
            if let Some(tx) = live.writer.upgrade() {
                let _ = tx.send(Outbound::stop());
            }
        }
        let _ = live.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// The writer keeps each lane's frames in order and shares the link fairly
/// between lanes, so a quiet one jumps ahead of a flooding one
/// ([`crate::writer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    /// Replies that belong to no session.
    Reply,
    /// A session's stream, and replies about it that must stay in order with it.
    Session(SessionId),
    /// `FileData`, so a big file doesn't hold up the replies behind it.
    File,
}

pub(crate) struct Outbound {
    pub(crate) frame: Frame,
    pub(crate) lane: Lane,
    _bytes: Option<ByteGuard>,
    /// Tells the writer to stop: the connection was dropped.
    pub(crate) stop: bool,
}

impl From<Frame> for Outbound {
    fn from(frame: Frame) -> Self {
        Self { frame, lane: Lane::Reply, _bytes: None, stop: false }
    }
}

impl Outbound {
    /// A frame of `session`'s stream.
    pub(crate) fn session(frame: Frame, session: SessionId) -> Self {
        Self { frame, lane: Lane::Session(session), _bytes: None, stop: false }
    }

    #[cfg(test)]
    pub(crate) fn file(frame: Frame) -> Self {
        Self { frame, lane: Lane::File, _bytes: None, stop: false }
    }

    fn stop() -> Self {
        Self { frame: Frame::from(&[][..]), lane: Lane::Reply, _bytes: None, stop: true }
    }
}

/// Input on its way to a session's PTY (see [`input_loop`]).
struct InputJob {
    session: Arc<Session>,
    data: Vec<u8>,
    /// `SendInput`'s request, answered once the input is written.
    reply: Option<goatty_proto::ReqId>,
    _bytes: ByteGuard,
}

/// Write a connection's input to the PTYs, in order. Off the reader thread:
/// a program that stops reading fills the PTY and blocks the write, and the
/// reader must still see pings, `Detach` and EOF meanwhile. Replies go
/// through a weak sender so a stuck write never keeps the writer alive.
fn input_loop(rx: flume::Receiver<InputJob>, replies: flume::WeakSender<Outbound>) {
    while let Ok(job) = rx.recv() {
        let _ = job.session.input.lock().write(&job.data);
        match job.reply {
            Some(req) => {
                if let (Some(tx), Ok(f)) = (replies.upgrade(), encode(&ServerMsg::Ok { req })) {
                    let _ = tx.send(Outbound::from(f));
                }
            }
            // Enter may have started a program that prints nothing.
            None => job.session.poke_foreground(),
        }
    }
}

pub(crate) struct WorkerGuard(Arc<AtomicUsize>);

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ByteGuard {
    counter: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for ByteGuard {
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

fn try_reserve_bytes(counter: &Arc<AtomicUsize>, bytes: usize, limit: usize) -> Option<ByteGuard> {
    let mut cur = counter.load(Ordering::Acquire);
    // Always admit a single valid frame when nothing else is queued so a
    // HARD_MAX_FILE_BYTES reply cannot be starved by metadata overhead.
    while cur == 0 || cur.saturating_add(bytes) <= limit {
        match counter.compare_exchange_weak(cur, cur.saturating_add(bytes), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(ByteGuard { counter: counter.clone(), bytes }),
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
    // carry a `ByteGuard` bounded by `MAX_QUEUED_FILE_BYTES`.
    let (tx, rx) = flume::unbounded::<Outbound>();
    // An empty frame (real ones never are) asks the writer to flush and say
    // so here: everything queued before it has been sent.
    let (flushed_tx, flushed_rx) = flume::bounded::<()>(1);
    let wstream = stream.try_clone()?;
    let live = Arc::new(Liveness::new(stream.try_clone()?, tx.downgrade()));
    let writer = std::thread::Builder::new().name(format!("c{conn}-writer")).spawn(move || {
        crate::writer::run(&rx, &wstream, &flushed_tx);
        let _ = wstream.shutdown(std::net::Shutdown::Both);
    })?;
    let (input_tx, input_rx) = flume::unbounded::<InputJob>();
    {
        let replies = tx.downgrade();
        std::thread::Builder::new().name(format!("c{conn}-input")).spawn(move || input_loop(input_rx, replies))?;
    }
    let input_queued = Arc::new(AtomicUsize::new(0));
    // Queue input for the PTY. Past the budget wait for room, unless the
    // connection was dropped meanwhile (`false`: stop reading).
    let queue_input = |session: Arc<Session>, data: Vec<u8>, reply| {
        loop {
            if let Some(guard) = try_reserve_bytes(&input_queued, data.len(), MAX_QUEUED_INPUT_BYTES) {
                let _ = input_tx.send(InputJob { session, data, reply, _bytes: guard });
                return true;
            }
            if live.dropped.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
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
    let flow = Arc::new(FlowControl::new());
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
                    features.flow_control = agreed >= FLOW_CONTROL_SINCE;
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
                            let sub = s.subscribe(SubConn { id: conn, features, flow: &flow, tx: &tx }, req, size, opts);
                            subs.insert(s.id, (s, sub));
                        }
                        Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                    }
                }
                ClientMsg::Run { req, size, cwd, argv, opts } => match daemon.run(conn, size, cwd, argv) {
                    Ok(s) => {
                        let sub = s.subscribe(SubConn { id: conn, features, flow: &flow, tx: &tx }, req, size, opts);
                        subs.insert(s.id, (s, sub));
                    }
                    Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                },
                ClientMsg::Attach { req, session, size, opts } => match daemon.get(session) {
                    Some(s) => {
                        let sub = s.subscribe(SubConn { id: conn, features, flow: &flow, tx: &tx }, req, size, opts);
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
                    if let Some((s, _)) = subs.get(&session)
                        && !queue_input(s.clone(), data, None)
                    {
                        return Ok(());
                    }
                }
                ClientMsg::Resize { session, size } => {
                    if let Some((s, _)) = subs.get(&session) {
                        s.resize(size, conn);
                    }
                }
                ClientMsg::Ack { session, consumed } => {
                    if let Some((s, sub)) = subs.get(&session)
                        && let Some(window) = s.ack(sub, consumed)
                    {
                        reply(ServerMsg::FlowWindow { window });
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
                        if !queue_input(s, data, Some(req)) {
                            return Ok(());
                        }
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
                ClientMsg::ClaimLayoutId { req, id } => reply(ServerMsg::Layout { req, layout: daemon.claim_layout_id(conn, id) }),
                ClientMsg::ListLayouts { req } => reply(ServerMsg::Layouts { req, layouts: daemon.free_layouts() }),
                ClientMsg::ReleaseLayout { id, sessions, json } => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    daemon.layouts.lock().release_one(conn, id, sessions, json, now);
                }
                ClientMsg::FetchHistory { req, session, before, count } => match daemon.get(session) {
                    Some(s) => {
                        let rows = s.term.history_rows(before, count as usize);
                        let msg = ServerMsg::History { req, session, before, first: rows.first, oldest: rows.oldest, data: rows.data };
                        if let Ok(f) = encode_server(&msg, features.zstd) {
                            let _ = tx.send(Outbound::session(f, session));
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
                                if let Some(byte_guard) = try_reserve_bytes(&file_queued_bytes, f.len(), MAX_QUEUED_FILE_BYTES) {
                                    let _ = tx.send(Outbound { frame: f, lane: Lane::File, _bytes: Some(byte_guard), stop: false });
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
                ClientMsg::Rtt { rtt_us } => {
                    if let Some(window) = flow.on_rtt(rtt_us) {
                        reply(ServerMsg::FlowWindow { window });
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
    // Queued input still goes out; the input thread ends after it.
    drop(input_tx);
    drop(tx);
    // A worker stuck on a hung mount keeps the writer waiting: stay
    // registered meanwhile, so silence still closes the socket and stops it.
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
        // A worker (stuck on a hung mount) still holds a sender of each queue.
        let (quiet_tx, quiet_rx) = flume::unbounded::<Outbound>();
        let (pinging_tx, pinging_rx) = flume::unbounded::<Outbound>();
        let quiet = Arc::new(Liveness::new(quiet, quiet_tx.downgrade()));
        let pinging = Arc::new(Liveness::new(pinging, pinging_tx.downgrade()));
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
        assert!(quiet_rx.is_empty(), "a kept connection's writer goes on");
        assert!(pinging_rx.try_recv().is_ok_and(|o| o.stop), "the dropped connection's writer is told to stop");

        // Later ticks don't queue more stops.
        reap_silent(&conns, later, SILENT_AFTER);
        assert!(pinging_rx.is_empty());
    }

    #[test]
    fn queued_file_byte_budget_bounds_and_releases() {
        let queued = Arc::new(AtomicUsize::new(0));
        let limit = 100;
        let g1 = try_reserve_bytes(&queued, 60, limit).expect("first fits");
        assert_eq!(queued.load(Ordering::Acquire), 60);
        assert!(try_reserve_bytes(&queued, 50, limit).is_none(), "second exceeds remaining budget");
        let g2 = try_reserve_bytes(&queued, 40, limit).expect("exact remaining fits");
        assert_eq!(queued.load(Ordering::Acquire), 100);
        drop(g1);
        assert_eq!(queued.load(Ordering::Acquire), 40);
        drop(g2);
        assert_eq!(queued.load(Ordering::Acquire), 0);

        // Single oversized frame is still admitted when the queue is empty.
        let big = try_reserve_bytes(&queued, 120, limit).expect("single frame when empty");
        assert_eq!(queued.load(Ordering::Acquire), 120);
        assert!(try_reserve_bytes(&queued, 1, limit).is_none(), "blocks while oversized frame queued");
        drop(big);
        assert_eq!(queued.load(Ordering::Acquire), 0);
    }
}
