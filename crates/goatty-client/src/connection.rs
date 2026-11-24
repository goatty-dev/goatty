//! [`Connection`]: a reconnecting, multiplexed link to one goattyd.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{BufWriter, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use goatty_proto::{
    ClientMsg, FLOW_CONTROL_SINCE, FileDiff, Frame, GenerationInfo, HISTORY_SINCE, HostInfo, NET_SINCE, PROTOCOL_VERSION,
    REMOTE_FILES_SINCE, RUN_SINCE, RemotePathInfo, ReqId, SHELL_SINCE, SIZE_CONTROL_SINCE, SaveOutcome, ServerMsg, SessionId, SessionInfo,
    StreamOpts, ViewId, WinSize, WorkspaceId, WorkspaceInfo, WorkspaceLayout, encode, read_msg,
};
use parking_lot::Mutex;

use crate::handshake::Outcome;
use crate::transport::{Closer, Endpoint, Transport};
use crate::{History, PaneEvent, PaneSink, handshake, remote};

/// How long to wait for a failed transport command's last words on stderr.
const STDERR_GRACE: Duration = Duration::from_secs(2);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Plugin operations download releases and registries.
pub const PLUGIN_TIMEOUT: Duration = Duration::from_secs(600);
const HEARTBEAT: Duration = Duration::from_secs(5);
const DEAD_AFTER: Duration = Duration::from_secs(20);
/// How long [`Connection::drop`] waits for the writer thread to flush queued
/// frames before tearing down the transport.
const WRITER_CLOSE_TIMEOUT: Duration = Duration::from_millis(250);

/// A request was sent to `goattyd`, but the link dropped or timed out before a
/// reply arrived. For state-changing requests, the daemon may still have applied
/// the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyLost;

impl fmt::Display for ReplyLost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("goattyd did not answer (connection lost?)")
    }
}

impl std::error::Error for ReplyLost {}

enum WriterCmd {
    Frame(Frame),
    Replace(Box<dyn Write + Send>),
    Close(flume::Sender<()>),
}

struct Pane {
    sink: Arc<dyn PaneSink>,
    size: WinSize,
    consumed: u64,
    acked: u64,
    /// Last `SetPalette` colors, re-sent after a reattach.
    palette: Option<Vec<u32>>,
    /// Subscribed on the current link: false from a dropped link until the
    /// reattach's `Attached`.
    linked: bool,
}

/// The host's settings, as the daemon reports them.
#[derive(Debug, Clone)]
pub struct HostSettingsReply {
    /// The settings document (`goatty_config::store::parse` reads it).
    pub json: String,
    pub issues: Vec<String>,
    pub env_overrides: Vec<String>,
    pub generation: Option<u32>,
}

/// Result of [`Connection::set_host_settings`].
#[derive(Debug, Clone)]
pub struct HostSettingsApplied {
    pub issues: Vec<String>,
    pub diff: Vec<FileDiff>,
    pub applied: bool,
    pub generation: Option<u32>,
}

/// One session to attach in [`Connection::attach_many`].
#[derive(Clone)]
pub struct AttachRequest {
    pub session: SessionId,
    pub size: WinSize,
    pub sink: Arc<dyn PaneSink>,
}

const ATTACH_ACTIVE: u8 = 0;
const ATTACH_CANCELLED: u8 = 1;
const ATTACH_FINISHING: u8 = 2;

enum Pending {
    Reply(flume::Sender<ServerMsg>),
    Attach {
        tx: flume::Sender<ServerMsg>,
        sink: Arc<dyn PaneSink>,
        size: WinSize,
        /// `(link_gen, session)` for `Attach` (`Create` and `Run` learn it from the reply).
        session: Option<(u64, SessionId)>,
        /// Coordinates `attach_many_within` timeout cancellation with `dispatch`.
        state: Option<Arc<AtomicU8>>,
    },
    Reattach(SessionId),
    /// `fetch_history`: the answer goes to the pane's sink.
    History(SessionId),
    /// `JoinWorkspace` / `CreateWorkspace`: the view is registered when the
    /// reply is read, so no `LayoutChanged` after it is missed; unless the
    /// window left meanwhile (`left` moved past `left_gen`).
    Join {
        tx: flume::Sender<ServerMsg>,
        view: ViewId,
        sink: Arc<dyn WorkspaceSink>,
        left_gen: u64,
    },
    /// A view joining its workspace again after a reconnect.
    Rejoin(ViewId),
}

/// What a connection tells a window about the workspace it shows there
/// (`WorkspaceSink`).
#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    /// Another window saved the workspace this one shows.
    Changed(WorkspaceLayout),
    /// The host's workspaces changed (made, renamed, closed, ended, or
    /// shown by another window). `seq`: larger is newer (events and list
    /// replies can arrive in either order).
    List { seq: u64, list: Vec<WorkspaceInfo> },
    /// The link came back and the window joined again: the workspace it
    /// showed, or the most recently active one when that's gone.
    Rejoined { info: WorkspaceInfo, layout: WorkspaceLayout },
}

/// Where a window hears about its workspaces. Called on the connection's
/// reader thread: queue it, don't block. `conn` is [`Connection::key`].
pub trait WorkspaceSink: Send + Sync {
    fn workspace(&self, conn: usize, event: WorkspaceEvent);
}

/// A window showing a workspace on this connection.
struct View {
    workspace: WorkspaceId,
    sink: Arc<dyn WorkspaceSink>,
}

type Reader = std::io::BufReader<Box<dyn Read + Send>>;

struct Shared {
    endpoint: Endpoint,
    opts: StreamOpts,
    /// Active flow-control window in bytes (starts at `opts.window` and
    /// tracks `ServerMsg::FlowWindow` on remote v15+ links).
    flow_window: AtomicU32,
    client_name: String,
    auto_reconnect: bool,
    out: flume::Sender<WriterCmd>,
    panes: Mutex<HashMap<SessionId, Pane>>,
    /// Number of in-flight `ClientMsg::Attach` requests per `(link_gen, session)`,
    /// used to serialize `detach_sink` against replacement attaches on a pooled link.
    attaching: Mutex<HashMap<(u64, SessionId), usize>>,
    /// The windows that show a workspace on this (pooled) connection: they
    /// join it again after a reconnect and hear its changes.
    views: Mutex<HashMap<ViewId, View>>,
    /// How often each window left ([`Connection::leave_workspace`]): a join
    /// answered after its window left doesn't register it.
    left: Mutex<HashMap<ViewId, u64>>,
    pending: Mutex<HashMap<ReqId, Pending>>,
    /// `SizeControl` that came before the `Attached` of a session we don't
    /// have a pane for yet (v14 sends it first); delivered before its snapshot.
    early_size: Mutex<HashMap<SessionId, PaneEvent>>,
    next_req: AtomicU32,
    host: Mutex<HostInfo>,
    /// The protocol version we announce: ours, or lower after an old
    /// exact-match daemon told us its version.
    announce: AtomicU32,
    /// The version agreed on the current link.
    version: AtomicU32,
    /// The daemon's pid on the current link.
    daemon_pid: AtomicU32,
    /// Incremented on every disconnect so state bound to one daemon connection
    /// (such as in-flight attaches) cannot bleed across a reconnect.
    link_gen: AtomicU64,
    connected: AtomicBool,
    closed: AtomicBool,
    closer: Mutex<Option<Closer>>,
    epoch: Instant,
    last_rx_us: AtomicU64,
    rtt_us: AtomicU64,
    /// [`Connection::reconnect`]: try now, and let ssh prompt.
    reconnect_tx: flume::Sender<()>,
    reconnect_rx: flume::Receiver<()>,
}

impl Shared {
    fn send(&self, msg: &ClientMsg) {
        match encode(msg) {
            Ok(f) => {
                let _ = self.out.send(WriterCmd::Frame(f));
            }
            Err(e) => log::error!("encode: {e}"),
        }
    }

    fn req_id(&self) -> ReqId {
        self.next_req.fetch_add(1, Ordering::Relaxed)
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn close_transport(&self) {
        if let Some(c) = self.closer.lock().take() {
            c.close();
        }
    }

    /// Open a transport and shake hands, announcing a lower version once if
    /// an old daemon asks for it.
    /// `interactive: false` for automatic reconnects (ssh must not prompt).
    fn connect(&self, interactive: bool) -> Result<(Reader, HostInfo, u32)> {
        for _ in 0..2 {
            if self.closed.load(Ordering::Acquire) {
                bail!("connection closed");
            }
            if let Some(ok) = self.handshake(self.endpoint.connect(interactive)?)? {
                return Ok(ok);
            }
        }
        bail!("goattyd via {} keeps rejecting the handshake", self.endpoint.label())
    }

    /// `None`: try again with the (now lowered) announced version.
    fn handshake(&self, mut t: Transport) -> Result<Option<(Reader, HostInfo, u32)>> {
        let announced = self.announce.load(Ordering::Relaxed);
        let stderr = t.stderr.clone();
        // A transport that never answers (hung ssh) is torn down by the caller
        // via the stored closer, which unblocks the read.
        {
            let mut closer = self.closer.lock();
            if self.closed.load(Ordering::Acquire) {
                drop(closer);
                t.closer.close();
                bail!("connection closed");
            }
            *closer = Some(t.closer.clone());
        }
        let greeting = t
            .writer
            .write_all(&handshake::hello(&self.client_name, announced)?)
            .and_then(|()| t.writer.flush())
            .map_err(anyhow::Error::from)
            .and_then(|()| handshake::read_greeting(&mut t.reader, self.endpoint.label(), announced));
        match greeting {
            Ok(Outcome::Ok(g)) => {
                let _ = self.out.send(WriterCmd::Replace(t.writer));
                self.version.store(g.version, Ordering::Relaxed);
                self.daemon_pid.store(g.pid, Ordering::Relaxed);
                self.flow_window.store(self.opts.window, Ordering::Relaxed);
                self.rtt_us.store(0, Ordering::Relaxed);
                self.last_rx_us.store(self.now_us(), Ordering::Relaxed);
                self.connected.store(true, Ordering::Release);
                // Snapshots and history are mostly repetitive text: worth
                // compressing on a remote link, not on the local socket.
                if g.version >= HISTORY_SINCE && !self.endpoint.is_local() {
                    self.send(&ClientMsg::Compress { zstd: true });
                }
                Ok(Some((t.reader, g.host, g.pid)))
            }
            Ok(Outcome::RetryWith(v)) => {
                log::info!("goattyd via {} only speaks protocol v{v}; reconnecting with that", self.endpoint.label());
                self.close_transport();
                self.announce.store(v, Ordering::Relaxed);
                Ok(None)
            }
            Err(e) => {
                self.close_transport();
                let lines = stderr.wait(STDERR_GRACE);
                Err(match &self.endpoint {
                    Endpoint::Ssh(target) => remote::diagnose(&target.host, e, &lines),
                    Endpoint::Local => crate::local::diagnose(e),
                    _ => remote::with_stderr(e, &lines),
                })
            }
        }
    }
}

/// One link to one goattyd. Cheap to share (`Arc`); dropping the last handle
/// closes the link (sessions keep running on the host).
pub struct Connection {
    shared: Arc<Shared>,
}

impl Connection {
    pub fn open(endpoint: Endpoint, client_name: &str) -> Result<Arc<Self>> {
        Self::open_inner(endpoint, client_name, true, None)
    }

