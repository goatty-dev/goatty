//! Importing the user's own shell setup (`shell.import`).
//!
//! Installers and years of habit put PATH entries, variables and aliases in
//! `.zshrc` or `.bashrc`, files goatty's managed zsh doesn't run (they load
//! their own prompt and frameworks, which would fight goatty's). Instead,
//! goatty runs the user's login shell the way their old terminal did
//! (`zsh -l -i -c …` or `bash -l -i -c …`) in a separate process, without
//! a terminal, and notes what it ends up with: the exported variables that
//! changed, PATH and the aliases. That *harvest* is cached
//! (`$XDG_STATE_HOME/goatty/import/`) and written as `import.zsh`, which
//! goatty's `.zshrc` applies: variables and PATH early (so the plugins find
//! the user's tools), aliases at the very end (goatty's own and the user's
//! additions win). The settings (`ImportSettings`: on/off, aliases, names to
//! skip) are applied by the `.zshrc`, so the harvest doesn't depend on them.
//!
//! A harvest runs again when one of the startup files changed (the session
//! waits for it, up to [`SYNC_TIMEOUT`]), and in the background once the
//! last one is older than [`MAX_AGE`] (to catch files those files source).
//! A startup that hangs or exits before reporting (an `exec tmux` in
//! `.zshrc`) is a failure: nothing new is imported and the reason shows in
//! the settings window. The shell gets `GOATTY_IMPORT=1`, so a `.zshrc`
//! can skip slow parts when it's goatty asking.
//!
//! Everything here runs on the host whose shells it describes.

use std::collections::BTreeMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use goatty_config::{HostSettings, ImportFrom};
use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::setup::tilde;
use crate::zsh::quote;

/// How long a new session waits for a harvest (first one, or a startup
/// file changed). A slower startup still finishes in the background.
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(5);
/// The limit for a harvest nobody waits for (background, "Check again").
pub const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(30);
/// A harvest older than this is redone in the background when a session starts.
pub const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Set in the harvested shell's environment, so a `.zshrc` can tell.
pub const MARKER_ENV: &str = "GOATTY_IMPORT";

const MAGIC: &str = "goatty-import";
const END: &str = "goatty-import-end";
const STDERR_LIMIT: usize = 4000;

/// Which shell's startup files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Zsh,
    Bash,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Zsh => "zsh",
            Kind::Bash => "bash",
        }
    }

    fn of(program: &str) -> Option<Kind> {
        let name = Path::new(program).file_name()?.to_str()?;
        if name == "zsh" || name.starts_with("zsh-") {
            Some(Kind::Zsh)
        } else if name == "bash" || name.starts_with("bash-") {
            Some(Kind::Bash)
        } else {
            None
        }
    }
}

/// Where a harvest's facts come from: the host's home, login shell and the
/// environment goattyd hands to new sessions. Injectable, so tests run in
/// a sandboxed `$HOME`.
#[derive(Debug, Clone)]
pub struct Context {
    pub home: PathBuf,
    /// The login shell (passwd), else `$SHELL`.
    pub login_shell: Option<String>,
    /// The environment the harvested shell starts with (and the baseline
    /// that tells which variables its startup files changed).
    pub env: Vec<(String, String)>,
}

impl Context {
    /// This process: its environment, `$HOME` and the passwd login shell.
    /// `zsh_dir`: goatty's own ZDOTDIR, never handed on as the user's.
    pub fn from_env(zsh_dir: &Path) -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
        let env: Vec<(String, String)> = std::env::vars().collect();
        let login_shell = passwd_shell().or_else(|| std::env::var("SHELL").ok()).filter(|s| !s.is_empty());
        Some(Self::new(home, login_shell, env, zsh_dir))
    }

    /// From parts; `env` is cleaned up for the harvest (goatty's variables
    /// out, the marker and the framework switches in).
    pub fn new(home: PathBuf, login_shell: Option<String>, env: Vec<(String, String)>, zsh_dir: &Path) -> Self {
        let mut env: Vec<(String, String)> =
            env.into_iter().filter(|(k, v)| !k.starts_with("GOATTY_") && !(k == "ZDOTDIR" && Path::new(v) == zsh_dir)).collect();
        let mut set = |k: &str, v: &str, replace: bool| match env.iter_mut().find(|(n, _)| n == k) {
            Some(e) if replace => e.1 = v.to_owned(),
            Some(_) => {}
            None => env.push((k.to_owned(), v.to_owned())),
        };
        set("HOME", &home.to_string_lossy(), true);
        set(MARKER_ENV, "1", true);
        // oh-my-zsh: no update check (it asks, or pulls from the network).
        set("DISABLE_AUTO_UPDATE", "true", false);
        // Powerlevel10k's instant prompt redirects output; there's no terminal anyway.
        set("POWERLEVEL9K_INSTANT_PROMPT", "off", false);
        // Some startup files return early for a dumb terminal.
        set("TERM", "xterm-256color", false);
        env.sort();
        Self { home, login_shell, env }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn which(&self, bin: &str) -> Option<String> {
        let path = self.var("PATH")?;
        std::env::split_paths(path).map(|d| d.join(bin)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
    }

    /// The user's ZDOTDIR (unset: `$HOME`), `known` when an earlier harvest
    /// saw their `.zshenv` set one.
    fn zdotdir(&self, known: Option<&str>) -> PathBuf {
        known.or(self.var("ZDOTDIR")).filter(|z| !z.is_empty()).map_or_else(|| self.home.clone(), PathBuf::from)
    }
}

/// The login shell from passwd.
fn passwd_shell() -> Option<String> {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes the entry into `pwd` and its strings into
    // `buf` (whose length we pass), and sets `result` to `pwd` on success
    // or null otherwise; nothing is read before that check.
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), pwd.as_mut_ptr(), buf.as_mut_ptr().cast(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: `result` is non-null, so getpwuid_r filled `pwd`.
    let pwd = unsafe { pwd.assume_init() };
    if pwd.pw_shell.is_null() {
        return None;
    }
    // SAFETY: pw_shell is a NUL-terminated string inside `buf`, which lives
    // until the end of this function.
    let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) };
    Some(shell.to_string_lossy().into_owned())
}

