//! What programs say about themselves: OSC 7501, the Program Status
//! Protocol (coding agents like Claude Code, Codex, OpenCode and
//! Antigravity use it to say they're working, waiting for the user, done or
//! failed), and the ConEmu progress report (`OSC 9 ; 4`).
//!
//! Wire format: `ESC ] 7501 ; key=value[:key=value…] ST`, or the probe
//! `ESC ] 7501 ; ? ST`, which the terminal that answers queries (the
//! daemon's) answers with the states and kinds it knows. Agents send the
//! probe followed by `CSI c` and take the protocol as supported only if
//! the answer comes before the DA1 reply; `Terminal::feed` applies the
//! probe where it ends in the stream, so its answer is queued first.
//!
//! Keys: `state` (required: `idle`, `working`, `blocked`, `done`, `error`
//! or `clear`), `kind` (`permission`, `question`, `auth`; with `blocked`),
//! `progress` (`0..=100`; with `working` and `blocked`), `id` (a path of up
//! to 8 segments; absent: the root record), `app` (which program; inherited
//! from the nearest ancestor when absent), `title` and `msg` (base64 UTF-8).
//! Each report replaces the record its `id` names; `state=clear` removes it
//! and its children (all records without an `id`). A report that breaks a
//! limit (sizes, encodings, control characters) is dropped whole.
//!
//! Lifetime: a new prompt (OSC 133;A) drops `idle`, `working` and
//! `blocked` records and keeps `done` and `error` ones until they're
//! cleared or replaced (the UI hides those once the user saw them); a full
//! reset (RIS) drops everything. Snapshots carry the records with their
//! sequence numbers (`goatty=restore.<seq>`, so replaying them neither
//! looks like news nor like a newer report than the user saw).
//!
//! News: a program's own report that makes the record deserving the most
//! attention (blocked > error > working > done, idle aside) blocked, done
//! or failed, unless that's what it already was. It's found per report, in
//! stream order, and travels with its [`UiEvent::Status`](crate::UiEvent),
//! so a quick `working` → `done` between two frames still counts.
//!
//! Text from programs is checked here (valid UTF-8, no control
//! characters, bidi and zero-width characters removed) but is still only
//! ever shown as plain text, and never written back to the PTY.

use std::fmt::Write as _;

/// The longest OSC 7501 sequence, introducer and terminator included.
const MAX_SEQUENCE: usize = 4096;
/// The longest payload after `7501;`: the sequence less `ESC ] 7501 ;`
/// and a two-byte `ST`.
pub(crate) const MAX_PAYLOAD: usize = MAX_SEQUENCE - 2 - 5 - 2;
const MAX_KEY: usize = 16;
const MAX_TITLE_B64: usize = 256;
const MAX_TITLE: usize = 192;
const MAX_MSG_B64: usize = 2732;
const MAX_MSG: usize = 2048;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_SEGMENT: usize = 32;
const MAX_SEGMENTS: usize = 8;
/// Records kept per terminal; the least recently reported one goes first.
pub(crate) const MAX_RECORDS: usize = 128;

/// The answer to `OSC 7501 ; ?`.
pub(crate) const PROBE_REPLY: &[u8] = b"\x1b]7501;?:states=idle,working,blocked,done,error:kinds=permission,question,auth\x1b\\";

/// What a program says it's doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// Running, nothing to do (waiting for a new task).
    Idle,
    /// Busy (with `progress` when it knows how far along).
    Working,
    /// Waiting for the user: a permission, an answer, a login.
    Blocked,
    /// Finished its task.
    Done,
    /// Failed.
    Error,
}

impl State {
    /// How much it deserves attention: blocked, error, working, done, idle.
    pub fn priority(self) -> u8 {
        match self {
            State::Blocked => 4,
            State::Error => 3,
            State::Working => 2,
            State::Done => 1,
            State::Idle => 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
            State::Done => "done",
            State::Error => "error",
        }
    }
}

/// What a blocked program waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockedKind {
    /// Permission to do something (run a command, edit a file).
    Permission,
    /// An answer to a question.
    Question,
    /// A login.
    Auth,
}

impl BlockedKind {
    fn name(self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }
}

