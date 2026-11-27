//! Tabs that come back after their shells ended with goattyd (#344).
//!
//! A restart keeps every session (`crate::restore`). A reboot, a crash
//! without systemd, `systemctl --user stop` or logging out don't: the
//! shells are gone. For those, goattyd keeps a small record in its state
//! directory ([`Keeper`]): each session's id, folder and size, the
//! workspaces (#215) that show them, and how the program in its foreground
//! starts again ([`Resume`]). The next goattyd that starts without any
//! session to take over ([`resurrect`]) opens a new shell for each, with the
//! old id, so the workspaces and the windows' records of them still fit,
//! and types the command at its first prompt.
//!
//! What starts again is decided per program, never "whatever ran":
//!
//! * coding agents resume their own conversation (`restore.agents`): the
//!   one their session hook reported when the user installed it
//!   (`goatty_shell::agent_hooks`, every platform), else what can be seen
//!   from the outside (Claude Code's `sessions/<pid>.json`, the session file
//!   Copilot holds open, the session id on the command line), else the
//!   agent's own "continue the last one in this folder" (`claude
//!   --continue`, `codex resume --last`);
//! * programs the user listed (`restore.programs`: lazygit, htop, …) start
//!   again with their arguments;
//! * anything else (a build, a training run) is not started again, and a
//!   session that ran a command instead of a shell comes back as a shell.
//!
//! The agents, their flags and the argument checks follow herdr's
//! `agent_resume` (<https://github.com/herdrdev/herdr>, Apache-2.0), which
//! learns the session ids through hooks it installs instead.
//!
//! `goattyd stop` keeps the record, so the next goattyd brings the tabs back
//! too. Closing a tab (`Kill`) drops its session from the record right away;
//! a shell that exits on its own is dropped after [`GRACE`], so the shells a
//! shutdown ends one by one before goattyd itself stay in it.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use goatty_config::RestoreSettings;
use goatty_proto::{SessionId, WinSize};
use goatty_shell::agent_hooks::{Report, read_report};
use goatty_term::process::Process;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::server::Daemon;
use crate::session::Session;
use crate::workspaces::{SavedWorkspace, Workspaces};

/// The record's format.
const VERSION: u32 = 1;
/// How often the record is brought up to date.
const EVERY: Duration = Duration::from_secs(5);
/// How long a session whose shell exited on its own stays in the record.
const GRACE: Duration = Duration::from_secs(15);
/// How long a resurrected managed zsh may take to its first prompt before
/// the command is typed anyway (zsh reads it once it's ready).
const PROMPT_WAIT: Duration = Duration::from_secs(5);
/// The same for other shells, which may not say when they're at a prompt.
const PLAIN_WAIT: Duration = Duration::from_secs(2);
/// How long a brought-back tab's command counts as what runs there while
/// its shell starts and the command hasn't shown up yet.
const PENDING: Duration = Duration::from_secs(20);
/// The agents goatty resumes.
const AGENTS: &[&str] = &["claude", "codex", "copilot", "agy", "opencode"];

const MAX_ID: usize = 512;
const MAX_ARGS: usize = 64;
const MAX_ARGV_BYTES: usize = 8192;

/// How the program in a tab's foreground starts again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Resume {
    /// The agent (`claude`, `codex`, …) or the program's name.
    pub what: String,
    /// A coding agent (`restore.agents`), else a listed program (`restore.programs`).
    pub agent: bool,
    /// The command, typed into the new shell (quoted by [`command_line`]).
    pub argv: Vec<String>,
    /// It names no session, only "the last one here" (`claude --continue`):
    /// two tabs in one folder would resume the same one, so only the first does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub latest: bool,
}

impl Resume {
    fn agent(what: &str, argv: &[&str]) -> Self {
        Self { what: what.into(), agent: true, argv: argv.iter().map(|&a| a.into()).collect(), latest: false }
    }

    fn agent_with(what: &str, argv: &[&str], id: &str) -> Self {
        let mut r = Self::agent(what, argv);
        match r.argv.last_mut() {
            Some(last) if last.ends_with('=') => last.push_str(id),
            _ => r.argv.push(id.into()),
        }
        r
    }

