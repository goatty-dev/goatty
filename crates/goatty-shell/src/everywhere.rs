//! goatty's shell in every terminal (#283): an opt-in line in the user's
//! `.zshenv` that makes every interactive zsh on the host run goatty's
//! managed configuration, not only goatty's own sessions (an IDE's
//! terminal, other terminals, tmux panes, SSH logins).
//!
//! ```zsh
//! # goatty: use goatty's managed zsh in every terminal (Settings → Shell). Delete these 2 lines to stop.
//! [[ -r ~/.local/share/goatty/current/zsh/entry.zsh ]] && source ~/.local/share/goatty/current/zsh/entry.zsh
//! ```
//!
//! `entry.zsh` (rendered into every generation) points `ZDOTDIR` at goatty's
//! startup files for interactive shells, the same way goattyd starts its
//! sessions. It fails safe: without the file (uninstalled, an old
//! generation) zsh stays plain. The login shell, `/etc/shells` and `$SHELL`
//! never change, so nothing can lock the user out.
//!
//! This is the one place goatty writes outside its own directories, and only
//! with the user's consent (the settings window and `goattyd shell
//! everywhere on` show the exact lines and file first). Turning it off
//! removes exactly goatty's lines and leaves the rest of the file as it was;
//! both keep a backup under `$XDG_STATE_HOME/goatty/zshenv-backups/`. A
//! goatty line the user changed is reported, never guessed at.
//!
//! Everything here runs on the host whose `.zshenv` it is: for a remote
//! goattyd, the remote host.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::service::HostService;
use crate::zsh::quote;

/// The comment goatty writes above its line.
pub const COMMENT: &str = "# goatty: use goatty's managed zsh in every terminal (Settings → Shell). Delete these 2 lines to stop.";

/// Backups of `.zshenv` files kept (all files together).
const BACKUPS_KEPT: usize = 10;

/// What to do with the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    On,
    Off,
}

/// Whether the user's `.zshenv` files have goatty's line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not in every file: only goatty's own sessions (and shells reading a
    /// file that has it) run goatty's shell.
    Off,
    /// Exactly goatty's line is in every file (wherever in it).
    On,
    /// A line mentions goatty's `entry.zsh` but isn't goatty's line (the
    /// user edited, commented out or copied it), in a file without goatty's
    /// line: goatty leaves it alone.
    Changed,
}

/// A line that mentions goatty's `entry.zsh` but isn't goatty's line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedLine {
    /// For people (`~/.zshenv`).
    pub file: String,
    /// From 1.
    pub line: usize,
    pub text: String,
}

impl std::fmt::Display for ChangedLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}, line {}: {}", self.file, self.line, self.text)
    }
}

/// The line in the user's `.zshenv`, for the settings window and
/// `goattyd shell everywhere status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The files goatty adds its line to, for people: `~/.zshenv`, and a
    /// second one when that file moves `ZDOTDIR` (shells started from a
    /// shell read `$ZDOTDIR/.zshenv`).
    pub files: Vec<String>,
    pub state: State,
    /// Exactly what goatty adds: the comment and the line.
    pub lines: Vec<String>,
    /// Lines that mention goatty's `entry.zsh` but aren't its line.
    #[serde(default)]
    pub changed: Vec<ChangedLine>,
    /// Things to fix by hand: a line after goatty's that sets `ZDOTDIR`.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// goatty's managed zsh is on and goatty's shell is zsh: the line
    /// makes shells goatty's (else it's there but does nothing).
    pub active: bool,
    /// Why it can't be turned on here; turning it off always works.
    pub unavailable: Option<String>,
    /// The newest backup, for people.
    #[serde(default)]
    pub backup: Option<String>,
}