/// One OSC 7501 record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Its path (`agent/build`); empty for the root record.
    pub id: String,
    /// The program (`claude-code`, `codex`, …), its own or inherited.
    pub app: Option<String>,
    pub state: State,
    /// What it waits for, when blocked.
    pub kind: Option<BlockedKind>,
    /// How far along, `0..=100`, when working or blocked.
    pub progress: Option<u8>,
    /// A short headline (plain text, checked).
    pub title: Option<String>,
    /// A longer message (plain text, checked).
    pub msg: Option<String>,
    /// When it was last reported: larger is newer, across the terminal's
    /// records (and its progress report).
    pub seq: u64,
}

impl Record {
    /// Which record, in which state: what tells news apart.
    fn key(&self) -> (String, State, Option<BlockedKind>) {
        (self.id.clone(), self.state, self.kind)
    }
}

/// `OSC 9 ; 4` progress states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    Normal,
    Error,
    Indeterminate,
    Paused,
}

/// A progress report (`OSC 9 ; 4`): a percentage unless indeterminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    pub percent: Option<u8>,
}

/// Everything a terminal's programs said about themselves; see
/// [`crate::Terminal::program_status`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramStatus {
    /// Oldest report first.
    pub records: Vec<Record>,
    /// The last `OSC 9 ; 4` report, unless it removed the bar.
    pub progress: Option<Progress>,
    /// The newest `seq` so far (counts reports, also removed ones).
    pub latest: u64,
}

impl ProgramStatus {
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.progress.is_none()
    }

    /// The program reporting: the root record's `app`, else the newest
    /// record's that has one.
    pub fn app(&self) -> Option<&str> {
        let root = self.records.iter().find(|r| r.id.is_empty()).and_then(|r| r.app.as_deref());
        root.or_else(|| self.records.iter().rev().find_map(|r| r.app.as_deref()))
    }

    /// The record that deserves the most attention among those `keep`
    /// accepts: by [`State::priority`], then the newest.
    pub fn top(&self, keep: impl Fn(&Record) -> bool) -> Option<&Record> {
        self.records.iter().filter(|r| keep(r)).max_by_key(|r| (r.state.priority(), r.seq))
    }
}

/// A parsed OSC 7501 sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Report {
    /// `?`: does the terminal support it?
    Probe,
    /// `state=clear`: the record `id` and its children, or everything.
    /// `restore`: goatty's snapshot, with the newest sequence number so far.
    Clear { id: Option<String>, restore: Option<u64> },
    /// A record to set. `restore`: goatty's snapshot replaying it, with its
    /// sequence number (0: unknown).
    Set { record: Record, restore: Option<u64> },
}

/// What applying a report did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    Unchanged,
    Changed,
    /// Changed, and this record is news (see the module docs).
    News(Record),
}