    fn latest(what: &str, argv: &[&str]) -> Self {
        Self { latest: true, ..Self::agent(what, argv) }
    }

    /// What two tabs mustn't both resume: the same session, or the same
    /// "last one" in the same folder.
    fn key(&self, cwd: &str) -> String {
        if self.latest { format!("{}\0{cwd}\0{}", self.what, self.argv.join("\0")) } else { self.argv.join("\0") }
    }

    /// Whether the settings let it start again.
    fn allowed(&self, settings: &RestoreSettings) -> bool {
        if self.agent { settings.agents } else { settings.programs.contains(&self.what) }
    }
}

/// How `what` resumes session `id`.
fn by_id(what: &str, id: &str) -> Option<Resume> {
    let resume: &[&str] = match what {
        "claude" => &["claude", "--resume"],
        "codex" => &["codex", "resume"],
        "copilot" => &["copilot", "--resume="],
        "agy" => &["agy", "--conversation"],
        "opencode" => &["opencode", "--session"],
        _ => return None,
    };
    valid_id(id).then(|| Resume::agent_with(what, resume, id))
}

/// How to start again what runs in the foreground of a session whose shell
/// is `shell`, if it's an agent or one of `programs`. `report`: what the
/// agent's session hook said. `None` while the shell itself is in the
/// foreground.
pub(crate) fn detect(fg: Process, shell: Option<u32>, programs: &[String], report: Option<&Report>) -> Option<Resume> {
    if Some(fg.0) == shell {
        return None;
    }
    let args = fg.args().unwrap_or_default();
    // macOS names a versioned binary by its file (Claude Code's `2.1.332`):
    // the name it was started as counts too.
    let names = [fg.name(), args.first().map(|a| program_name(a).to_string())];
    let name = names.into_iter().flatten().find(|n| AGENTS.contains(&n.as_str()) || programs.contains(n))?;
    let own = own_args(&args);
    if let Some(r) = report.filter(|r| r.agent == name).and_then(|r| by_id(&name, &r.session)) {
        return Some(r).filter(|r| valid_argv(&r.argv));
    }
    let resume = match name.as_str() {
        "claude" => claude(fg, own),
        "codex" => codex(fg, own),
        "opencode" => Some(flag_id(own, &["--session", "-s"]).map_or_else(
            || Resume::latest("opencode", &["opencode", "--continue"]),
            |id| Resume::agent_with("opencode", &["opencode", "--session"], &id),
        )),
        "copilot" => copilot(fg, own),
        "agy" => Some(flag_id(own, &["--conversation"]).map_or_else(
            || Resume::latest("agy", &["agy", "--continue"]),
            |id| Resume::agent_with("agy", &["agy", "--conversation"], &id),
        )),
        _ => Some(program(&name, &args)),
    };
    resume.filter(|r| valid_argv(&r.argv))
}

/// The arguments after the program itself: `node …/codex.js resume X` and
/// `codex resume X` both give `resume X`.
fn own_args(args: &[String]) -> &[String] {
    let Some(first) = args.first() else { return args };
    if !goatty_term::process::is_runtime(program_name(first)) {
        return &args[1..];
    }
    // Past the runtime's own options to the script it runs.
    match args.iter().skip(1).position(|a| !a.starts_with('-')) {
        Some(i) => &args[i + 2..],
        None => &[],
    }
}

/// `/usr/bin/lazygit` → `lazygit`, `C:\\tools\\lazygit.exe` → `lazygit`.
fn program_name(path: &str) -> &str {
    let leaf = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match leaf.len().checked_sub(4) {
        Some(i) if leaf.get(i..).is_some_and(|ext| ext.eq_ignore_ascii_case(".exe")) => &leaf[..i],
        _ => leaf,
    }
}

/// A listed program, with the arguments it was started with when its
/// command line is known.
fn program(name: &str, args: &[String]) -> Resume {
    let mut argv = vec![name.to_string()];
    if args.first().is_some_and(|a| program_name(a) == name) {
        argv.extend(args[1..].iter().cloned());
    }
    Resume { what: name.into(), agent: false, argv, latest: false }
}

