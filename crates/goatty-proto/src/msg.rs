//! Message types. Any change here is a wire change: bump [`PROTOCOL_VERSION`].
//!
//! # Compatibility
//!
//! Peers support a *range* of protocol versions, [`MIN_PROTOCOL_VERSION`]
//! ..= [`PROTOCOL_VERSION`], and agree on one in the handshake:
//!
//! 1. The client sends `Hello { version: its newest }`.
//! 2. The daemon answers `Hello { version: v }` with
//!    `v = min(client, daemon newest)` ([`negotiate`]). If `v` is older than
//!    the daemon supports, it answers with its own newest version instead and
//!    closes the connection, so the client can tell "daemon too new" from
//!    "daemon too old".
//! 3. Both sides then use `v`: neither sends a message newer than `v`.
//!
//! That only works if the wire format grows **append-only**: new messages
//! are new variants at the *end* of [`ClientMsg`]/[`ServerMsg`] (postcard
//! encodes variants by index), existing variants never change their fields,
//! and each addition bumps [`PROTOCOL_VERSION`] and notes `vN:` in its doc.
//! A change that can't be made that way raises [`MIN_PROTOCOL_VERSION`].
//!
//! Daemons up to v5 required an exact match and answer any other version
//! with `Error { message: "protocol mismatch: daemon vN, client vM" }`. The
//! client recognizes that and reconnects announcing `vN` when it supports it.

use serde::{Deserialize, Serialize};

/// The newest protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 16;

/// The oldest protocol version this build still speaks.
pub const MIN_PROTOCOL_VERSION: u32 = 7;

/// v6: `ClientMsg::Run` (a session running a given command).
pub const RUN_SINCE: u32 = 6;

/// v7: `ServerMsg::Process` and `SessionInfo::process`.
pub const PROCESS_SINCE: u32 = 7;

/// v8: `SaveLayout` / `ClaimLayout` (saved window layouts).
pub const LAYOUT_SINCE: u32 = 8;

/// v9: `Shell` (setup and diagnostics of the managed shell).
pub const SHELL_SINCE: u32 = 9;

/// v10: `Restart` (the daemon re-executes itself and keeps its sessions).
/// Daemons from v10 on also keep their sessions across a crash or
/// `systemctl --user restart` under the systemd service.
pub const RESTART_SINCE: u32 = 10;

/// v11: line numbers (`ServerMsg::TopLine`), scrollback on demand
/// (`FetchHistory` / `History`) and zstd-compressed snapshots
/// (`Compress` / `ServerMsg::Zstd`).
pub const HISTORY_SINCE: u32 = 11;

/// v12: `Net` (the host's availability on the user's tailnet).
pub const NET_SINCE: u32 = 12;

/// v13: remote working directory updates (`ServerMsg::Cwd`), path resolution
/// and stat (`StatPath` / `PathStat`), and reading remote files
/// (`ReadFile` / `FileData`).
pub const REMOTE_FILES_SINCE: u32 = 13;

/// v14: one controlling client per session sets its size (`TakeSize` /
/// `ServerMsg::SizeControl`). Other v14 clients watch at the session's size;
/// clients older than v14 take control with every `Attach` and `Resize`.
pub const SIZE_CONTROL_SINCE: u32 = 14;

/// v15: adaptive flow-control window on remote links (`ClientMsg::Rtt` /
/// `ServerMsg::FlowWindow`, #9).
pub const FLOW_CONTROL_SINCE: u32 = 15;

/// v16: listing the free window layouts (`ListLayouts` / `ServerMsg::Layouts`),
/// claiming one by id (`ClaimLayoutId`), so a client restores the layout it
/// saved (a window's tabs on that host, #252) rather than the newest, and
/// releasing one without disconnecting (`ReleaseLayout`: detached tabs).
pub const LAYOUT_LIST_SINCE: u32 = 16;

