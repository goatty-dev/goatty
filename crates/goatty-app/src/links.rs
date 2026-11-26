//! `goatty://` deep links: parsing, GUI-to-GUI handoff, and UI navigation (#305).
//!
//! Security rule: a link **only navigates**. It never changes a setting, runs
//! or types a command, connects to a host, or adds a registry. Values extracted
//! from a link (setting keys, plugin ids, versions, registry names) are matched
//! against the settings index and the host's plugin catalog — never interpolated
//! into paths, commands, or shell code.
//!
//! Handoff (Linux/Windows, and CLI `goatty open` on every platform):
//! - Each GUI process listens on `<runtime_dir>/app-<pid>.sock` (mode 0600 in a
//!   0700 dir on Unix; a Named Pipe via `goatty_client::ipc` with a marker file
//!   on Windows).
//! - The process whose window gained focus most recently records its PID in
//!   `<runtime_dir>/app-active` off the UI thread.
//! - `goatty open <URL>` tries the active PID first, then any other live
//!   `app-*.sock` (cleaning up stale sockets), and falls back to launching the
//!   GUI with the link as an initial link.
//! - On macOS, `Application::on_open_urls` feeds the exact same [`LinkInbox`].

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use goatty_client::{LocalListener, LocalStream};
use gpui::{App, Global};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::session::{WakeListener, Waker};
use crate::settings_window::{self, Page};
use crate::toast::{self, Toast};

/// Fixed lowercase scheme for goatty links.
pub const SCHEME_PREFIX: &str = "goatty://";

/// Maximum accepted URL length in bytes (2 KiB).
pub const MAX_URL_LEN: usize = 2048;

/// Maximum IPC JSON request/reply line size in bytes.
const MAX_IPC_LINE: u64 = 4096;

/// Read/write timeout on the local GUI-to-GUI socket.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Toast shown when a syntactically valid `goatty://` link targets an unknown
/// target, page, setting key, or plugin.
pub const UNKNOWN_TOAST: &str = "goatty doesn't know this link. Maybe it needs an update.";

const SOCKET_PREFIX: &str = "app-";
const SOCKET_SUFFIX: &str = ".sock";
const ACTIVE_FILE: &str = "app-active";

/// A settings page segment in a `goatty://settings/<page>` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageTarget {
    Known(Page),
    Unknown(String),
}

/// A parsed `goatty://` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `goatty://settings`, `goatty://settings/<page>`, or
    /// `goatty://settings/<page>?setting=<dotted.key>`.
    Settings { page: Option<PageTarget>, setting: Option<String> },
    /// `goatty://plugins`: Settings → Plugins → Browse.
    PluginsBrowse,
    /// `goatty://plugins/<id>[?version=<v>&registry=<name>]`: install review, or
    /// the plugin's detail page when already installed.
    Plugin { id: String, version: Option<String>, registry: Option<String> },
    /// A syntactically valid `goatty://` link whose target this build doesn't
    /// recognize (e.g. a link meant for a newer goatty).
    Unknown { target: String },
}

/// Why a string could not be parsed as a `goatty://` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(pub String);

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LinkError {}

fn err(msg: impl Into<String>) -> LinkError {
    LinkError(msg.into())
}