/// The value of the first of `flags` (`--flag id`, `--flag=id`).
fn flag_id(args: &[String], flags: &[&str]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for f in flags {
            if a == f {
                return it.next().filter(|v| valid_id(v)).cloned();
            }
            if let Some(v) = a.strip_prefix(f).and_then(|r| r.strip_prefix('='))
                && valid_id(v)
            {
                return Some(v.into());
            }
        }
    }
    None
}

/// Claude Code keeps `sessions/<pid>.json` in its config dir while it runs,
/// with the conversation's current `sessionId` (it changes with `/clear`).
fn claude(fg: Process, args: &[String]) -> Option<Resume> {
    let dir = fg.env("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| goatty_shell::Dirs::home_dir().map(|h| h.join(".claude")));
    let from_file =
        dir.and_then(|dir| fg.tree(8).into_iter().find_map(|p| claude_session(&dir.join("sessions").join(format!("{}.json", p.0)))));
    let id = from_file.or_else(|| flag_id(args, &["--resume", "-r", "--session-id"]));
    Some(id.map_or_else(
        || Resume::latest("claude", &["claude", "--continue"]),
        |id| Resume::agent_with("claude", &["claude", "--resume"], &id),
    ))
}

fn claude_session(file: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    v.get("sessionId")?.as_str().filter(|id| valid_id(id)).map(String::from)
}

/// Codex names its session on the command line when resumed (`codex resume
/// <id>`); its session file is open only while it writes, so otherwise it's
/// the newest session in the folder (`resume --last` only looks there).
fn codex(fg: Process, args: &[String]) -> Option<Resume> {
    let named = match args {
        [cmd, id, ..] if cmd == "resume" && valid_id(id) && is_uuid(id) => Some(id.clone()),
        _ => None,
    };
    let open = || open_files(fg).into_iter().find_map(|f| codex_rollout(&f));
    Some(named.or_else(open).map_or_else(
        || Resume::latest("codex", &["codex", "resume", "--last"]),
        |id| Resume::agent_with("codex", &["codex", "resume"], &id),
    ))
}

/// `…/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl` → the uuid.
fn codex_rollout(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (is_uuid(id) && path.components().any(|c| c.as_os_str() == "sessions")).then(|| id.to_string())
}

/// Copilot holds `session-state/<id>/inuse.<pid>.hold` open while a session runs.
fn copilot(fg: Process, args: &[String]) -> Option<Resume> {
    let held = open_files(fg).into_iter().find_map(|f| {
        let name = f.file_name()?.to_str()?;
        if !(name.starts_with("inuse.") && name.ends_with(".hold")) {
            return None;
        }
        let dir = f.parent()?;
        (dir.parent()?.file_name()? == "session-state").then(|| dir.file_name()?.to_str().map(String::from)).flatten()
    });
    let id = held.filter(|id| valid_id(id)).or_else(|| flag_id(args, &["--resume", "-r"]));
    Some(id.map_or_else(
        || Resume::latest("copilot", &["copilot", "--continue"]),
        |id| Resume::agent_with("copilot", &["copilot", "--resume="], &id),
    ))
}

fn open_files(fg: Process) -> Vec<PathBuf> {
    fg.tree(16).into_iter().flat_map(|p| p.open_files(512)).collect()
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() })
}

/// A session id is data, never an option or shell text.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID && !id.starts_with('-') && !id.chars().any(char::is_control)
}

/// What's typed into the new shell: a plain command name first (shells
/// disagree on running a quoted path), no control characters (a newline
/// would run what follows), nothing a shell reads specially inside single
/// quotes (see [`command_line`]), and not too long.
fn valid_argv(argv: &[String]) -> bool {
    let Some(cmd) = argv.first() else { return false };
    argv.len() <= MAX_ARGS
        && argv.iter().map(String::len).sum::<usize>() <= MAX_ARGV_BYTES
        && !argv.iter().any(|a| a.chars().any(special_in_quotes))
        && goatty_config::is_program_name(cmd)
}

/// A character that would end or change a single-quoted argument in one of
/// the shells: `'`, the typographic single quotes PowerShell reads as `'`,
/// and (outside Windows, where fish reads `\'` and `\\` in quotes) a backslash.
fn special_in_quotes(c: char) -> bool {
    c.is_control() || matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') || (c == '\\' && !cfg!(windows))
}