/// The shell a harvest runs, and why that one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: Kind,
    pub program: String,
    /// For people: "your login shell", "chosen in the settings", …
    pub why: String,
}

/// The user's startup files of `kind` (`zdotdir` for zsh's).
fn user_files(kind: Kind, home: &Path, zdotdir: &Path) -> Vec<PathBuf> {
    match kind {
        Kind::Zsh => [".zshenv", ".zprofile", ".zshrc", ".zlogin"].iter().map(|f| zdotdir.join(f)).collect(),
        Kind::Bash => [".bash_profile", ".bash_login", ".profile", ".bashrc"].iter().map(|f| home.join(f)).collect(),
    }
}

/// System-wide files a login shell of `kind` reads (some only on some distros).
fn system_files(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Zsh => &[
            "/etc/zshenv",
            "/etc/zsh/zshenv",
            "/etc/zprofile",
            "/etc/zsh/zprofile",
            "/etc/zshrc",
            "/etc/zsh/zshrc",
            "/etc/zlogin",
            "/etc/zsh/zlogin",
            "/etc/profile",
            "/etc/profile.d",
        ],
        Kind::Bash => &["/etc/profile", "/etc/profile.d", "/etc/bash.bashrc", "/etc/bashrc"],
    }
}

fn any_exists(files: &[PathBuf]) -> bool {
    files.iter().any(|f| f.is_file())
}

/// Which shell to harvest for `from`, or why there's none.
pub fn source(from: ImportFrom, ctx: &Context) -> std::result::Result<Source, String> {
    let login = ctx.login_shell.as_deref();
    let login_kind = login.and_then(Kind::of);
    let program_for = |kind: Kind| match (login, login_kind) {
        (Some(l), Some(k)) if k == kind && Path::new(l).is_file() => Some(l.to_owned()),
        _ => ctx.which(kind.name()),
    };
    let zsh_files = any_exists(&user_files(Kind::Zsh, &ctx.home, &ctx.zdotdir(None)));
    let bash_files = [".bashrc", ".bash_profile"].iter().any(|f| ctx.home.join(f).is_file());
    let (kind, why) = match from {
        ImportFrom::Zsh => (Kind::Zsh, "chosen in the settings".to_owned()),
        ImportFrom::Bash => (Kind::Bash, "chosen in the settings".to_owned()),
        ImportFrom::Auto => match login_kind {
            Some(Kind::Zsh) if !zsh_files && bash_files => {
                (Kind::Bash, "your login shell is zsh, but your startup files are bash's".to_owned())
            }
            Some(k) => (k, "your login shell".to_owned()),
            None if zsh_files => (Kind::Zsh, "you have zsh startup files".to_owned()),
            None if bash_files => (Kind::Bash, "you have bash startup files".to_owned()),
            None => {
                let name = login.and_then(|l| Path::new(l).file_name()).map(|n| n.to_string_lossy().into_owned());
                return Err(match name {
                    Some(n) => format!("Your login shell is {n}: goatty imports from zsh and bash only."),
                    None => "No login shell and no zsh or bash startup files found.".to_owned(),
                });
            }
        },
    };
    let program = program_for(kind).ok_or_else(|| format!("{} isn't installed on this host.", kind.name()))?;
    Ok(Source { kind, program, why })
}

/// One of the user's startup files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupFile {
    /// `~/…`.
    pub path: String,
    pub exists: bool,
}

/// An exported variable the startup files set or changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Var {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasKind {
    /// `alias ll='ls -l'`
    Regular,
    /// zsh's `alias -g`: expands anywhere on the line.
    Global,
    /// zsh's `alias -s`: opens files by extension.
    Suffix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alias {
    pub name: String,
    pub value: String,
    pub kind: AliasKind,
}

/// What the user's shell ended up with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harvest {
    pub source: Source,
    /// Unix seconds.
    pub at: u64,
    pub ms: u64,
    /// Exported variables that the startup files set or changed, without
    /// PATH and without shell or terminal state ([`is_shell_state`]).
    pub vars: Vec<Var>,
    /// The PATH the shell ended with, and the one it started with.
    pub path: Vec<String>,
    pub base_path: Vec<String>,
    pub aliases: Vec<Alias>,
    /// The ZDOTDIR the startup files left (zsh; `None`: unset).
    pub zdotdir: Option<String>,
    /// What the startup printed to stderr (shortened).
    pub stderr: String,
    /// The startup files' state when it ran ([`fingerprint`]).
    pub fingerprint: String,
}