/// Parse the payload after `7501;`. `None`: malformed or over a limit
/// (the whole report is dropped).
pub(crate) fn parse(payload: &[u8]) -> Option<Report> {
    if payload.first() == Some(&b'?') {
        return Some(Report::Probe);
    }
    if payload.is_empty() || payload.len() > MAX_PAYLOAD {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    let mut fields: Vec<(&str, &str)> = Vec::new();
    for pair in text.split(':') {
        let (key, value) = pair.split_once('=')?;
        if key.is_empty() || key.len() > MAX_KEY || fields.iter().any(|(k, _)| *k == key) {
            return None;
        }
        fields.push((key, value));
    }
    let get = |key: &str| fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    // goatty's own snapshots: `goatty=restore[.<seq>]`.
    let restore =
        get("goatty").and_then(|v| v.strip_prefix("restore")).map(|seq| seq.strip_prefix('.').and_then(|n| n.parse().ok()).unwrap_or(0));
    let id = match get("id") {
        Some(id) => Some(valid_id(id)?.to_string()),
        None => None,
    };
    let state = match get("state")? {
        "clear" => return Some(Report::Clear { id, restore }),
        "idle" => State::Idle,
        "working" => State::Working,
        "blocked" => State::Blocked,
        "done" => State::Done,
        "error" => State::Error,
        _ => return None,
    };
    let app = match get("app") {
        Some(app) if valid_app(app) => Some(app.to_string()),
        Some(_) => return None,
        None => None,
    };
    // A kind this goatty doesn't know yet is still "blocked".
    let kind = match (state, get("kind")) {
        (State::Blocked, Some("permission")) => Some(BlockedKind::Permission),
        (State::Blocked, Some("question")) => Some(BlockedKind::Question),
        (State::Blocked, Some("auth")) => Some(BlockedKind::Auth),
        _ => None,
    };
    let progress = match get("progress") {
        Some(p) => {
            let n: u8 = (p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok()).flatten()?;
            if n > 100 {
                return None;
            }
            matches!(state, State::Working | State::Blocked).then_some(n)
        }
        None => None,
    };
    let title = match get("title") {
        Some(t) => text_field(t, MAX_TITLE_B64, MAX_TITLE)?,
        None => None,
    };
    let msg = match get("msg") {
        Some(m) => text_field(m, MAX_MSG_B64, MAX_MSG)?,
        None => None,
    };
    let record = Record { id: id.unwrap_or_default(), app, state, kind, progress, title, msg, seq: 0 };
    Some(Report::Set { record, restore })
}

/// `[A-Za-z0-9_.+-]{1,32}`.
fn valid_app(app: &str) -> bool {
    (1..=MAX_APP).contains(&app.len()) && app.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// `segment(/segment)*`: up to 8 segments of 1 to 32 visible ASCII bytes,
/// 128 bytes in all.
fn valid_id(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID
        && id.split('/').count() <= MAX_SEGMENTS
        && id.split('/').all(|s| (1..=MAX_SEGMENT).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic() && b != b';'));
    ok.then_some(id)
}

/// A base64 text field: `None` when it breaks a limit, `Some(None)` when
/// empty after cleaning.
fn text_field(b64: &str, max_b64: usize, max: usize) -> Option<Option<String>> {
    if b64.len() > max_b64 {
        return None;
    }
    let bytes = base64_decode(b64.as_bytes())?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    let clean: String = text.chars().filter(|&c| !invisible(c)).collect();
    let clean = clean.trim();
    Some((!clean.is_empty()).then(|| clean.to_string()))
}

/// Bidi controls and zero-width characters: they'd reorder or hide the
/// text around them.
fn invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{061c}' | '\u{180e}' | '\u{feff}')
}

/// Standard base64, padding optional; `None` on anything else.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let input = match input.iter().position(|&b| b == b'=') {
        Some(p) if input[p..].iter().all(|&b| b == b'=') && input.len() - p <= 2 && input.len().is_multiple_of(4) => &input[..p],
        Some(_) => return None,
        None => input,
    };
    if input.len() % 4 == 1 {
        return None;
    }
    let value = |b: u8| -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for chunk in input.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            n |= value(b)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

fn base64_encode(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
}

/// A terminal's records and progress report.
#[derive(Debug, Default)]
pub(crate) struct Store {
    /// Oldest report first.
    records: Vec<Record>,
    progress: Option<Progress>,
    seq: u64,
}

impl Store {
    pub(crate) fn status(&self) -> ProgramStatus {
        ProgramStatus { records: self.records.clone(), progress: self.progress, latest: self.seq }
    }

    /// Apply a report.
    pub(crate) fn apply(&mut self, report: Report) -> Applied {
        let changed = match report {
            Report::Probe => false,
            Report::Clear { id, restore } => {
                if let Some(seq) = restore {
                    self.seq = self.seq.max(seq);
                }
                let before = self.records.len();
                match id {
                    Some(id) => self.records.retain(|r| !within(&r.id, &id)),
                    None => self.records.clear(),
                }
                before != self.records.len()
            }
            Report::Set { mut record, restore } => {
                let before = self.notable().map(Record::key);
                if record.app.is_none() {
                    record.app = self.inherited_app(&record.id);
                }
                self.records.retain(|r| r.id != record.id);
                record.seq = match restore {
                    Some(seq) if seq > 0 => seq,
                    _ => self.seq + 1,
                };
                self.seq = self.seq.max(record.seq);
                self.records.push(record);
                if self.records.len() > MAX_RECORDS {
                    self.records.remove(0);
                }
                let news = self
                    .notable()
                    .filter(|r| restore.is_none() && matches!(r.state, State::Blocked | State::Done | State::Error))
                    .filter(|r| Some(r.key()) != before);
                return match news {
                    Some(r) => Applied::News(r.clone()),
                    None => Applied::Changed,
                };
            }
        };
        if changed { Applied::Changed } else { Applied::Unchanged }
    }

    /// The record deserving the most attention, idle ones aside.
    fn notable(&self) -> Option<&Record> {
        self.records.iter().filter(|r| r.state != State::Idle).max_by_key(|r| (r.state.priority(), r.seq))
    }