/// `argv` as one command line for zsh, bash, sh, fish and PowerShell:
/// arguments that aren't plain words go in single quotes, which all of them
/// read alike as long as nothing special is inside ([`valid_argv`]).
pub(crate) fn command_line(argv: &[String]) -> String {
    let quote = |a: &String| {
        let plain = !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./:%+=".contains(&b));
        if plain { a.clone() } else { format!("'{a}'") }
    };
    argv.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// One session in the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume: Option<Resume>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    version: u32,
    #[serde(default)]
    next_session: u64,
    #[serde(default)]
    sessions: Vec<Saved>,
    #[serde(default)]
    workspaces: Vec<SavedWorkspace>,
}

/// Keeps the record (see the module docs).
pub(crate) struct Keeper {
    /// `None` without a state directory (no `$HOME`): nothing is kept.
    file: Option<PathBuf>,
    /// Where each session's agent hook reports its session (`<id>.json`).
    reports: Option<PathBuf>,
    /// Held while the record is built and written, so [`Keeper::stop`]
    /// waits for a write under way.
    state: Mutex<KeeperState>,
    /// Sessions whose tab was closed: they leave the record right away. Its
    /// own short lock, so closing a tab never waits for a write.
    closed: Mutex<HashSet<SessionId>>,
    /// What brought-back tabs start again, until it runs (or [`PENDING`]
    /// passed): the shell is in the foreground meanwhile, and the record
    /// mustn't lose the command then.
    pending: Mutex<HashMap<SessionId, (Resume, Instant)>>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct KeeperState {
    /// What the file holds.
    written: Option<Record>,
    /// When each session missing from the daemon was first seen missing.
    missing: HashMap<SessionId, Instant>,
}

impl Keeper {
    /// The record for the daemon on `socket`, in `state_dir` (goatty's
    /// `$XDG_STATE_HOME`): one per socket, so a test daemon never sees the user's.
    pub(crate) fn new(state_dir: Option<&Path>, socket: &Path) -> Self {
        let name = format!("{:016x}", fnv(socket.as_os_str().as_encoded_bytes()));
        let file = state_dir.map(|d| d.join("sessions").join(format!("{name}.json")));
        let reports = state_dir.map(|d| d.join("sessions").join(name));
        Self {
            file,
            reports,
            state: Mutex::default(),
            closed: Mutex::default(),
            pending: Mutex::default(),
            stopped: AtomicBool::new(false),
        }
    }

    /// The file session `id`'s agent hook writes to (`GOATTY_AGENT_REPORT`),
    /// its folder made.
    pub(crate) fn report_file(&self, id: SessionId) -> Option<PathBuf> {
        let dir = self.reports.as_ref()?;
        make_private_dir(dir).ok()?;
        Some(dir.join(format!("{id}.json")))
    }