impl Harvest {
    /// PATH folders the startup files added, in order.
    pub fn added_path(&self) -> Vec<&str> {
        self.path.iter().filter(|d| !self.base_path.contains(d)).map(String::as_str).collect()
    }
}

/// A harvest that didn't work out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub at: u64,
    pub message: String,
    pub stderr: String,
    pub fingerprint: String,
}

/// `harvest.json`: the last good harvest and the last failure (newer than it).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Record {
    pub harvest: Option<Harvest>,
    pub failure: Option<Failure>,
}

impl Record {
    pub fn load(dirs: &Dirs) -> Record {
        std::fs::read_to_string(dirs.import_record()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self, dirs: &Dirs) -> Result<()> {
        write_atomic(&dirs.import_record(), &(serde_json::to_string_pretty(self)? + "\n"))
    }

    /// The fingerprint and time of the newest attempt.
    fn newest(&self) -> Option<(&str, u64)> {
        let h = self.harvest.as_ref().map(|h| (h.fingerprint.as_str(), h.at));
        let f = self.failure.as_ref().map(|f| (f.fingerprint.as_str(), f.at));
        match (h, f) {
            (Some(h), Some(f)) => Some(if f.1 >= h.1 { f } else { h }),
            (h, f) => h.or(f),
        }
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("no parent")?;
    std::fs::create_dir_all(dir)?;
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    let tmp = dir.join(format!(".{}.tmp-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The state of everything a harvest depends on: the shell, its startup
/// files (size and mtime), the environment it starts with. A change means
/// the next session harvests again before it starts.
pub fn fingerprint(source: &Source, ctx: &Context, zdotdir: Option<&str>) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.kind.hash(&mut h);
    source.program.hash(&mut h);
    ctx.env.hash(&mut h);
    let mut files = user_files(source.kind, &ctx.home, &ctx.zdotdir(zdotdir));
    // A ZDOTDIR set by the user's .zshenv moves the other files.
    if source.kind == Kind::Zsh {
        files.extend(user_files(Kind::Zsh, &ctx.home, &ctx.zdotdir(None)));
    }
    files.extend(system_files(source.kind).iter().map(PathBuf::from));
    for f in files {
        f.hash(&mut h);
        if let Ok(m) = std::fs::metadata(&f) {
            m.len().hash(&mut h);
            m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).hash(&mut h);
        }
    }
    format!("{:016x}", h.finish())
}

/// The user's startup files for `source`, for people.
pub fn startup_files(source: &Source, ctx: &Context, zdotdir: Option<&str>) -> Vec<StartupFile> {
    user_files(source.kind, &ctx.home, &ctx.zdotdir(zdotdir))
        .into_iter()
        .map(|p| StartupFile { path: tilde_in(&p, &ctx.home), exists: p.is_file() })
        .collect()
}

fn tilde_in(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// The script the harvested shell runs once its startup is done: write
/// every exported variable, the aliases and ZDOTDIR to `out` as
/// NUL-separated records. Alias expansion is off first (the startup files'
/// aliases would otherwise apply to this code).
fn script(kind: Kind, out: &Path, ctx: &Context) -> String {
    let out = quote(&out.to_string_lossy());
    match kind {
        Kind::Zsh => {
            let body = format!(
                r#"builtin emulate -L zsh
builtin zmodload zsh/parameter 2>/dev/null
local k
{{
  builtin print -rn -- "{MAGIC}"$'\0'"1"$'\0'
  for k in ${{(k)parameters[(R)*export*]}}; do
    [[ ${{parameters[$k]}} == (array|association)* ]] || builtin print -rn -- "E$k=${{(P)k}}"$'\0'
  done
  for k in ${{(k)aliases}}; do builtin print -rn -- "A$k=${{aliases[$k]}}"$'\0'; done
  for k in ${{(k)galiases}}; do builtin print -rn -- "G$k=${{galiases[$k]}}"$'\0'; done
  for k in ${{(k)saliases}}; do builtin print -rn -- "S$k=${{saliases[$k]}}"$'\0'; done
  if (( ${{+ZDOTDIR}} )); then builtin print -rn -- "Z$ZDOTDIR"$'\0'; fi
  builtin print -rn -- "{END}"$'\0'
}} > {out}"#
            );
            format!(r"\builtin unsetopt aliases; () {{ \builtin eval {} }}", quote(&body))
        }
        Kind::Bash => {
            let body = format!(
                r#"IFS=$' \t\n'; set +eu; set -f
{{
  builtin printf '%s\0' "{MAGIC}" 1
  for k in $(compgen -e); do builtin printf 'E%s=%s\0' "$k" "${{!k}}"; done
  for k in "${{!BASH_ALIASES[@]}}"; do builtin printf 'A%s=%s\0' "$k" "${{BASH_ALIASES[$k]}}"; done
  builtin printf '%s\0' "{END}"
}} > {out}"#
            );
            // A login bash reads .bashrc only when the login file sources
            // it; a terminal's bash reads it anyway, so do too.
            let rc = ctx.home.join(".bashrc");
            let source_rc = if rc.is_file() && !login_file_sources_bashrc(&ctx.home) {
                format!(r"\builtin source {}; ", quote(&rc.to_string_lossy()))
            } else {
                String::new()
            };
            format!(r"{source_rc}\builtin shopt -u expand_aliases; \builtin eval {}", quote(&body))
        }
    }
}

/// The file a login bash reads (the first of `.bash_profile`, `.bash_login`,
/// `.profile`) mentions `.bashrc`.
fn login_file_sources_bashrc(home: &Path) -> bool {
    [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .map(|f| home.join(f))
        .find(|p| p.is_file())
        .is_some_and(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("bashrc")))
}

/// Variables that belong to the shell or the terminal it runs in, not to
/// the user's environment: never imported.
pub fn is_shell_state(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "_",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "HOME",
        "USER",
        "LOGNAME",
        "USERNAME",
        "SHELL",
        "ZDOTDIR",
        "ZSH",
        "FPATH",
        "IFS",
        "PS1",
        "PS2",
        "PS3",
        "PS4",
        "PROMPT",
        "RPROMPT",
        "RPS1",
        "PROMPT_COMMAND",
        "HISTFILE",
        "HISTSIZE",
        "SAVEHIST",
        "HISTFILESIZE",
        "HISTCONTROL",
        "HISTIGNORE",
        "HISTTIMEFORMAT",
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "LINES",
        "COLUMNS",
        "GPG_TTY",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_TTY",
        "DISABLE_AUTO_UPDATE",
    ];
    const PREFIXES: &[&str] = &["GOATTY_", "ZSH_", "STARSHIP_", "P9K_", "_P9K", "POWERLEVEL9K_", "BASH_FUNC_"];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// A name the managed zsh can export.
fn is_var_name(name: &str) -> bool {
    let mut b = name.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// What the dump script wrote: `None` when it's cut short or not ours.
#[derive(Debug, Default, PartialEq, Eq)]
struct Dump {
    env: BTreeMap<String, String>,
    aliases: Vec<Alias>,
    zdotdir: Option<String>,
}

fn parse_dump(bytes: &[u8]) -> Option<Dump> {
    let text = String::from_utf8_lossy(bytes);
    let mut records = text.split('\0');
    if records.next()? != MAGIC || records.next()? != "1" {
        return None;
    }
    let mut dump = Dump::default();
    let mut ended = false;
    for r in records {
        if r == END {
            ended = true;
            break;
        }
        let mut chars = r.chars();
        let Some(tag) = chars.next() else { continue };
        let rest = chars.as_str();
        let pair = || rest.split_once('=').filter(|(k, _)| !k.is_empty()).map(|(k, v)| (k.to_owned(), v.to_owned()));
        let alias = |kind| pair().map(|(name, value)| Alias { name, value, kind });
        match tag {
            'E' => {
                if let Some((k, v)) = pair() {
                    dump.env.insert(k, v);
                }
            }
            'A' => dump.aliases.extend(alias(AliasKind::Regular)),
            'G' => dump.aliases.extend(alias(AliasKind::Global)),
            'S' => dump.aliases.extend(alias(AliasKind::Suffix)),
            'Z' => dump.zdotdir = Some(rest.to_owned()),
            _ => {}
        }
    }
    dump.aliases.sort_by(|a, b| (a.kind as u8, &a.name).cmp(&(b.kind as u8, &b.name)));
    ended.then_some(dump)
}

fn split_path(p: Option<&str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    p.unwrap_or_default().split(':').filter(|d| !d.is_empty() && seen.insert(*d)).map(str::to_owned).collect()
}

/// Lines of stderr worth showing: without bash's complaints about running
/// interactively without a terminal, shortened from the front.
fn clean_stderr(raw: &str) -> String {
    let noise = ["cannot set terminal process group", "no job control in this shell", "Inappropriate ioctl for device"];
    let kept: Vec<&str> = raw.lines().filter(|l| !noise.iter().any(|n| l.contains(n))).collect();
    let text = kept.join("\n").trim().to_owned();
    if text.len() <= STDERR_LIMIT {
        return text;
    }
    let mut cut = text.len() - STDERR_LIMIT;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &text[cut..])
}

/// Run `source`'s shell with the user's startup files and collect what it
/// ends up with. Temporary files go to `work`. Killed (with everything it
/// started in its process group) after `timeout`.
/// `zdotdir`: the ZDOTDIR an earlier harvest saw the startup set.
pub fn harvest(
    source: &Source,
    ctx: &Context,
    work: &Path,
    timeout: Duration,
    zdotdir: Option<&str>,
) -> std::result::Result<Harvest, Failure> {
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fp = fingerprint(source, ctx, zdotdir);
    let fail = |message: String, stderr: String| Failure { at: now(), message, stderr, fingerprint: fp.clone() };
    std::fs::create_dir_all(work).map_err(|e| fail(format!("can't create {}: {e}", work.display()), String::new()))?;
    let id = format!("{}-{}", std::process::id(), RUNS.fetch_add(1, Ordering::Relaxed));
    let out = work.join(format!(".dump-{id}"));
    let err = work.join(format!(".stderr-{id}"));
    let result = run(source, ctx, &out, &err, timeout);
    let stderr = clean_stderr(&std::fs::read_to_string(&err).unwrap_or_default());
    let dump = std::fs::read(&out).ok();
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&err);
    let elapsed = result.map_err(|e| fail(format!("{e:#}"), stderr.clone()))?;
    let Some(dump) = dump.as_deref().and_then(parse_dump) else {
        return Err(fail(
            format!(
                "Your {} startup exited before goatty could read what it set up. A startup file that runs `exec` (tmux, another shell) or `exit` does that.",
                source.kind.name()
            ),
            stderr,
        ));
    };
    let base: BTreeMap<&str, &str> = ctx.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let vars = dump
        .env
        .iter()
        .filter(|(k, v)| *k != "PATH" && is_var_name(k) && !is_shell_state(k) && base.get(k.as_str()) != Some(&v.as_str()))
        .map(|(k, v)| Var { name: k.clone(), value: v.clone() })
        .collect();
    Ok(Harvest {
        source: source.clone(),
        at: now(),
        ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        vars,
        path: split_path(dump.env.get("PATH").map(String::as_str)),
        base_path: split_path(ctx.var("PATH")),
        aliases: dump.aliases,
        // The fingerprint for later checks takes the ZDOTDIR the startup left.
        fingerprint: fingerprint(source, ctx, dump.zdotdir.as_deref()),
        zdotdir: dump.zdotdir,
        stderr,
    })
}

/// Spawn the shell and wait for it (killing its process group afterwards,
/// so nothing it left running in the background outlives the harvest).
fn run(source: &Source, ctx: &Context, out: &Path, err: &Path, timeout: Duration) -> Result<Duration> {
    let stderr = File::create(err).with_context(|| format!("create {}", err.display()))?;
    let start = Instant::now();
    let mut child = Command::new(&source.program)
        .args(["-l", "-i", "-c", &script(source.kind, out, ctx)])
        .env_clear()
        .envs(ctx.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&ctx.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {}", source.program))?;
    let pgid = libc::pid_t::try_from(child.id()).ok();
    let kill_group = || {
        if let Some(pgid) = pgid {
            // SAFETY: kill(2) has no memory-safety preconditions; the
            // negative pid addresses the process group the child leads.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > timeout {
            kill_group();
            let _ = child.wait();
            anyhow::bail!(
                "Your {} startup didn't finish within {} s. goatty keeps what it imported before; a startup file that waits for input or a network does this.",
                source.kind.name(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let elapsed = start.elapsed();
    kill_group();
    log::debug!("import: {} exited with {status} after {elapsed:?}", source.program);
    Ok(elapsed)
}

/// `import.zsh`: the harvest as zsh arrays, which goatty's `.zshrc` applies
/// with the settings (skipped names, aliases on or off).
pub fn render(record: &Record, ctx: Option<&Context>) -> String {
    let mut out = String::from(
        "# goatty: what your own shell sets up, imported (shell.import).\n\
         # Rewritten by goatty: change what's imported in goatty's settings (Shell → From your shell).\n",
    );
    let Some(h) = &record.harvest else {
        out.push_str("# Nothing imported yet.\n");
        if let Some(f) = &record.failure {
            out.push_str(&format!("# {}\n", f.message.replace('\n', " ")));
        }
        out.push_str("typeset -ga _goatty_imported_vars=() _goatty_imported_path=() _goatty_imported_aliases=()\ntypeset -g _goatty_imported_from=''\n");
        return out;
    };
    let from = ctx.map_or_else(
        || h.source.kind.name().to_owned(),
        |c| {
            startup_files(&h.source, c, h.zdotdir.as_deref())
                .into_iter()
                .filter(|f| f.exists)
                .map(|f| f.path)
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    let from = if from.is_empty() { h.source.kind.name().to_owned() } else { from };
    out.push_str(&format!("# From {} ({}).\n", h.source.program, from));
    out.push_str(&format!("typeset -g _goatty_imported_from={}\n", quote(&from)));
    let words = |items: Vec<&str>| items.into_iter().map(quote).collect::<Vec<_>>().join(" ");
    out.push_str(&format!(
        "typeset -ga _goatty_imported_vars=({})\n",
        words(h.vars.iter().flat_map(|v| [v.name.as_str(), v.value.as_str()]).collect())
    ));
    let path: Vec<&str> = if h.path == h.base_path { Vec::new() } else { h.path.iter().map(String::as_str).collect() };
    out.push_str(&format!("typeset -ga _goatty_imported_path=({})\n", words(path)));
    let kind = |k: AliasKind| match k {
        AliasKind::Regular => "a",
        AliasKind::Global => "g",
        AliasKind::Suffix => "s",
    };
    out.push_str(&format!(
        "typeset -ga _goatty_imported_aliases=({})\n",
        words(h.aliases.iter().flat_map(|a| [kind(a.kind), a.name.as_str(), a.value.as_str()]).collect())
    ));
    out
}

/// Only one background harvest per process at a time.
static BACKGROUND: AtomicBool = AtomicBool::new(false);

/// What [`prepare`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepared {
    /// The cached harvest is current.
    Cached,
    /// Harvested now.
    Harvested,
    /// Harvested now, and it failed (the previous harvest, if any, stays).
    Failed,
    /// Cached, and a fresh harvest runs in the background.
    Refreshing,
    /// No shell to import from (fish, nothing installed).
    NoSource,
}

/// Make `import.zsh` current for a new session: harvest now when there's
/// no harvest or a startup file changed (waiting up to [`SYNC_TIMEOUT`]),
/// in the background when the last one is older than [`MAX_AGE`].
pub fn prepare(dirs: &Dirs, settings: &HostSettings, ctx: &Context) -> Result<Prepared> {
    let source = match source(settings.shell.import.from, ctx) {
        Ok(s) => s,
        Err(why) => {
            let record = Record {
                harvest: None,
                failure: Some(Failure { at: now(), message: why, stderr: String::new(), fingerprint: String::new() }),
            };
            write_atomic(&dirs.import_file(), &render(&record, Some(ctx)))?;
            return Ok(Prepared::NoSource);
        }
    };
    let record = Record::load(dirs);
    let zdotdir = record.harvest.as_ref().and_then(|h| h.zdotdir.clone());
    let fp = fingerprint(&source, ctx, zdotdir.as_deref());
    let outcome = match record.newest() {
        Some((seen, at)) if seen == fp => {
            if now().saturating_sub(at) > MAX_AGE.as_secs() {
                refresh_in_background(dirs.clone(), settings.shell.import.from, ctx.clone());
                Prepared::Refreshing
            } else {
                Prepared::Cached
            }
        }
        _ => {
            let (ok, _) = refresh(dirs, settings.shell.import.from, ctx, SYNC_TIMEOUT, false)?;
            if ok { Prepared::Harvested } else { Prepared::Failed }
        }
    };
    // Written every time: a harvest by another process, or an older goatty's file.
    write_atomic(&dirs.import_file(), &render(&Record::load(dirs), Some(ctx)))?;
    Ok(outcome)
}

/// Harvest now (under the lock, so processes don't run the user's startup
/// twice at once) and save the result. `force`: even when another process
/// just harvested the same files. Returns whether it worked, and the record.
pub fn refresh(dirs: &Dirs, from: ImportFrom, ctx: &Context, timeout: Duration, force: bool) -> Result<(bool, Record)> {
    let source = source(from, ctx).map_err(anyhow::Error::msg)?;
    let dir = dirs.import_dir();
    std::fs::create_dir_all(&dir)?;
    let lock = File::create(dir.join(".lock"))?;
    lock.lock()?;
    let mut record = Record::load(dirs);
    let zdotdir = record.harvest.as_ref().and_then(|h| h.zdotdir.clone());
    if !force
        && record.newest().is_some_and(|(seen, at)| seen == fingerprint(&source, ctx, zdotdir.as_deref()) && now().saturating_sub(at) < 5)
    {
        return Ok((record.failure.is_none(), record));
    }
    let ok = match harvest(&source, ctx, &dir, timeout, zdotdir.as_deref()) {
        Ok(h) => {
            log::info!(
                "import: {} variables, {} PATH folders, {} aliases from {} in {} ms",
                h.vars.len(),
                h.added_path().len(),
                h.aliases.len(),
                h.source.program,
                h.ms
            );
            record = Record { harvest: Some(h), failure: None };
            true
        }
        Err(f) => {
            log::warn!("import from {} failed: {}", source.program, f.message);
            record.failure = Some(f);
            false
        }
    };
    record.save(dirs)?;
    write_atomic(&dirs.import_file(), &render(&record, Some(ctx)))?;
    drop(lock);
    Ok((ok, record))
}

/// [`refresh`] on a thread of its own, unless one is running already.
pub fn refresh_in_background(dirs: Dirs, from: ImportFrom, ctx: Context) {
    if BACKGROUND.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("shell-import".into()).spawn(move || {
        if let Err(e) = refresh(&dirs, from, &ctx, BACKGROUND_TIMEOUT, false) {
            log::warn!("import: {e:#}");
        }
        BACKGROUND.store(false, Ordering::Release);
    });
    if let Err(e) = spawned {
        log::warn!("import: can't start a thread: {e}");
        BACKGROUND.store(false, Ordering::Release);
    }
}

/// Where a PATH folder imported from the user's shell came from, for the
/// PATH page: their `.zshrc` or `.bashrc` (the file installers write to).
pub fn path_origin(dirs: &Dirs) -> String {
    let record = Record::load(dirs);
    let file = match record.harvest.as_ref().map(|h| h.source.kind) {
        Some(Kind::Bash) => std::env::var_os("HOME").map(|h| tilde(&Path::new(&h).join(".bashrc"))),
        Some(Kind::Zsh) => {
            let zdotdir = record
                .harvest
                .as_ref()
                .and_then(|h| h.zdotdir.clone())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
            zdotdir.map(|z| tilde(&z.join(".zshrc")))
        }
        None => None,
    };
    format!("{} (imported)", file.unwrap_or_else(|| "your shell's startup files".into()))
}

/// The import as the settings window shows it (`setup::Request::Import`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Status {
    /// `shell.import.enabled` on the host.
    pub enabled: bool,
    /// Sessions run goatty's managed zsh (the only shell that applies it).
    pub managed: bool,
    /// The shell a harvest runs; else `problem` says why there's none.
    pub source: Option<Source>,
    pub problem: Option<String>,
    /// The user's startup files of that shell.
    pub files: Vec<StartupFile>,
    pub harvest: Option<Harvest>,
    /// The last attempt failed (newer than `harvest`).
    pub failure: Option<Failure>,
    /// Unix seconds on the host, to say how old the harvest is.
    pub now: u64,
}

/// The import's state; `refresh`: harvest now first (even while it's off,
/// so the settings window can show what turning it on would bring).
pub fn status(dirs: &Dirs, settings: &HostSettings, managed: bool, ctx: &Context, refresh_now: bool) -> Status {
    let imp = &settings.shell.import;
    let mut s = Status { enabled: imp.enabled, managed, now: now(), ..Status::default() };
    match source(imp.from, ctx) {
        Ok(src) => {
            if refresh_now && let Err(e) = refresh(dirs, imp.from, ctx, BACKGROUND_TIMEOUT, true) {
                s.problem = Some(format!("{e:#}"));
            }
            let record = Record::load(dirs);
            // A harvest of another shell (the setting changed) isn't this one's.
            let mine = |h: &Harvest| h.source.kind == src.kind;
            s.files = startup_files(&src, ctx, record.harvest.as_ref().filter(|h| mine(h)).and_then(|h| h.zdotdir.as_deref()));
            s.harvest = record.harvest.filter(|h| mine(h));
            s.failure = record.failure;
            s.source = Some(src);
        }
        Err(why) => s.problem = Some(why),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(home: &Path, login: Option<&str>, path: &str) -> Context {
        Context::new(home.to_path_buf(), login.map(Into::into), vec![("PATH".into(), path.into())], Path::new("/goatty/current/zsh"))
    }

    #[test]
    fn context_cleans_the_environment() {
        let c = Context::new(
            "/h".into(),
            None,
            vec![
                ("GOATTY_SOCKET".into(), "/s".into()),
                ("ZDOTDIR".into(), "/goatty/current/zsh".into()),
                ("TERM".into(), "foot".into()),
                ("PATH".into(), "/bin".into()),
            ],
            Path::new("/goatty/current/zsh"),
        );
        assert_eq!(c.var("GOATTY_SOCKET"), None);
        assert_eq!(c.var("ZDOTDIR"), None, "goatty's own ZDOTDIR isn't the user's");
        assert_eq!(c.var("TERM"), Some("foot"), "kept");
        assert_eq!(c.var(MARKER_ENV), Some("1"));
        assert_eq!(c.var("HOME"), Some("/h"));
        assert_eq!(c.var("DISABLE_AUTO_UPDATE"), Some("true"));
    }

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("/usr/bin/zsh"), Some(Kind::Zsh));
        assert_eq!(Kind::of("bash"), Some(Kind::Bash));
        assert_eq!(Kind::of("/bin/bash-5.2"), Some(Kind::Bash));
        assert_eq!(Kind::of("/usr/bin/fish"), None);
        assert_eq!(Kind::of("/bin/zshx"), None);
    }

    #[test]
    fn source_follows_the_login_shell_and_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for b in ["zsh", "bash"] {
            std::fs::write(bin.join(b), "").unwrap();
        }
        let path = bin.to_string_lossy().into_owned();
        let zsh = bin.join("zsh").to_string_lossy().into_owned();
        let c = ctx(&home, Some(&zsh), &path);
        let s = source(ImportFrom::Auto, &c).unwrap();
        assert_eq!((s.kind, s.why.as_str()), (Kind::Zsh, "your login shell"));
        assert_eq!(s.program, zsh);

        // zsh without any startup files, bash with: bash's.
        std::fs::write(home.join(".bashrc"), "").unwrap();
        assert_eq!(source(ImportFrom::Auto, &c).unwrap().kind, Kind::Bash);
        std::fs::write(home.join(".zshrc"), "").unwrap();
        assert_eq!(source(ImportFrom::Auto, &c).unwrap().kind, Kind::Zsh);
        assert_eq!(source(ImportFrom::Bash, &c).unwrap().program, bin.join("bash").to_string_lossy());

        let fish = ctx(&home, Some("/usr/bin/fish"), &path);
        assert_eq!(source(ImportFrom::Auto, &fish).unwrap().kind, Kind::Zsh, "zsh files decide");
        let fish_only = ctx(&tmp.path().join("empty"), Some("/usr/bin/fish"), &path);
        assert!(source(ImportFrom::Auto, &fish_only).unwrap_err().contains("fish"));
        assert!(source(ImportFrom::Zsh, &ctx(&home, None, "/nonexistent")).unwrap_err().contains("isn't installed"));
    }

    #[test]
    fn dumps_parse_and_need_their_end() {
        let raw = b"goatty-import\x001\x00EPATH=/a:/b\x00EEDITOR=nvim\x00EMULTI=a\nb=c\x00All=ls -l\x00G...=../..\x00Spdf=zathura\x00A-=cd -\x00Z/h/.config/zsh\x00goatty-import-end\x00";
        let d = parse_dump(raw).unwrap();
        assert_eq!(d.env["EDITOR"], "nvim");
        assert_eq!(d.env["MULTI"], "a\nb=c", "values keep newlines and =");
        assert_eq!(d.zdotdir.as_deref(), Some("/h/.config/zsh"));
        let names: Vec<_> = d.aliases.iter().map(|a| (a.name.as_str(), a.kind)).collect();
        assert_eq!(names, [("-", AliasKind::Regular), ("ll", AliasKind::Regular), ("...", AliasKind::Global), ("pdf", AliasKind::Suffix)]);
        assert!(parse_dump(&raw[..raw.len() - 20]).is_none(), "cut short");
        assert!(parse_dump(b"noise").is_none());
    }

    #[test]
    fn shell_state_stays_out() {
        for n in ["PWD", "SHLVL", "ZSH", "ZSH_CACHE_DIR", "STARSHIP_SHELL", "P9K_TTY", "GOATTY_X", "HISTFILE", "SSH_AUTH_SOCK", "PS1"] {
            assert!(is_shell_state(n), "{n}");
        }
        for n in ["EDITOR", "NVM_DIR", "CARGO_HOME", "FZF_DEFAULT_OPTS", "LANG", "PAGER", "LESS"] {
            assert!(!is_shell_state(n), "{n}");
        }
        assert!(is_var_name("A_1") && is_var_name("_x"));
        assert!(!is_var_name("1A") && !is_var_name("a-b") && !is_var_name(""));
    }

    #[test]
    fn stderr_drops_bash_noise_and_keeps_the_end() {
        let s = clean_stderr(
            "bash: cannot set terminal process group (-1): Inappropriate ioctl for device\nbash: no job control in this shell\nreal problem\n",
        );
        assert_eq!(s, "real problem");
        let long = "é".repeat(STDERR_LIMIT);
        let s = clean_stderr(&long);
        assert!(s.starts_with('…') && s.len() <= STDERR_LIMIT + 4);
    }

    fn sample() -> Harvest {
        Harvest {
            source: Source { kind: Kind::Zsh, program: "/bin/zsh".into(), why: String::new() },
            at: 1,
            ms: 2,
            vars: vec![Var { name: "EDITOR".into(), value: "it's vim".into() }],
            path: vec!["/home/u/.cargo/bin".into(), "/usr/bin".into()],
            base_path: vec!["/usr/bin".into()],
            aliases: vec![Alias { name: "ll".into(), value: "ls -l".into(), kind: AliasKind::Regular }],
            zdotdir: None,
            stderr: String::new(),
            fingerprint: "x".into(),
        }
    }

    #[test]
    fn renders_arrays_for_the_zshrc() {
        let h = sample();
        assert_eq!(h.added_path(), ["/home/u/.cargo/bin"]);
        let text = render(&Record { harvest: Some(h.clone()), failure: None }, None);
        assert!(text.contains(r"_goatty_imported_vars=('EDITOR' 'it'\''s vim')"), "{text}");
        assert!(text.contains("_goatty_imported_path=('/home/u/.cargo/bin' '/usr/bin')"), "{text}");
        assert!(text.contains("_goatty_imported_aliases=('a' 'll' 'ls -l')"), "{text}");
        let same = Harvest { path: h.base_path.clone(), ..h };
        assert!(
            render(&Record { harvest: Some(same), failure: None }, None).contains("_goatty_imported_path=()"),
            "PATH unchanged: nothing to prepend"
        );
        let none = render(
            &Record {
                harvest: None,
                failure: Some(Failure { at: 0, message: "boom".into(), stderr: String::new(), fingerprint: String::new() }),
            },
            None,
        );
        assert!(none.contains("# boom") && none.contains("_goatty_imported_vars=()"), "{none}");
    }

    #[test]
    fn newest_attempt_decides_freshness() {
        let h = sample();
        let f = Failure { at: 5, message: String::new(), stderr: String::new(), fingerprint: "y".into() };
        assert_eq!(Record { harvest: Some(h.clone()), failure: None }.newest(), Some(("x", 1)));
        assert_eq!(Record { harvest: Some(h), failure: Some(f) }.newest(), Some(("y", 5)));
        assert_eq!(Record::default().newest(), None);
    }

    #[test]
    fn fingerprint_changes_with_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path(), None, "/bin");
        let src = Source { kind: Kind::Zsh, program: "/bin/zsh".into(), why: String::new() };
        let a = fingerprint(&src, &c, None);
        assert_eq!(a, fingerprint(&src, &c, None));
        std::fs::write(tmp.path().join(".zshrc"), "export A=1\n").unwrap();
        let b = fingerprint(&src, &c, None);
        assert_ne!(a, b);
        std::fs::create_dir(tmp.path().join("z")).unwrap();
        std::fs::write(tmp.path().join("z/.zshrc"), "x").unwrap();
        assert_ne!(b, fingerprint(&src, &c, Some(&tmp.path().join("z").to_string_lossy())), "a ZDOTDIR the startup set counts");
        let bash = Source { kind: Kind::Bash, ..src };
        assert_ne!(b, fingerprint(&bash, &c, None));
    }
}