/// Maximum single-frame remote file transfer size (`MAX_FRAME` minus headroom
/// for postcard headers and path metadata).
pub const HARD_MAX_FILE_BYTES: u64 = (crate::MAX_FRAME - 65_536) as u64;

/// The version to use with a peer whose newest version is `peer`, or
/// `None` when the ranges don't overlap.
pub fn negotiate(peer: u32) -> Option<u32> {
    let v = peer.min(PROTOCOL_VERSION);
    (v >= MIN_PROTOCOL_VERSION).then_some(v)
}

pub type SessionId = u64;
/// Client-chosen correlation id for request/response pairs.
pub type ReqId = u32;
/// Client-chosen id of a window layout (random, so windows on different
/// machines don't collide).
pub type LayoutId = u64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl WinSize {
    /// The most columns a session may have.
    pub const MAX_COLS: u16 = 4096;
    /// The most rows a session may have.
    pub const MAX_ROWS: u16 = 2048;
    /// The most cells a session may have (a grid is allocated per size).
    pub const MAX_CELLS: u32 = 1 << 21;

    /// Within the limits above. A peer refuses (never clamps) a size outside
    /// them: a snapshot made for one grid size can't be fed into another.
    pub fn is_sane(&self) -> bool {
        self.cols <= Self::MAX_COLS && self.rows <= Self::MAX_ROWS && u32::from(self.cols) * u32::from(self.rows) <= Self::MAX_CELLS
    }
}

/// Per-subscription stream parameters, chosen by the client per connection
/// type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamOpts {
    /// Max unacknowledged stream bytes before the daemon switches this
    /// subscriber to skip-and-resync. For remote links this is the starting
    /// window; the daemon adapts it from delivery rate and RTT (#9).
    pub window: u32,
    /// Max scrollback lines replayed in the attach snapshot.
    pub history: u32,
}

impl StreamOpts {
    /// Minimum flow-control window (64 KiB).
    pub const MIN_WINDOW: u32 = 64 << 10;
    /// Maximum adaptive flow-control window (8 MiB, matching `LOCAL`).
    pub const MAX_WINDOW: u32 = 8 << 20;
    /// Unix socket: effectively lossless and full history.
    pub const LOCAL: Self = Self { window: Self::MAX_WINDOW, history: 5_000 };
    /// SSH/network: bounded initial in-flight data; adapted across
    /// `MIN_WINDOW..=MAX_WINDOW` from measured delivery rate and RTT (#9).
    pub const REMOTE: Self = Self { window: 256 << 10, history: 2_000 };
}

