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
//! * coding agents resume their own conversation (`restore.agents`), found
//!   from the outside without any hook in the agent's config: Claude Code's
//!   `sessions/<pid>.json`, the session file Copilot holds open, the session
//!   id on the command line, else the agent's own "continue the last one in
//!   this folder" (`claude --continue`, `codex resume --last`);
//! * programs the user listed (`restore.programs`: lazygit, htop, …) start
//!   again with their arguments;
//! * anything else (a build, a training run) is not started again, and a
//!   session that ran a command instead of a shell comes back as a shell.
//!
//! The agents, their flags and the argument checks follow herdr's
//! `agent_resume` (<https://github.com/herdrdev/herdr>, Apache-2.0), which
//! learns the session ids through hooks it installs instead.
//!
//! `goattyd stop` forgets the record: stopping ends the sessions on purpose.
//! Closing a tab (`Kill`) drops its session from the record right away; a
//! shell that exits on its own is dropped after [`GRACE`], so the shells a
//! shutdown ends one by one before goattyd itself stay in it.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use goatty_config::RestoreSettings;
use goatty_proto::{SessionId, WinSize};
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
/// The same for other shells, which don't say when they're at a prompt.
const PLAIN_WAIT: Duration = Duration::from_millis(500);

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

/// How to start again what runs in the foreground of a session whose shell
/// is `shell`, if it's an agent or one of `programs`. `None` while the
/// shell itself is in the foreground.
pub(crate) fn detect(fg: Process, shell: Option<u32>, programs: &[String]) -> Option<Resume> {
    if Some(fg.0) == shell {
        return None;
    }
    let name = fg.name()?;
    let args = fg.args().unwrap_or_default();
    let own = own_args(&args);
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
        _ if programs.contains(&name) => Some(program(&name, &args)),
        _ => None,
    };
    resume.filter(|r| valid_argv(&r.argv))
}

