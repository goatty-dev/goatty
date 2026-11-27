//! Session hooks in coding agents' own configs (#344), installed only when
//! the user asks (Settings → Shell → After a restart, `goattyd agent-hooks`).
//!
//! A hook makes the agent tell goatty which conversation runs in a tab, the
//! moment it starts, resumes or `/clear`s one: the agent runs
//! `goattyd agent-hook <agent>` with its session as JSON on stdin, and that
//! writes `{"agent", "session"}` to the file goattyd named in the session's
//! [`goatty_config::AGENT_REPORT_VAR`]. goattyd then resumes exactly that
//! conversation after a reboot, on every platform. Without a hook it
//! guesses from the outside (`goatty-daemon/src/resurrect.rs`). Outside a
//! goattyd session the hook does nothing.
//!
//! | Agent | File | Event |
//! |---|---|---|
//! | Claude Code | `~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR`) | `SessionStart` |
//! | Codex | `~/.codex/hooks.json` (`$CODEX_HOME`); Codex asks once to trust it | `SessionStart` |
//! | Copilot CLI | `~/.copilot/settings.json` (`$COPILOT_HOME`) | `SessionStart` |
//! | Antigravity | `~/.gemini/config/hooks.json`, a block named `goatty` | `PreInvocation` |
//! | OpenCode | `~/.config/opencode/plugins/goatty-resume.js` (a plugin) | `session.created`/`updated` |
//!
//! The formats follow herdr's integrations (<https://github.com/herdrdev/herdr>,
//! Apache-2.0). Editing keeps the file's other keys in their order (it's
//! written back with two-space indentation), touches only goatty's own
//! entries (any command running `goattyd … agent-hook <agent>`), and keeps a
//! copy of the file as it was in `$XDG_STATE_HOME/goatty/agent-hooks/`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use ordered::Json;

/// Claude Code's `SessionStart` sources that name a conversation.
const CLAUDE_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";
const TIMEOUT: u64 = 10;
const AGY_BLOCK: &str = "goatty";
const MAX_INPUT: u64 = 1 << 20;
const MAX_ID: usize = 512;

/// The OpenCode plugin (OpenCode loads every file in `plugins/`).
const OPENCODE_PLUGIN: &str = r#"// Installed by goatty (Settings > Shell > After a restart); remove it there.
// Tells goatty which OpenCode session runs in a goatty tab, so the tab
// resumes it after a reboot. Does nothing outside goatty.
import { renameSync, writeFileSync } from "node:fs";

const report = process.env.GOATTY_AGENT_REPORT;

export const GoattyResume = async () => ({
  event: async ({ event }) => {
    if (!report || (event?.type !== "session.created" && event?.type !== "session.updated")) return;
    const info = event.properties?.info;
    if (typeof info?.id !== "string" || !info.id || info.parentID) return;
    try {
      writeFileSync(`${report}.tmp`, JSON.stringify({ agent: "opencode", session: info.id }));
      renameSync(`${report}.tmp`, report);
    } catch {}
  },
});
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Claude,
    Codex,
    Copilot,
    Antigravity,
    OpenCode,
}

impl Agent {
    pub const ALL: [Agent; 5] = [Agent::Claude, Agent::Codex, Agent::Copilot, Agent::Antigravity, Agent::OpenCode];