/// Compatibility rule: `Hello` stays variant 0 with `version` as its first
/// field in both enums forever, so peers of any version can detect a
/// mismatch and report it instead of misparsing. Everything else is
/// append-only (see the module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        version: u32,
        client: String,
    },
    ListSessions {
        req: ReqId,
    },
    /// Spawn a new shell session and subscribe this connection to it.
    /// `cwd` is a path *on the daemon's host*.
    Create {
        req: ReqId,
        size: WinSize,
        cwd: Option<String>,
        opts: StreamOpts,
    },
    /// Subscribe this connection to an existing session.
    Attach {
        req: ReqId,
        session: SessionId,
        size: WinSize,
        opts: StreamOpts,
    },
    Detach {
        session: SessionId,
    },
    Input {
        session: SessionId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Resize {
        session: SessionId,
        size: WinSize,
    },
    /// Cumulative stream bytes (snapshot + output) consumed for `session`
    /// since this connection subscribed.
    Ack {
        session: SessionId,
        consumed: u64,
    },
    Kill {
        req: ReqId,
        session: SessionId,
    },
    /// Write input to a session without subscribing (scripting/tests).
    SendInput {
        req: ReqId,
        session: SessionId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Ping {
        nonce: u64,
    },
    /// Stop the daemon (kills all sessions).
    Shutdown {
        req: ReqId,
    },
    /// The host's settings document (`host.json`). Reply: `HostSettings`.
    GetHostSettings {
        req: ReqId,
    },
    /// Replace the host settings with `json` (a settings document). The
    /// daemon validates it and builds the shell configuration first; with
    /// `apply` it then saves the file and activates the new generation,
    /// without it only reports issues and the diff. Reply: `HostSettingsApplied`.
    SetHostSettings {
        req: ReqId,
        json: String,
        apply: bool,
    },
    /// Reply: `Generations`.
    ListGenerations {
        req: ReqId,
    },
    /// Activate generation `generation` (default: the previous one) and
    /// restore its settings. Reply: `HostSettings`.
    Rollback {
        req: ReqId,
        generation: Option<u32>,
    },
    /// The client's colors for this session, so the daemon answers color
    /// queries (OSC 4/10/11) with what the user actually sees: 16 ANSI
    /// colors, then fg, bg, cursor, selection fg, selection bg (`0xRRGGBB`).
    SetPalette {
        session: SessionId,
        colors: Vec<u32>,
    },
    /// Ask the running managed shells to restart with the active
    /// generation (each at its next idle prompt). Reply: `ShellsReloaded`.
    ReloadShells {
        req: ReqId,
    },
    /// v5: a plugin operation (`goatty_plugin::api::Request` as JSON:
    /// catalog, plan, install, uninstall). Answered off the connection's
    /// reader thread, since installs download. Reply: `Plugin`.
    Plugin {
        req: ReqId,
        request: String,
    },
    /// v6: like `Create`, but the session runs `argv` (looked up in the
    /// daemon's `PATH`) instead of the user's shell, e.g. `ssh host` for a
    /// plain SSH session. The session ends when the program exits.
    Run {
        req: ReqId,
        size: WinSize,
        cwd: Option<String>,
        argv: Vec<String>,
        opts: StreamOpts,
    },
    /// v8: remember a window's layout (tabs and splits) so a later window
    /// can restore it. `json` is the client's own format, opaque to the
    /// daemon; `sessions` are the sessions it shows. Saving also claims the
    /// layout for this connection. No `sessions`: forget it. No reply.
    SaveLayout {
        id: LayoutId,
        sessions: Vec<SessionId>,
        json: String,
    },
    /// v8: take over the most recently saved layout that no connection
    /// holds and that still shows a running session. One window gets it;
    /// the next one gets the next layout or none. Reply: `Layout`.
    ClaimLayout {
        req: ReqId,
    },
    /// v9: a shell setup or diagnostics operation (`goatty_shell::setup::Request`
    /// as JSON: status, install the built-in tools, offered, dismiss).
    /// Answered off the connection's reader thread, since the status starts a
    /// shell and installs download. Reply: `Shell`.
    Shell {
        req: ReqId,
        request: String,
    },
    /// v10: replace the daemon with `exe` (default: the goattyd it was
    /// started as, which after an update is the new build) without ending
    /// any session: the PTYs, the screens and the saved layouts carry over.
    /// The reply `Ok` comes before the switch; every connection closes with
    /// it and clients reconnect to the same socket. `Error` when it can't
    /// (e.g. `exe` isn't a goattyd).
    Restart {
        req: ReqId,
        exe: Option<String>,
    },
    /// v11: up to `count` rows of the session's primary screen just before
    /// host line `before` (see `ServerMsg::TopLine`), to fill the client's
    /// scrollback. Answered with `History` even when the daemon has none
    /// of them any more.
    FetchHistory {
        req: ReqId,
        session: SessionId,
        before: u64,
        count: u32,
    },
    /// v11: from now on, send snapshots and history on this connection
    /// zstd-compressed (`ServerMsg::Zstd`) when that makes them smaller.
    /// Live output is never compressed. Remote clients ask for it; on the
    /// local socket it would only cost time. No reply.
    Compress {
        zstd: bool,
    },
    /// v12: a tailnet operation (`goatty_net::api::Request` as JSON:
    /// status, make this host available or not, the pairing window, paired
    /// devices). Answered off the reader thread, since it runs `tailscale`.
    /// Reply: `Net`.
    Net {
        req: ReqId,
        request: String,
    },
    /// v13: resolve and stat the first existing path among `candidates` on
    /// the daemon's host (`cwd` pins relative resolution to the caller's
    /// working directory; when `None`, falls back to `session`'s current cwd;
    /// `~/` resolves against `$HOME`). Reply: `PathStat`.
    StatPath {
        req: ReqId,
        session: Option<SessionId>,
        cwd: Option<String>,
        candidates: Vec<String>,
    },
    /// v13: read up to `max_bytes` from regular file `path` on the daemon's
    /// host. Reply: `FileData` (or `Error` if missing, non-regular, or over
    /// the limit).
    ReadFile {
        req: ReqId,
        path: String,
        max_bytes: u64,
    },
    /// v14: make this connection the controlling client of `session` (it
    /// must be subscribed). The session takes the size this connection last
    /// asked for (`Attach`/`Resize`), and every v14 subscriber hears
    /// `SizeControl`.
    TakeSize {
        session: SessionId,
    },
    /// v15: latest round-trip time measured by the client from `Ping`/`Pong`,
    /// in microseconds. The daemon combines this with delivery rate from
    /// `Ack`s to size the adaptive flow-control window on remote links (#9).
    /// No reply.
    Rtt {
        rtt_us: u64,
    },
    /// v16: the layouts no connection holds that still show a running
    /// session, newest first, with only those sessions listed. Nothing is
    /// claimed. Reply: `Layouts`.
    ListLayouts {
        req: ReqId,
    },
    /// v16: take over layout `id` if no connection holds it and it still
    /// shows a running session (like `ClaimLayout`, for one layout). Reply:
    /// `Layout` (`None` when it's gone or held).
    ClaimLayoutId {
        req: ReqId,
        id: LayoutId,
    },
    /// v16: keep layout `id` with `sessions` and `json` as a free layout
    /// (detached tabs), the newest one, for any window to claim, unless
    /// another connection holds it. Safe to send again (a retry after a link
    /// drop never takes it from a window that claimed it meanwhile). No reply.
    ReleaseLayout {
        id: LayoutId,
        sessions: Vec<SessionId>,
        json: String,
    },
}

/// Why the daemon sent `ServerMsg::TopLine`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TopLineCause {
    /// Right after `Attached`: every row of the client's terminal came from
    /// the snapshot.
    Attach,
    /// Right after `Resync`: the screen is current, the history above it
    /// misses what was skipped.
    Resync,
    /// The session was resized: rows reflowed on both sides.
    Resize,
}