/// Parse a `goatty://` link strictly.
///
/// The scheme (`goatty://`) is fixed and lowercase. Targets, paths, and keys
/// are normalized to lowercase. Syntactically valid links with an unknown
/// target, page, key, or plugin parse into [`Link`] so the GUI can land on the
/// closest page with [`UNKNOWN_TOAST`]; malformed or junk input returns `Err`.
pub fn parse(raw: &str) -> Result<Link, LinkError> {
    if raw.is_empty() {
        return Err(err("empty link"));
    }
    if raw.len() > MAX_URL_LEN {
        return Err(err(format!("link is too long ({} bytes; max {MAX_URL_LEN})", raw.len())));
    }
    if raw.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(err("link contains whitespace or control characters"));
    }
    let rest = raw.strip_prefix(SCHEME_PREFIX).ok_or_else(|| err("link must start with lowercase `goatty://`"))?;
    if rest.contains('#') {
        return Err(err("fragments (`#...`) aren't supported in goatty links"));
    }

    let (authority_and_path, query_str) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let (target_raw, path_raw) = match authority_and_path.split_once('/') {
        Some((t, p)) => (t, Some(p)),
        None => (authority_and_path, None),
    };

    if target_raw.is_empty() {
        return Err(err("missing target after `goatty://`"));
    }
    if target_raw.contains('@') || target_raw.contains(':') {
        return Err(err("userinfo and ports aren't allowed in goatty links"));
    }
    if !target_raw.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
        return Err(err(format!("invalid target {target_raw:?}")));
    }
    let target = target_raw.to_ascii_lowercase();

    let segments = parse_path(path_raw)?;
    let query = parse_query(query_str)?;

    match target.as_str() {
        "settings" => parse_settings(segments, query),
        "plugins" => parse_plugins(segments, query),
        _ => Ok(Link::Unknown { target }),
    }
}

fn parse_path(path_raw: Option<&str>) -> Result<Vec<String>, LinkError> {
    let Some(path) = path_raw else {
        return Ok(Vec::new());
    };
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() {
            return Err(err("empty path segment in link"));
        }
        let decoded = percent_decode(seg)?;
        if decoded.is_empty() || decoded == "." || decoded == ".." || decoded.contains(['/', '\\']) {
            return Err(err(format!("invalid path segment {seg:?}")));
        }
        segments.push(decoded);
    }
    Ok(segments)
}

fn parse_query(query_str: Option<&str>) -> Result<Vec<(String, String)>, LinkError> {
    let Some(q) = query_str else {
        return Ok(Vec::new());
    };
    if q.is_empty() {
        return Err(err("empty query string"));
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in q.split('&') {
        let Some((k_raw, v_raw)) = part.split_once('=') else {
            return Err(err(format!("query parameter {part:?} is missing `=`")));
        };
        let key = percent_decode(k_raw)?.to_ascii_lowercase();
        let value = percent_decode(v_raw)?;
        if key.is_empty() {
            return Err(err("empty query parameter name"));
        }
        if value.is_empty() {
            return Err(err(format!("empty value for query parameter `{key}`")));
        }
        if pairs.iter().any(|(existing, _)| existing == &key) {
            return Err(err(format!("duplicate query parameter `{key}`")));
        }
        pairs.push((key, value));
    }
    Ok(pairs)
}

fn percent_decode(input: &str) -> Result<String, LinkError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(err(format!("incomplete percent-encoding in {input:?}")));
            }
            let hi = hex_val(bytes[i + 1]).ok_or_else(|| err(format!("invalid percent-encoding in {input:?}")))?;
            let lo = hex_val(bytes[i + 2]).ok_or_else(|| err(format!("invalid percent-encoding in {input:?}")))?;
            let decoded = (hi << 4) | lo;
            if decoded < 0x20 || decoded == 0x7f {
                return Err(err("percent-encoded control character in link"));
            }
            out.push(decoded);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| err("invalid UTF-8 in percent-encoded link"))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_settings(segments: Vec<String>, query: Vec<(String, String)>) -> Result<Link, LinkError> {
    if segments.len() > 1 {
        return Err(err("settings link accepts at most one page segment"));
    }
    let page = match segments.into_iter().next() {
        Some(seg) => {
            if !seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
                return Err(err(format!("invalid settings page name {seg:?}")));
            }
            Some(match Page::from_name(&seg) {
                Some(p) => PageTarget::Known(p),
                None => PageTarget::Unknown(seg.to_ascii_lowercase()),
            })
        }
        None => None,
    };

    let mut setting = None;
    for (k, v) in query {
        match k.as_str() {
            "setting" => {
                let norm = v.to_ascii_lowercase();
                if norm.starts_with('.')
                    || norm.ends_with('.')
                    || norm.contains("..")
                    || !norm.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                {
                    return Err(err(format!("invalid setting key {v:?}")));
                }
                setting = Some(norm);
            }
            other => return Err(err(format!("unknown query parameter `{other}` for settings link"))),
        }
    }

    Ok(Link::Settings { page, setting })
}