    /// Its program's name: what goattyd sees in a tab's foreground, and the
    /// argument of `goattyd agent-hook`.
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
            Agent::Antigravity => "agy",
            Agent::OpenCode => "opencode",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Copilot => "Copilot CLI",
            Agent::Antigravity => "Antigravity",
            Agent::OpenCode => "OpenCode",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    /// The file installing changes, under [`Self::dir`].
    fn file(self) -> &'static str {
        match self {
            Agent::Claude | Agent::Copilot => "settings.json",
            Agent::Codex | Agent::Antigravity => "hooks.json",
            Agent::OpenCode => "plugins/goatty-resume.js",
        }
    }

    /// The agent's config folder (it exists when the agent is set up here).
    fn dir(self, ctx: &Ctx<'_>) -> PathBuf {
        let from = |var: &str| (ctx.var)(var).filter(|v| !v.is_empty()).map(PathBuf::from);
        match self {
            Agent::Claude => from("CLAUDE_CONFIG_DIR").unwrap_or_else(|| ctx.home.join(".claude")),
            Agent::Codex => from("CODEX_HOME").unwrap_or_else(|| ctx.home.join(".codex")),
            Agent::Copilot => from("COPILOT_HOME").unwrap_or_else(|| ctx.home.join(".copilot")),
            Agent::Antigravity => ctx.home.join(".gemini").join("config"),
            Agent::OpenCode => from("OPENCODE_CONFIG_DIR")
                .or_else(|| from("XDG_CONFIG_HOME").map(|d| d.join("opencode")))
                .unwrap_or_else(|| ctx.home.join(".config").join("opencode")),
        }
    }

    /// The session id in the JSON the agent gives its hook.
    fn session_of(self, input: &serde_json::Value) -> Option<String> {
        let keys: &[&str] = match self {
            Agent::Claude | Agent::Codex => &["session_id"],
            Agent::Copilot => &["session_id", "sessionId"],
            Agent::Antigravity => &["conversationId"],
            Agent::OpenCode => &[],
        };
        keys.iter().find_map(|k| input.get(k)?.as_str()).filter(|id| valid_id(id)).map(String::from)
    }
}

/// What a session hook wrote: the agent and its session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub agent: String,
    pub session: String,
}

/// The report in `file`, if there's a usable one.
pub fn read_report(file: &Path) -> Option<Report> {
    let r: Report = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    valid_id(&r.session).then_some(r)
}

/// A session id is data: never an option, a path to run, or shell text.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID && !id.starts_with('-') && !id.chars().any(char::is_control)
}

/// `goattyd agent-hook <agent>`: what the hook runs. Reads the agent's JSON
/// from `input` and writes the report to `report` (none outside goattyd).
/// Never fails the agent: problems are ignored.
pub fn run_hook(agent: &str, input: impl Read, mut out: impl Write, report: Option<&Path>) {
    let agent = Agent::from_id(agent);
    if let (Some(agent), Some(file)) = (agent, report) {
        let mut text = Vec::new();
        let _ = input.take(MAX_INPUT).read_to_end(&mut text);
        if let Some(session) = serde_json::from_slice(&text).ok().and_then(|v| agent.session_of(&v)) {
            let r = Report { agent: agent.id().into(), session };
            let tmp = file.with_extension("tmp");
            if let Ok(bytes) = serde_json::to_vec(&r)
                && std::fs::write(&tmp, bytes).is_ok()
            {
                let _ = std::fs::rename(&tmp, file);
            }
        }
    }
    // Antigravity reads an answer from every hook.
    if agent == Some(Agent::Antigravity) {
        let _ = out.write_all(b"{}\n");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    Install,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Off,
    On,
    /// goatty's hook is there, but runs another goattyd (moved or reinstalled):
    /// installing again updates it.
    Outdated,
}

/// One agent's hook on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hook {
    pub agent: Agent,
    pub name: String,
    /// The agent is set up here (its config folder exists).
    pub available: bool,
    pub state: State,
    /// The file installing changes, for people (`~/.claude/settings.json`).
    pub file: String,
    /// Something to know: Codex asks once to trust a new hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The file can't be read as the agent's config: goatty leaves it alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Status {
    pub hooks: Vec<Hook>,
    /// Why hooks can't be installed here (goattyd doesn't know its own path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// The copy of the last file changed, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// Where things are on this host.
pub struct Ctx<'a> {
    pub home: &'a Path,
    /// The goattyd hooks run.
    pub exe: Option<&'a Path>,
    /// Copies of changed files.
    pub backups: &'a Path,
    /// The environment (`CLAUDE_CONFIG_DIR`, …).
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub windows: bool,
}

impl Ctx<'_> {
    /// For people: `~/.claude/settings.json`.
    fn show(&self, path: &Path) -> String {
        match path.strip_prefix(self.home) {
            Ok(rest) => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
            Err(_) => path.display().to_string(),
        }
    }
}