    /// One-shot link that does not automatically reconnect if it drops, and
    /// optionally bounds the initial `Hello` handshake with `handshake_timeout`.
    pub fn open_once(endpoint: Endpoint, client_name: &str, handshake_timeout: Option<Duration>) -> Result<Arc<Self>> {
        Self::open_inner(endpoint, client_name, false, handshake_timeout)
    }

    fn open_inner(endpoint: Endpoint, client_name: &str, auto_reconnect: bool, handshake_timeout: Option<Duration>) -> Result<Arc<Self>> {
        let opts = endpoint.default_opts();
        let (out, out_rx) = flume::unbounded();
        let (reconnect_tx, reconnect_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            endpoint,
            opts,
            flow_window: AtomicU32::new(opts.window),
            client_name: client_name.into(),
            auto_reconnect,
            out,
            panes: Mutex::new(HashMap::new()),
            attaching: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            left: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            early_size: Mutex::new(HashMap::new()),
            next_req: AtomicU32::new(1),
            host: Mutex::new(HostInfo::default()),
            announce: AtomicU32::new(PROTOCOL_VERSION),
            version: AtomicU32::new(0),
            daemon_pid: AtomicU32::new(0),
            link_gen: AtomicU64::new(1),
            connected: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closer: Mutex::new(None),
            epoch: Instant::now(),
            last_rx_us: AtomicU64::new(0),
            rtt_us: AtomicU64::new(0),
            reconnect_tx,
            reconnect_rx,
        });

        std::thread::Builder::new().name("goatty-writer".into()).spawn(move || writer_loop(out_rx))?;

        let (reader, host, pid) = if let Some(limit) = handshake_timeout {
            let (tx, rx) = flume::bounded(1);
            let shared_bg = shared.clone();
            std::thread::Builder::new().name("goatty-handshake".into()).spawn(move || {
                let _ = tx.send(shared_bg.connect(true));
            })?;
            match rx.recv_timeout(limit) {
                Ok(res) => res?,
                Err(_) => {
                    shared.closed.store(true, Ordering::Release);
                    let cleanup = shared.clone();
                    if std::thread::Builder::new().name("goatty-handshake-cleanup".into()).spawn(move || cleanup.close_transport()).is_err()
                    {
                        shared.close_transport();
                    }
                    bail!("timed out waiting for goattyd greeting via {}", shared.endpoint.label());
                }
            }
        } else {
            shared.connect(true)?
        };
        log::info!(
            "connected to goattyd {} on {}@{} (pid {pid}, via {}, protocol v{})",
            host.daemon_version,
            host.user,
            host.hostname,
            shared.endpoint.label(),
            shared.version.load(Ordering::Relaxed)
        );
        *shared.host.lock() = host;

        let s = shared.clone();
        std::thread::Builder::new().name("goatty-reader".into()).spawn(move || reader_main(s, reader))?;
        if auto_reconnect && !shared.endpoint.is_local() {
            // Measure RTT right away (for adaptive flow control, echo
            // prediction and the host switcher) instead of waiting 5 s.
            shared.send(&ClientMsg::Ping { nonce: shared.now_us() });
            let weak = Arc::downgrade(&shared);
            std::thread::Builder::new().name("goatty-heartbeat".into()).spawn(move || heartbeat(weak))?;
        }
        Ok(Arc::new(Self { shared }))
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.shared.endpoint
    }

    pub fn host(&self) -> HostInfo {
        self.shared.host.lock().clone()
    }

    /// The protocol version agreed with the daemon on the current link.
    pub fn protocol_version(&self) -> u32 {
        self.shared.version.load(Ordering::Relaxed)
    }