fn parse_plugins(segments: Vec<String>, query: Vec<(String, String)>) -> Result<Link, LinkError> {
    if segments.len() > 1 {
        return Err(err("plugins link accepts at most one plugin id segment"));
    }
    let Some(id_raw) = segments.into_iter().next() else {
        if !query.is_empty() {
            return Err(err("query parameters on `goatty://plugins` require a plugin id"));
        }
        return Ok(Link::PluginsBrowse);
    };

    let id = id_raw.to_ascii_lowercase();
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
        return Err(err(format!("invalid plugin id {id_raw:?}")));
    }

    let mut version = None;
    let mut registry = None;
    for (k, v) in query {
        match k.as_str() {
            "version" => {
                if !v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_')) {
                    return Err(err(format!("invalid plugin version {v:?}")));
                }
                version = Some(v);
            }
            "registry" => {
                let norm = v.to_ascii_lowercase();
                if !norm.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
                    return Err(err(format!("invalid registry name {v:?}")));
                }
                registry = Some(norm);
            }
            other => return Err(err(format!("unknown query parameter `{other}` for plugins link"))),
        }
    }

    Ok(Link::Plugin { id, version, registry })
}

// ---- Inbox + UI waking ------------------------------------------------------

/// Thread-safe queue of incoming links, waking the UI thread via [`Waker`].
pub struct LinkInbox {
    queue: Mutex<VecDeque<Link>>,
    waker: Waker,
}

impl LinkInbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { queue: Mutex::new(VecDeque::new()), waker: Waker::default() })
    }

    /// Push a link from any thread (the socket listener or macOS `on_open_urls`)
    /// and wake the UI thread.
    pub fn push(&self, link: Link) {
        self.queue.lock().push_back(link);
        self.waker.wake();
    }

    /// Take every queued link in arrival order.
    pub fn drain(&self) -> Vec<Link> {
        self.queue.lock().drain(..).collect()
    }

    /// Subscribe the UI thread to wake-ups when links arrive.
    pub(crate) fn subscribe(&self) -> (Arc<WakeListener>, flume::Receiver<()>) {
        self.waker.subscribe()
    }
}

// ---- Wire protocol ----------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct OpenRequest {
    open: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenReply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---- Socket paths & `app-active` --------------------------------------------

pub fn socket_path_in(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{SOCKET_PREFIX}{pid}{SOCKET_SUFFIX}"))
}

pub fn active_path_in(dir: &Path) -> PathBuf {
    dir.join(ACTIVE_FILE)
}

fn parse_socket_pid(name: &str) -> Option<u32> {
    let mid = name.strip_prefix(SOCKET_PREFIX)?.strip_suffix(SOCKET_SUFFIX)?;
    if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    mid.parse().ok()
}

fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically write `pid` to `<dir>/app-active`.
pub fn write_active_in(dir: &Path, pid: u32) -> io::Result<()> {
    ensure_runtime_dir(dir)?;
    let target = active_path_in(dir);
    let tmp = dir.join(format!("{ACTIVE_FILE}.{pid}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        writeln!(f, "{pid}")?;
        f.flush()?;
    }
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

fn read_active_in(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(active_path_in(dir)).ok()?.trim().parse().ok()
}

fn remove_active_if_pid(dir: &Path, pid: u32) {
    if read_active_in(dir) == Some(pid) {
        let _ = std::fs::remove_file(active_path_in(dir));
    }
}

// ---- Off-UI active-PID writer -----------------------------------------------

/// Global handle for updating `app-active` off the UI thread whenever a window
/// gains focus.
struct ActiveWriter {
    tx: mpsc::Sender<()>,
}

impl Global for ActiveWriter {}

/// Start the background `app-active` writer and record this process as active.
pub fn init_active_writer(cx: &mut App) {
    let dir = goatty_proto::runtime_dir();
    let pid = std::process::id();
    let (tx, rx) = mpsc::channel::<()>();
    let _ = std::thread::Builder::new().name("app-active".into()).spawn(move || {
        while rx.recv().is_ok() {
            // Coalesce bursts of focus events into one write.
            while rx.try_recv().is_ok() {}
            if let Err(e) = write_active_in(&dir, pid) {
                log::debug!("writing app-active: {e}");
            }
        }
    });
    let _ = tx.send(());
    cx.set_global(ActiveWriter { tx });
}

/// Record this process as the most recently focused goatty GUI, off the UI thread.
pub fn mark_active(cx: &App) {
    if let Some(w) = cx.try_global::<ActiveWriter>() {
        let _ = w.tx.send(());
    }
}

// ---- Per-process app socket listener ----------------------------------------

/// Listening end of `<runtime_dir>/app-<pid>.sock`. Dropping stops the
/// listener thread and removes the socket file.
pub struct AppListener {
    dir: PathBuf,
    pid: u32,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AppListener {
    /// Bind the app socket for this process in `goatty_proto::runtime_dir()`.
    pub fn bind(inbox: Arc<LinkInbox>) -> Result<Self> {
        Self::bind_in(&goatty_proto::runtime_dir(), std::process::id(), inbox)
    }

    /// Bind an app socket for `pid` inside `dir` (sandboxable for tests).
    pub fn bind_in(dir: &Path, pid: u32, inbox: Arc<LinkInbox>) -> Result<Self> {
        ensure_runtime_dir(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = socket_path_in(dir, pid);
        #[cfg(unix)]
        {
            if path.exists() {
                if LocalStream::connect(&path).is_ok() {
                    bail!("app socket {} is already active", path.display());
                }
                let _ = std::fs::remove_file(&path);
            }
        }
        let listener = LocalListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", path.display()))?;
        }
        #[cfg(windows)]
        {
            // On Windows `LocalListener` uses a Named Pipe derived from `path`
            // and doesn't create a file in `dir`. Keep a zero-byte marker file
            // so `send_to_running` can enumerate live instances identically.
            let _ = std::fs::File::create(&path);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("link-listener-{pid}"))
            .spawn(move || accept_loop(listener, inbox, stop_loop))
            .context("spawn link-listener thread")?;

        Ok(Self { dir: dir.to_path_buf(), pid, path, stop, thread: Some(thread) })
    }
}

impl Drop for AppListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unblock `accept()` cleanly on both Unix sockets and Windows Named Pipes.
        let _ = LocalStream::connect(&self.path);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
        remove_active_if_pid(&self.dir, self.pid);
    }
}

fn accept_loop(listener: LocalListener, inbox: Arc<LinkInbox>, stop: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                log::debug!("app socket accept: {e}");
                continue;
            }
        };
        if let Err(e) = handle_client(stream, &inbox) {
            log::debug!("app socket request failed: {e:#}");
        }
    }
}

fn handle_client(mut stream: LocalStream, inbox: &LinkInbox) -> Result<()> {
    let _ = stream.set_read_timeout(Some(IPC_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IPC_TIMEOUT));
    let mut line = String::new();
    let n = BufReader::new((&mut stream).take(MAX_IPC_LINE)).read_line(&mut line)?;
    if n == 0 {
        return Ok(());
    }
    let reply = match serde_json::from_str::<OpenRequest>(line.trim_end_matches(['\r', '\n'])) {
        Ok(req) => match parse(&req.open) {
            Ok(link) => {
                inbox.push(link);
                OpenReply { ok: true, error: None }
            }
            Err(e) => OpenReply { ok: false, error: Some(e.to_string()) },
        },
        Err(e) => OpenReply { ok: false, error: Some(format!("invalid request: {e}")) },
    };
    let mut encoded = serde_json::to_string(&reply)?;
    encoded.push('\n');
    stream.write_all(encoded.as_bytes())?;
    stream.flush()?;
    Ok(())
}