    fn drop_report(&self, id: SessionId) {
        if let Some(dir) = &self.reports {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }

    fn load(&self) -> Option<Record> {
        let record: Record = serde_json::from_slice(&std::fs::read(self.file.as_ref()?).ok()?).ok()?;
        (record.version <= VERSION).then_some(record)
    }

    /// The tab showing `id` was closed.
    pub(crate) fn closed(&self, id: SessionId) {
        self.closed.lock().insert(id);
    }

    /// goattyd is stopping: the record stays as it is, for the next one.
    pub(crate) fn stop(&self) {
        let _state = self.state.lock();
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Keep the record up to date on its own thread from now on.
    pub(crate) fn start(daemon: &Arc<Daemon>) -> std::io::Result<()> {
        if daemon.resurrect.file.is_none() {
            return Ok(());
        }
        let d = daemon.clone();
        std::thread::Builder::new().name("resurrect".into()).spawn(move || {
            loop {
                d.resurrect.update(&d, Instant::now());
                if d.resurrect.stopped.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(EVERY);
            }
        })?;
        Ok(())
    }

    fn update(&self, daemon: &Daemon, now: Instant) {
        let mut state = self.state.lock();
        if self.stopped.load(Ordering::SeqCst) || daemon.restarting.load(Ordering::SeqCst) {
            return;
        }
        let programs = daemon.host.settings().value.restore.programs;
        // Lock order: `workspaces`, then `sessions`.
        let (workspaces, sessions) = {
            let w = daemon.workspaces.lock();
            let s = daemon.sessions.lock();
            (w.export(), s.values().cloned().collect::<Vec<Arc<Session>>>())
        };
        let mut saved: Vec<Saved> = sessions.iter().map(|s| self.saved(s, &programs, now)).collect();
        saved.sort_by_key(|s| s.id);
        let next = Record { version: VERSION, next_session: daemon.next_session.load(Ordering::SeqCst), sessions: saved, workspaces };
        let closed = self.closed.lock().clone();
        let KeeperState { written, missing } = &mut *state;
        if let Some(written) = written {
            let live: HashSet<SessionId> = next.sessions.iter().map(|s| s.id).collect();
            let gone: Vec<SessionId> = written.sessions.iter().map(|s| s.id).filter(|id| !live.contains(id)).collect();
            missing.retain(|id, _| gone.contains(id));
            let mut waiting = false;
            for id in gone {
                let since = *missing.entry(id).or_insert(now);
                waiting |= !closed.contains(&id) && now.duration_since(since) < GRACE;
            }
            // A shell that just exited may be the first of a shutdown's:
            // keep the record as it was a little longer.
            if waiting || *written == next {
                return;
            }
        }
        match self.write(&next) {
            Ok(()) => {
                let live: HashSet<SessionId> = next.sessions.iter().map(|s| s.id).collect();
                for gone in state.written.iter().flat_map(|w| &w.sessions).filter(|s| !live.contains(&s.id)) {
                    self.drop_report(gone.id);
                }
                self.closed.lock().retain(|id| live.contains(id));
                state.missing.clear();
                state.written = Some(next);
            }
            Err(e) => log::warn!("couldn't save the sessions to bring back after a reboot: {e}"),
        }
    }

    fn write(&self, record: &Record) -> std::io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        if record.sessions.is_empty() {
            return match std::fs::remove_file(file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        make_private_dir(file.parent().expect("the record is in a directory"))?;
        let tmp = file.with_extension("json.tmp");
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        open.mode(0o600);
        let mut f = open.open(&tmp)?;
        f.write_all(&serde_json::to_vec(record).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    }
}

impl Keeper {
    fn saved(&self, s: &Session, programs: &[String], now: Instant) -> Saved {
        let (cols, rows) = s.term.size();
        let fg = s.pty.lock().foreground_pid();
        let shell = fg.is_none_or(|fg| Some(fg) == s.pid);
        let pending = {
            let mut p = self.pending.lock();
            match p.get(&s.id) {
                Some((r, until)) if now < *until => Some(r.clone()),
                Some(_) => {
                    p.remove(&s.id);
                    None
                }
                None => None,
            }
        };
        // The shell is back: what an agent's hook said is over (unless the
        // tab was just brought back and its command hasn't started yet).
        if shell && pending.is_none() {
            self.drop_report(s.id);
        }
        let report = self.reports.as_ref().and_then(|d| read_report(&d.join(format!("{}.json", s.id))));
        let resume = fg.and_then(|fg| detect(Process(fg), s.pid, programs, report.as_ref()));
        if resume.is_some() {
            self.pending.lock().remove(&s.id);
        }
        let resume = resume.or(pending.filter(|_| shell));
        Saved { id: s.id, cwd: s.current_cwd(), cols, rows, resume }
    }
}

/// A folder only this user can read (0700 on Unix).
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    b.mode(0o700);
    b.create(dir)
}

/// FNV-1a: a stable name for a socket path.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// A goattyd that starts with no session to take over brings back the ones
/// in the record (see the module docs). Call before the socket accepts.
pub(crate) fn resurrect(daemon: &Arc<Daemon>) {
    if !daemon.sessions.lock().is_empty() {
        return;
    }
    let Some(record) = daemon.resurrect.load() else { return };
    let settings = daemon.host.settings().value.restore;
    if !settings.sessions || record.sessions.is_empty() {
        return;
    }
    let top = record.sessions.iter().map(|s| s.id).max().unwrap_or(0);
    daemon.next_session.fetch_max(record.next_session.max(top), Ordering::SeqCst);
    let is_dir = |c: &str| Path::new(c).is_dir();
    let plans = plan(&record.sessions, &settings, is_dir);
    let mut back = 0;
    for (s, resume) in record.sessions.iter().zip(plans) {
        let cwd = s.cwd.clone().filter(|c| is_dir(c));
        let size = WinSize { cols: s.cols.max(2), rows: s.rows.max(1), cell_width: 0, cell_height: 0 };
        match daemon.resurrect_session(s.id, size, cwd) {
            Ok(session) => {
                back += 1;
                if let Some(r) = resume {
                    log::info!("session {}: starting {} again", s.id, r.what);
                    type_when_ready(session, command_line(&r.argv));
                    daemon.resurrect.pending.lock().insert(s.id, (r, Instant::now() + PENDING));
                }
            }
            Err(e) => log::warn!("session {}: couldn't bring it back: {e:#}", s.id),
        }
    }
    let live = daemon.live();
    *daemon.workspaces.lock() = Workspaces::import(record.workspaces, |s| live.contains(&s));
    log::info!("brought back {back} of {} sessions whose shells ended with the last goattyd", record.sessions.len());
}

/// What each saved session starts again (`None`: only its shell), in order.
/// Only in its own folder (in another one an agent would resume another
/// project's conversation, or none), and never one conversation twice: an
/// exact session in the first tab that had it, a "last one here" once per
/// folder, and not at all in a folder where the same agent resumes an exact
/// session (the last one there may be that one). Listed programs start in
/// every tab that had them.
fn plan(sessions: &[Saved], settings: &RestoreSettings, is_dir: impl Fn(&str) -> bool) -> Vec<Option<Resume>> {
    let usable: Vec<Option<Resume>> = sessions
        .iter()
        .map(|s| s.resume.clone().filter(|r| s.cwd.as_deref().is_some_and(&is_dir) && r.allowed(settings) && valid_argv(&r.argv)))
        .collect();
    let folder = |s: &Saved| s.cwd.clone().unwrap_or_default();
    let exact: HashSet<(String, String)> = sessions
        .iter()
        .zip(&usable)
        .filter_map(|(s, r)| r.as_ref().filter(|r| r.agent && !r.latest).map(|r| (r.what.clone(), folder(s))))
        .collect();
    let mut started = HashSet::new();
    sessions
        .iter()
        .zip(usable)
        .map(|(s, r)| {
            let r = r?;
            if !r.agent {
                return Some(r);
            }
            if r.latest && exact.contains(&(r.what.clone(), folder(s))) {
                return None;
            }
            started.insert(r.key(&folder(s))).then_some(r)
        })
        .collect()
}

/// Type `line` and Enter into the new shell once it's at its first prompt.
fn type_when_ready(session: Arc<Session>, line: String) {
    let spawned = std::thread::Builder::new().name(format!("s{}-resume", session.id)).spawn(move || {
        let deadline = Instant::now() + if session.managed { PROMPT_WAIT } else { PLAIN_WAIT };
        while session.term.prompts_marked() == 0 && Instant::now() < deadline {
            if session.exited.lock().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut bytes = line.into_bytes();
        bytes.push(b'\r');
        if let Err(e) = session.input.lock().write(&bytes) {
            log::warn!("session {}: couldn't type the command: {e}", session.id);
        }
    });
    if let Err(e) = spawned {
        log::warn!("couldn't start the resume thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn own_args_skip_the_runtime_and_its_script() {
        assert_eq!(own_args(&args(&["codex", "resume", "x"])), args(&["resume", "x"]));
        assert_eq!(
            own_args(&args(&["/usr/bin/node", "--no-warnings", "/usr/lib/node_modules/@openai/codex/bin/codex.js", "resume", "x"])),
            args(&["resume", "x"])
        );
        assert!(own_args(&args(&["node"])).is_empty());
        assert!(own_args(&[]).is_empty());
    }

    #[test]
    fn session_ids_from_flags() {
        let a = args(&["--model", "opus", "--resume", "abc"]);
        assert_eq!(flag_id(&a, &["--resume", "-r"]).as_deref(), Some("abc"));
        assert_eq!(flag_id(&args(&["--resume=abc"]), &["--resume"]).as_deref(), Some("abc"));
        assert_eq!(flag_id(&args(&["-r", "--verbose"]), &["--resume", "-r"]), None, "the picker, not an id");
        assert_eq!(flag_id(&args(&["--resume"]), &["--resume"]), None);
        assert_eq!(flag_id(&args(&["--resume", "a\nrm -rf ~"]), &["--resume"]), None);
    }

    #[test]
    fn codex_rollout_files_name_the_session() {
        let p = Path::new("/home/u/.codex/sessions/2026/11/20/rollout-2026-11-20T18-33-25-01a1fa05-2b4b-7d91-af26-327b9ffeb386.jsonl");
        assert_eq!(codex_rollout(p).as_deref(), Some("01a1fa05-2b4b-7d91-af26-327b9ffeb386"));
        assert_eq!(codex_rollout(Path::new("/home/u/.codex/logs_2.sqlite")), None);
        assert_eq!(codex_rollout(Path::new("/tmp/rollout-x.jsonl")), None);
    }

    #[test]
    fn claude_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("9192.json");
        std::fs::write(&f, r#"{"pid":9192,"sessionId":"a0e10215-9cef-4ce6-8675-b17dd9fe20d0","cwd":"/x"}"#).unwrap();
        assert_eq!(claude_session(&f).as_deref(), Some("a0e10215-9cef-4ce6-8675-b17dd9fe20d0"));
        std::fs::write(&f, r#"{"sessionId":"--dangerously-skip-permissions"}"#).unwrap();
        assert_eq!(claude_session(&f), None);
    }

    #[test]
    fn resume_commands() {
        let r = Resume::agent_with("copilot", &["copilot", "--resume="], "abc");
        assert_eq!(r.argv, args(&["copilot", "--resume=abc"]));
        let r = Resume::agent_with("claude", &["claude", "--resume"], "abc");
        assert_eq!(r.argv, args(&["claude", "--resume", "abc"]));
        assert_eq!(program("lazygit", &args(&["/usr/bin/lazygit", "-p", "/src"])).argv, args(&["lazygit", "-p", "/src"]));
        assert_eq!(program("htop", &args(&["node", "x.js"])).argv, args(&["htop"]), "arguments only when it's the program's own");
    }

    #[test]
    fn what_may_start_again() {
        let s = RestoreSettings::default();
        assert!(Resume::latest("claude", &["claude", "--continue"]).allowed(&s));
        assert!(program("lazygit", &[]).allowed(&s));
        assert!(!program("cargo", &[]).allowed(&s));
        let off = RestoreSettings { agents: false, programs: vec![], ..s };
        assert!(!Resume::latest("claude", &["claude", "--continue"]).allowed(&off));
        assert!(!program("lazygit", &[]).allowed(&off));
    }

    fn tab(id: SessionId, cwd: &str, resume: Resume) -> Saved {
        Saved { id, cwd: Some(cwd.into()), cols: 80, rows: 24, resume: Some(resume) }
    }

    #[test]
    fn listed_programs_start_in_every_tab_that_had_them() {
        let s = RestoreSettings::default();
        let tabs = [tab(1, "/a", program("lazygit", &[])), tab(2, "/b", program("lazygit", &[])), tab(3, "/a", program("lazygit", &[]))];
        assert!(plan(&tabs, &s, |_| true).iter().all(Option::is_some));
    }

    #[test]
    fn a_continue_never_picks_a_conversation_an_exact_resume_has() {
        let s = RestoreSettings::default();
        let exact = || Resume::agent_with("claude", &["claude", "--resume"], "S");
        let latest = || Resume::latest("claude", &["claude", "--continue"]);
        for tabs in [[tab(1, "/a", exact()), tab(2, "/a", latest())], [tab(1, "/a", latest()), tab(2, "/a", exact())]] {
            let p = plan(&tabs, &s, |_| true);
            assert_eq!(p.iter().filter(|r| r.is_some()).count(), 1, "{p:?}");
            assert!(p.iter().flatten().all(|r| !r.latest), "the exact one resumes: {p:?}");
        }
        // Another folder, or another agent, is another conversation.
        let other = [tab(1, "/a", exact()), tab(2, "/b", latest()), tab(3, "/a", Resume::latest("codex", &["codex", "resume", "--last"]))];
        assert!(plan(&other, &s, |_| true).iter().all(Option::is_some));
        // Two tabs with one session: the first.
        let twice = [tab(1, "/a", exact()), tab(2, "/b", exact())];
        assert_eq!(plan(&twice, &s, |_| true).iter().filter(|r| r.is_some()).count(), 1);
        // A folder that's gone starts nothing.
        assert_eq!(plan(&[tab(1, "/gone", exact())], &s, |_| false), [None]);
    }

    #[test]
    fn two_tabs_never_resume_the_same_conversation() {
        let latest = Resume::latest("claude", &["claude", "--continue"]);
        assert_eq!(latest.key("/a"), latest.key("/a"));
        assert_ne!(latest.key("/a"), latest.key("/b"));
        let id = Resume::agent_with("claude", &["claude", "--resume"], "x");
        assert_eq!(id.key("/a"), id.key("/b"), "an id is the same session anywhere");
    }

    #[test]
    fn argv_checks_and_quoting() {
        assert!(valid_argv(&args(&["claude", "--resume", "id"])));
        assert!(!valid_argv(&args(&["/usr/bin/claude"])));
        assert!(!valid_argv(&args(&["claude", "a\rb"])));
        assert!(!valid_argv(&args(&["lazygit", "it's"])), "PowerShell reads ' in quotes differently");
        for q in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            assert!(!valid_argv(&[String::from("lazygit"), format!("./it{q}s; calc")]), "PowerShell reads {q} as '");
        }
        assert_eq!(valid_argv(&args(&["lazygit", r"/tmp/dir\"])), cfg!(windows), "fish reads \\ in quotes");
        assert!(!valid_argv(&[]));
        assert_eq!(
            command_line(&args(&["lazygit", "-p", "/src/my repo", r"C:\src", "a,b", ""])),
            r"lazygit -p '/src/my repo' 'C:\src' 'a,b' ''"
        );
        assert_eq!(command_line(&args(&["codex", "resume", "abc; rm -rf /"])), "codex resume 'abc; rm -rf /'");
    }

    #[test]
    fn shell_in_the_foreground_starts_nothing() {
        let me = std::process::id();
        assert_eq!(detect(Process(me), Some(me), &[], None), None);
    }

    #[test]
    fn program_names_on_every_platform() {
        assert_eq!(program_name("/usr/bin/lazygit"), "lazygit");
        assert_eq!(program_name(r"C:\tools\LazyGit.EXE"), "LazyGit");
        assert_eq!(program_name("claude"), "claude");
        assert_eq!(program_name(".exe"), "");
    }

    #[test]
    fn a_hooks_report_names_the_session() {
        let r = by_id("copilot", "abc").unwrap();
        assert_eq!(r.argv, args(&["copilot", "--resume=abc"]));
        assert!(!r.latest);
        assert_eq!(by_id("claude", "--help"), None);
        assert_eq!(by_id("nano", "x"), None);
    }

    #[test]
    fn the_record_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let k = Keeper::new(Some(dir.path()), Path::new("/run/user/1/goatty/goattyd.sock"));
        let other = Keeper::new(Some(dir.path()), Path::new("/tmp/test/goattyd.sock"));
        assert_ne!(k.file, other.file, "one record per socket");
        let record = Record {
            version: VERSION,
            next_session: 7,
            sessions: vec![Saved { id: 3, cwd: Some("/src".into()), cols: 120, rows: 40, resume: Some(program("htop", &[])) }],
            workspaces: vec![],
        };
        k.write(&record).unwrap();
        assert_eq!(k.load(), Some(record));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(k.file.as_ref().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let report = k.report_file(3).unwrap();
        assert!(report.parent().unwrap().is_dir());
        assert_ne!(report.parent(), other.report_file(3).unwrap().parent(), "one report folder per socket");
    }
}