    /// The daemon's pid (on its host) on the current link.
    pub fn daemon_pid(&self) -> u32 {
        self.shared.daemon_pid.load(Ordering::Relaxed)
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// Last measured round-trip time (remote links only).
    pub fn rtt(&self) -> Option<Duration> {
        match self.shared.rtt_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(us)),
        }
    }

    /// Active flow-control window in bytes (starts at `opts.window` and adapts
    /// on remote v15+ links).
    pub fn flow_window(&self) -> u32 {
        self.shared.flow_window.load(Ordering::Relaxed)
    }

    fn request(
        &self,
        pending: impl FnOnce(flume::Sender<ServerMsg>) -> Pending,
        msg: impl FnOnce(ReqId) -> ClientMsg,
    ) -> Result<ServerMsg> {
        self.request_within(REQUEST_TIMEOUT, pending, msg)
    }

    fn request_within(
        &self,
        timeout: Duration,
        pending: impl FnOnce(flume::Sender<ServerMsg>) -> Pending,
        msg: impl FnOnce(ReqId) -> ClientMsg,
    ) -> Result<ServerMsg> {
        self.request_with_gen(timeout, pending, msg).map(|(_, m)| m)
    }

    fn request_with_gen(
        &self,
        timeout: Duration,
        pending: impl FnOnce(flume::Sender<ServerMsg>) -> Pending,
        msg: impl FnOnce(ReqId) -> ClientMsg,
    ) -> Result<(u64, ServerMsg)> {
        let req = self.shared.req_id();
        let (tx, rx) = flume::bounded(1);
        let link_gen = {
            // Checked under the lock the disconnect drain takes: a request
            // registered here is either drained (waking `rx`) or goes out on a
            // live link with this exact `link_gen`.
            let mut map = self.shared.pending.lock();
            if !self.is_connected() {
                bail!("not connected to goattyd");
            }
            let link_gen = self.shared.link_gen.load(Ordering::Acquire);
            map.insert(req, pending(tx));
            link_gen
        };
        self.shared.send(&msg(req));
        let reply = rx.recv_timeout(timeout);
        self.shared.pending.lock().remove(&req);
        match reply {
            Ok(ServerMsg::Error { message, .. }) => Err(anyhow!("goattyd: {message}")),
            Ok(m) => Ok((link_gen, m)),
            Err(_) => Err(ReplyLost.into()),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        match self.request(Pending::Reply, |req| ClientMsg::ListSessions { req })? {
            ServerMsg::Sessions { list, .. } => Ok(list),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Spawn a shell on the host and attach `sink` to it. `cwd` is a path on
    /// the host; pass `None` for remote endpoints.
    pub fn create(&self, size: WinSize, cwd: Option<String>, sink: Arc<dyn PaneSink>) -> Result<SessionId> {
        ensure_sane(size)?;
        let opts = self.shared.opts;
        match self.request(
            |tx| Pending::Attach { tx, sink, size, session: None, state: None },
            |req| ClientMsg::Create { req, size, cwd, opts },
        )? {
            ServerMsg::Attached { session, .. } => Ok(session),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Start `argv` in a new session on the host (not the user's shell) and
    /// attach `sink` to it. Needs a daemon with protocol v6.
    pub fn run(&self, size: WinSize, cwd: Option<String>, argv: Vec<String>, sink: Arc<dyn PaneSink>) -> Result<SessionId> {
        ensure_sane(size)?;
        let version = self.protocol_version();
        if version < RUN_SINCE {
            bail!("goattyd via {} is too old to run commands in sessions (protocol v{version}); restart it", self.endpoint().label());
        }
        let opts = self.shared.opts;
        match self.request(
            |tx| Pending::Attach { tx, sink, size, session: None, state: None },
            |req| ClientMsg::Run { req, size, cwd, argv, opts },
        )? {
            ServerMsg::Attached { session, .. } => Ok(session),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    pub fn attach(&self, session: SessionId, size: WinSize, sink: Arc<dyn PaneSink>) -> Result<()> {
        // Refused here, not by goattyd: a refused replacement attach would
        // leave the pane that stays with output decoded at the old size.
        self.attach_many(vec![AttachRequest { session, size, sink }]).pop().expect("one attach request yields one result")
    }

    /// Attach to every session in `requests` in one pipelined burst: all
    /// `Attach` messages go out before waiting for any `Attached` reply, so
    /// restoring a window with N panes costs one round trip instead of N over
    /// any remote transport (SSH, Tailnet, or `--via`).
    ///
    /// Returns one result per request, in input order. An invalid size,
    /// duplicate session in the same batch, or missing session fails only its
    /// own entry; a dropped link or idle timeout fails every entry whose reply
    /// has not arrived yet with [`ReplyLost`].
    pub fn attach_many(&self, requests: Vec<AttachRequest>) -> Vec<Result<()>> {
        self.attach_many_within(requests, REQUEST_TIMEOUT)
    }

    /// Like [`Self::attach_many`], failing remaining entries if no reply
    /// arrives for `timeout`.
    pub fn attach_many_within(&self, requests: Vec<AttachRequest>, timeout: Duration) -> Vec<Result<()>> {
        if requests.is_empty() {
            return Vec::new();
        }
        let n = requests.len();
        let mut results: Vec<Option<Result<()>>> = (0..n).map(|_| None).collect();
        let mut seen = HashSet::with_capacity(n);
        let mut to_send: Vec<(usize, AttachRequest)> = Vec::with_capacity(n);

        for (i, req) in requests.into_iter().enumerate() {
            // Refused here, not by goattyd: a refused replacement attach would
            // leave the pane that stays with output decoded at the old size.
            if let Err(e) = ensure_sane(req.size) {
                results[i] = Some(Err(e));
                continue;
            }
            if !seen.insert(req.session) {
                results[i] = Some(Err(anyhow!("duplicate attach for session {}", req.session)));
                continue;
            }
            to_send.push((i, req));
        }

        if to_send.is_empty() {
            return results.into_iter().map(|r| r.expect("every attach entry resolved")).collect();
        }

        let opts = self.shared.opts;
        let (tx, rx) = flume::bounded(to_send.len());
        let mut in_flight: HashMap<ReqId, (usize, SessionId, Arc<AtomicU8>)> = HashMap::with_capacity(to_send.len());
        let mut msgs: Vec<ClientMsg> = Vec::with_capacity(to_send.len());

        let link_gen = {
            // Checked under the lock the disconnect drain takes: every request
            // registered here is either drained (closing `rx`) or sent on a
            // live link with this exact `link_gen`.
            let mut map = self.shared.pending.lock();
            if !self.is_connected() {
                for (i, _) in to_send {
                    results[i] = Some(Err(ReplyLost.into()));
                }
                return results.into_iter().map(|r| r.expect("every attach entry resolved")).collect();
            }
            let link_gen = self.shared.link_gen.load(Ordering::Acquire);
            let mut attaching = self.shared.attaching.lock();
            for (i, r) in to_send {
                let req = self.shared.req_id();
                let state = Arc::new(AtomicU8::new(ATTACH_ACTIVE));
                map.insert(
                    req,
                    Pending::Attach {
                        tx: tx.clone(),
                        sink: r.sink,
                        size: r.size,
                        session: Some((link_gen, r.session)),
                        state: Some(state.clone()),
                    },
                );
                in_flight.insert(req, (i, r.session, state));
                *attaching.entry((link_gen, r.session)).or_insert(0) += 1;
                msgs.push(ClientMsg::Attach { req, session: r.session, size: r.size, opts });
            }
            // Queue every frame back-to-back while holding `attaching` so they
            // batch together and serialize against `detach_sink`.
            for msg in &msgs {
                self.shared.send(msg);
            }
            link_gen
        };
        // Drop the local sender so `rx` closes as soon as every pending entry
        // is answered or drained on disconnect.
        drop(tx);

        let apply_reply =
            |msg: ServerMsg, in_flight: &mut HashMap<ReqId, (usize, SessionId, Arc<AtomicU8>)>, results: &mut [Option<Result<()>>]| {
                let Some(req) = msg.reply_to() else {
                    log::warn!("attach_many received message without req id: {msg:?}");
                    return;
                };
                let Some((i, _, _)) = in_flight.remove(&req) else {
                    return;
                };
                results[i] = Some(match msg {
                    ServerMsg::Attached { .. } => Ok(()),
                    ServerMsg::Error { message, .. } => Err(anyhow!("goattyd: {message}")),
                    other => Err(anyhow!("unexpected reply {other:?}")),
                });
            };

        while !in_flight.is_empty() {
            match rx.recv_timeout(timeout) {
                Ok(msg) => apply_reply(msg, &mut in_flight, &mut results),
                Err(_) => break,
            }
        }

        if !in_flight.is_empty() {
            while let Ok(msg) = rx.try_recv() {
                apply_reply(msg, &mut in_flight, &mut results);
            }
        }

        if !in_flight.is_empty() {
            let mut timed_out_sessions = Vec::new();
            {
                let mut map = self.shared.pending.lock();
                in_flight.retain(|&req, &mut (i, session, ref state)| {
                    let removed = map.remove(&req).is_some();
                    let cancelled = state.compare_exchange(ATTACH_ACTIVE, ATTACH_CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok();
                    if removed || cancelled {
                        // Cancelled before `dispatch` committed to installing
                        // the pane (or already drained on disconnect).
                        results[i] = Some(Err(ReplyLost.into()));
                        if cancelled {
                            timed_out_sessions.push(session);
                        }
                        false
                    } else {
                        // `dispatch` already transitioned to `ATTACH_FINISHING`
                        // and is installing the pane / sending the reply.
                        true
                    }
                });
            }
            // Wait for any reply that `dispatch` already committed to
            // installing before we locked `pending`, so an installed pane is
            // never reported as failed (and leaked).
            while !in_flight.is_empty() {
                match rx.recv() {
                    Ok(msg) => apply_reply(msg, &mut in_flight, &mut results),
                    Err(_) => break,
                }
            }
            for (_, (i, session, state)) in in_flight {
                results[i] = Some(Err(ReplyLost.into()));
                if state.compare_exchange(ATTACH_ACTIVE, ATTACH_CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    timed_out_sessions.push(session);
                }
            }
            if !timed_out_sessions.is_empty() {
                let panes = self.shared.panes.lock();
                let mut attaching = self.shared.attaching.lock();
                let live_link = self.shared.link_gen.load(Ordering::Acquire) == link_gen && self.is_connected();
                let mut early = self.shared.early_size.lock();
                for id in timed_out_sessions {
                    // Hold `attaching` and `early_size` through the `Detach`
                    // enqueue so a replacement cannot register or stash its
                    // `SizeControl` in between.
                    if finish_attaching(&mut attaching, link_gen, id) && live_link && !panes.contains_key(&id) {
                        early.remove(&id);
                        self.shared.send(&ClientMsg::Detach { session: id });
                    }
                }
            }
        }

        results.into_iter().map(|r| r.expect("every attach entry resolved")).collect()
    }

    /// Ask for up to `count` rows of `session`'s history before host line
    /// `before` (what `goatty_term::Terminal::wanted_history` says). Never
    /// blocks: the answer goes to the pane's [`PaneSink::history`]. False
    /// when it wasn't sent (no link, or a daemon older than protocol v11).
    pub fn fetch_history(&self, session: SessionId, before: u64, count: u32) -> bool {
        let req = self.shared.req_id();
        {
            // Checked under the lock the disconnect drain takes: a request
            // registered here is either drained (and answered with `None`) or
            // goes out on a live link.
            let mut pending = self.shared.pending.lock();
            if !self.is_connected() || self.protocol_version() < HISTORY_SINCE {
                return false;
            }
            pending.insert(req, Pending::History(session));
        }
        self.shared.send(&ClientMsg::FetchHistory { req, session, before, count });
        true
    }

    pub fn detach(&self, session: SessionId) {
        self.shared.panes.lock().remove(&session);
        self.shared.early_size.lock().remove(&session);
        self.shared.send(&ClientMsg::Detach { session });
    }

    /// Unsubscribe `sink` from `session` only if `sink` still owns the active
    /// subscription for `session` on this connection. If another window on the
    /// same pooled connection reattached `session` (or has an `Attach` for it
    /// in flight), its sink and daemon subscription are left intact.
    pub fn detach_sink(&self, session: SessionId, sink: &Arc<dyn PaneSink>) -> bool {
        let mut panes = self.shared.panes.lock();
        let Some(current) = panes.get(&session) else {
            return false;
        };
        if !Arc::ptr_eq(&current.sink, sink) {
            return false;
        }
        panes.remove(&session);
        let attaching = self.shared.attaching.lock();
        let cur = self.shared.link_gen.load(Ordering::Acquire);
        if attaching.get(&(cur, session)).copied().unwrap_or(0) == 0 {
            self.shared.early_size.lock().remove(&session);
            self.shared.send(&ClientMsg::Detach { session });
        }
        true
    }

    pub fn kill(&self, session: SessionId) -> Result<()> {
        self.request(Pending::Reply, |req| ClientMsg::Kill { req, session }).map(drop)
    }

    /// Never blocks: queued for the writer thread. Input typed while the link
    /// is down is dropped (the UI shows the disconnected state).
    pub fn input(&self, session: SessionId, data: &[u8]) {
        if self.is_connected() {
            self.shared.send(&ClientMsg::Input { session, data: data.to_vec() });
        }
    }

    pub fn resize(&self, session: SessionId, size: WinSize) {
        if !size.is_sane() {
            // goattyd would refuse it, and a reattach with it would fail.
            log::warn!("session {session}: not sending a {}×{} size", size.cols, size.rows);
            return;
        }
        // Sent under the lock the reattach takes: either the reattach's
        // `Attach` already has this size, or the `Resize` follows it.
        let mut panes = self.shared.panes.lock();
        if let Some(p) = panes.get_mut(&session) {
            p.size = size;
        }
        self.shared.send(&ClientMsg::Resize { session, size });
    }

    /// Take control of `session`'s size from the client that has it (v14):
    /// the session takes the size this connection last sent. Never blocks.
    /// False when nothing was sent: the link is down, or the daemon is older.
    pub fn take_size(&self, session: SessionId) -> bool {
        // Checked and sent under the lock a dropped link takes to unlink the
        // panes, before a new link (maybe to an older daemon) gets the writer.
        let panes = self.shared.panes.lock();
        let linked = panes.get(&session).is_some_and(|p| p.linked);
        let send = linked && self.is_connected() && self.supports_size_control();
        if send {
            self.shared.send(&ClientMsg::TakeSize { session });
        }
        send
    }

    /// Whether the daemon on this link speaks protocol v14 (one controlling
    /// client per session sets its size; `take_size`).
    pub fn supports_size_control(&self) -> bool {
        self.protocol_version() >= SIZE_CONTROL_SINCE
    }

    /// Tell the daemon which colors this session is shown with, so color
    /// queries get matching answers. Remembered and re-sent on reconnect.
    pub fn set_palette(&self, session: SessionId, colors: Vec<u32>) {
        if let Some(p) = self.shared.panes.lock().get_mut(&session) {
            p.palette = Some(colors.clone());
        }
        if self.is_connected() {
            self.shared.send(&ClientMsg::SetPalette { session, colors });
        }
    }

    /// The host's settings document. Blocks: call off the UI thread.
    pub fn host_settings(&self) -> Result<HostSettingsReply> {
        match self.request(Pending::Reply, |req| ClientMsg::GetHostSettings { req })? {
            ServerMsg::HostSettings { json, issues, env_overrides, generation, .. } => {
                Ok(HostSettingsReply { json, issues, env_overrides, generation })
            }
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Validate `json` and preview the generated-file changes; with `apply`,
    /// also activate and save. Blocks: call off the UI thread.
    pub fn set_host_settings(&self, json: String, apply: bool) -> Result<HostSettingsApplied> {
        self.set_host_settings_within(json, apply, REQUEST_TIMEOUT)
    }

    /// Like [`Self::set_host_settings`], waiting up to `timeout` for the reply.
    pub fn set_host_settings_within(&self, json: String, apply: bool, timeout: Duration) -> Result<HostSettingsApplied> {
        match self.request_within(timeout, Pending::Reply, |req| ClientMsg::SetHostSettings { req, json, apply })? {
            ServerMsg::HostSettingsApplied { issues, diff, applied, generation, .. } => {
                Ok(HostSettingsApplied { issues, diff, applied, generation })
            }
            other => bail!("unexpected reply {other:?}"),
        }
    }

    pub fn generations(&self) -> Result<Vec<GenerationInfo>> {
        match self.request(Pending::Reply, |req| ClientMsg::ListGenerations { req })? {
            ServerMsg::Generations { list, .. } => Ok(list),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Roll the host back to `generation` (default: the previous one).
    pub fn rollback(&self, generation: Option<u32>) -> Result<HostSettingsReply> {
        match self.request(Pending::Reply, |req| ClientMsg::Rollback { req, generation })? {
            ServerMsg::HostSettings { json, issues, env_overrides, generation, .. } => {
                Ok(HostSettingsReply { json, issues, env_overrides, generation })
            }
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Ask the host's running managed shells to reload their configuration.
    /// Returns how many were asked. Blocks: call off the UI thread.
    pub fn reload_shells(&self) -> Result<u32> {
        match self.request(Pending::Reply, |req| ClientMsg::ReloadShells { req })? {
            ServerMsg::ShellsReloaded { count, .. } => Ok(count),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// A plugin operation (`goatty_plugin::api::Request` as JSON); returns
    /// the reply JSON. Installs download, so this waits up to
    /// [`PLUGIN_TIMEOUT`]. Blocks: call off the UI thread.
    pub fn plugin(&self, request: String) -> Result<String> {
        match self.request_within(PLUGIN_TIMEOUT, Pending::Reply, |req| ClientMsg::Plugin { req, request })? {
            ServerMsg::Plugin { reply, .. } => Ok(reply),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// A shell setup or diagnostics operation (`goatty_shell::setup::Request`
    /// as JSON); returns the reply JSON. Installs download, so this waits up
    /// to [`PLUGIN_TIMEOUT`]. Needs a daemon with protocol v9. Blocks: call
    /// off the UI thread.
    pub fn shell(&self, request: String) -> Result<String> {
        let version = self.protocol_version();
        if version < SHELL_SINCE {
            bail!(
                "goattyd via {} is too old for shell setup and diagnostics (protocol v{version}); restart it to use the new version",
                self.endpoint().label()
            );
        }
        match self.request_within(PLUGIN_TIMEOUT, Pending::Reply, |req| ClientMsg::Shell { req, request })? {
            ServerMsg::Shell { reply, .. } => Ok(reply),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// A tailnet operation on this host (`goatty_net::api::Request` as
    /// JSON); returns the reply JSON. Changing availability runs
    /// `tailscale`. Needs a daemon with protocol v12. Blocks: call off the
    /// UI thread.
    pub fn net(&self, request: String) -> Result<String> {
        let version = self.protocol_version();
        if version < NET_SINCE {
            bail!(
                "goattyd via {} is too old for the tailnet (protocol v{version}); restart it to use the new version",
                self.endpoint().label()
            );
        }
        match self.request_within(REQUEST_TIMEOUT * 4, Pending::Reply, |req| ClientMsg::Net { req, request })? {
            ServerMsg::Net { reply, .. } => Ok(reply),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// This connection's identity, as [`WorkspaceSink`]s hear it.
    pub fn key(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// The host's workspaces, in the order they were made. From then on,
    /// this connection hears when they change (`WorkspaceEvent::List`, to
    /// the windows that show one).
    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        self.list_workspaces_seq().map(|(_, list)| list)
    }

    /// [`Self::list_workspaces`] with the list's version (`WorkspaceEvent::List`).
    pub fn list_workspaces_seq(&self) -> Result<(u64, Vec<WorkspaceInfo>)> {
        match self.request(Pending::Reply, |req| ClientMsg::ListWorkspaces { req })? {
            ServerMsg::Workspaces { seq, list, .. } => Ok((seq, list)),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Window `view` shows workspace `id` on this host from now on (`None`
    /// or gone: the most recently active one); `sink` hears about it, and
    /// the window joins it again after a reconnect. Blocks.
    pub fn join_workspace(
        &self,
        view: ViewId,
        id: Option<WorkspaceId>,
        sink: Arc<dyn WorkspaceSink>,
    ) -> Result<(WorkspaceInfo, WorkspaceLayout)> {
        let left_gen = self.shared.left.lock().get(&view).copied().unwrap_or(0);
        match self.request(|tx| Pending::Join { tx, view, sink, left_gen }, |req| ClientMsg::JoinWorkspace { req, view, id })? {
            ServerMsg::Joined { info, layout, .. } => Ok((info, layout)),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// A new workspace (`name`, else "Workspace N"), which window `view`
    /// shows from now on (as [`Self::join_workspace`]). Blocks.
    pub fn create_workspace(
        &self,
        view: ViewId,
        name: Option<String>,
        sink: Arc<dyn WorkspaceSink>,
    ) -> Result<(WorkspaceInfo, WorkspaceLayout)> {
        let left_gen = self.shared.left.lock().get(&view).copied().unwrap_or(0);
        match self.request(|tx| Pending::Join { tx, view, sink, left_gen }, |req| ClientMsg::CreateWorkspace { req, view, name })? {
            ServerMsg::Joined { info, layout, .. } => Ok((info, layout)),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Window `view` stops showing a workspace on this host (it closed or
    /// detached from the host). Doesn't block.
    pub fn leave_workspace(&self, view: ViewId) {
        // A join on its way when the window leaves doesn't register it.
        *self.shared.left.lock().entry(view).or_insert(0) += 1;
        self.shared.views.lock().remove(&view);
        if self.is_connected() {
            self.shared.send(&ClientMsg::LeaveWorkspace { view });
        }
    }

    /// The workspace window `view` shows here, if any.
    pub fn shown_workspace(&self, view: ViewId) -> Option<WorkspaceId> {
        self.shared.views.lock().get(&view).map(|v| v.workspace)
    }

    /// Save workspace `id`'s layout as window `view` edited it from
    /// revision `base` (see `ClientMsg::SaveWorkspace`). Blocks.
    pub fn save_workspace(
        &self,
        view: ViewId,
        id: WorkspaceId,
        base: u64,
        tabs: u32,
        sessions: Vec<SessionId>,
        json: String,
    ) -> Result<SaveOutcome> {
        match self.request(Pending::Reply, |req| ClientMsg::SaveWorkspace { req, view, id, base, tabs, sessions, json })? {
            ServerMsg::WorkspaceSaved { outcome, .. } => Ok(outcome),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Workspace `id`'s layout without showing it. Blocks.
    pub fn get_workspace(&self, id: WorkspaceId) -> Result<Option<WorkspaceLayout>> {
        match self.request(Pending::Reply, |req| ClientMsg::GetWorkspace { req, id })? {
            ServerMsg::WorkspaceLayout { layout, .. } => Ok(layout),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    pub fn rename_workspace(&self, id: WorkspaceId, name: String) -> Result<()> {
        self.request(Pending::Reply, |req| ClientMsg::RenameWorkspace { req, id, name }).map(drop)
    }

    /// Forget workspace `id` on the host; its sessions keep running (move
    /// or end them first). Blocks.
    pub fn close_workspace(&self, id: WorkspaceId) -> Result<()> {
        self.request(Pending::Reply, |req| ClientMsg::CloseWorkspace { req, id }).map(drop)
    }

    /// Whether a pane on this connection shows `session`.
    pub fn shows(&self, session: SessionId) -> bool {
        self.shared.panes.lock().contains_key(&session)
    }

    /// Reconnect now, with prompts (password, passphrase) if ssh needs them.
    /// Automatic retries never prompt; this is the explicit way to.
    pub fn reconnect(&self) {
        let _ = self.shared.reconnect_tx.try_send(());
    }

    /// Drop the current link as if the network failed (debugging/tests).
    pub fn force_disconnect(&self) {
        self.shared.close_transport();
    }

    /// Connectable SSH host destination for this endpoint (`SshTarget::host`, or
    /// the Tailnet MagicDNS name without a trailing dot), or `None` for local
    /// and custom `--via` command endpoints.
    pub fn ssh_host(&self) -> Option<String> {
        match self.endpoint() {
            Endpoint::Ssh(t) => Some(t.host.clone()),
            Endpoint::Tailnet(t) => {
                let dns = t.dns.trim_end_matches('.');
                if dns.is_empty() { Some(t.name.clone()) } else { Some(dns.to_string()) }
            }
            Endpoint::Local | Endpoint::Command { .. } => None,
        }
    }

    /// Whether the daemon on this link speaks protocol v13 (`Cwd`, `StatPath`,
    /// `ReadFile`).
    pub fn supports_remote_files(&self) -> bool {
        self.protocol_version() >= REMOTE_FILES_SINCE
    }

    /// Resolve and stat the first existing regular file or directory among
    /// `candidates` on the daemon's host. When `cwd` is `Some`, relative
    /// candidates resolve against that directory rather than the session's
    /// latest working directory. Blocks: call off the UI thread.
    pub fn stat_path(&self, session: Option<SessionId>, cwd: Option<String>, candidates: Vec<String>) -> Result<Option<RemotePathInfo>> {
        let version = self.protocol_version();
        if version < REMOTE_FILES_SINCE {
            bail!(
                "goattyd via {} is too old for remote file resolution (protocol v{version}); restart it to use the new version",
                self.endpoint().label()
            );
        }
        match self.request(Pending::Reply, |req| ClientMsg::StatPath { req, session, cwd, candidates })? {
            ServerMsg::PathStat { info, .. } => Ok(info),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Fetch up to `max_bytes` from regular file `path` on the daemon's host.
    /// Blocks: call off the UI thread.
    pub fn read_file(&self, path: &str, max_bytes: u64) -> Result<(String, Vec<u8>)> {
        let version = self.protocol_version();
        if version < REMOTE_FILES_SINCE {
            bail!(
                "goattyd via {} is too old to read remote files (protocol v{version}); restart it to use the new version",
                self.endpoint().label()
            );
        }
        let timeout = Duration::from_secs(60 + (max_bytes / (256 * 1024)));
        match self.request_within(timeout, Pending::Reply, |req| ClientMsg::ReadFile { req, path: path.to_string(), max_bytes })? {
            ServerMsg::FileData { path, data, .. } => Ok((path, data)),
            other => bail!("unexpected reply {other:?}"),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        let (done_tx, done_rx) = flume::bounded(1);
        if self.shared.out.send(WriterCmd::Close(done_tx)).is_ok() {
            let _ = done_rx.recv_timeout(WRITER_CLOSE_TIMEOUT);
        }
        self.shared.close_transport();
    }
}

/// goattyd refuses sizes no screen has (`WinSize::is_sane`).
fn ensure_sane(size: WinSize) -> Result<()> {
    if !size.is_sane() {
        bail!("the pane is too large ({}×{} cells)", size.cols, size.rows);
    }
    Ok(())
}

/// Decrement the in-flight attach count for `(link_gen, session)` while the
/// caller holds `attaching`; returns `true` when no in-flight `Attach` remains
/// for `(link_gen, session)`.
fn finish_attaching(attaching: &mut HashMap<(u64, SessionId), usize>, link_gen: u64, session: SessionId) -> bool {
    let key = (link_gen, session);
    let Some(count) = attaching.get_mut(&key) else {
        return true;
    };
    *count = count.saturating_sub(1);
    if *count == 0 {
        attaching.remove(&key);
        true
    } else {
        false
    }
}

fn writer_loop(rx: flume::Receiver<WriterCmd>) {
    let mut w: Option<BufWriter<Box<dyn Write + Send>>> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            WriterCmd::Replace(nw) => w = Some(BufWriter::with_capacity(64 << 10, nw)),
            WriterCmd::Frame(f) => {
                if let Some(ww) = &mut w
                    && ww.write_all(&f).is_err()
                {
                    w = None; // the reader notices too and reconnects
                }
            }
            WriterCmd::Close(done) => {
                if let Some(ww) = &mut w {
                    let _ = ww.flush();
                }
                let _ = done.send(());
                break;
            }
        }
        // Batch under load, flush as soon as the queue is idle (keystrokes
        // go out immediately).
        if rx.is_empty()
            && let Some(ww) = &mut w
            && ww.flush().is_err()
        {
            w = None;
        }
    }
}

fn heartbeat(weak: Weak<Shared>) {
    loop {
        std::thread::sleep(HEARTBEAT);
        let Some(s) = weak.upgrade() else { return };
        if s.closed.load(Ordering::Acquire) {
            return;
        }
        if !s.connected.load(Ordering::Acquire) {
            continue;
        }
        let silent = Duration::from_micros(s.now_us().saturating_sub(s.last_rx_us.load(Ordering::Relaxed)));
        if silent > DEAD_AFTER {
            log::warn!("no data from goattyd for {silent:?}; dropping link");
            s.close_transport();
            continue;
        }
        s.send(&ClientMsg::Ping { nonce: s.now_us() });
    }
}

fn reader_main(s: Arc<Shared>, mut reader: Reader) {
    let mut backoff = [200u64, 500, 1000, 2000, 5000].iter().copied().chain(std::iter::repeat(5000));
    loop {
        let err = read_loop(&s, &mut reader);
        s.connected.store(false, Ordering::Release);
        s.link_gen.fetch_add(1, Ordering::AcqRel);
        s.rtt_us.store(0, Ordering::Relaxed);
        s.close_transport();
        if s.closed.load(Ordering::Acquire) {
            return;
        }
        log::warn!("link to goattyd lost: {err}");
        // Fail in-flight requests; keep panes for reattach.
        let failed: Vec<Pending> = s.pending.lock().drain().map(|(_, p)| p).collect();
        // Geometry from the old link is released on disconnect.
        s.attaching.lock().clear();
        s.early_size.lock().clear();
        for p in failed {
            if let Pending::History(session) = p
                && let Some(sink) = s.panes.lock().get(&session).map(|p| p.sink.clone())
            {
                sink.history(None);
            }
        }
        let sinks: Vec<_> = s
            .panes
            .lock()
            .values_mut()
            .map(|p| {
                p.linked = false;
                p.sink.clone()
            })
            .collect();
        for sink in &sinks {
            sink.event(PaneEvent::Disconnected(err.to_string()));
        }
        if !s.auto_reconnect {
            return;
        }
        let mut needs_user = false;
        loop {
            if s.closed.load(Ordering::Acquire) {
                return;
            }
            // Waiting for the backoff; an explicit request cuts it short and
            // allows prompts. After an auth failure, wait for one instead of
            // retrying (every retry would fail the same way).
            let interactive = if needs_user {
                loop {
                    if s.closed.load(Ordering::Acquire) {
                        return;
                    }
                    if s.reconnect_rx.recv_timeout(Duration::from_secs(1)).is_ok() {
                        break true;
                    }
                }
            } else {
                s.reconnect_rx.recv_timeout(Duration::from_millis(backoff.next().unwrap_or(5000))).is_ok()
            };
            if s.closed.load(Ordering::Acquire) {
                return;
            }
            match s.connect(interactive) {
                Ok((r, host, pid)) => {
                    log::info!("reconnected to goattyd (pid {pid})");
                    *s.host.lock() = host;
                    reader = r;
                    backoff = [200u64, 500, 1000, 2000, 5000].iter().copied().chain(std::iter::repeat(5000));
                    if !s.endpoint.is_local() {
                        s.send(&ClientMsg::Ping { nonce: s.now_us() });
                    }
                    // Under the panes lock, so a resize meanwhile is either in
                    // the `Attach` or sent after it (see `Connection::resize`).
                    let panes = s.panes.lock();
                    for (&session, p) in panes.iter() {
                        let req = s.req_id();
                        s.pending.lock().insert(req, Pending::Reattach(session));
                        s.send(&ClientMsg::Attach { req, session, size: p.size, opts: s.opts });
                    }
                    drop(panes);
                    // Every window joins its workspace again (the daemon
                    // forgot this connection's windows when the link dropped).
                    let views = s.views.lock();
                    for (&view, v) in views.iter() {
                        let req = s.req_id();
                        s.pending.lock().insert(req, Pending::Rejoin(view));
                        s.send(&ClientMsg::JoinWorkspace { req, view, id: Some(v.workspace) });
                    }
                    drop(views);
                    break;
                }
                Err(e) => {
                    s.close_transport();
                    log::info!("reconnect failed: {e:#}");
                    if !interactive && needs_user_input(&e) {
                        needs_user = true;
                        for sink in &sinks {
                            sink.event(PaneEvent::NeedsReconnect(format!("{e:#}")));
                        }
                    } else if interactive {
                        needs_user = false;
                    }
                }
            }
        }
    }
}

/// Did ssh fail where only the user can help (no key/agent worked, or the
/// host key is unknown or changed)? Or a tailnet host that refused this
/// device (revoked) or shows another key: retrying won't change that.
fn needs_user_input(e: &anyhow::Error) -> bool {
    use goatty_net::gateway::{GatewayError, Refusal};
    let text = format!("{e:#}");
    let gateway = e.chain().any(|c| {
        matches!(
            c.downcast_ref::<GatewayError>(),
            Some(GatewayError::HostKeyChanged { .. } | GatewayError::Refused { code: Refusal::NotPaired, .. })
        )
    });
    gateway || text.contains("Permission denied") || text.contains("Host key verification failed")
}

fn read_loop(s: &Shared, reader: &mut Reader) -> anyhow::Error {
    loop {
        let msg: ServerMsg = match read_msg(reader).and_then(ServerMsg::unpack) {
            Ok(m) => m,
            Err(e) => return e.into(),
        };
        s.last_rx_us.store(s.now_us(), Ordering::Relaxed);
        dispatch(s, msg);
        // Ack policy: always once the transport buffer is drained (so a
        // lagging subscriber is never left waiting on a partial ack), and in
        // between every quarter of the active window so the stream keeps flowing.
        let drained = reader.buffer().is_empty();
        let quarter = u64::from((s.flow_window.load(Ordering::Relaxed) / 4).max(StreamOpts::MIN_WINDOW / 4));
        let mut acks = Vec::new();
        for (id, p) in s.panes.lock().iter_mut() {
            if !p.linked {
                continue;
            }
            let unacked = p.consumed - p.acked;
            if unacked > 0 && (drained || unacked >= quarter) {
                p.acked = p.consumed;
                acks.push((*id, p.consumed));
            }
        }
        for (session, consumed) in acks {
            s.send(&ClientMsg::Ack { session, consumed });
        }
    }
}

/// The request `req`'s waiter, taken out of the map (not held across the
/// handling: it locks the views and panes too).
fn take_pending(s: &Shared, req: ReqId) -> Option<Pending> {
    s.pending.lock().remove(&req)
}

/// Feed `bytes` to the pane's sink outside the lock, then count them.
fn feed_pane(s: &Shared, session: SessionId, bytes: &[u8]) -> Option<Arc<dyn PaneSink>> {
    let sink = s.panes.lock().get(&session).map(|p| p.sink.clone())?;
    sink.feed(bytes);
    if let Some(p) = s.panes.lock().get_mut(&session) {
        p.consumed += bytes.len() as u64;
    }
    Some(sink)
}

fn dispatch(s: &Shared, msg: ServerMsg) {
    match msg {
        ServerMsg::Output { session, data } => {
            feed_pane(s, session, &data);
        }
        ServerMsg::Resync { session, skipped, snapshot } => {
            log::debug!("session {session}: resync after skipping {skipped} bytes ({} byte snapshot)", snapshot.len());
            if let Some(sink) = feed_pane(s, session, &snapshot) {
                sink.event(PaneEvent::Resynced { skipped });
            }
        }
        ServerMsg::Attached { req, session, snapshot } => {
            let pending = s.pending.lock().remove(&req);
            match pending {
                Some(Pending::Attach { tx, sink, size, session: target, state }) => {
                    if state.as_ref().is_some_and(|st| st.load(Ordering::Acquire) == ATTACH_CANCELLED) {
                        return;
                    }
                    if let Some(ev) = s.early_size.lock().remove(&session) {
                        sink.event(ev);
                    }
                    sink.feed(&snapshot);
                    let n = snapshot.len() as u64;
                    {
                        let mut panes = s.panes.lock();
                        let mut attaching = s.attaching.lock();
                        let committed = state.as_ref().is_none_or(|st| {
                            st.compare_exchange(ATTACH_ACTIVE, ATTACH_FINISHING, Ordering::AcqRel, Ordering::Acquire).is_ok()
                        });
                        if !committed {
                            return;
                        }
                        if let Some((link_gen, id)) = target {
                            finish_attaching(&mut attaching, link_gen, id);
                        }
                        panes.insert(session, Pane { sink, size, consumed: n, acked: 0, palette: None, linked: true });
                    }
                    let _ = tx.send(ServerMsg::Attached { req, session, snapshot: Vec::new() });
                }
                Some(Pending::Reattach(_)) => {
                    let pane = s.panes.lock().get_mut(&session).map(|p| {
                        // New subscription: stream offsets restart at zero.
                        p.consumed = snapshot.len() as u64;
                        p.acked = 0;
                        p.linked = true;
                        (p.sink.clone(), p.palette.clone())
                    });
                    if let Some((sink, palette)) = pane {
                        if let Some(colors) = palette {
                            s.send(&ClientMsg::SetPalette { session, colors });
                        }
                        sink.feed(&snapshot);
                        sink.event(PaneEvent::Reconnected);
                    }
                }
                _ => log::warn!("unsolicited Attached for session {session}"),
            }
        }
        ServerMsg::Exited { session, code } => {
            if let Some(p) = s.panes.lock().remove(&session) {
                p.sink.event(PaneEvent::Exited(code));
            }
        }
        ServerMsg::Process { session, name } => {
            let sink = s.panes.lock().get(&session).map(|p| p.sink.clone());
            if let Some(sink) = sink {
                sink.event(PaneEvent::Process(name));
            }
        }
        ServerMsg::Cwd { session, cwd } => {
            let sink = s.panes.lock().get(&session).map(|p| p.sink.clone());
            if let Some(sink) = sink {
                sink.event(PaneEvent::Cwd(cwd));
            }
        }
        ServerMsg::SizeControl { session, size, controlling } => {
            if !size.is_sane() {
                log::warn!("session {session}: ignoring a {}×{} size from the daemon", size.cols, size.rows);
                return;
            }
            let ev = PaneEvent::SizeControl { cols: size.cols, rows: size.rows, controlling };
            // Before an `Attached` we're waiting for, it's for the sink that
            // attach installs, not a pane the session may still have here.
            let attaching =
                s.pending.lock().values().any(|p| matches!(p, Pending::Attach { session: Some((_, id)), .. } if *id == session));
            let sink = if attaching { None } else { s.panes.lock().get(&session).map(|p| p.sink.clone()) };
            match sink {
                Some(sink) => sink.event(ev),
                // Comes before the `Attached` that adds the pane.
                // Or after we detached: keep such leftovers bounded.
                None => {
                    let overflow = {
                        let mut early = s.early_size.lock();
                        early.insert(session, ev);
                        early.len() > 64
                    };
                    if overflow {
                        let pending_sessions: HashSet<SessionId> = s
                            .pending
                            .lock()
                            .values()
                            .filter_map(|p| match p {
                                Pending::Attach { session: Some((_, id)), .. } => Some(*id),
                                _ => None,
                            })
                            .collect();
                        s.early_size.lock().retain(|id, _| *id == session || pending_sessions.contains(id));
                    }
                }
            }
        }
        ServerMsg::FlowWindow { window } => {
            if !s.endpoint.is_local() {
                let clamped = window.clamp(StreamOpts::MIN_WINDOW, StreamOpts::MAX_WINDOW);
                s.flow_window.store(clamped, Ordering::Relaxed);
            }
        }
        ServerMsg::TopLine { session, line, cause } => {
            let sink = s.panes.lock().get(&session).map(|p| p.sink.clone());
            if let Some(sink) = sink {
                sink.top_line(line, cause);
            }
        }
        ServerMsg::History { req, session, before, first, oldest, data } => {
            let pending = s.pending.lock().remove(&req);
            let sink = s.panes.lock().get(&session).map(|p| p.sink.clone());
            if let (Some(Pending::History(_)), Some(sink)) = (pending, sink) {
                sink.history(Some(History { before, first, oldest, data }));
            }
        }
        ServerMsg::Zstd { .. } => log::warn!("nested compressed frame"),
        ServerMsg::Pong { nonce } => {
            let now = s.now_us();
            if nonce <= now {
                let rtt_us = now.saturating_sub(nonce).max(1);
                s.rtt_us.store(rtt_us, Ordering::Relaxed);
                if s.version.load(Ordering::Relaxed) >= FLOW_CONTROL_SINCE && !s.endpoint.is_local() {
                    s.send(&ClientMsg::Rtt { rtt_us });
                }
            }
        }
        ServerMsg::Sessions { req, .. }
        | ServerMsg::Ok { req }
        | ServerMsg::HostSettings { req, .. }
        | ServerMsg::HostSettingsApplied { req, .. }
        | ServerMsg::Generations { req, .. }
        | ServerMsg::ShellsReloaded { req, .. }
        | ServerMsg::Plugin { req, .. }
        | ServerMsg::Workspaces { req, .. }
        | ServerMsg::Joined { req, .. }
        | ServerMsg::WorkspaceSaved { req, .. }
        | ServerMsg::WorkspaceLayout { req, .. }
        | ServerMsg::Shell { req, .. }
        | ServerMsg::Net { req, .. }
        | ServerMsg::PathStat { req, .. }
        | ServerMsg::FileData { req, .. } => match take_pending(s, req) {
            Some(Pending::Reply(tx)) => {
                let _ = tx.send(msg);
            }
            Some(Pending::Join { tx, view, sink, left_gen }) => {
                if let ServerMsg::Joined { info, .. } = &msg {
                    if s.left.lock().get(&view).copied().unwrap_or(0) == left_gen {
                        s.views.lock().insert(view, View { workspace: info.id, sink });
                    } else {
                        // The window left while it was joining.
                        s.send(&ClientMsg::LeaveWorkspace { view });
                    }
                }
                let _ = tx.send(msg);
            }
            Some(Pending::Rejoin(view)) => {
                if let ServerMsg::Joined { info, layout, .. } = msg {
                    let sink = {
                        let mut views = s.views.lock();
                        views.get_mut(&view).map(|v| {
                            v.workspace = info.id;
                            v.sink.clone()
                        })
                    };
                    if let Some(sink) = sink {
                        sink.workspace(s as *const Shared as usize, WorkspaceEvent::Rejoined { info, layout });
                    }
                }
            }
            Some(Pending::Attach { tx, session, state, .. }) => {
                let committed = state
                    .as_ref()
                    .is_none_or(|st| st.compare_exchange(ATTACH_ACTIVE, ATTACH_FINISHING, Ordering::AcqRel, Ordering::Acquire).is_ok());
                if committed {
                    if let Some((link_gen, session)) = session {
                        let panes = s.panes.lock();
                        let mut attaching = s.attaching.lock();
                        let live_link = s.link_gen.load(Ordering::Acquire) == link_gen && s.connected.load(Ordering::Acquire);
                        if finish_attaching(&mut attaching, link_gen, session) && live_link && !panes.contains_key(&session) {
                            s.early_size.lock().remove(&session);
                            s.send(&ClientMsg::Detach { session });
                        }
                    }
                    let _ = tx.send(msg);
                }
            }
            _ => {}
        },
        ServerMsg::Error { req: Some(req), ref message } => {
            let pending = s.pending.lock().remove(&req);
            match pending {
                Some(Pending::Attach { tx, session, state, .. }) => {
                    let committed = state
                        .as_ref()
                        .is_none_or(|st| st.compare_exchange(ATTACH_ACTIVE, ATTACH_FINISHING, Ordering::AcqRel, Ordering::Acquire).is_ok());
                    // A refused replacement attach: a live size change held
                    // for it belongs to the pane that stays.
                    if let Some((link_gen, session)) = session {
                        let deliver = {
                            let panes = s.panes.lock();
                            let mut attaching = s.attaching.lock();
                            let live_link = s.link_gen.load(Ordering::Acquire) == link_gen && s.connected.load(Ordering::Acquire);
                            let still_attaching = if committed {
                                !finish_attaching(&mut attaching, link_gen, session)
                            } else {
                                attaching.get(&(link_gen, session)).copied().unwrap_or(0) > 0
                            };
                            if let Some(sink) = panes.get(&session).map(|p| p.sink.clone()) {
                                s.early_size.lock().remove(&session).map(|ev| (sink, ev))
                            } else {
                                if !still_attaching && live_link {
                                    s.early_size.lock().remove(&session);
                                    s.send(&ClientMsg::Detach { session });
                                }
                                None
                            }
                        };
                        if let Some((sink, ev)) = deliver {
                            sink.event(ev);
                        }
                    }
                    if committed {
                        let _ = tx.send(msg);
                    }
                }
                Some(Pending::Reply(tx) | Pending::Join { tx, .. }) => {
                    let _ = tx.send(msg);
                }
                Some(Pending::Rejoin(view)) => log::warn!("window {view:x} couldn't join its workspace again: {message}"),
                Some(Pending::Reattach(session)) => {
                    if let Some(p) = s.panes.lock().remove(&session) {
                        p.sink.event(PaneEvent::Lost(message.clone()));
                    }
                }
                Some(Pending::History(session)) => {
                    log::debug!("session {session}: no history: {message}");
                    let sink = s.panes.lock().get(&session).map(|p| p.sink.clone());
                    if let Some(sink) = sink {
                        sink.history(None);
                    }
                }
                None => {}
            }
        }
        ServerMsg::LayoutChanged { by, layout } => {
            let sinks: Vec<Arc<dyn WorkspaceSink>> =
                s.views.lock().iter().filter(|(view, v)| **view != by && v.workspace == layout.id).map(|(_, v)| v.sink.clone()).collect();
            for sink in sinks {
                sink.workspace(s as *const Shared as usize, WorkspaceEvent::Changed(layout.clone()));
            }
        }
        ServerMsg::WorkspacesChanged { seq, list } => {
            let mut sinks: Vec<Arc<dyn WorkspaceSink>> = Vec::new();
            for v in s.views.lock().values() {
                if !sinks.iter().any(|x| Arc::ptr_eq(x, &v.sink)) {
                    sinks.push(v.sink.clone());
                }
            }
            for sink in sinks {
                sink.workspace(s as *const Shared as usize, WorkspaceEvent::List { seq, list: list.clone() });
            }
        }
        ServerMsg::RetiredLayout | ServerMsg::RetiredLayouts => log::warn!("goattyd sent a retired layout message"),
        ServerMsg::Error { req: None, message } => log::error!("goattyd: {message}"),
        ServerMsg::Hello { .. } => log::warn!("unexpected second Hello"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_auth_failures_wait_for_the_user() {
        let denied = anyhow::anyhow!("no valid greeting").context("user@box: Permission denied (publickey,password).");
        assert!(needs_user_input(&denied));
        assert!(needs_user_input(&anyhow::anyhow!("Host key verification failed.")));
        assert!(!needs_user_input(&anyhow::anyhow!("ssh: connect to host box port 22: Network is unreachable")));
        use goatty_net::gateway::{GatewayError, Refusal};
        let revoked = anyhow::Error::new(GatewayError::Refused { code: Refusal::NotPaired, reason: "not paired".into() });
        assert!(needs_user_input(&revoked));
        assert!(needs_user_input(&anyhow::Error::new(GatewayError::HostKeyChanged { name: "box".into() }).context("connecting")));
        let busy = anyhow::Error::new(GatewayError::Refused { code: Refusal::TooMany, reason: "later".into() });
        assert!(!needs_user_input(&busy));
    }

    #[test]
    fn open_once_times_out_silent_handshake() {
        let ep = Endpoint::command_line("exec sleep 5").unwrap();
        let start = Instant::now();
        let err = match Connection::open_once(ep, "test", Some(Duration::from_millis(50))) {
            Ok(_) => panic!("silent endpoint should time out"),
            Err(e) => e,
        };
        assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());
        assert!(err.to_string().contains("timed out waiting for goattyd greeting"), "{err:#}");
    }

    #[test]
    fn open_once_times_out_when_descendant_keeps_pipe_open() {
        // `; true` prevents shells from exec-replacing sh, so a child keeps stdout open if only sh is killed.
        let ep = Endpoint::command_line("sleep 2; true").unwrap();
        let start = Instant::now();
        let err = match Connection::open_once(ep, "test", Some(Duration::from_millis(50))) {
            Ok(_) => panic!("descendant keeping pipe open should still time out"),
            Err(e) => e,
        };
        assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
        assert!(err.to_string().contains("timed out waiting for goattyd greeting"), "{err:#}");
    }

    #[test]
    fn open_once_does_not_reconnect_and_reports_reply_lost() {
        let (tx_req, rx_req) = flume::unbounded::<ClientMsg>();
        let (client_w, server_r) = std::os::unix::net::UnixStream::pair().unwrap();
        let (mut server_w, client_r) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(server_r);
            let hello = ServerMsg::Hello {
                version: PROTOCOL_VERSION,
                daemon_pid: 1,
                host: HostInfo { hostname: "h".into(), user: "u".into(), os: "linux".into(), daemon_version: "0".into() },
            };
            server_w.write_all(&encode(&hello).unwrap()).unwrap();
            // Read one request, record it, and drop the link without answering.
            if let Ok(msg) = read_msg::<_, ClientMsg>(&mut r) {
                let _ = tx_req.send(msg);
            }
            drop(server_w);
            drop(r);
        });

        let (out, out_rx) = flume::unbounded();
        let _ = out.send(WriterCmd::Replace(Box::new(client_w.try_clone().unwrap())));
        let (reconnect_tx, reconnect_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            endpoint: Endpoint::command_line("false").unwrap(),
            opts: StreamOpts::LOCAL,
            flow_window: AtomicU32::new(StreamOpts::LOCAL.window),
            client_name: "test".into(),
            auto_reconnect: false,
            out,
            panes: Mutex::new(HashMap::new()),
            attaching: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            left: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            early_size: Mutex::new(HashMap::new()),
            next_req: AtomicU32::new(1),
            host: Mutex::new(HostInfo::default()),
            announce: AtomicU32::new(PROTOCOL_VERSION),
            version: AtomicU32::new(PROTOCOL_VERSION),
            daemon_pid: AtomicU32::new(1),
            link_gen: AtomicU64::new(1),
            connected: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            closer: Mutex::new(None),
            epoch: Instant::now(),
            last_rx_us: AtomicU64::new(0),
            rtt_us: AtomicU64::new(42_000),
            reconnect_tx,
            reconnect_rx,
        });
        std::thread::spawn(move || writer_loop(out_rx));
        let mut reader = std::io::BufReader::new(Box::new(client_r) as Box<dyn Read + Send>);
        // Consume the server's Hello as handshake() would.
        assert!(matches!(read_msg::<_, ServerMsg>(&mut reader).unwrap(), ServerMsg::Hello { .. }));
        let s = shared.clone();
        let (done_tx, done_rx) = flume::bounded(1);
        let reader_handle = std::thread::spawn(move || {
            reader_main(s, reader);
            let _ = done_tx.send(());
        });
        let conn = Connection { shared };
        assert_eq!(conn.rtt(), Some(Duration::from_millis(42)));

        let err = conn.set_host_settings_within("{}".into(), true, Duration::from_secs(5)).unwrap_err();
        assert!(err.downcast_ref::<ReplyLost>().is_some(), "expected ReplyLost, got: {err:#}");
        assert!(rx_req.recv_timeout(Duration::from_secs(1)).is_ok());
        // `reader_main` must exit immediately rather than attempting to reconnect.
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).is_ok(), "reader did not exit after one-shot disconnect");
        reader_handle.join().unwrap();
        assert!(!conn.is_connected());
        assert_eq!(conn.rtt(), None, "disconnect must clear stale RTT");
        drop(client_w);
        server.join().unwrap();
    }

    #[derive(Default)]
    struct RecSink {
        fed: Mutex<Vec<u8>>,
        events: Mutex<Vec<String>>,
        top_lines: Mutex<Vec<(u64, goatty_proto::TopLineCause)>>,
    }

    impl PaneSink for RecSink {
        fn feed(&self, bytes: &[u8]) {
            self.fed.lock().extend_from_slice(bytes);
        }
        fn event(&self, ev: PaneEvent) {
            self.events.lock().push(format!("{ev:?}"));
        }
        fn top_line(&self, line: u64, cause: goatty_proto::TopLineCause) {
            self.top_lines.lock().push((line, cause));
        }
    }

    fn mock_connection(
        server_fn: impl FnOnce(std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream) + Send + 'static,
    ) -> (Connection, std::thread::JoinHandle<()>, std::thread::JoinHandle<()>) {
        mock_connection_v(PROTOCOL_VERSION, server_fn)
    }

    /// [`mock_connection`] with a daemon that speaks `version`.
    fn mock_connection_v(
        version: u32,
        server_fn: impl FnOnce(std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream) + Send + 'static,
    ) -> (Connection, std::thread::JoinHandle<()>, std::thread::JoinHandle<()>) {
        let (client_w, server_r) = std::os::unix::net::UnixStream::pair().unwrap();
        let (mut server_w, client_r) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let hello = ServerMsg::Hello {
                version,
                daemon_pid: 1,
                host: HostInfo { hostname: "h".into(), user: "u".into(), os: "linux".into(), daemon_version: "0".into() },
            };
            server_w.write_all(&encode(&hello).unwrap()).unwrap();
            server_fn(std::io::BufReader::new(server_r), server_w);
        });
        let (out, out_rx) = flume::unbounded();
        let _ = out.send(WriterCmd::Replace(Box::new(client_w.try_clone().unwrap())));
        let (reconnect_tx, reconnect_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            endpoint: Endpoint::command_line("false").unwrap(),
            opts: StreamOpts::REMOTE,
            flow_window: AtomicU32::new(StreamOpts::REMOTE.window),
            client_name: "test".into(),
            auto_reconnect: false,
            out,
            panes: Mutex::new(HashMap::new()),
            attaching: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            left: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            early_size: Mutex::new(HashMap::new()),
            next_req: AtomicU32::new(1),
            host: Mutex::new(HostInfo::default()),
            announce: AtomicU32::new(PROTOCOL_VERSION),
            version: AtomicU32::new(version),
            daemon_pid: AtomicU32::new(1),
            link_gen: AtomicU64::new(1),
            connected: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            closer: Mutex::new(Some(Closer(Arc::new(Mutex::new(Some(crate::transport::CloseHandle::Socket(client_w))))))),
            epoch: Instant::now(),
            last_rx_us: AtomicU64::new(0),
            rtt_us: AtomicU64::new(0),
            reconnect_tx,
            reconnect_rx,
        });
        std::thread::spawn(move || writer_loop(out_rx));
        let mut reader = std::io::BufReader::new(Box::new(client_r) as Box<dyn Read + Send>);
        assert!(matches!(read_msg::<_, ServerMsg>(&mut reader).unwrap(), ServerMsg::Hello { .. }));
        let s = shared.clone();
        let reader_handle = std::thread::spawn(move || reader_main(s, reader));
        (Connection { shared }, reader_handle, server)
    }

    #[test]
    fn attach_many_pipelines_all_requests_before_any_reply() {
        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            // Read all 8 Attach requests BEFORE answering a single one: a
            // sequential implementation would deadlock here.
            let mut reqs = Vec::new();
            for _ in 0..8 {
                match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                    ClientMsg::Attach { req, session, .. } => reqs.push((req, session)),
                    other => panic!("unexpected client msg: {other:?}"),
                }
            }
            // Answer in reverse order to prove replies are matched by `req`.
            for (req, session) in reqs.into_iter().rev() {
                let sz = ServerMsg::SizeControl { session, size, controlling: session.is_multiple_of(2) };
                let att = ServerMsg::Attached { req, session, snapshot: format!("snap-{session}").into_bytes() };
                let top = ServerMsg::TopLine { session, line: session * 10, cause: goatty_proto::TopLineCause::Attach };
                let cwd = ServerMsg::Cwd { session, cwd: Some(format!("/dir/{session}")) };
                for m in [sz, att, top, cwd] {
                    w.write_all(&encode(&m).unwrap()).unwrap();
                }
            }
            w.flush().unwrap();
            // Keep stream open until client finishes reading follow-up frames.
            let _ = read_msg::<_, ClientMsg>(&mut r);
        });

        let sinks: Vec<Arc<RecSink>> = (0..8).map(|_| Arc::new(RecSink::default())).collect();
        let requests: Vec<AttachRequest> =
            (10u64..18).zip(sinks.iter().cloned()).map(|(session, sink)| AttachRequest { session, size, sink }).collect();
        let results = conn.attach_many_within(requests, Duration::from_secs(5));
        assert_eq!(results.len(), 8);
        for (i, res) in results.into_iter().enumerate() {
            res.unwrap_or_else(|e| panic!("entry {i} failed: {e:#}"));
            let session = 10 + i as u64;
            assert_eq!(String::from_utf8_lossy(&sinks[i].fed.lock()), format!("snap-{session}"));
            assert!(
                sinks[i]
                    .events
                    .lock()
                    .iter()
                    .any(|e| e.contains("SizeControl") && e.contains(&format!("controlling: {}", session.is_multiple_of(2)))),
                "missing SizeControl for {session}: {:?}",
                sinks[i].events.lock()
            );
        }
        // Wait briefly for follow-up TopLine/Cwd frames pushed right after Attached.
        let deadline = Instant::now() + Duration::from_secs(2);
        while sinks.iter().any(|s| s.top_lines.lock().is_empty() || !s.events.lock().iter().any(|e| e.contains("Cwd"))) {
            assert!(Instant::now() < deadline, "follow-up frames did not arrive");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(conn);
        reader_h.join().unwrap();
        server_h.join().unwrap();
    }

    #[test]
    fn attach_many_handles_partial_errors_invalid_sizes_and_duplicates() {
        let ok_size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let bad_size = WinSize { cols: 9999, rows: 9999, cell_width: 0, cell_height: 0 };
        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            // Only 3 valid unique requests (sessions 1, 3, 4) should hit the wire.
            let mut reqs = Vec::new();
            for _ in 0..3 {
                match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                    ClientMsg::Attach { req, session, .. } => reqs.push((req, session)),
                    other => panic!("unexpected client msg: {other:?}"),
                }
            }
            for (req, session) in reqs {
                let msg = if session == 3 {
                    ServerMsg::Error { req: Some(req), message: "no session 3".into() }
                } else {
                    ServerMsg::Attached { req, session, snapshot: format!("ok-{session}").into_bytes() }
                };
                w.write_all(&encode(&msg).unwrap()).unwrap();
            }
            w.flush().unwrap();
            let _ = read_msg::<_, ClientMsg>(&mut r);
        });

        let sinks: Vec<Arc<RecSink>> = (0..5).map(|_| Arc::new(RecSink::default())).collect();
        let requests = vec![
            AttachRequest { session: 1, size: ok_size, sink: sinks[0].clone() },
            AttachRequest { session: 2, size: bad_size, sink: sinks[1].clone() },
            AttachRequest { session: 1, size: ok_size, sink: sinks[2].clone() },
            AttachRequest { session: 3, size: ok_size, sink: sinks[3].clone() },
            AttachRequest { session: 4, size: ok_size, sink: sinks[4].clone() },
        ];
        let results = conn.attach_many_within(requests, Duration::from_secs(5));
        assert!(results[0].is_ok());
        assert!(results[1].as_ref().unwrap_err().to_string().contains("too large"));
        assert!(results[2].as_ref().unwrap_err().to_string().contains("duplicate"));
        assert!(results[3].as_ref().unwrap_err().to_string().contains("no session 3"));
        assert!(results[4].is_ok());
        assert_eq!(String::from_utf8_lossy(&sinks[0].fed.lock()), "ok-1");
        assert_eq!(String::from_utf8_lossy(&sinks[4].fed.lock()), "ok-4");
        drop(conn);
        reader_h.join().unwrap();
        server_h.join().unwrap();
    }

    #[test]
    fn attach_many_disconnect_mid_batch_fails_remaining_immediately() {
        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            let mut reqs = Vec::new();
            for _ in 0..4 {
                match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                    ClientMsg::Attach { req, session, .. } => reqs.push((req, session)),
                    other => panic!("unexpected client msg: {other:?}"),
                }
            }
            // Answer only the first two, then drop the link.
            for (req, session) in &reqs[..2] {
                let msg = ServerMsg::Attached { req: *req, session: *session, snapshot: b"hi".to_vec() };
                w.write_all(&encode(&msg).unwrap()).unwrap();
            }
            w.flush().unwrap();
            drop(w);
        });

        let start = Instant::now();
        let requests: Vec<AttachRequest> =
            (1u64..=4).map(|session| AttachRequest { session, size, sink: Arc::new(RecSink::default()) }).collect();
        let results = conn.attach_many_within(requests, Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
        assert!(results[2].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        assert!(results[3].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        drop(conn);
        reader_h.join().unwrap();
        server_h.join().unwrap();
    }

    #[test]
    fn attach_many_timeout_cleans_pending_and_early_size() {
        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (tx_done, rx_done) = flume::bounded::<()>(1);
        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            for _ in 0..2 {
                let _ = read_msg::<_, ClientMsg>(&mut r).unwrap();
            }
            // Send SizeControl for session 1, then stall without sending Attached.
            let sz = ServerMsg::SizeControl { session: 1, size, controlling: true };
            w.write_all(&encode(&sz).unwrap()).unwrap();
            w.flush().unwrap();
            let _ = rx_done.recv_timeout(Duration::from_secs(2));
        });

        let requests = vec![
            AttachRequest { session: 1, size, sink: Arc::new(RecSink::default()) },
            AttachRequest { session: 2, size, sink: Arc::new(RecSink::default()) },
        ];
        let results = conn.attach_many_within(requests, Duration::from_millis(80));
        assert!(results[0].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        assert!(results[1].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        assert!(conn.shared.pending.lock().is_empty(), "timed-out pending entries must be removed");
        assert!(conn.shared.early_size.lock().is_empty(), "stale early_size entries must be removed");
        let _ = tx_done.send(());
        drop(conn);
        reader_h.join().unwrap();
        server_h.join().unwrap();
    }

    #[test]
    fn attach_many_when_disconnected_returns_reply_lost() {
        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (conn, reader_h, server_h) = mock_connection(move |_r, w| {
            drop(w);
        });
        reader_h.join().unwrap();
        assert!(!conn.is_connected());

        let results = conn.attach_many_within(
            vec![
                AttachRequest { session: 1, size, sink: Arc::new(RecSink::default()) },
                AttachRequest { session: 2, size, sink: Arc::new(RecSink::default()) },
            ],
            Duration::from_secs(1),
        );
        assert_eq!(results.len(), 2);
        assert!(results[0].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        assert!(results[1].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        drop(conn);
        server_h.join().unwrap();
    }

    #[test]
    fn attach_many_timeout_during_slow_sink_feed_does_not_leak_pane() {
        struct SlowSink {
            entered: flume::Sender<()>,
            release: flume::Receiver<()>,
        }
        impl PaneSink for SlowSink {
            fn feed(&self, _bytes: &[u8]) {
                let _ = self.entered.send(());
                let _ = self.release.recv_timeout(Duration::from_secs(5));
            }
            fn event(&self, _ev: PaneEvent) {}
        }

        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (entered_tx, entered_rx) = flume::bounded::<()>(1);
        let (release_tx, release_rx) = flume::bounded::<()>(1);
        let (detach_tx, detach_rx) = flume::bounded::<SessionId>(1);
        let (tx_done, rx_done) = flume::bounded::<()>(1);

        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            let req = match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                ClientMsg::Attach { req, session: 1, .. } => req,
                other => panic!("unexpected client msg: {other:?}"),
            };
            let att = ServerMsg::Attached { req, session: 1, snapshot: b"slow-snapshot".to_vec() };
            w.write_all(&encode(&att).unwrap()).unwrap();
            w.flush().unwrap();

            if let Ok(ClientMsg::Detach { session }) = read_msg::<_, ClientMsg>(&mut r) {
                let _ = detach_tx.send(session);
            }
            let _ = rx_done.recv_timeout(Duration::from_secs(5));
            drop(w);
        });

        let sink = Arc::new(SlowSink { entered: entered_tx, release: release_rx });
        let start = Instant::now();
        let results = conn.attach_many_within(vec![AttachRequest { session: 1, size, sink }], Duration::from_millis(120));
        assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
        assert!(entered_rx.recv_timeout(Duration::from_secs(1)).is_ok(), "sink.feed should have started before timeout");
        assert!(results[0].as_ref().unwrap_err().downcast_ref::<ReplyLost>().is_some());
        assert_eq!(detach_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);

        // Allow `sink.feed` to finish; the cancelled attach must not install a pane.
        let shared = conn.shared.clone();
        let _ = release_tx.send(());
        let _ = tx_done.send(());
        drop(conn);
        reader_h.join().unwrap();
        assert!(shared.panes.lock().is_empty(), "cancelled attach must not leak pane");
        server_h.join().unwrap();
    }

    #[test]
    fn workspace_views_hear_changes_from_others_and_lists() {
        #[derive(Default)]
        struct Heard(Mutex<Vec<(usize, String)>>);
        impl WorkspaceSink for Heard {
            fn workspace(&self, conn: usize, ev: WorkspaceEvent) {
                let what = match ev {
                    WorkspaceEvent::Changed(l) => format!("changed {} r{}", l.json, l.revision),
                    WorkspaceEvent::List { list, .. } => format!("list {}", list.len()),
                    WorkspaceEvent::Rejoined { info, .. } => format!("rejoined {}", info.id),
                };
                self.0.lock().push((conn, what));
            }
        }
        let info = |id| WorkspaceInfo { id, name: "Main".into(), revision: 1, tabs: 1, sessions: vec![1], views: 2, active: 1 };
        let layout = |id, json: &str| WorkspaceLayout { id, revision: 2, json: json.into(), sessions: vec![1] };
        let (tx_drop, rx_drop) = flume::bounded::<()>(1);
        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            let mut send = |m: ServerMsg| {
                w.write_all(&encode(&m).unwrap()).unwrap();
                w.flush().unwrap();
            };
            for _ in 0..2 {
                match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                    ClientMsg::JoinWorkspace { req, id, .. } => {
                        let id = id.unwrap_or(7);
                        send(ServerMsg::Joined { req, info: info(id), layout: layout(id, "") });
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            // Window 1 saved: window 2 hears it, window 1 doesn't.
            send(ServerMsg::LayoutChanged { by: 1, layout: layout(7, "a") });
            // Another workspace's change: nobody here shows it.
            send(ServerMsg::LayoutChanged { by: 9, layout: layout(8, "b") });
            send(ServerMsg::WorkspacesChanged { seq: 3, list: vec![info(7), info(8)] });
            match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                ClientMsg::LeaveWorkspace { view: 2 } => {}
                other => panic!("unexpected {other:?}"),
            }
            let _ = rx_drop.recv_timeout(Duration::from_secs(5));
            drop(w);
        });
        let (one, two) = (Arc::new(Heard::default()), Arc::new(Heard::default()));
        assert_eq!(conn.join_workspace(1, None, one.clone()).unwrap().0.id, 7);
        assert_eq!(conn.join_workspace(2, Some(7), two.clone()).unwrap().1.revision, 2);
        assert_eq!(conn.shown_workspace(2), Some(7));
        let deadline = Instant::now() + Duration::from_secs(2);
        while one.0.lock().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let key = conn.key();
        assert_eq!(*one.0.lock(), [(key, "list 2".to_string())]);
        assert_eq!(*two.0.lock(), [(key, "changed a r2".to_string()), (key, "list 2".to_string())]);
        conn.leave_workspace(2);
        assert_eq!(conn.shown_workspace(2), None);
        conn.leave_workspace(2);

        let _ = tx_drop.send(());
        reader_h.join().unwrap();
        drop(conn);
        server_h.join().unwrap();
    }

    #[test]
    fn a_window_that_leaves_while_joining_isnt_registered() {
        struct Quiet;
        impl WorkspaceSink for Quiet {
            fn workspace(&self, _: usize, _: WorkspaceEvent) {}
        }
        let (seen_tx, seen_rx) = flume::unbounded::<String>();
        let (go_tx, go_rx) = flume::bounded::<()>(1);
        let (tx_drop, rx_drop) = flume::bounded::<()>(1);
        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            let req = match read_msg::<_, ClientMsg>(&mut r).unwrap() {
                ClientMsg::JoinWorkspace { req, .. } => req,
                other => panic!("unexpected {other:?}"),
            };
            let _ = seen_tx.send("join".into());
            // The window leaves before the answer comes.
            assert!(matches!(read_msg::<_, ClientMsg>(&mut r).unwrap(), ClientMsg::LeaveWorkspace { view: 4 }));
            let _ = go_rx.recv_timeout(Duration::from_secs(5));
            let info = WorkspaceInfo { id: 7, name: "Main".into(), revision: 0, tabs: 0, sessions: vec![], views: 1, active: 1 };
            let layout = WorkspaceLayout { id: 7, revision: 0, json: String::new(), sessions: vec![] };
            w.write_all(&encode(&ServerMsg::Joined { req, info, layout }).unwrap()).unwrap();
            w.flush().unwrap();
            // ... so the daemon hears it leave again, after the join.
            if let Ok(ClientMsg::LeaveWorkspace { view: 4 }) = read_msg::<_, ClientMsg>(&mut r) {
                let _ = seen_tx.send("left again".into());
            }
            let _ = rx_drop.recv_timeout(Duration::from_secs(5));
            drop(w);
        });
        let conn = Arc::new(conn);
        let joining = {
            let conn = conn.clone();
            std::thread::spawn(move || conn.join_workspace(4, None, Arc::new(Quiet)))
        };
        assert_eq!(seen_rx.recv_timeout(Duration::from_secs(5)).unwrap(), "join");
        conn.leave_workspace(4);
        let _ = go_tx.send(());
        assert_eq!(joining.join().unwrap().unwrap().0.id, 7);
        assert_eq!(conn.shown_workspace(4), None, "not registered: it left");
        assert_eq!(seen_rx.recv_timeout(Duration::from_secs(5)).unwrap(), "left again");

        let _ = tx_drop.send(());
        reader_h.join().unwrap();
        drop(conn);
        server_h.join().unwrap();
    }

    #[test]
    fn detach_sink_only_unsubscribes_matching_owner() {
        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (detach_tx, detach_rx) = flume::bounded::<SessionId>(2);
        let (tx_drop, rx_drop) = flume::bounded::<()>(1);

        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            while let Ok(msg) = read_msg::<_, ClientMsg>(&mut r) {
                match msg {
                    ClientMsg::Attach { req, session, .. } => {
                        let reply = ServerMsg::Attached { req, session, snapshot: b"hi".to_vec() };
                        w.write_all(&encode(&reply).unwrap()).unwrap();
                        w.flush().unwrap();
                    }
                    ClientMsg::Detach { session } => {
                        let _ = detach_tx.send(session);
                        break;
                    }
                    _ => {}
                }
            }
            let _ = rx_drop.recv_timeout(Duration::from_secs(5));
            drop(w);
        });

        let sink_a: Arc<dyn PaneSink> = Arc::new(RecSink::default());
        let sink_b: Arc<dyn PaneSink> = Arc::new(RecSink::default());
        conn.attach(5, size, sink_a.clone()).unwrap();
        // Second window attaches to the same session on the pooled connection.
        conn.attach(5, size, sink_b.clone()).unwrap();

        // Detaching the old pane must not remove `sink_b` or send `Detach`.
        assert!(!conn.detach_sink(5, &sink_a));
        assert!(conn.shared.panes.lock().contains_key(&5));
        assert!(detach_rx.try_recv().is_err());

        // Detaching the current owner unsubscribes normally.
        assert!(conn.detach_sink(5, &sink_b));
        assert!(!conn.shared.panes.lock().contains_key(&5));
        assert_eq!(detach_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 5);

        let _ = tx_drop.send(());
        reader_h.join().unwrap();
        drop(conn);
        server_h.join().unwrap();
    }

    #[test]
    fn detach_sink_during_pending_replacement_attach_keeps_replacement_subscribed() {
        let size = WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        let (second_attach_seen_tx, second_attach_seen_rx) = flume::bounded::<()>(1);
        let (allow_second_reply_tx, allow_second_reply_rx) = flume::bounded::<()>(1);
        let (marker_seen_tx, marker_seen_rx) = flume::bounded::<()>(1);
        let (detach_tx, detach_rx) = flume::bounded::<SessionId>(2);
        let (tx_drop, rx_drop) = flume::bounded::<()>(1);

        let (conn, reader_h, server_h) = mock_connection(move |mut r, mut w| {
            let mut attaches = 0;
            let mut marker_seen = false;
            while let Ok(msg) = read_msg::<_, ClientMsg>(&mut r) {
                match msg {
                    ClientMsg::Attach { req, session, .. } => {
                        attaches += 1;
                        if attaches == 2 {
                            let _ = second_attach_seen_tx.send(());
                            let _ = allow_second_reply_rx.recv_timeout(Duration::from_secs(5));
                        }
                        let reply = ServerMsg::Attached { req, session, snapshot: b"hi".to_vec() };
                        w.write_all(&encode(&reply).unwrap()).unwrap();
                        w.flush().unwrap();
                    }
                    ClientMsg::Input { session: 5, ref data } if data == b"marker" => {
                        marker_seen = true;
                        let _ = marker_seen_tx.send(());
                    }
                    ClientMsg::Detach { session } => {
                        assert!(marker_seen, "premature Detach frame arrived before post-attach marker");
                        let _ = detach_tx.send(session);
                        break;
                    }
                    _ => {}
                }
            }
            let _ = rx_drop.recv_timeout(Duration::from_secs(5));
            drop(w);
        });

        let conn = Arc::new(conn);
        let sink_a: Arc<dyn PaneSink> = Arc::new(RecSink::default());
        let sink_b: Arc<dyn PaneSink> = Arc::new(RecSink::default());
        conn.attach(5, size, sink_a.clone()).unwrap();

        let conn_b = conn.clone();
        let sink_b_thread = sink_b.clone();
        let attach_b = std::thread::spawn(move || conn_b.attach(5, size, sink_b_thread));

        second_attach_seen_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // `sink_a` detaches while `sink_b`'s replacement `Attach` is in flight:
        // `sink_a` is removed locally, but `Detach` is suppressed so it cannot
        // unsubscribe `sink_b` after its `Attach`.
        assert!(conn.detach_sink(5, &sink_a));
        assert!(detach_rx.try_recv().is_err());

        let _ = allow_second_reply_tx.send(());
        attach_b.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(&conn.shared.panes.lock().get(&5).unwrap().sink, &sink_b));

        // Send an ordered marker on the same writer stream and wait for the
        // server to observe it before detaching `sink_b`, proving no premature
        // `Detach` was queued ahead of the marker.
        conn.input(5, b"marker");
        marker_seen_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(detach_rx.try_recv().is_err());

        // Detaching `sink_b` now sends `Detach`.
        assert!(conn.detach_sink(5, &sink_b));
        assert_eq!(detach_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 5);

        let _ = tx_drop.send(());
        reader_h.join().unwrap();
        drop(conn);
        server_h.join().unwrap();
    }
}