// ---- Sending a link to a running GUI process --------------------------------

/// Outcome of trying to hand a URL to a running goatty GUI process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Delivered to a running GUI process.
    Delivered,
    /// No running GUI process was reachable.
    NoRunningApp,
}

/// Try to send `url` to the most recently focused running goatty GUI process
/// (or any other live one), cleaning up stale sockets along the way.
pub fn send_to_running(url: &str) -> Result<SendOutcome> {
    send_to_running_in(&goatty_proto::runtime_dir(), url, std::process::id())
}

/// [`send_to_running`] scoped to `dir` (for sandboxed tests). `self_pid` is
/// skipped so a process never sends to itself.
pub fn send_to_running_in(dir: &Path, url: &str, self_pid: u32) -> Result<SendOutcome> {
    parse(url)?;
    if !dir.exists() {
        return Ok(SendOutcome::NoRunningApp);
    }

    let active_pid = read_active_in(dir).filter(|&p| p != self_pid);
    let mut candidates: Vec<(u32, PathBuf)> = Vec::new();
    let mut active_missing = false;

    if let Some(pid) = active_pid {
        let p = socket_path_in(dir, pid);
        if p.exists() {
            candidates.push((pid, p));
        } else {
            active_missing = true;
        }
    }

    let mut others: Vec<(SystemTime, u32, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            let Some(pid) = parse_socket_pid(&name) else { continue };
            if pid == self_pid || Some(pid) == active_pid {
                continue;
            }
            let path = entry.path();
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            others.push((mtime, pid, path));
        }
    }
    others.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    candidates.extend(others.into_iter().map(|(_, pid, path)| (pid, path)));

    for (pid, path) in candidates {
        match send_one(&path, url) {
            SendAttempt::Delivered => return Ok(SendOutcome::Delivered),
            SendAttempt::Rejected(msg) => bail!("{msg}"),
            SendAttempt::Stale => {
                let _ = std::fs::remove_file(&path);
                if Some(pid) == active_pid {
                    remove_active_if_pid(dir, pid);
                }
            }
            SendAttempt::Unreachable(e) => {
                log::debug!("skipping {}: {e}", path.display());
            }
        }
    }

    if active_missing && let Some(pid) = active_pid {
        remove_active_if_pid(dir, pid);
    }

    Ok(SendOutcome::NoRunningApp)
}

enum SendAttempt {
    Delivered,
    Rejected(String),
    Stale,
    Unreachable(anyhow::Error),
}

fn is_stale_connect_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound | io::ErrorKind::ConnectionReset | io::ErrorKind::InvalidInput
    )
}

fn send_one(path: &Path, url: &str) -> SendAttempt {
    let mut stream = match LocalStream::connect(path) {
        Ok(s) => s,
        Err(e) if is_stale_connect_error(&e) => return SendAttempt::Stale,
        Err(e) => return SendAttempt::Unreachable(e.into()),
    };
    if let Err(e) = stream.peer_owner_pid(path) {
        return SendAttempt::Unreachable(e);
    }
    let _ = stream.set_read_timeout(Some(IPC_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IPC_TIMEOUT));

    let mut payload = match serde_json::to_string(&OpenRequest { open: url.to_owned() }) {
        Ok(s) => s,
        Err(e) => return SendAttempt::Unreachable(e.into()),
    };
    payload.push('\n');
    if let Err(e) = stream.write_all(payload.as_bytes()).and_then(|()| stream.flush()) {
        return SendAttempt::Unreachable(e.into());
    }

    let mut line = String::new();
    match BufReader::new((&mut stream).take(MAX_IPC_LINE)).read_line(&mut line) {
        Ok(0) => SendAttempt::Unreachable(anyhow::anyhow!("running goatty closed the connection without replying")),
        Ok(_) => match serde_json::from_str::<OpenReply>(line.trim_end_matches(['\r', '\n'])) {
            Ok(reply) if reply.ok => SendAttempt::Delivered,
            Ok(reply) => SendAttempt::Rejected(reply.error.unwrap_or_else(|| "link rejected".into())),
            Err(e) => SendAttempt::Unreachable(e.into()),
        },
        Err(e) => SendAttempt::Unreachable(e.into()),
    }
}