impl Status {
    /// `~/.zshenv`, or `~/.zshenv and ~/.config/zsh/.zshenv`.
    pub fn files_text(&self) -> String {
        match self.files.as_slice() {
            [] => "~/.zshenv".to_owned(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        }
    }
}

/// goatty's two lines for `entry` (the `entry.zsh` of the `current`
/// generation): `~/…` under `home` when the path needs no quoting, so a
/// `.zshenv` synced between machines keeps working; quoted otherwise.
pub fn lines(entry: &Path, home: &Path) -> [String; 2] {
    let plain = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-+@%,".contains(&b));
    let shown = match entry.strip_prefix(home) {
        Ok(rel) if plain(&rel.to_string_lossy()) => format!("~/{}", rel.display()),
        _ => quote(&entry.to_string_lossy()),
    };
    [COMMENT.to_owned(), format!("[[ -r {shown} ]] && source {shown}")]
}

const MARK: &str = "goatty-zdotdir:";

/// The `.zshenv` files interactive zsh shells on this host read, in order:
///
/// 1. what a shell with a clean environment (an SSH login) reads:
///    `$ZDOTDIR/.zshenv` when `/etc/zshenv` sets `ZDOTDIR`, else `~/.zshenv`
///    (asked from `zsh -f`, which still reads `/etc/zshenv`);
/// 2. `$ZDOTDIR/.zshenv` when that file sets `ZDOTDIR` (the `~/.config/zsh`
///    pattern): shells started from a shell inherit it and read that file;
/// 3. `$ZDOTDIR/.zshenv` when `env` (goattyd's, so the desktop session's)
///    sets `ZDOTDIR`: new terminals start with it.
///
/// `env` minus goatty's own variables and `ZDOTDIR` (`goatty_dir`). Without
/// zsh, or when asking it fails, decides from `env`.
pub fn zshenv_files(zsh: Option<&str>, home: &Path, env: &[(String, String)], goatty_dir: &Path) -> Vec<PathBuf> {
    let usable = |dir: &str| {
        let p = Path::new(dir);
        (!dir.is_empty() && !dir.contains(['\n', '\r', '\0']) && p.is_absolute() && p != goatty_dir).then(|| p.to_path_buf())
    };
    let env_zd = env.iter().find(|(k, _)| k == "ZDOTDIR").and_then(|(_, v)| usable(v));
    // Without ZDOTDIR: what the startup files decide on their own.
    let clean: Vec<&(String, String)> = env.iter().filter(|(k, _)| !k.starts_with("GOATTY_") && k != "ZDOTDIR").collect();
    let ask = |zsh: &str, args: &[&str]| -> Option<PathBuf> {
        let mut cmd = Command::new(zsh);
        cmd.args(args)
            .arg(format!("builtin print -rn -- {MARK}${{ZDOTDIR:-$HOME}}"))
            .env_clear()
            .envs(clean.iter().map(|(k, v)| (k, v)))
            .env("HOME", home)
            // entry.zsh does nothing in the import's runs (and this isn't interactive anyway).
            .env(crate::import::MARKER_ENV, "1")
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let out = String::from_utf8(output_within(cmd, Duration::from_secs(3))?).ok()?;
        usable(out.rsplit_once(MARK)?.1)
    };
    let mut dirs = Vec::new();
    match zsh {
        Some(zsh) => {
            let first = ask(zsh, &["-f", "-c"]).unwrap_or_else(|| home.to_path_buf());
            dirs.push(first);
            // Runs the user's .zshenv (not interactive): where it leaves ZDOTDIR.
            dirs.extend(ask(zsh, &["-c"]));
        }
        None => dirs.push(home.to_path_buf()),
    }
    dirs.extend(env_zd);
    let mut files: Vec<PathBuf> = Vec::new();
    for f in dirs.into_iter().map(|d| d.join(".zshenv")) {
        if !files.contains(&f) {
            files.push(f);
        }
    }
    files
}

/// Run `cmd` and return its stdout, or `None` when it fails or takes
/// longer than `timeout` (it's killed then).
fn output_within(mut cmd: Command, timeout: Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return reader.join().ok(),
            Ok(Some(_)) => return None,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A line that assigns or unsets `ZDOTDIR` (not one that only reads it).
fn sets_zdotdir(line: &str) -> bool {
    let code = line.split([' ', '\t']).take_while(|w| !w.starts_with('#')).collect::<Vec<_>>().join(" ");
    let words: Vec<&str> = code.split([';', '&', '|', ' ', '(', ')', '{', '}']).filter(|w| !w.is_empty()).collect();
    words.iter().any(|w| w.starts_with("ZDOTDIR=") || w.starts_with("ZDOTDIR+="))
        || (words.contains(&"unset") && words.contains(&"ZDOTDIR"))
}

/// What one file's `text` says about goatty's `lines`: whether goatty's line
/// is there, the changed goatty lines (line, text), and the lines after
/// goatty's that set `ZDOTDIR`.
fn scan(text: &str, lines: &[String; 2]) -> (bool, Vec<(usize, String)>, Vec<usize>) {
    let hook = lines[1].as_str();
    // goatty's entry.zsh as the line spells it (a data folder without
    // "goatty" in its path still counts).
    let entry = hook.strip_prefix("[[ -r ").and_then(|r| r.split(" ]]").next()).unwrap_or("entry.zsh");
    let mut on = false;
    let mut changed = Vec::new();
    let mut late_zdotdir = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_end();
        if t == hook {
            on = true;
        } else if t.contains(entry) || (t.contains("goatty") && t.contains("entry.zsh")) {
            changed.push((i + 1, t.to_owned()));
        } else if on && sets_zdotdir(t) {
            late_zdotdir.push(i + 1);
        }
    }
    (on, changed, late_zdotdir)
}

/// `text` with goatty's line added at the end (after everything else, so a
/// `ZDOTDIR` the file sets is the user's), or `None` when it's there.
fn with_lines(text: &str, lines: &[String; 2]) -> Option<String> {
    if text.lines().any(|l| l.trim_end() == lines[1]) {
        return None;
    }
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&lines[0]);
    out.push('\n');
    out.push_str(&lines[1]);
    out.push('\n');
    Some(out)
}