    /// The `app` of the nearest ancestor of `id` that has one (the root is
    /// every record's ancestor).
    fn inherited_app(&self, id: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        let mut path = id;
        loop {
            path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if let Some(app) = self.records.iter().find(|r| r.id == path).and_then(|r| r.app.clone()) {
                return Some(app);
            }
            if path.is_empty() {
                return None;
            }
        }
    }

    /// A new prompt: the program that reported is gone. Keep only what it
    /// finished with (`done`, `error`); the progress bar goes too.
    pub(crate) fn prompt(&mut self) -> bool {
        let before = self.records.len();
        self.records.retain(|r| matches!(r.state, State::Done | State::Error));
        let changed = before != self.records.len() || self.progress.is_some();
        self.progress = None;
        changed
    }

    /// RIS: everything goes.
    pub(crate) fn reset(&mut self) -> bool {
        let changed = !self.records.is_empty() || self.progress.is_some();
        self.records.clear();
        self.progress = None;
        changed
    }

    pub(crate) fn set_progress(&mut self, progress: Option<Progress>) -> bool {
        if self.progress == progress {
            return false;
        }
        self.seq += 1;
        self.progress = progress;
        true
    }

    /// The records and progress as sequences that rebuild them on a client
    /// terminal, with their sequence numbers. `replace`: the client may have
    /// a progress bar already (a resync); its records are cleared anyway.
    pub(crate) fn write_snapshot(&self, replace: bool, out: &mut String) {
        let _ = write!(out, "\x1b]7501;state=clear:goatty=restore.{}\x1b\\", self.seq);
        for r in &self.records {
            let _ = write!(out, "\x1b]7501;state={}", r.state.name());
            if !r.id.is_empty() {
                let _ = write!(out, ":id={}", r.id);
            }
            if let Some(app) = &r.app {
                let _ = write!(out, ":app={app}");
            }
            if let Some(kind) = r.kind {
                let _ = write!(out, ":kind={}", kind.name());
            }
            if let Some(p) = r.progress {
                let _ = write!(out, ":progress={p}");
            }
            for (key, text) in [("title", &r.title), ("msg", &r.msg)] {
                if let Some(text) = text {
                    let _ = write!(out, ":{key}=");
                    base64_encode(text.as_bytes(), out);
                }
            }
            let _ = write!(out, ":goatty=restore.{}\x1b\\", r.seq);
        }
        match self.progress {
            Some(Progress { state, percent }) => {
                let n = match state {
                    ProgressState::Normal => 1,
                    ProgressState::Error => 2,
                    ProgressState::Indeterminate => 3,
                    ProgressState::Paused => 4,
                };
                let _ = write!(out, "\x1b]9;4;{n}");
                if let Some(p) = percent {
                    let _ = write!(out, ";{p}");
                }
                out.push('\x07');
            }
            None if replace => out.push_str("\x1b]9;4;0\x07"),
            None => {}
        }
    }
}