// ---- Applying a Link on the UI thread ---------------------------------------

/// Apply `link` on the UI thread: raise the app and navigate to the target
/// page, row, or plugin review/detail.
pub fn apply(cx: &mut App, link: Link) {
    cx.activate(true);
    match link {
        Link::Settings { page, setting } => settings_window::open_link_settings(cx, page, setting),
        Link::PluginsBrowse => settings_window::open_link_plugins_browse(cx),
        Link::Plugin { id, version, registry } => settings_window::open_link_plugin(cx, id, version, registry),
        Link::Unknown { .. } => {
            activate_any_window(cx);
            toast::show(cx, Toast::info(UNKNOWN_TOAST).key("links.unknown"));
        }
    }
}

fn activate_any_window(cx: &mut App) {
    if let Some(handle) = cx.try_global::<crate::hosts::LastWorkspace>().map(|w| w.0)
        && handle.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }
    if let Some(handle) = cx.windows().into_iter().next() {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_documented_links() {
        assert_eq!(parse("goatty://settings"), Ok(Link::Settings { page: None, setting: None }));
        assert_eq!(parse("goatty://settings/"), Ok(Link::Settings { page: None, setting: None }));
        assert_eq!(
            parse("goatty://settings/terminal"),
            Ok(Link::Settings { page: Some(PageTarget::Known(Page::Terminal)), setting: None })
        );
        assert_eq!(parse("goatty://settings/ssh"), Ok(Link::Settings { page: Some(PageTarget::Known(Page::Hosts)), setting: None }));
        assert_eq!(
            parse("goatty://settings/terminal?setting=links.open_on"),
            Ok(Link::Settings { page: Some(PageTarget::Known(Page::Terminal)), setting: Some("links.open_on".into()) })
        );
        assert_eq!(parse("goatty://settings?setting=font.size"), Ok(Link::Settings { page: None, setting: Some("font.size".into()) }));
        assert_eq!(parse("goatty://plugins"), Ok(Link::PluginsBrowse));
        assert_eq!(parse("goatty://plugins/"), Ok(Link::PluginsBrowse));
        assert_eq!(parse("goatty://plugins/eza"), Ok(Link::Plugin { id: "eza".into(), version: None, registry: None }));
        assert_eq!(
            parse("goatty://plugins/eza?version=1.2.3&registry=official"),
            Ok(Link::Plugin { id: "eza".into(), version: Some("1.2.3".into()), registry: Some("official".into()) })
        );
        assert_eq!(
            parse("goatty://plugins/eza?version=1.2.3+build.1"),
            Ok(Link::Plugin { id: "eza".into(), version: Some("1.2.3+build.1".into()), registry: None })
        );
    }

    #[test]
    fn case_insensitivity_and_aliases() {
        // Scheme must be lowercase.
        assert!(parse("GOATTY://settings").is_err());
        assert!(parse("Goatty://settings").is_err());

        // Targets, paths, aliases, query keys, setting keys, plugin ids, registry names are case-insensitive.
        assert_eq!(
            parse("goatty://SETTINGS/SSH?SETTING=SSH.Open_In"),
            Ok(Link::Settings { page: Some(PageTarget::Known(Page::Hosts)), setting: Some("ssh.open_in".into()) })
        );
        assert_eq!(
            parse("goatty://Settings/Keybindings"),
            Ok(Link::Settings { page: Some(PageTarget::Known(Page::Shortcuts)), setting: None })
        );
        assert_eq!(parse("goatty://Settings/Tailnet"), Ok(Link::Settings { page: Some(PageTarget::Known(Page::Network)), setting: None }));
        assert_eq!(
            parse("goatty://PLUGINS/FZF-Tab?VERSION=2.0.0-RC1&REGISTRY=Community"),
            Ok(Link::Plugin { id: "fzf-tab".into(), version: Some("2.0.0-RC1".into()), registry: Some("community".into()) })
        );
        // Percent-encoded characters work.
        assert_eq!(
            parse("goatty://settings/terminal?setting=links%2Eopen_on"),
            Ok(Link::Settings { page: Some(PageTarget::Known(Page::Terminal)), setting: Some("links.open_on".into()) })
        );
    }

    #[test]
    fn unknown_targets_and_pages_parse_for_graceful_fallback() {
        assert_eq!(parse("goatty://pair?code=goatty-pair:abc"), Ok(Link::Unknown { target: "pair".into() }));
        assert_eq!(parse("goatty://theme?name=nord"), Ok(Link::Unknown { target: "theme".into() }));
        assert_eq!(parse("goatty://future/a/b"), Ok(Link::Unknown { target: "future".into() }));
        assert_eq!(
            parse("goatty://settings/future-page"),
            Ok(Link::Settings { page: Some(PageTarget::Unknown("future-page".into())), setting: None })
        );
        assert_eq!(
            parse("goatty://settings/terminal?setting=future.key"),
            Ok(Link::Settings { page: Some(PageTarget::Known(Page::Terminal)), setting: Some("future.key".into()) })
        );
        assert_eq!(
            parse("goatty://plugins/does-not-exist"),
            Ok(Link::Plugin { id: "does-not-exist".into(), version: None, registry: None })
        );
    }

    #[test]
    fn rejects_length_cap_and_junk() {
        assert!(parse("").is_err());
        assert!(parse("http://settings").is_err());
        assert!(parse("goatty:settings").is_err());
        assert!(parse("goatty://").is_err());
        assert!(parse("goatty:///settings").is_err());
        assert!(parse("goatty://?setting=a").is_err());
        assert!(parse("goatty://user@settings").is_err());
        assert!(parse("goatty://settings:8080").is_err());
        assert!(parse("goatty://settings/terminal#frag").is_err());
        assert!(parse("goatty://settings/terminal/extra").is_err());
        assert!(parse("goatty://settings//terminal").is_err());
        assert!(parse("goatty://settings/..").is_err());
        assert!(parse("goatty://settings/%2e%2e").is_err());
        assert!(parse("goatty://settings/foo%2fbar").is_err());
        assert!(parse("goatty://settings?").is_err());
        assert!(parse("goatty://settings?setting").is_err());
        assert!(parse("goatty://settings?setting=").is_err());
        assert!(parse("goatty://settings?setting=a&setting=b").is_err());
        assert!(parse("goatty://settings?unknown=1").is_err());
        assert!(parse("goatty://settings/terminal?setting=.bad").is_err());
        assert!(parse("goatty://settings/terminal?setting=bad..key").is_err());
        assert!(parse("goatty://plugins?version=1.0").is_err());
        assert!(parse("goatty://plugins/a/b").is_err());
        assert!(parse("goatty://plugins/eza?foo=bar").is_err());
        assert!(parse("goatty://plugins/eza?version=1;rm").is_err());
        assert!(parse("goatty://settings/terminal\n").is_err());
        assert!(parse("goatty://settings/terminal ").is_err());
        assert!(parse("goatty://settings/%00").is_err());
        assert!(parse("goatty://settings/%ZZ").is_err());
        assert!(parse("goatty://settings/%2").is_err());

        // Length cap (2048 bytes).
        let base = "goatty://settings/terminal?setting=";
        let at_cap = format!("{base}{}", "a".repeat(MAX_URL_LEN - base.len()));
        assert_eq!(at_cap.len(), MAX_URL_LEN);
        assert!(parse(&at_cap).is_ok());

        let over_cap = format!("{at_cap}a");
        assert_eq!(over_cap.len(), MAX_URL_LEN + 1);
        assert!(parse(&over_cap).is_err());
    }

    #[test]
    fn ipc_prefers_active_process_falls_back_and_cleans_stale_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let inbox1 = LinkInbox::new();
        let inbox2 = LinkInbox::new();
        let (w1, rx1) = inbox1.subscribe();
        let (w2, rx2) = inbox2.subscribe();
        // Drain initial subscription wake-ups and re-arm the coalescing wakers.
        let _ = rx1.try_recv();
        let _ = rx2.try_recv();
        w1.clear();
        w2.clear();

        let l1 = AppListener::bind_in(dir.path(), 1001, inbox1.clone()).unwrap();
        let _l2 = AppListener::bind_in(dir.path(), 1002, inbox2.clone()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
            let sock_mode = std::fs::metadata(socket_path_in(dir.path(), 1001)).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(sock_mode, 0o600);
        }

        // Mark 1002 active: delivery goes to 1002.
        write_active_in(dir.path(), 1002).unwrap();
        assert_eq!(send_to_running_in(dir.path(), "goatty://settings/terminal", 9999).unwrap(), SendOutcome::Delivered);
        assert!(rx2.recv_timeout(Duration::from_secs(2)).is_ok());
        assert_eq!(inbox2.drain(), vec![Link::Settings { page: Some(PageTarget::Known(Page::Terminal)), setting: None }]);
        assert!(inbox1.drain().is_empty());

        // Switch focus to 1001: delivery goes to 1001.
        write_active_in(dir.path(), 1001).unwrap();
        assert_eq!(send_to_running_in(dir.path(), "goatty://plugins/eza", 9999).unwrap(), SendOutcome::Delivered);
        assert!(rx1.recv_timeout(Duration::from_secs(2)).is_ok());
        assert_eq!(inbox1.drain(), vec![Link::Plugin { id: "eza".into(), version: None, registry: None }]);
        assert!(inbox2.drain().is_empty());

        // Create a stale socket for a dead process (e.g. crashed without Drop),
        // and point app-active at it.
        let stale_path = socket_path_in(dir.path(), 5555);
        #[cfg(unix)]
        {
            let dead_listener = std::os::unix::net::UnixListener::bind(&stale_path).unwrap();
            drop(dead_listener);
        }
        #[cfg(windows)]
        {
            std::fs::File::create(&stale_path).unwrap();
        }
        assert!(stale_path.exists());
        write_active_in(dir.path(), 5555).unwrap();

        // Sending cleans up the stale socket and falls back to a live listener.
        assert_eq!(send_to_running_in(dir.path(), "goatty://plugins", 9999).unwrap(), SendOutcome::Delivered);
        assert!(!stale_path.exists(), "stale socket should be removed");
        let got: Vec<Link> = inbox1.drain().into_iter().chain(inbox2.drain()).collect();
        assert_eq!(got, vec![Link::PluginsBrowse]);

        // Drop listener 1: its socket is removed and delivery reaches listener 2.
        drop(l1);
        assert!(!socket_path_in(dir.path(), 1001).exists());
        assert_eq!(send_to_running_in(dir.path(), "goatty://settings", 9999).unwrap(), SendOutcome::Delivered);
        assert_eq!(inbox2.drain(), vec![Link::Settings { page: None, setting: None }]);
    }

    #[test]
    fn ipc_returns_no_running_app_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(send_to_running_in(dir.path(), "goatty://settings", 9999).unwrap(), SendOutcome::NoRunningApp);
    }

    #[test]
    fn ipc_rejects_invalid_url_before_touching_sockets() {
        let dir = tempfile::tempdir().unwrap();
        assert!(send_to_running_in(dir.path(), "http://example.com", 9999).is_err());
    }
}