/// `text` without goatty's comment and line (wherever they are; nothing
/// else changes), or `None` when neither is there.
fn without_lines(text: &str, lines: &[String; 2]) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut removed = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_end();
        if t == lines[0] || t == lines[1] {
            removed = true;
        } else {
            out.push_str(line);
        }
    }
    removed.then_some(out)
}

/// A number no other writer in this process uses (temporary files, backups).
fn unique() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Copy `file` (`shown` for people) to the backups (private), keeping the
/// newest [`BACKUPS_KEPT`]. Named `<time>-<n>.<file>`, so names sort by age.
fn backup(dirs: &Dirs, file: &Path, shown: &str) -> Result<Option<PathBuf>> {
    let Ok(text) = std::fs::read(file) else { return Ok(None) };
    let dir = dirs.zshenv_backups();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let name: String = shown.chars().map(|c| if c == '/' { '%' } else { c }).collect();
    let path = dir.join(format!("{:010}.{:09}-{:010}-{:06}.{name}", now.as_secs(), now.subsec_nanos(), std::process::id(), unique()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    std::io::Write::write_all(&mut f, &text)?;
    let mut all = backups(dirs);
    while all.len() > BACKUPS_KEPT {
        let _ = std::fs::remove_file(all.remove(0));
    }
    Ok(Some(path))
}

/// The backups, oldest first.
fn backups(dirs: &Dirs) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> =
        std::fs::read_dir(dirs.zshenv_backups()).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    all.sort();
    all
}

/// Replace `file` with `text`: through a temporary file next to the real
/// file (a link from a dotfiles repository stays a link), with the old
/// file's permissions (new: 0644, its folder created).
fn write(file: &Path, text: &str) -> Result<()> {
    let real = if file.is_symlink() {
        std::fs::canonicalize(file).with_context(|| format!("{} is a link to a file that isn't there", file.display()))?
    } else {
        file.to_path_buf()
    };
    let mode = std::fs::metadata(&real).map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let dir = real.parent().context("the file has no folder")?;
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("zshenv");
    let tmp = dir.join(format!(".{name}.goatty-{}-{}", std::process::id(), unique()));
    let written = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("can't write in {}", dir.display()))?;
        std::io::Write::write_all(&mut f, text.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, &real).with_context(|| format!("replace {}", real.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// For people: `~/…` under `home`.
fn shown(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// `path`'s text; a missing file is empty.
fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Where the hook lives on one host: its `.zshenv` files, goatty's lines.
pub struct Hook<'a> {
    pub dirs: &'a Dirs,
    pub home: &'a Path,
    pub files: Vec<PathBuf>,
    pub lines: [String; 2],
}

impl<'a> Hook<'a> {
    pub fn new(dirs: &'a Dirs, home: &'a Path, files: Vec<PathBuf>) -> Self {
        let lines = lines(&dirs.zsh_dir().join("entry.zsh"), home);
        Self { dirs, home, files, lines }
    }

    /// Serializes changes to the files (the GUI, the CLI and goattyd's
    /// threads may ask at once).
    fn lock(&self) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.dirs.state).with_context(|| format!("create {}", self.dirs.state.display()))?;
        let path = self.dirs.state.join(".zshenv.lock");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        f.lock()?;
        Ok(f)
    }

    /// What the files have now. `active`: goatty's zsh is on here;
    /// `unavailable`: why it can't be turned on.
    pub fn status(&self, active: bool, unavailable: Option<String>) -> Status {
        let mut all_on = true;
        let mut any_on = false;
        let mut changed_only = false;
        let mut changed = Vec::new();
        let mut warnings = Vec::new();
        let mut missing = Vec::new();
        for f in &self.files {
            let file = shown(f, self.home);
            let text = match read(f) {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("{file} can't be read ({e:#}), so goatty can't tell or change what it has."));
                    String::new()
                }
            };
            let (on, ch, late) = scan(&text, &self.lines);
            all_on &= on;
            any_on |= on;
            changed_only |= !on && !ch.is_empty();
            if !on {
                missing.push(file.clone());
            }
            for n in late {
                warnings.push(format!(
                    "{file}, line {n}: it sets ZDOTDIR after goatty's line, so goatty's shell doesn't load. Move goatty's line below it."
                ));
            }
            changed.extend(ch.into_iter().map(|(line, text)| ChangedLine { file: file.clone(), line, text }));
        }
        let state = if changed_only {
            State::Changed
        } else if any_on {
            // Partly (a file edited by hand): on, so switching it off cleans up.
            if !all_on {
                warnings.push(format!(
                    "{} doesn't have goatty's line, so shells that read it stay plain. Switch this off and on again to add it.",
                    missing.join(" and ")
                ));
            }
            State::On
        } else {
            State::Off
        };
        Status {
            files: self.files.iter().map(|f| shown(f, self.home)).collect(),
            state,
            lines: self.lines.to_vec(),
            changed,
            warnings,
            active,
            unavailable,
            backup: backups(self.dirs).pop().map(|p| shown(&p, self.home)),
        }
    }

    /// Add goatty's lines at the end of every file that doesn't have them
    /// (a backup first). Refuses, before writing anything, when a file has
    /// a changed goatty line instead: that's the user's to sort out.
    pub fn enable(&self) -> Result<()> {
        let _lock = self.lock()?;
        let mut todo = Vec::new();
        for f in &self.files {
            let text = read(f)?;
            let (on, changed, _) = scan(&text, &self.lines);
            if on {
                continue;
            }
            if let Some((n, line)) = changed.first() {
                bail!("{}, line {n} has a changed goatty line ({line}). Edit or remove it there, then try again", shown(f, self.home));
            }
            todo.push((f, text));
        }
        // All or nothing: a file that can't be written puts back the ones
        // written before it, so no shell is left half switched.
        let mut written: Vec<(&PathBuf, String)> = Vec::new();
        for (f, text) in todo {
            let Some(new) = with_lines(&text, &self.lines) else { continue };
            let done = backup(self.dirs, f, &shown(f, self.home)).and_then(|_| write(f, &new));
            if let Err(e) = done {
                for (back, old) in written.iter().rev() {
                    if let Err(e) = write(back, old) {
                        log::warn!("couldn't put {} back: {e:#}", back.display());
                    }
                }
                return Err(self.manual(f, e, "add these lines at its end"));
            }
            written.push((f, text));
        }
        Ok(())
    }

    /// Remove exactly goatty's lines from every file (a backup first); the
    /// rest of each file stays byte for byte. Refuses when no file has
    /// goatty's line but one has a changed goatty line.
    pub fn disable(&self) -> Result<()> {
        let _lock = self.lock()?;
        let mut todo = Vec::new();
        let mut changed = None;
        for f in &self.files {
            let text = read(f)?;
            let (on, ch, _) = scan(&text, &self.lines);
            // A file whose goatty line was edited by hand stays as it is,
            // goatty's comment above it included.
            if !on && !ch.is_empty() {
                changed = changed.or(ch.into_iter().next().map(|c| (f, c)));
                continue;
            }
            if let Some(new) = without_lines(&text, &self.lines) {
                todo.push((f, new));
            }
        }
        if todo.is_empty()
            && let Some((f, (n, line))) = changed
        {
            bail!("{}, line {n} has a changed goatty line ({line}). Remove it there yourself", shown(f, self.home));
        }
        for (f, new) in todo {
            backup(self.dirs, f, &shown(f, self.home))?;
            write(f, &new).map_err(|e| self.manual(f, e, "remove these lines from it"))?;
        }
        Ok(())
    }

    /// A write to `file` that failed: say what to do by hand.
    fn manual(&self, file: &Path, e: anyhow::Error, what: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "Couldn't change {} ({e:#}). If a dotfiles tool manages it, {what} there:\n{}\n{}",
            shown(file, self.home),
            self.lines[0],
            self.lines[1]
        )
    }
}