/// The hooks' state, after `action` on `agent` (every available agent when `None`).
pub fn apply(ctx: &Ctx<'_>, action: Action, agent: Option<Agent>) -> Result<Status> {
    let mut backup = None;
    if action != Action::Status {
        let agents: Vec<Agent> = match agent {
            Some(a) => vec![a],
            None => Agent::ALL.into_iter().filter(|a| a.dir(ctx).is_dir()).collect(),
        };
        for a in agents {
            let path = a.dir(ctx).join(a.file());
            backup = match action {
                Action::Install => {
                    let exe = ctx.exe.context("goattyd doesn't know where it is installed, so a hook couldn't run it")?;
                    if !a.dir(ctx).is_dir() {
                        bail!("{} isn't set up here ({} doesn't exist)", a.name(), ctx.show(&a.dir(ctx)));
                    }
                    install(ctx, a, &path, &command(a, exe, ctx.windows))?
                }
                _ => remove(ctx, a, &path)?,
            }
            .or(backup);
        }
    }
    Ok(Status {
        hooks: Agent::ALL.into_iter().map(|a| hook(ctx, a)).collect(),
        unavailable: ctx.exe.is_none().then(|| "goattyd doesn't know where it is installed".into()),
        backup: backup.map(|b| ctx.show(&b)),
    })
}

fn hook(ctx: &Ctx<'_>, a: Agent) -> Hook {
    let path = a.dir(ctx).join(a.file());
    let expected = ctx.exe.map(|exe| command(a, exe, ctx.windows));
    let (state, problem) = match state(a, &path, expected.as_deref()) {
        Ok(s) => (s, None),
        Err(e) => (State::Off, Some(format!("{e:#}"))),
    };
    let note = (a == Agent::Codex).then(|| "Codex asks once to trust the hook".to_string());
    Hook { agent: a, name: a.name().into(), available: a.dir(ctx).is_dir(), state, file: ctx.show(&path), note, problem }
}

/// What the agent runs: this goattyd's `agent-hook`.
pub fn command(a: Agent, exe: &Path, windows: bool) -> String {
    let id = a.id();
    if windows {
        let p = exe.display().to_string().replace('\'', "''");
        // Copilot's `powershell` field is PowerShell already; the others may run
        // the command in cmd, PowerShell or Git Bash, where this works alike.
        if a == Agent::Copilot {
            format!("& '{p}' agent-hook {id}")
        } else {
            format!("powershell -NoProfile -NonInteractive -Command \"& '{p}' agent-hook {id}\"")
        }
    } else {
        format!("'{}' agent-hook {id}", exe.display().to_string().replace('\'', r"'\''"))
    }
}

/// A command goatty installed for `a` (any goattyd, any path).
fn ours(cmd: &str, a: Agent) -> bool {
    cmd.contains("goattyd") && cmd.contains(&format!("agent-hook {}", a.id()))
}

/// Copilot's command field on this platform.
fn copilot_field(windows: bool) -> &'static str {
    if windows { "powershell" } else { "bash" }
}

fn state(a: Agent, path: &Path, expected: Option<&str>) -> Result<State> {
    if a == Agent::OpenCode {
        return Ok(match std::fs::read_to_string(path) {
            Ok(text) if text == OPENCODE_PLUGIN => State::On,
            Ok(_) => State::Outdated,
            Err(_) => State::Off,
        });
    }
    let Some(root) = read_json(path)? else { return Ok(State::Off) };
    let found = commands(a, &root);
    Ok(if found.is_empty() {
        State::Off
    } else if expected.is_some_and(|e| found.iter().any(|c| c == e)) {
        State::On
    } else {
        State::Outdated
    })
}

/// goatty's commands in the agent's config.
fn commands(a: Agent, root: &Json) -> Vec<String> {
    let entries: Vec<&Json> = match a {
        Agent::Claude | Agent::Codex => root
            .get("hooks")
            .and_then(|h| h.get("SessionStart"))
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|group| group.get("hooks").and_then(Json::as_array))
            .flatten()
            .collect(),
        Agent::Copilot => root.get("hooks").and_then(|h| h.get("SessionStart")).and_then(Json::as_array).into_iter().flatten().collect(),
        Agent::Antigravity => {
            root.get(AGY_BLOCK).and_then(|b| b.get("PreInvocation")).and_then(Json::as_array).into_iter().flatten().collect()
        }
        Agent::OpenCode => Vec::new(),
    };
    entries
        .into_iter()
        .flat_map(|e| ["command", "bash", "powershell"].into_iter().filter_map(|k| e.get(k)?.as_str()))
        .filter(|c| ours(c, a))
        .map(String::from)
        .collect()
}