/// The arguments after the program itself: `node …/codex.js resume X` and
/// `codex resume X` both give `resume X`.
fn own_args(args: &[String]) -> &[String] {
    let Some(first) = args.first() else { return args };
    if !goatty_term::process::is_runtime(base(first)) {
        return &args[1..];
    }
    // Past the runtime's own options to the script it runs.
    match args.iter().skip(1).position(|a| !a.starts_with('-')) {
        Some(i) => &args[i + 2..],
        None => &[],
    }
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A listed program, with the arguments it was started with when its
/// command line is known.
fn program(name: &str, args: &[String]) -> Resume {
    let mut argv = vec![name.to_string()];
    if args.first().is_some_and(|a| base(a) == name) {
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
/// would run what follows), and not too long.
fn valid_argv(argv: &[String]) -> bool {
    let Some(cmd) = argv.first() else { return false };
    argv.len() <= MAX_ARGS
        && argv.iter().map(String::len).sum::<usize>() <= MAX_ARGV_BYTES
        && !argv.iter().any(|a| a.chars().any(char::is_control))
        && goatty_config::is_program_name(cmd)
}

/// `argv` as one command line for zsh, bash, sh or fish: arguments that
/// aren't plain words go in single quotes (`'` as `'\''`, which all of them read).
pub(crate) fn command_line(argv: &[String]) -> String {
    let quote = |a: &String| {
        let plain = !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./:@%+=,".contains(&b));
        if plain { a.clone() } else { format!("'{}'", a.replace('\'', r"'\''")) }
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
    /// Held while the record is built and written, so [`Keeper::forget`]
    /// never races a write.
    state: Mutex<KeeperState>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct KeeperState {
    /// What the file holds.
    written: Option<Record>,
    /// Sessions whose tab was closed: they leave the record right away.
    closed: HashSet<SessionId>,
    /// When each session missing from the daemon was first seen missing.
    missing: HashMap<SessionId, Instant>,
}

impl Keeper {
    /// The record for the daemon on `socket`, in `state_dir` (goatty's
    /// `$XDG_STATE_HOME`): one per socket, so a test daemon never sees the user's.
    pub(crate) fn new(state_dir: Option<&Path>, socket: &Path) -> Self {
        let file = state_dir.map(|d| d.join("sessions").join(format!("{:016x}.json", fnv(socket.as_os_str().as_encoded_bytes()))));
        Self { file, state: Mutex::default(), stopped: AtomicBool::new(false) }
    }

    fn load(&self) -> Option<Record> {
        let record: Record = serde_json::from_slice(&std::fs::read(self.file.as_ref()?).ok()?).ok()?;
        (record.version <= VERSION).then_some(record)
    }

    /// The tab showing `id` was closed.
    pub(crate) fn closed(&self, id: SessionId) {
        self.state.lock().closed.insert(id);
    }

    /// `goattyd stop`: the sessions end on purpose, so nothing comes back.
    pub(crate) fn forget(&self) {
        let _state = self.state.lock();
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(f) = &self.file {
            let _ = std::fs::remove_file(f);
        }
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
        let mut saved: Vec<Saved> = sessions.iter().map(|s| saved(s, &programs)).collect();
        saved.sort_by_key(|s| s.id);
        let next = Record { version: VERSION, next_session: daemon.next_session.load(Ordering::SeqCst), sessions: saved, workspaces };
        let KeeperState { written, closed, missing } = &mut *state;
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
                state.closed.retain(|id| live.contains(id));
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
        let dir = file.parent().expect("the record is in a directory");
        std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir)?;
        let tmp = file.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(record).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    }
}

fn saved(s: &Session, programs: &[String]) -> Saved {
    let (cols, rows) = s.term.size();
    let fg = s.pty.lock().foreground_pid();
    let resume = fg.and_then(|fg| detect(Process(fg), s.pid, programs));
    Saved { id: s.id, cwd: s.current_cwd(), cols, rows, resume }
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
    let mut started = HashSet::new();
    let mut back = 0;
    for s in &record.sessions {
        let cwd = s.cwd.clone().filter(|c| Path::new(c).is_dir());
        let size = WinSize { cols: s.cols.max(2), rows: s.rows.max(1), cell_width: 0, cell_height: 0 };
        // In another folder, an agent would resume another project's
        // conversation (or none): only in the same one.
        let resume = s
            .resume
            .clone()
            .filter(|r| cwd.is_some() && r.allowed(&settings) && valid_argv(&r.argv))
            .filter(|r| started.insert(r.key(cwd.as_deref().unwrap_or_default())));
        match daemon.resurrect_session(s.id, size, cwd) {
            Ok(session) => {
                back += 1;
                if let Some(r) = resume {
                    log::info!("session {}: starting {} again", s.id, r.what);
                    type_when_ready(session, command_line(&r.argv));
                }
            }
            Err(e) => log::warn!("session {}: couldn't bring it back: {e:#}", s.id),
        }
    }
    let live = daemon.live();
    *daemon.workspaces.lock() = Workspaces::import(record.workspaces, |s| live.contains(&s));
    log::info!("brought back {back} of {} sessions whose shells ended with the last goattyd", record.sessions.len());
}

/// Type `line` and Enter into the new shell once it's at its first prompt.
fn type_when_ready(session: Arc<Session>, line: String) {
    let spawned = std::thread::Builder::new().name(format!("s{}-resume", session.id)).spawn(move || {
        let deadline = Instant::now() + if session.managed { PROMPT_WAIT } else { PLAIN_WAIT };
        while session.term.prompt_generation() == 0 && Instant::now() < deadline {
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
        assert!(!valid_argv(&[]));
        assert_eq!(command_line(&args(&["lazygit", "-p", "/src/my repo", "it's", ""])), r"lazygit -p '/src/my repo' 'it'\''s' ''");
        assert_eq!(command_line(&args(&["codex", "resume", "abc; rm -rf /"])), "codex resume 'abc; rm -rf /'");
    }

    #[test]
    fn shell_in_the_foreground_starts_nothing() {
        let me = std::process::id();
        assert_eq!(detect(Process(me), Some(me), &[]), None);
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
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(k.file.as_ref().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
        k.forget();
        assert_eq!(k.load(), None);
    }
}