impl HostService {
    /// goatty's shell in every terminal on this host: the line's state,
    /// after turning it on or off.
    pub fn everywhere(&self, action: Action) -> Result<Status> {
        let dirs = self.launcher.dirs().context("$HOME is not set, so goatty has nowhere to keep its shell environment")?;
        let ctx = self.launcher.import_context().context("no home directory on this host")?;
        let host = self.settings();
        let zsh = host
            .value
            .shell
            .program
            .clone()
            .filter(|p| crate::launch::is_zsh(p) && Path::new(p).is_file())
            .or_else(|| ctx.which("zsh"))
            .or_else(|| ctx.login_shell.clone().filter(|s| crate::launch::is_zsh(s) && Path::new(s).is_file()));
        let files = zshenv_files(zsh.as_deref(), &ctx.home, &ctx.env, &dirs.zsh_dir());
        let hook = Hook::new(dirs, &ctx.home, files);
        let active = crate::zsh::hook_active(&host.value);
        let unavailable = if zsh.is_none() {
            Some("zsh isn't installed on this host".to_owned())
        } else if !host.value.shell.managed {
            Some("Needs goatty's managed zsh, which is off".to_owned())
        } else if !active {
            Some("goatty's shell here isn't zsh".to_owned())
        } else {
            None
        };
        match action {
            Action::Status => {}
            Action::On => {
                if let Some(why) = &unavailable {
                    bail!("{why}");
                }
                // entry.zsh comes with the generation: make sure it's there.
                self.launcher.apply(&host).context("couldn't build goatty's shell configuration")?;
                hook.enable()?;
            }
            Action::Off => hook.disable()?,
        }
        if action != Action::Status {
            // The .zshenv changed: bring the import up to date now (and note
            // it), so the next terminal doesn't stop to ask for a refresh.
            if let Err(e) = self.launcher.refresh(&host) {
                log::warn!("after changing .zshenv: {e:#}");
            }
        }
        Ok(hook.status(active, unavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_lines() -> [String; 2] {
        lines(Path::new("/home/u/.local/share/goatty/current/zsh/entry.zsh"), Path::new("/home/u"))
    }

    #[test]
    fn the_line_uses_tilde_when_it_can() {
        let [comment, line] = hook_lines();
        assert_eq!(comment, COMMENT);
        assert_eq!(line, "[[ -r ~/.local/share/goatty/current/zsh/entry.zsh ]] && source ~/.local/share/goatty/current/zsh/entry.zsh");
        let [_, odd] = lines(Path::new("/data/it's/goatty/current/zsh/entry.zsh"), Path::new("/home/u"));
        assert_eq!(odd, r"[[ -r '/data/it'\''s/goatty/current/zsh/entry.zsh' ]] && source '/data/it'\''s/goatty/current/zsh/entry.zsh'");
        let [_, spaced] = lines(Path::new("/home/u/my data/goatty/current/zsh/entry.zsh"), Path::new("/home/u"));
        assert!(spaced.starts_with("[[ -r '/home/u/my data/"), "{spaced}");
    }

    #[test]
    fn adding_and_removing_keeps_the_rest_byte_for_byte() {
        let l = hook_lines();
        for original in ["", "export A=1\n", "export A=1", "# top\r\nexport B=2\n\n"] {
            let added = with_lines(original, &l).unwrap();
            assert!(added.starts_with(original));
            assert!(added.ends_with(&format!("{}\n{}\n", l[0], l[1])), "{added:?}");
            assert_eq!(with_lines(&added, &l), None, "never twice");
            assert!(scan(&added, &l).0);
            let removed = without_lines(&added, &l).unwrap();
            // A missing last newline was added before goatty's lines.
            let expected = if original.is_empty() || original.ends_with('\n') { original.to_owned() } else { format!("{original}\n") };
            assert_eq!(removed, expected);
            assert_eq!(without_lines(&removed, &l), None);
        }
        // Moved into the middle: still goatty's, removed from there.
        let moved = format!("a\n{}\n{}\nb\n", l[0], l[1]);
        assert_eq!(without_lines(&moved, &l).unwrap(), "a\nb\n");
    }

    #[test]
    fn changed_lines_are_reported_not_guessed() {
        let l = hook_lines();
        let edited = format!("{}\n[[ -r ~/elsewhere/goatty/entry.zsh ]] && source ~/elsewhere/goatty/entry.zsh\n", l[0]);
        let (on, changed, _) = scan(&edited, &l);
        assert!(!on);
        assert_eq!(changed[0].0, 2);
        assert_eq!(scan(&format!("# {}\n", l[1]), &l).1.len(), 1, "commented out");
        assert!(scan("export GOATTY_X=1\n", &l).1.is_empty());
        // Removing takes goatty's comment along but leaves the changed line.
        assert_eq!(without_lines(&edited, &l).unwrap(), edited.lines().nth(1).unwrap().to_owned() + "\n");
    }

    #[test]
    fn zdotdir_set_after_goattys_line_is_found() {
        let l = hook_lines();
        for late in [
            "export ZDOTDIR=~/.config/zsh",
            "ZDOTDIR=$HOME/z",
            "typeset -gx ZDOTDIR=/z",
            "[[ -d /z ]] && export ZDOTDIR=/z",
            "unset ZDOTDIR",
        ] {
            assert_eq!(scan(&format!("{}\n{late}\n", l[1]), &l).2, [2], "{late}");
        }
        assert_eq!(scan(&format!("{}\nunset ZDOTDIR; true\n", l[1]), &l).2, [2]);
        for harmless in
            ["fpath+=($ZDOTDIR/functions)", "export A=1 # ZDOTDIR=x", "echo ok\t# ZDOTDIR=x", "# export ZDOTDIR=/z", "print $ZDOTDIR"]
        {
            assert!(scan(&format!("{}\n{harmless}\n", l[1]), &l).2.is_empty(), "{harmless}");
        }
        assert!(scan(&format!("export ZDOTDIR=/z\n{}\n", l[1]), &l).2.is_empty(), "before goatty's line it's the user's");
    }

    fn sandbox() -> (tempfile::TempDir, PathBuf, Dirs) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let dirs = Dirs::under(&tmp.path().join("xdg"));
        (tmp, home, dirs)
    }

    #[test]
    fn files_keep_permissions_links_and_backups() {
        let (tmp, home, dirs) = sandbox();
        // A dotfiles repository, linked into home.
        let repo = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("zshenv"), "export EDITOR=vi\n").unwrap();
        std::fs::set_permissions(repo.join("zshenv"), std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(repo.join("zshenv"), home.join(".zshenv")).unwrap();

        let hook = Hook::new(&dirs, &home, vec![home.join(".zshenv")]);
        assert_eq!(hook.status(true, None).state, State::Off);
        hook.enable().unwrap();
        hook.enable().unwrap();
        assert!(home.join(".zshenv").is_symlink(), "the link stays a link");
        let text = std::fs::read_to_string(repo.join("zshenv")).unwrap();
        assert_eq!(text.matches("&& source").count(), 1, "{text}");
        assert_eq!(std::fs::metadata(repo.join("zshenv")).unwrap().permissions().mode() & 0o777, 0o640);
        let status = hook.status(true, None);
        assert_eq!(status.state, State::On);
        assert_eq!(status.files_text(), "~/.zshenv");
        assert!(status.backup.as_deref().is_some_and(|b| b.ends_with(".~%.zshenv")), "{:?}", status.backup);

        hook.disable().unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("zshenv")).unwrap(), "export EDITOR=vi\n");
        let all = backups(&dirs);
        assert_eq!(all.len(), 2);
        assert_eq!(std::fs::read_to_string(&all[0]).unwrap(), "export EDITOR=vi\n", "the backup is the file before goatty changed it");
        assert!(
            std::fs::read_dir(&repo).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("goatty")),
            "no temporary files left"
        );