fn read_json(path: &Path) -> Result<Option<Json>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some(Json::Object(Vec::new())));
    }
    let v: Json = serde_json::from_str(&text).with_context(|| format!("{} isn't valid JSON", path.display()))?;
    if !matches!(v, Json::Object(_)) {
        bail!("{} isn't a JSON object", path.display());
    }
    Ok(Some(v))
}

/// Keep a copy of `path` as it is; returns where.
fn back_up(ctx: &Ctx<'_>, a: Agent, path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    std::fs::create_dir_all(ctx.backups).with_context(|| format!("create {}", ctx.backups.display()))?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Never over an earlier copy (two changes in one second).
    let to = (0..)
        .map(|n| ctx.backups.join(if n == 0 { format!("{}-{name}.{secs}", a.id()) } else { format!("{}-{name}.{secs}-{n}", a.id()) }))
        .find(|p| !p.exists())
        .expect("a free name");
    std::fs::copy(path, &to).with_context(|| format!("back up {}", path.display()))?;
    Ok(Some(to))
}

/// Write `text` to `path` through a file next to it (readers never see half
/// of it). On Unix it keeps the file's permissions (a private config stays
/// private); a new file is 0600, since agents' configs can hold tokens.
fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("goatty-tmp");
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
        open.mode(mode);
        mode
    };
    let mut f = open.open(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    // Exactly the mode: the umask may have taken bits away, and a file left
    // behind by an earlier try keeps its own.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        f.set_permissions(std::fs::Permissions::from_mode(mode)).with_context(|| format!("write {}", tmp.display()))?;
    }
    f.write_all(text.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

/// Whether `text` is goatty's OpenCode plugin (any version of it).
fn is_our_plugin(text: &str) -> bool {
    text.starts_with("// Installed by goatty")
}

/// Change the agent's JSON config with `edit` (returns whether it changed
/// anything); the file is backed up first. Returns the backup.
fn edit_json(ctx: &Ctx<'_>, a: Agent, path: &Path, edit: impl FnOnce(&mut Json)) -> Result<Option<PathBuf>> {
    let before = read_json(path)?;
    let mut root = before.clone().unwrap_or(Json::Object(Vec::new()));
    edit(&mut root);
    if Some(&root) == before.as_ref() || (before.is_none() && root == Json::Object(Vec::new())) {
        return Ok(None);
    }
    let backup = back_up(ctx, a, path)?;
    let mut text = serde_json::to_string_pretty(&root)?;
    text.push('\n');
    write(path, &text)?;
    Ok(backup)
}

fn install(ctx: &Ctx<'_>, a: Agent, path: &Path, cmd: &str) -> Result<Option<PathBuf>> {
    if a == Agent::OpenCode {
        match std::fs::read_to_string(path) {
            Ok(t) if t == OPENCODE_PLUGIN => return Ok(None),
            Ok(t) if !is_our_plugin(&t) => bail!("{} isn't goatty's plugin; leaving it alone", ctx.show(path)),
            _ => {}
        }
        let backup = back_up(ctx, a, path)?;
        write(path, OPENCODE_PLUGIN)?;
        return Ok(backup);
    }
    let cmd = Json::Str(cmd.into());
    edit_json(ctx, a, path, |root| {
        strip(a, root);
        match a {
            Agent::Claude | Agent::Codex => {
                let mut group = vec![];
                if a == Agent::Claude {
                    group.push(("matcher".into(), Json::Str(CLAUDE_MATCHER.into())));
                }
                let hook = Json::Object(vec![
                    ("type".into(), Json::Str("command".into())),
                    ("command".into(), cmd),
                    ("timeout".into(), Json::Num(TIMEOUT.into())),
                ]);
                group.push(("hooks".into(), Json::Array(vec![hook])));
                root.entry("hooks").entry("SessionStart").push(Json::Object(group));
            }
            Agent::Copilot => {
                let hook = Json::Object(vec![
                    ("type".into(), Json::Str("command".into())),
                    (copilot_field(ctx.windows).into(), cmd),
                    ("timeoutSec".into(), Json::Num(TIMEOUT.into())),
                ]);
                root.entry("hooks").entry("SessionStart").push(hook);
            }
            Agent::Antigravity => {
                let hook = Json::Object(vec![
                    ("type".into(), Json::Str("command".into())),
                    ("command".into(), cmd),
                    ("timeout".into(), Json::Num(TIMEOUT.into())),
                ]);
                root.set(AGY_BLOCK, Json::Object(vec![("PreInvocation".into(), Json::Array(vec![hook]))]));
            }
            Agent::OpenCode => {}
        }
    })
}

fn remove(ctx: &Ctx<'_>, a: Agent, path: &Path) -> Result<Option<PathBuf>> {
    if a == Agent::OpenCode {
        if !path.is_file() {
            return Ok(None);
        }
        // Only goatty's own plugin (any version of it).
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if !is_our_plugin(&text) {
            bail!("{} isn't goatty's plugin; leaving it alone", ctx.show(path));
        }
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        return Ok(None);
    }
    if !path.is_file() {
        return Ok(None);
    }
    edit_json(ctx, a, path, |root| strip(a, root))
}

/// Take goatty's entries out of the agent's config, and the lists and
/// objects that held only them.
fn strip(a: Agent, root: &mut Json) {
    let is_ours = |e: &Json| ["command", "bash", "powershell"].iter().any(|k| e.get(k).and_then(Json::as_str).is_some_and(|c| ours(c, a)));
    match a {
        Agent::Claude | Agent::Codex => {
            if let Some(list) = root.get_mut("hooks").and_then(|h| h.get_mut("SessionStart")).and_then(Json::as_array_mut) {
                for group in list.iter_mut() {
                    if let Some(hooks) = group.get_mut("hooks").and_then(Json::as_array_mut) {
                        hooks.retain(|h| !is_ours(h));
                    }
                }
                list.retain(|g| g.get("hooks").and_then(Json::as_array).is_none_or(|h| !h.is_empty()));
            }
        }
        Agent::Copilot => {
            if let Some(list) = root.get_mut("hooks").and_then(|h| h.get_mut("SessionStart")).and_then(Json::as_array_mut) {
                list.retain(|h| !is_ours(h));
            }
        }
        Agent::Antigravity => root.remove(AGY_BLOCK),
        Agent::OpenCode => {}
    }
    if let Some(hooks) = root.get_mut("hooks") {
        hooks.remove_if_empty("SessionStart");
    }
    root.remove_if_empty("hooks");
}

/// JSON that keeps its objects' keys in order (`serde_json::Value` sorts
/// them), so a user's config reads the same after goatty's edit.
mod ordered {
    use serde::de::{MapAccess, SeqAccess, Visitor};
    use serde::ser::{SerializeMap, SerializeSeq};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Debug, Clone, PartialEq)]
    pub enum Json {
        Null,
        Bool(bool),
        Num(serde_json::Number),
        Str(String),
        Array(Vec<Json>),
        Object(Vec<(String, Json)>),
    }

    impl Json {
        pub fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Json::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
            match self {
                Json::Object(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        pub fn as_str(&self) -> Option<&str> {
            if let Json::Str(s) = self { Some(s) } else { None }
        }

        pub fn as_array(&self) -> Option<&Vec<Json>> {
            if let Json::Array(a) = self { Some(a) } else { None }
        }

        pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json>> {
            if let Json::Array(a) = self { Some(a) } else { None }
        }

        /// The value at `key` of this object, made an object or array as
        /// it's used (a value of another kind there is replaced).
        pub fn entry(&mut self, key: &str) -> &mut Json {
            if !matches!(self, Json::Object(_)) {
                *self = Json::Object(Vec::new());
            }
            let Json::Object(m) = self else { unreachable!() };
            let i = match m.iter().position(|(k, _)| k == key) {
                Some(i) => i,
                None => {
                    m.push((key.into(), Json::Null));
                    m.len() - 1
                }
            };
            &mut m[i].1
        }

        /// Append to this array (made one when it isn't).
        pub fn push(&mut self, v: Json) {
            if !matches!(self, Json::Array(_)) {
                *self = Json::Array(Vec::new());
            }
            if let Json::Array(a) = self {
                a.push(v);
            }
        }

        pub fn set(&mut self, key: &str, v: Json) {
            *self.entry(key) = v;
        }

        pub fn remove(&mut self, key: &str) {
            if let Json::Object(m) = self {
                m.retain(|(k, _)| k != key);
            }
        }

        /// Remove `key` when it holds an empty array or object.
        pub fn remove_if_empty(&mut self, key: &str) {
            let empty = match self.get(key) {
                Some(Json::Array(a)) => a.is_empty(),
                Some(Json::Object(m)) => m.is_empty(),
                _ => false,
            };
            if empty {
                self.remove(key);
            }
        }
    }

    impl Serialize for Json {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            match self {
                Json::Null => s.serialize_unit(),
                Json::Bool(b) => s.serialize_bool(*b),
                Json::Num(n) => n.serialize(s),
                Json::Str(t) => s.serialize_str(t),
                Json::Array(a) => {
                    let mut seq = s.serialize_seq(Some(a.len()))?;
                    for v in a {
                        seq.serialize_element(v)?;
                    }
                    seq.end()
                }
                Json::Object(m) => {
                    let mut map = s.serialize_map(Some(m.len()))?;
                    for (k, v) in m {
                        map.serialize_entry(k, v)?;
                    }
                    map.end()
                }
            }
        }
    }

    impl<'de> Deserialize<'de> for Json {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Json;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("JSON")
                }
                fn visit_unit<E>(self) -> Result<Json, E> {
                    Ok(Json::Null)
                }
                fn visit_none<E>(self) -> Result<Json, E> {
                    Ok(Json::Null)
                }
                fn visit_bool<E>(self, b: bool) -> Result<Json, E> {
                    Ok(Json::Bool(b))
                }
                fn visit_i64<E>(self, n: i64) -> Result<Json, E> {
                    Ok(Json::Num(n.into()))
                }
                fn visit_u64<E>(self, n: u64) -> Result<Json, E> {
                    Ok(Json::Num(n.into()))
                }
                fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<Json, E> {
                    serde_json::Number::from_f64(n).map(Json::Num).ok_or_else(|| E::custom("not a JSON number"))
                }
                fn visit_str<E>(self, s: &str) -> Result<Json, E> {
                    Ok(Json::Str(s.into()))
                }
                fn visit_string<E>(self, s: String) -> Result<Json, E> {
                    Ok(Json::Str(s))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                    let mut a = Vec::new();
                    while let Some(v) = seq.next_element()? {
                        a.push(v);
                    }
                    Ok(Json::Array(a))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                    let mut m: Vec<(String, Json)> = Vec::new();
                    while let Some((k, v)) = map.next_entry::<String, Json>()? {
                        // A repeated key: the last one counts, as for serde_json.
                        m.retain(|(old, _)| *old != k);
                        m.push((k, v));
                    }
                    Ok(Json::Object(m))
                }
            }
            d.deserialize_any(V)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox {
        dir: tempfile::TempDir,
    }

    impl Sandbox {
        fn new(agents: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for a in agents {
                std::fs::create_dir_all(dir.path().join(a)).unwrap();
            }
            Self { dir }
        }

        fn home(&self) -> &Path {
            self.dir.path()
        }

        fn run(&self, action: Action, agent: Option<Agent>, windows: bool) -> Result<Status> {
            let backups = self.home().join("backups");
            let ctx =
                Ctx { home: self.home(), exe: Some(Path::new("/opt/goatty/bin/goattyd")), backups: &backups, var: &|_| None, windows };
            apply(&ctx, action, agent)
        }

        fn read(&self, rel: &str) -> String {
            std::fs::read_to_string(self.home().join(rel)).unwrap()
        }
    }

    fn state_of(s: &Status, a: Agent) -> State {
        s.hooks.iter().find(|h| h.agent == a).unwrap().state
    }

    #[test]
    fn claude_hook_keeps_the_users_settings_and_hooks() {
        let s = Sandbox::new(&[".claude"]);
        let before = r#"{
  "zeta": 1,
  "model": "sonnet",
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "other-tool emit", "timeout": 10}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "other-tool emit"}]}]
  },
  "alpha": {"b": 2, "a": 1.5}
}
"#;
        std::fs::write(s.home().join(".claude/settings.json"), before).unwrap();
        let st = s.run(Action::Status, None, false).unwrap();
        assert_eq!(state_of(&st, Agent::Claude), State::Off);
        assert!(st.hooks.iter().find(|h| h.agent == Agent::Claude).unwrap().available);
        assert!(!st.hooks.iter().find(|h| h.agent == Agent::Codex).unwrap().available);

        let st = s.run(Action::Install, Some(Agent::Claude), false).unwrap();
        assert_eq!(state_of(&st, Agent::Claude), State::On);
        assert!(st.backup.is_some(), "a copy of the file as it was");
        let text = s.read(".claude/settings.json");
        let keys: Vec<usize> = ["\"zeta\"", "\"model\"", "\"hooks\"", "\"alpha\""].iter().map(|k| text.find(k).unwrap()).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys keep their order: {text}");
        assert!(text.find("\"b\"").unwrap() < text.find("\"a\": 1.5").unwrap(), "{text}");
        assert!(text.contains("other-tool emit") && text.contains(CLAUDE_MATCHER));
        assert!(text.contains(r#""command": "'/opt/goatty/bin/goattyd' agent-hook claude""#), "{text}");

        // Twice is once.
        s.run(Action::Install, Some(Agent::Claude), false).unwrap();
        assert_eq!(s.read(".claude/settings.json").matches("agent-hook claude").count(), 1);

        let st = s.run(Action::Remove, Some(Agent::Claude), false).unwrap();
        assert_eq!(state_of(&st, Agent::Claude), State::Off);
        let copies = std::fs::read_dir(s.home().join("backups")).unwrap().count();
        assert_eq!(copies, 2, "each change keeps its own copy");
        let after: serde_json::Value = serde_json::from_str(&s.read(".claude/settings.json")).unwrap();
        assert_eq!(after, serde_json::from_str::<serde_json::Value>(before).unwrap(), "removing gives back what was there");
    }

    #[test]
    fn a_hook_for_another_goattyd_is_outdated_and_reinstalling_replaces_it() {
        let s = Sandbox::new(&[".codex"]);
        std::fs::write(
            s.home().join(".codex/hooks.json"),
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "'/old/goattyd' agent-hook codex"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(state_of(&s.run(Action::Status, None, false).unwrap(), Agent::Codex), State::Outdated);
        assert_eq!(state_of(&s.run(Action::Install, Some(Agent::Codex), false).unwrap(), Agent::Codex), State::On);
        let text = s.read(".codex/hooks.json");
        assert!(!text.contains("/old/goattyd") && text.contains("/opt/goatty/bin/goattyd"), "{text}");
        s.run(Action::Remove, Some(Agent::Codex), false).unwrap();
        assert_eq!(s.read(".codex/hooks.json").trim(), "{}");
    }

    #[test]
    fn every_agent_installs_where_it_is_set_up() {
        let s = Sandbox::new(&[".claude", ".codex", ".copilot", ".gemini/config", ".config/opencode"]);
        std::fs::write(s.home().join(".gemini/config/hooks.json"), r#"{"herdr": {"PreInvocation": []}}"#).unwrap();
        let st = s.run(Action::Install, None, false).unwrap();
        assert!(st.hooks.iter().all(|h| h.state == State::On), "{st:?}");
        assert!(s.read(".copilot/settings.json").contains(r#""bash": "'/opt/goatty/bin/goattyd' agent-hook copilot""#));
        let agy = s.read(".gemini/config/hooks.json");
        assert!(agy.contains("\"herdr\"") && agy.contains("\"goatty\"") && agy.contains("PreInvocation"), "{agy}");
        assert!(s.read(".config/opencode/plugins/goatty-resume.js").contains("GOATTY_AGENT_REPORT"));
        let st = s.run(Action::Remove, None, false).unwrap();
        assert!(st.hooks.iter().all(|h| h.state == State::Off), "{st:?}");
        assert!(!s.home().join(".config/opencode/plugins/goatty-resume.js").exists());
        assert!(s.read(".gemini/config/hooks.json").contains("\"herdr\""), "others' blocks stay");
    }

    #[test]
    fn a_users_own_plugin_with_that_name_is_left_alone() {
        let s = Sandbox::new(&[".config/opencode/plugins"]);
        let mine = s.home().join(".config/opencode/plugins/goatty-resume.js");
        std::fs::write(&mine, "export const Mine = async () => ({});\n").unwrap();
        assert!(s.run(Action::Install, Some(Agent::OpenCode), false).is_err());
        assert!(s.run(Action::Remove, Some(Agent::OpenCode), false).is_err());
        assert_eq!(s.read(".config/opencode/plugins/goatty-resume.js"), "export const Mine = async () => ({});\n");
        // An older goatty plugin is updated.
        std::fs::write(&mine, "// Installed by goatty (an older one)\n").unwrap();
        assert_eq!(state_of(&s.run(Action::Install, Some(Agent::OpenCode), false).unwrap(), Agent::OpenCode), State::On);
    }

    #[cfg(unix)]
    #[test]
    fn a_private_config_stays_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let s = Sandbox::new(&[".claude", ".codex"]);
        let file = s.home().join(".claude/settings.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        s.run(Action::Install, Some(Agent::Claude), false).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        s.run(Action::Remove, Some(Agent::Claude), false).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644, "and a shared one shared");
        s.run(Action::Install, Some(Agent::Codex), false).unwrap();
        let new = s.home().join(".codex/hooks.json");
        assert_eq!(std::fs::metadata(&new).unwrap().permissions().mode() & 0o777, 0o600, "a new one is private");
    }

    #[test]
    fn a_broken_config_is_left_alone() {
        let s = Sandbox::new(&[".claude"]);
        std::fs::write(s.home().join(".claude/settings.json"), "{ not json").unwrap();
        assert!(s.run(Action::Install, Some(Agent::Claude), false).is_err());
        assert_eq!(s.read(".claude/settings.json"), "{ not json");
        let st = s.run(Action::Status, None, false).unwrap();
        assert!(st.hooks.iter().find(|h| h.agent == Agent::Claude).unwrap().problem.is_some());
        assert!(s.run(Action::Install, Some(Agent::Codex), false).is_err(), "Codex isn't set up");
    }

    #[test]
    fn windows_commands_run_in_any_shell() {
        let exe = Path::new(r"C:\Users\it's me\goattyd.exe");
        assert_eq!(command(Agent::Copilot, exe, true), r"& 'C:\Users\it''s me\goattyd.exe' agent-hook copilot");
        assert_eq!(
            command(Agent::Claude, exe, true),
            r#"powershell -NoProfile -NonInteractive -Command "& 'C:\Users\it''s me\goattyd.exe' agent-hook claude""#
        );
        assert_eq!(command(Agent::Claude, Path::new("/a b/goattyd"), false), "'/a b/goattyd' agent-hook claude");
        let s = Sandbox::new(&[".copilot"]);
        s.run(Action::Install, Some(Agent::Copilot), true).unwrap();
        assert!(s.read(".copilot/settings.json").contains("\"powershell\""));
    }

    #[test]
    fn the_hook_writes_the_session_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("7.json");
        let mut out = Vec::new();
        run_hook("claude", &br#"{"session_id":"abc-123","hook_event_name":"SessionStart"}"#[..], &mut out, Some(&file));
        assert_eq!(read_report(&file), Some(Report { agent: "claude".into(), session: "abc-123".into() }));
        assert!(out.is_empty());
        run_hook("agy", &br#"{"conversationId":"conv-9"}"#[..], &mut out, Some(&file));
        assert_eq!(read_report(&file).unwrap().session, "conv-9");
        assert_eq!(out, b"{}\n", "Antigravity gets its answer");
        run_hook("copilot", &br#"{"sessionId":"--evil"}"#[..], std::io::sink(), Some(&file));
        assert_eq!(read_report(&file).unwrap().session, "conv-9", "not an id");
        let mut out = Vec::new();
        run_hook("agy", &b"{}"[..], &mut out, None);
        assert_eq!(out, b"{}\n", "outside goatty it only answers");
    }
}