/// `id` is `parent` or under it.
fn within(id: &str, parent: &str) -> bool {
    id.strip_prefix(parent).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        let mut out = String::new();
        base64_encode(s.as_bytes(), &mut out);
        out
    }

    fn set(payload: &str) -> Record {
        match parse(payload.as_bytes()) {
            Some(Report::Set { record, .. }) => record,
            other => panic!("{payload}: {other:?}"),
        }
    }

    #[test]
    fn parses_what_claude_code_sends() {
        let r = set(&format!("state=blocked:app=claude-code:kind=permission:title={}:msg={}", b64("Bash"), b64("Run cargo test?")));
        assert_eq!(r.state, State::Blocked);
        assert_eq!(r.app.as_deref(), Some("claude-code"));
        assert_eq!(r.kind, Some(BlockedKind::Permission));
        assert_eq!(r.title.as_deref(), Some("Bash"));
        assert_eq!(r.msg.as_deref(), Some("Run cargo test?"));
        assert!(r.id.is_empty());
        let r = set("state=working:app=codex:id=agent/1:progress=40");
        assert_eq!((r.state, r.progress, r.id.as_str()), (State::Working, Some(40), "agent/1"));
        assert_eq!(parse(b"?"), Some(Report::Probe));
        assert_eq!(parse(b"state=clear"), Some(Report::Clear { id: None, restore: None }));
        assert_eq!(parse(b"state=clear:id=a/b"), Some(Report::Clear { id: Some("a/b".into()), restore: None }));
        assert_eq!(parse(b"state=clear:goatty=restore.9"), Some(Report::Clear { id: None, restore: Some(9) }));
        assert!(matches!(parse(b"state=done:goatty=restore"), Some(Report::Set { restore: Some(0), .. })));
    }

    #[test]
    fn drops_reports_that_break_the_rules() {
        for bad in [
            "",
            "app=x",
            "state=sleeping",
            "state=done:state=idle",
            "state=done:nokey",
            "state=working:progress=101",
            "state=working:progress=-1",
            "state=working:progress=1000",
            "state=done:app=has space",
            "state=done:app=a23456789012345678901234567890123",
            "state=done:id=a//b",
            "state=done:id=a/b/c/d/e/f/g/h/i",
            "state=done:waytoolongakeyname=1",
            "state=done:title=*notbase64*",
        ] {
            assert_eq!(parse(bad.as_bytes()), None, "{bad}");
        }
        let long_segment = format!("state=done:id={}", "a".repeat(33));
        assert_eq!(parse(long_segment.as_bytes()), None);
        // Control characters in text, and text over its limit.
        assert_eq!(parse(format!("state=done:title={}", b64("a\nb")).as_bytes()), None);
        assert_eq!(parse(format!("state=done:msg={}", b64("\u{9b}31m")).as_bytes()), None);
        assert_eq!(parse(format!("state=done:title={}", b64(&"x".repeat(193))).as_bytes()), None);
        assert!(parse(format!("state=done:title={}", b64(&"x".repeat(192))).as_bytes()).is_some());
        assert!(parse(format!("state=done:msg={}", b64(&"x".repeat(2048))).as_bytes()).is_some());
        assert_eq!(parse(format!("state=done:msg={}", b64(&"x".repeat(2049))).as_bytes()), None);
        assert_eq!(parse(&vec![b'a'; MAX_PAYLOAD + 1]), None);
    }

    #[test]
    fn cleans_text_and_ignores_what_doesnt_apply() {
        let r = set(&format!("state=done:title={}:kind=permission:progress=50:future=1", b64("\u{202e}evil\u{200b} ")));
        assert_eq!(r.title.as_deref(), Some("evil"));
        assert_eq!((r.kind, r.progress), (None, None), "kind and progress only go with blocked/working");
        let r = set("state=blocked:kind=telepathy");
        assert_eq!(r.kind, None, "an unknown kind is still blocked");
        // Unpadded base64.
        let r = set("state=done:title=aGk");
        assert_eq!(r.title.as_deref(), Some("hi"));
        assert_eq!(set(&format!("state=done:title={}", b64("é✓"))).title.as_deref(), Some("é✓"));
    }

    #[test]
    fn records_replace_inherit_and_clear() {
        let mut s = Store::default();
        let apply = |s: &mut Store, p: &str| s.apply(parse(p.as_bytes()).unwrap()) != Applied::Unchanged;
        apply(&mut s, "state=working:app=claude-code");
        apply(&mut s, "state=blocked:id=sub/a");
        apply(&mut s, "state=done:id=sub/b:app=helper");
        let st = s.status();
        assert_eq!(st.records.len(), 3);
        assert_eq!(st.records[1].app.as_deref(), Some("claude-code"), "inherited from the root");
        assert_eq!(st.app(), Some("claude-code"));
        assert_eq!(st.top(|_| true).map(|r| r.state), Some(State::Blocked));
        // Replacing keeps one record per id and moves it to the end.
        apply(&mut s, "state=working:id=sub/a");
        let st = s.status();
        assert_eq!(st.records.len(), 3);
        assert_eq!(st.records.last().map(|r| (r.id.as_str(), r.state)), Some(("sub/a", State::Working)));
        assert_eq!(st.records.last().unwrap().app.as_deref(), Some("claude-code"));
        // Clearing a parent takes its children.
        assert!(apply(&mut s, "state=clear:id=sub"));
        assert_eq!(s.status().records.len(), 1);
        assert!(!apply(&mut s, "state=clear:id=sub"));
        assert!(apply(&mut s, "state=clear"));
        assert!(s.status().records.is_empty());
        // A new record of the same id doesn't take the old one's app: only
        // an ancestor's.
        apply(&mut s, "state=done:app=codex");
        apply(&mut s, "state=working");
        assert_eq!(s.status().app(), None);
    }

    #[test]
    fn news_is_found_per_report() {
        let mut s = Store::default();
        let mut news = |p: &str| match s.apply(parse(p.as_bytes()).unwrap()) {
            Applied::News(r) => Some((r.id, r.state)),
            _ => None,
        };
        assert_eq!(news("state=working:app=codex"), None);
        assert_eq!(news("state=done"), Some((String::new(), State::Done)));
        assert_eq!(news("state=done"), None, "the same again");
        // A quick turn between two frames: each report counts on its own.
        assert_eq!(news("state=working"), None);
        assert_eq!(news("state=done"), Some((String::new(), State::Done)));
        assert_eq!(news("state=blocked:kind=permission"), Some((String::new(), State::Blocked)));
        assert_eq!(news("state=blocked:kind=question"), Some((String::new(), State::Blocked)), "waiting for something else");
        // A sub-record finishing under a waiting root isn't the news.
        assert_eq!(news("state=done:id=sub"), None);
        assert_eq!(news("state=idle:goatty=restore.3"), None);
        assert_eq!(news("state=error:id=x:goatty=restore.4"), None, "replays are never news");
    }

    #[test]
    fn a_prompt_keeps_only_results_and_reset_drops_all() {
        let mut s = Store::default();
        for p in ["state=working:id=a", "state=blocked:id=b", "state=idle:id=c", "state=done:id=d", "state=error:id=e"] {
            s.apply(parse(p.as_bytes()).unwrap());
        }
        s.set_progress(Some(Progress { state: ProgressState::Normal, percent: Some(3) }));
        assert!(s.prompt());
        let st = s.status();
        assert_eq!(st.records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["d", "e"]);
        assert_eq!(st.progress, None);
        assert!(!s.prompt());
        assert!(s.reset());
        assert!(s.status().is_empty());
    }

    #[test]
    fn keeps_a_bounded_number_of_records() {
        let mut s = Store::default();
        for i in 0..MAX_RECORDS + 10 {
            s.apply(parse(format!("state=working:id=r{i}").as_bytes()).unwrap());
        }
        let st = s.status();
        assert_eq!(st.records.len(), MAX_RECORDS);
        assert_eq!(st.records[0].id, "r10", "the oldest went first");
    }

    #[test]
    fn snapshot_rebuilds_the_records() {
        let mut s = Store::default();
        s.apply(
            parse(format!("state=blocked:app=codex:kind=question:progress=7:title={}:msg={}", b64("Pick"), b64("A or B?")).as_bytes())
                .unwrap(),
        );
        s.apply(parse(b"state=done:id=x/y").unwrap());
        s.set_progress(Some(Progress { state: ProgressState::Indeterminate, percent: None }));
        let mut out = String::new();
        s.write_snapshot(true, &mut out);
        // Parse it back as the client would: into a fresh one, and into one
        // that already has its own records and a higher count.
        let mut fresh = Store::default();
        let mut stale = Store::default();
        for _ in 0..5 {
            stale.apply(parse(b"state=working:id=old").unwrap());
        }
        for seq in out.split("\x1b]7501;").skip(1) {
            let payload = seq.split('\x1b').next().unwrap();
            let report = parse(payload.as_bytes()).unwrap();
            assert!(matches!(report, Report::Set { restore: Some(_), .. } | Report::Clear { restore: Some(_), .. }), "{payload}");
            for t in [&mut fresh, &mut stale] {
                assert!(!matches!(t.apply(report.clone()), Applied::News(_)));
            }
        }
        // The same records with the same sequence numbers, so what the user
        // saw stays seen.
        assert_eq!(fresh.status().records, s.status().records);
        assert_eq!(stale.status().records, s.status().records);
        assert!(fresh.status().latest >= s.status().records.last().unwrap().seq);
        assert!(out.ends_with("\x1b]9;4;3\x07"));
    }

    fn terminal(answer_queries: bool) -> (crate::Terminal, crate::TerminalChannels) {
        crate::Terminal::new(crate::TermSize::new(40, 5), answer_queries, std::sync::Arc::new(crate::Palette::default()))
    }

    #[test]
    fn the_probe_is_answered_before_da1_and_only_by_the_daemon() {
        let (t, ch) = terminal(true);
        t.feed(b"\x1b]7501;?\x1b\\\x1b[c");
        let replies: Vec<Vec<u8>> = ch.replies.try_iter().collect();
        assert_eq!(replies.first().map(Vec::as_slice), Some(PROBE_REPLY));
        assert!(replies.len() == 2 && replies[1].starts_with(b"\x1b[?"), "then DA1: {replies:?}");
        // Split across reads, terminated by BEL.
        t.feed(b"\x1b]75");
        t.feed(b"01;?\x07");
        assert_eq!(ch.replies.try_iter().collect::<Vec<_>>(), [PROBE_REPLY.to_vec()]);

        let (mirror, ch) = terminal(false);
        mirror.feed(b"\x1b]7501;?\x1b\\\x1b[c");
        assert!(ch.replies.try_iter().next().is_none());
    }

    #[test]
    fn terminal_tracks_reports_through_prompts_and_resets() {
        use crate::UiEvent;
        let (t, ch) = terminal(false);
        let title = b64(&"t".repeat(150));
        let report = format!("\x1b]133;C\x07\x1b]7501;state=working:app=claude-code:title={title}\x07");
        // Split anywhere, it's the same report.
        for cut in [3, 9, 30, 80, report.len() - 1] {
            let (a, b) = report.as_bytes().split_at(cut);
            t.feed(a);
            t.feed(b);
            let st = t.program_status();
            assert_eq!(st.records.len(), 1, "cut at {cut}");
            assert_eq!(st.records[0].title.as_deref().map(str::len), Some(150));
        }
        assert!(ch.ui.try_iter().all(|e| matches!(e, UiEvent::Status { news: None })), "working isn't news");
        t.feed(b"\x1b]7501;state=done\x07\x1b]9;4;3\x07");
        let news: Vec<_> = ch.ui.try_iter().filter_map(|e| if let UiEvent::Status { news } = e { news } else { None }).collect();
        assert_eq!(
            news.iter().map(|r| (r.state, r.app.as_deref())).collect::<Vec<_>>(),
            [(State::Done, None)],
            "no app: the root has no ancestor"
        );
        assert_eq!(t.program_status().progress, Some(Progress { state: ProgressState::Indeterminate, percent: None }));
        // The shell's next prompt: what's left is the result.
        t.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        let st = t.program_status();
        assert_eq!(st.records.iter().map(|r| r.state).collect::<Vec<_>>(), [State::Done]);
        assert_eq!(st.progress, None);
        let _ = ch.ui.try_iter().count();
        t.feed(b"\x1bc");
        assert!(t.program_status().is_empty());
        assert!(matches!(ch.ui.try_iter().collect::<Vec<_>>().as_slice(), [UiEvent::Status { news: None }]));
        // Too long: dropped, and what follows is still seen.
        let huge = format!("\x1b]7501;state=done:msg={}\x07\x1b]7501;state=error\x07", "A".repeat(5000));
        t.feed(huge.as_bytes());
        assert_eq!(t.program_status().records.iter().map(|r| r.state).collect::<Vec<_>>(), [State::Error]);
    }

    #[test]
    fn terminal_reports_desktop_notifications() {
        use crate::UiEvent;
        let (t, ch) = terminal(false);
        t.feed(b"\x1b]9;Build finished\x07\x1b]777;notify;Codex;Turn complete\x1b\\\x1b]9;1;500\x07\x1b]9;4;1;20\x07");
        let notes: Vec<(String, String)> = ch
            .ui
            .try_iter()
            .filter_map(|e| match e {
                UiEvent::Notification { title, body } => Some((title, body)),
                _ => None,
            })
            .collect();
        assert_eq!(notes, [(String::new(), "Build finished".into()), ("Codex".into(), "Turn complete".into())]);
        assert_eq!(t.program_status().progress, Some(Progress { state: ProgressState::Normal, percent: Some(20) }));
    }

    #[test]
    fn base64_roundtrip() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "✓ done"] {
            assert_eq!(base64_decode(b64(s).as_bytes()).unwrap(), s.as_bytes());
        }
        assert_eq!(base64_decode(b"Zm9vYg=="), Some(b"foob".to_vec()));
        assert_eq!(base64_decode(b"Zm9vYg="), None, "padding must complete the group");
        assert_eq!(base64_decode(b"Zm=9"), None);
        assert_eq!(base64_decode(b"Z"), None);
    }
}