        // Older backups go once there are more than BACKUPS_KEPT.
        for _ in 0..BACKUPS_KEPT {
            hook.enable().unwrap();
            hook.disable().unwrap();
        }
        assert_eq!(backups(&dirs).len(), BACKUPS_KEPT);
    }

    #[test]
    fn new_files_and_folders_are_created() {
        let (_tmp, home, dirs) = sandbox();
        // ZDOTDIR's folder doesn't exist yet.
        let fresh = Hook::new(&dirs, &home, vec![home.join("zd/.zshenv")]);
        fresh.enable().unwrap();
        assert_eq!(std::fs::metadata(home.join("zd/.zshenv")).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(fresh.status(true, None).backup, None, "nothing to back up");
        fresh.disable().unwrap();
        assert_eq!(std::fs::read_to_string(home.join("zd/.zshenv")).unwrap(), "", "off again leaves it empty, not gone");
    }

    #[test]
    fn changed_lines_block_both_ways() {
        let (_tmp, home, dirs) = sandbox();
        let file = home.join(".zshenv");
        let hook = Hook::new(&dirs, &home, vec![file.clone()]);
        let changed = "[[ -r ~/x/entry.zsh ]] && source ~/x/entry.zsh # goatty\n";
        std::fs::write(&file, changed).unwrap();
        let s = hook.status(true, None);
        assert_eq!(s.state, State::Changed);
        assert_eq!(s.changed[0].to_string(), format!("~/.zshenv, line 1: {}", changed.trim_end()));
        assert!(hook.enable().unwrap_err().to_string().contains("line 1"));
        assert!(hook.disable().unwrap_err().to_string().contains("yourself"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), changed, "neither touched the file");

        // goatty's line next to a changed one: on, with the changed one shown;
        // off removes goatty's and leaves the other.
        std::fs::write(&file, format!("{changed}{}\n", hook.lines[1])).unwrap();
        let s = hook.status(true, None);
        assert_eq!((s.state, s.changed.len()), (State::On, 1));
        hook.enable().unwrap();
        hook.disable().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), changed);
        assert_eq!(hook.status(true, None).state, State::Changed);

        // goatty's comment above a line edited by hand: off leaves both.
        let edited = format!("{}\n# {}\n", hook.lines[0], hook.lines[1]);
        std::fs::write(&file, &edited).unwrap();
        assert_eq!(hook.status(true, None).state, State::Changed);
        assert!(hook.disable().is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);
        // Only goatty's comment left: off tidies it away.
        std::fs::write(&file, format!("a\n{}\n", hook.lines[0])).unwrap();
        hook.disable().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\n");
    }

    #[test]
    fn read_only_files_say_what_to_add_by_hand() {
        let (tmp, home, dirs) = sandbox();
        // Like a Nix store: a link into a folder nobody may write.
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("zshenv"), "export A=1\n").unwrap();
        std::os::unix::fs::symlink(store.join("zshenv"), home.join(".zshenv")).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o555)).unwrap();
        let hook = Hook::new(&dirs, &home, vec![home.join(".zshenv")]);
        let err = hook.enable();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        if nix_root() {
            return;
        }
        let err = err.unwrap_err().to_string();
        assert!(err.contains("add these lines") && err.contains(&hook.lines[1]), "{err}");
        assert_eq!(std::fs::read_dir(&store).unwrap().count(), 1, "no temporary file left");
    }

    /// root can write anywhere: the read-only case can't be tested.
    fn nix_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn two_files_when_zshenv_moves_zdotdir() {
        let (_tmp, home, dirs) = sandbox();
        let files = vec![home.join(".zshenv"), home.join(".config/zsh/.zshenv")];
        let hook = Hook::new(&dirs, &home, files.clone());
        std::fs::write(&files[0], "export ZDOTDIR=~/.config/zsh\n").unwrap();
        hook.enable().unwrap();
        let s = hook.status(true, None);
        assert_eq!((s.state, s.files_text()), (State::On, "~/.zshenv and ~/.config/zsh/.zshenv".to_owned()));
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        // Only one of them has it (edited by hand): on, with a warning, so
        // switching it off cleans up.
        std::fs::write(&files[1], "").unwrap();
        let s = hook.status(true, None);
        assert_eq!(s.state, State::On);
        assert!(s.warnings.iter().any(|w| w.starts_with("~/.config/zsh/.zshenv doesn't have")), "{:?}", s.warnings);
        hook.disable().unwrap();
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), "export ZDOTDIR=~/.config/zsh\n");
    }

    #[test]
    fn a_file_that_cant_be_written_puts_the_others_back() {
        if nix_root() {
            return;
        }
        let (tmp, home, dirs) = sandbox();
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("zshenv"), "").unwrap();
        std::os::unix::fs::symlink(store.join("zshenv"), home.join(".zshenv2")).unwrap();
        std::fs::write(home.join(".zshenv"), "export A=1\n").unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o555)).unwrap();
        let hook = Hook::new(&dirs, &home, vec![home.join(".zshenv"), home.join(".zshenv2")]);
        let err = hook.enable();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(home.join(".zshenv")).unwrap(), "export A=1\n", "the first file is back as it was");
        assert_eq!(hook.status(true, None).state, State::Off);
    }

    #[test]
    fn unreadable_files_are_reported() {
        let (_tmp, home, dirs) = sandbox();
        std::fs::create_dir_all(home.join(".zshenv")).unwrap();
        let s = Hook::new(&dirs, &home, vec![home.join(".zshenv")]).status(true, None);
        assert!(s.warnings.iter().any(|w| w.contains("can't be read")), "{:?}", s.warnings);
    }

    #[test]
    fn the_files_follow_zdotdir() {
        let home = Path::new("/home/u");
        let goatty = Path::new("/home/u/.local/share/goatty/current/zsh");
        let env = |z: &str| vec![("ZDOTDIR".to_owned(), z.to_owned())];
        // Without zsh: from the environment, never goatty's own directory.
        assert_eq!(zshenv_files(None, home, &[], goatty), [Path::new("/home/u/.zshenv")]);
        assert_eq!(
            zshenv_files(None, home, &env("/home/u/.config/zsh"), goatty),
            [Path::new("/home/u/.zshenv"), Path::new("/home/u/.config/zsh/.zshenv")]
        );
        assert_eq!(zshenv_files(None, home, &env(&goatty.to_string_lossy()), goatty), [Path::new("/home/u/.zshenv")]);
        // zsh agrees (when it's here), and a .zshenv that moves ZDOTDIR adds its target.
        if let Some(zsh) = crate::launch::which("zsh") {
            let tmp = tempfile::tempdir().unwrap();
            let h = tmp.path();
            assert_eq!(zshenv_files(Some(&zsh), h, &[], goatty), [h.join(".zshenv")]);
            let zd = h.join("zd");
            assert_eq!(zshenv_files(Some(&zsh), h, &env(&zd.to_string_lossy()), goatty), [h.join(".zshenv"), zd.join(".zshenv")]);
            std::fs::write(h.join(".zshenv"), "echo noise\nexport ZDOTDIR=$HOME/.config/zsh\n").unwrap();
            assert_eq!(zshenv_files(Some(&zsh), h, &[], goatty), [h.join(".zshenv"), h.join(".config/zsh/.zshenv")]);
        }
    }
}