/// A saved window layout (see `ClientMsg::SaveLayout`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayoutInfo {
    pub id: LayoutId,
    pub json: String,
    /// The layout's sessions that are still running.
    pub sessions: Vec<SessionId>,
    pub saved_unix: u64,
}

/// One changed file of the generated shell configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiff {
    pub name: String,
    pub unified: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationInfo {
    pub number: u32,
    pub created_unix: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub pid: Option<u32>,
    pub title: String,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub attached: u32,
    pub created_unix: u64,
    /// v7: the foreground program (`zsh` at the prompt, `nvim` while that
    /// runs), if the host can tell.
    pub process: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub user: String,
    pub os: String,
    pub daemon_version: String,
}

/// v13: metadata for a resolved path on the daemon's host (`ServerMsg::PathStat`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotePathInfo {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello {
        version: u32,
        daemon_pid: u32,
        host: HostInfo,
    },
    Sessions {
        req: ReqId,
        list: Vec<SessionInfo>,
    },
    /// Reply to Create/Attach. `snapshot` is a VT byte stream that rebuilds
    /// the current state in a *fresh* terminal (it starts with RIS). Live
    /// `Output` follows with no gap and no overlap.
    Attached {
        req: ReqId,
        session: SessionId,
        #[serde(with = "serde_bytes")]
        snapshot: Vec<u8>,
    },
    Output {
        session: SessionId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// The subscriber fell behind and `skipped` bytes of output were not
    /// sent. `snapshot` brings an existing client terminal to the current
    /// state without resetting its scrollback.
    Resync {
        session: SessionId,
        skipped: u64,
        #[serde(with = "serde_bytes")]
        snapshot: Vec<u8>,
    },
    Exited {
        session: SessionId,
        code: Option<u32>,
    },
    Pong {
        nonce: u64,
    },
    Ok {
        req: ReqId,
    },
    Error {
        req: Option<ReqId>,
        message: String,
    },
    HostSettings {
        req: ReqId,
        /// The settings file as stored (non-default values only).
        json: String,
        /// Problems found loading it (goatty used fallbacks).
        issues: Vec<String>,
        /// `GOATTY_*` variables overriding settings in the daemon's environment.
        env_overrides: Vec<String>,
        /// The active generation.
        generation: Option<u32>,
    },
    HostSettingsApplied {
        req: ReqId,
        issues: Vec<String>,
        /// Changes to the generated files relative to the active generation
        /// (computed before applying).
        diff: Vec<FileDiff>,
        applied: bool,
        generation: Option<u32>,
    },
    Generations {
        req: ReqId,
        list: Vec<GenerationInfo>,
    },
    ShellsReloaded {
        req: ReqId,
        /// How many shells were asked.
        count: u32,
    },
    /// v5: `goatty_plugin::api::Reply` as JSON.
    Plugin {
        req: ReqId,
        reply: String,
    },
    /// v7: the session's foreground program changed (also sent right after
    /// `Attached`). Tabs show it as an icon and title fallback.
    Process {
        session: SessionId,
        name: Option<String>,
    },
    /// v8: reply to `ClaimLayout`.
    Layout {
        req: ReqId,
        layout: Option<LayoutInfo>,
    },
    /// v9: `goatty_shell::setup::Reply` as JSON.
    Shell {
        req: ReqId,
        reply: String,
    },
    /// v11: the top row of the session's primary screen is host line
    /// `line`, as of this point in the stream. Rows count from the
    /// session's start, so a number keeps meaning the same row while output
    /// scrolls; the client pins its terminal's rows to them and asks for
    /// what it misses with `FetchHistory`. Sent after `Attached`, `Resync`
    /// and a resize, while the primary screen is active. Not stream data.
    TopLine {
        session: SessionId,
        line: u64,
        cause: TopLineCause,
    },
    /// v11: reply to `FetchHistory`: the rows from host line `first` up to
    /// `before` as VT text (each row ends with a newline; a soft-wrapped
    /// row doesn't, so it wraps again), for the client to insert into its
    /// scrollback. `oldest` is the oldest line the daemon still has; empty
    /// `data` means it has none of them.
    History {
        req: ReqId,
        session: SessionId,
        before: u64,
        first: u64,
        oldest: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// v11: another `ServerMsg` (postcard), zstd-compressed. Only after the
    /// client sent `Compress`, and only for `Attached`, `Resync`, `History`,
    /// and `FileData`. Flow control counts the inner message's stream bytes.
    Zstd {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// v12: `goatty_net::api::Reply` as JSON.
    Net {
        req: ReqId,
        reply: String,
    },
    /// v13: the session's working directory changed (also sent right after
    /// `Attached`).
    Cwd {
        session: SessionId,
        cwd: Option<String>,
    },
    /// v13: reply to `StatPath`.
    PathStat {
        req: ReqId,
        info: Option<RemotePathInfo>,
    },
    /// v13: reply to `ReadFile`.
    FileData {
        req: ReqId,
        path: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// v14: the session's size and whether this connection controls it. In
    /// stream order (sent under the subscriber lock): right *before*
    /// `Attached` (so a watching client sizes its terminal before the
    /// snapshot; it may precede the reply that tells a `Create` its session
    /// id), and whenever the controller or the size changes, before the reflowed
    /// output and its `TopLine`. A client that doesn't control the session
    /// keeps its mirror at `size` (letterboxed in its pane); the controlling
    /// client's mirror follows its own pane, whose sizes it sends as `Resize`.
    SizeControl {
        session: SessionId,
        /// Only `cols` and `rows` are meaningful; the cell size is 0.
        size: WinSize,
        controlling: bool,
    },
    /// v15: active adaptive flow-control window for this connection, in bytes.
    /// Sent when the window changes materially so the client matches its `Ack`
    /// cadence (`window / 4`) to the daemon's active window. Not stream data.
    FlowWindow {
        window: u32,
    },
    /// v16: reply to `ListLayouts`.
    Layouts {
        req: ReqId,
        layouts: Vec<LayoutInfo>,
    },
}

impl ServerMsg {
    /// The request this message answers, if it's a reply.
    pub fn reply_to(&self) -> Option<ReqId> {
        match self {
            ServerMsg::Sessions { req, .. }
            | ServerMsg::Attached { req, .. }
            | ServerMsg::Ok { req }
            | ServerMsg::HostSettings { req, .. }
            | ServerMsg::HostSettingsApplied { req, .. }
            | ServerMsg::Generations { req, .. }
            | ServerMsg::ShellsReloaded { req, .. }
            | ServerMsg::Plugin { req, .. }
            | ServerMsg::Layout { req, .. }
            | ServerMsg::Layouts { req, .. }
            | ServerMsg::Shell { req, .. }
            | ServerMsg::Net { req, .. }
            | ServerMsg::History { req, .. }
            | ServerMsg::PathStat { req, .. }
            | ServerMsg::FileData { req, .. } => Some(*req),
            ServerMsg::Error { req, .. } => *req,
            _ => None,
        }
    }
}

impl ServerMsg {
    /// Stream payload size counted against the flow-control window.
    pub fn stream_len(&self) -> usize {
        match self {
            ServerMsg::Attached { snapshot, .. } | ServerMsg::Resync { snapshot, .. } => snapshot.len(),
            ServerMsg::Output { data, .. } => data.len(),
            _ => 0,
        }
    }

    /// Bytes of snapshot, history, or file data it carries (what compression is for).
    pub fn payload_len(&self) -> usize {
        match self {
            ServerMsg::History { data, .. } | ServerMsg::FileData { data, .. } => data.len(),
            m => m.stream_len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_picks_the_newest_common_version() {
        assert_eq!(negotiate(PROTOCOL_VERSION), Some(PROTOCOL_VERSION));
        assert_eq!(negotiate(PROTOCOL_VERSION + 3), Some(PROTOCOL_VERSION));
        assert_eq!(negotiate(MIN_PROTOCOL_VERSION), Some(MIN_PROTOCOL_VERSION));
        assert_eq!(negotiate(MIN_PROTOCOL_VERSION - 1), None);
    }

    #[test]
    fn window_sizes_are_bounded() {
        let ws = |cols, rows| WinSize { cols, rows, cell_width: 8, cell_height: 16 };
        assert!(ws(80, 24).is_sane());
        assert!(ws(1536, 600).is_sane());
        assert!(!ws(WinSize::MAX_COLS + 1, 1).is_sane());
        assert!(!ws(1, WinSize::MAX_ROWS + 1).is_sane());
        assert!(!ws(4096, 2048).is_sane());
        assert!(!ws(u16::MAX, u16::MAX).is_sane());
    }

    #[test]
    fn hello_layout_is_frozen() {
        // Variant 0, version first: what a peer of any version decodes.
        let bytes = postcard::to_allocvec(&ClientMsg::Hello { version: 9, client: "x".into() }).unwrap();
        assert_eq!(&bytes[..2], &[0, 9]);
        let bytes = postcard::to_allocvec(&ServerMsg::Hello { version: 9, daemon_pid: 1, host: HostInfo::default() }).unwrap();
        assert_eq!(&bytes[..2], &[0, 9]);
    }

    #[test]
    fn v5_messages_keep_their_variant_index() {
        // A v5 peer must still decode everything it knows.
        let bytes = postcard::to_allocvec(&ClientMsg::Plugin { req: 1, request: String::new() }).unwrap();
        assert_eq!(bytes[0], 18);
        let bytes = postcard::to_allocvec(&ServerMsg::Plugin { req: 1, reply: String::new() }).unwrap();
        assert_eq!(bytes[0], 13);
        // v6 additions come after them.
        let size = WinSize { cols: 1, rows: 1, cell_width: 1, cell_height: 1 };
        let run = ClientMsg::Run { req: 1, size, cwd: None, argv: vec![], opts: StreamOpts::LOCAL };
        assert_eq!(postcard::to_allocvec(&run).unwrap()[0], 19);
        // v9 additions come after v8's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::Shell { req: 1, request: String::new() }).unwrap()[0], 22);
        assert_eq!(postcard::to_allocvec(&ServerMsg::Shell { req: 1, reply: String::new() }).unwrap()[0], 16);
        // v11 additions come after v10's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::Restart { req: 1, exe: None }).unwrap()[0], 23);
        let fetch = ClientMsg::FetchHistory { req: 1, session: 1, before: 9, count: 1 };
        assert_eq!(postcard::to_allocvec(&fetch).unwrap()[0], 24);
        assert_eq!(postcard::to_allocvec(&ClientMsg::Compress { zstd: true }).unwrap()[0], 25);
        let top = ServerMsg::TopLine { session: 1, line: 9, cause: TopLineCause::Attach };
        assert_eq!(postcard::to_allocvec(&top).unwrap()[0], 17);
        assert_eq!(postcard::to_allocvec(&ServerMsg::Zstd { data: vec![] }).unwrap()[0], 19);
        // v12 additions come after v11's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::Net { req: 1, request: String::new() }).unwrap()[0], 26);
        assert_eq!(postcard::to_allocvec(&ServerMsg::Net { req: 1, reply: String::new() }).unwrap()[0], 20);
        // v13 additions come after v12's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::StatPath { req: 1, session: None, cwd: None, candidates: vec![] }).unwrap()[0], 27);
        assert_eq!(postcard::to_allocvec(&ClientMsg::ReadFile { req: 1, path: String::new(), max_bytes: 1 }).unwrap()[0], 28);
        assert_eq!(postcard::to_allocvec(&ServerMsg::Cwd { session: 1, cwd: None }).unwrap()[0], 21);
        assert_eq!(postcard::to_allocvec(&ServerMsg::PathStat { req: 1, info: None }).unwrap()[0], 22);
        assert_eq!(postcard::to_allocvec(&ServerMsg::FileData { req: 1, path: String::new(), data: vec![] }).unwrap()[0], 23);
        // v14 additions come after v13's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::TakeSize { session: 1 }).unwrap()[0], 29);
        assert_eq!(postcard::to_allocvec(&ServerMsg::SizeControl { session: 1, size, controlling: true }).unwrap()[0], 24);
        // v15 additions come after v14's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::Rtt { rtt_us: 1000 }).unwrap()[0], 30);
        assert_eq!(postcard::to_allocvec(&ServerMsg::FlowWindow { window: StreamOpts::REMOTE.window }).unwrap()[0], 25);
        // v16 additions come after v15's.
        assert_eq!(postcard::to_allocvec(&ClientMsg::ListLayouts { req: 1 }).unwrap()[0], 31);
        assert_eq!(postcard::to_allocvec(&ClientMsg::ClaimLayoutId { req: 1, id: 2 }).unwrap()[0], 32);
        assert_eq!(postcard::to_allocvec(&ClientMsg::ReleaseLayout { id: 2, sessions: vec![1], json: String::new() }).unwrap()[0], 33);
        assert_eq!(postcard::to_allocvec(&ServerMsg::Layouts { req: 1, layouts: Vec::new() }).unwrap()[0], 26);
    }
}
