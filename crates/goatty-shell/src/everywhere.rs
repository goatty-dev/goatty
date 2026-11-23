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

/// Backups of the `.zshenv` kept.
const BACKUPS_KEPT: usize = 10;

/// What to do with the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    On,
    Off,
}

/// Whether the user's `.zshenv` has goatty's line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not there: only goatty's own sessions run goatty's shell.
    Off,
    /// Exactly goatty's line is there (wherever in the file).
    On,
    /// A line mentions goatty's `entry.zsh` but isn't goatty's line (the
    /// user edited, commented out or copied it): goatty leaves it alone.
    Changed,
}

/// The line in the user's `.zshenv`, for the settings window and
/// `goattyd shell everywhere status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The file goatty adds its line to, for people (`~/.zshenv`).
    pub file: String,
    pub state: State,
    /// Exactly what goatty adds: the comment and the line.
    pub lines: Vec<String>,
    /// Lines that mention goatty's `entry.zsh` but aren't its line:
    /// (line number from 1, the line).
    #[serde(default)]
    pub changed: Vec<(usize, String)>,
    /// Things to fix by hand: a line after goatty's that sets `ZDOTDIR`
    /// (it would take effect after goatty's shell took over).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// goatty's managed zsh is on and goatty's shell is zsh: the line
    /// makes shells goatty's (else it's there but does nothing).
    pub active: bool,
    /// Why it can't be turned on here; turning it off always works.
    pub unavailable: Option<String>,
    /// The newest backup of the file, for people.
    #[serde(default)]
    pub backup: Option<String>,
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

/// The `.zshenv` a new interactive zsh reads: `$ZDOTDIR/.zshenv` when the
/// environment (or `/etc/zshenv`) sets `ZDOTDIR`, else `~/.zshenv`. Asks
/// `zsh` itself (`zsh -f` still reads `/etc/zshenv`), with `env` minus
/// goatty's own `ZDOTDIR` (`goatty_dir`); without zsh, or when that fails,
/// decides from `env`.
pub fn zshenv_file(zsh: Option<&str>, home: &Path, env: &[(String, String)], goatty_dir: &Path) -> PathBuf {
    let env: Vec<&(String, String)> =
        env.iter().filter(|(k, v)| !k.starts_with("GOATTY_") && !(k == "ZDOTDIR" && Path::new(v) == goatty_dir)).collect();
    let probed = zsh.and_then(|zsh| {
        let mut cmd = Command::new(zsh);
        cmd.args(["-f", "-c", "builtin print -rn -- ${ZDOTDIR:-$HOME}"])
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .env("HOME", home)
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let out = output_within(cmd, Duration::from_secs(3))?;
        let dir = String::from_utf8(out).ok()?;
        (!dir.is_empty() && Path::new(&dir).is_absolute() && Path::new(&dir) != goatty_dir).then(|| PathBuf::from(dir))
    });
    let dir = probed.unwrap_or_else(|| {
        env.iter().find(|(k, v)| k == "ZDOTDIR" && Path::new(v).is_absolute()).map_or_else(|| home.to_path_buf(), |(_, v)| PathBuf::from(v))
    });
    dir.join(".zshenv")
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

/// What `text` (a `.zshenv`) says about goatty's `lines`.
fn scan(text: &str, lines: &[String; 2]) -> (State, Vec<(usize, String)>, Vec<String>) {
    let hook = lines[1].as_str();
    let mut on = false;
    let mut changed = Vec::new();
    let mut warnings = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_end();
        if t == hook {
            on = true;
        } else if t.contains("goatty") && t.contains("entry.zsh") {
            changed.push((i + 1, t.to_owned()));
        } else if on && t.contains("ZDOTDIR") && !t.trim_start().starts_with('#') {
            warnings.push(format!(
                "Line {} sets ZDOTDIR after goatty's line, so it only applies once goatty's shell has taken over: move goatty's line below it",
                i + 1
            ));
        }
    }
    let state = match (on, changed.is_empty()) {
        (true, _) => State::On,
        (false, false) => State::Changed,
        (false, true) => State::Off,
    };
    (state, changed, warnings)
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

/// Copy `file` to the backups (private), keeping the newest [`BACKUPS_KEPT`].
fn backup(dirs: &Dirs, file: &Path) -> Result<Option<PathBuf>> {
    let Ok(text) = std::fs::read(file) else { return Ok(None) };
    let dir = dirs.zshenv_backups();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("zshenv").trim_start_matches('.');
    let path = dir.join(format!("{name}.{}.{:09}", now.as_secs(), now.subsec_nanos()));
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
    all.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    all
}

/// Replace `file` with `text`: through a temporary file next to the real
/// file (a link from a dotfiles repository stays a link), with the old
/// file's permissions (new: 0644).
fn write(file: &Path, text: &str) -> Result<()> {
    let real = if file.is_symlink() {
        std::fs::canonicalize(file).with_context(|| format!("{} is a link to a file that isn't there", file.display()))?
    } else {
        file.to_path_buf()
    };
    let mode = std::fs::metadata(&real).map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let dir = real.parent().context("the file has no folder")?;
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("zshenv");
    let tmp = dir.join(format!(".{name}.goatty-{}", std::process::id()));
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

/// Where the hook lives on one host: its `.zshenv`, goatty's lines.
pub struct Hook<'a> {
    pub dirs: &'a Dirs,
    pub home: &'a Path,
    pub file: PathBuf,
    pub lines: [String; 2],
}

impl<'a> Hook<'a> {
    pub fn new(dirs: &'a Dirs, home: &'a Path, file: PathBuf) -> Self {
        let lines = lines(&dirs.zsh_dir().join("entry.zsh"), home);
        Self { dirs, home, file, lines }
    }

    /// What the file has now. `active`: goatty's zsh is on here;
    /// `unavailable`: why it can't be turned on.
    pub fn status(&self, active: bool, unavailable: Option<String>) -> Status {
        let text = std::fs::read_to_string(&self.file).unwrap_or_default();
        let (state, changed, warnings) = scan(&text, &self.lines);
        Status {
            file: shown(&self.file, self.home),
            state,
            lines: self.lines.to_vec(),
            changed,
            warnings,
            active,
            unavailable,
            backup: backups(self.dirs).pop().map(|p| shown(&p, self.home)),
        }
    }

    /// Add goatty's lines at the end (a backup first). Refuses when a
    /// changed goatty line is there: that's the user's to sort out.
    pub fn enable(&self) -> Result<()> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.file.display())),
        };
        let (_, changed, _) = scan(&text, &self.lines);
        if let Some((n, line)) = changed.first() {
            bail!(
                "{} has a goatty line that was changed (line {n}: {line}). Fix or remove it there, then try again",
                shown(&self.file, self.home)
            );
        }
        let Some(new) = with_lines(&text, &self.lines) else { return Ok(()) };
        backup(self.dirs, &self.file)?;
        write(&self.file, &new).map_err(|e| self.manual(e, "add these lines at its end"))
    }

    /// Remove exactly goatty's lines (a backup first); the rest of the file
    /// stays byte for byte. Refuses when only a changed goatty line is
    /// there.
    pub fn disable(&self) -> Result<()> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.file.display())),
        };
        let Some(new) = without_lines(&text, &self.lines) else {
            let (_, changed, _) = scan(&text, &self.lines);
            if let Some((n, line)) = changed.first() {
                bail!(
                    "{} has a goatty line goatty didn't write (line {n}: {line}). Remove it there yourself",
                    shown(&self.file, self.home)
                );
            }
            return Ok(());
        };
        backup(self.dirs, &self.file)?;
        write(&self.file, &new).map_err(|e| self.manual(e, "remove these lines from it"))
    }

    /// A write that failed: say what to do by hand.
    fn manual(&self, e: anyhow::Error, what: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "couldn't change {} ({e:#}). If a dotfiles tool manages it, {what} there:\n{}\n{}",
            shown(&self.file, self.home),
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
        let zsh = ctx.which("zsh").or_else(|| ctx.login_shell.clone().filter(|s| crate::launch::is_zsh(s)));
        let file = zshenv_file(zsh.as_deref(), &ctx.home, &ctx.env, &dirs.zsh_dir());
        let hook = Hook::new(dirs, &ctx.home, file);
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
            assert_eq!(scan(&added, &l).0, State::On);
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
        let (state, changed, _) = scan(&edited, &l);
        assert_eq!(state, State::Changed);
        assert_eq!(changed[0].0, 2);
        let commented = format!("# {}\n", l[1]);
        assert_eq!(scan(&commented, &l).0, State::Changed);
        assert_eq!(scan("export GOATTY_X=1\n", &l).0, State::Off);
        // Removing takes goatty's comment along but leaves the changed line.
        assert_eq!(without_lines(&edited, &l).unwrap(), edited.lines().nth(1).unwrap().to_owned() + "\n");
        // A ZDOTDIR set after goatty's line is worth a warning.
        let late = format!("{}\nexport ZDOTDIR=~/.config/zsh\n", l[1]);
        assert_eq!(scan(&late, &l).2.len(), 1);
        let early = format!("export ZDOTDIR=~/.config/zsh\n{}\n", l[1]);
        assert!(scan(&early, &l).2.is_empty());
    }

    #[test]
    fn files_keep_permissions_links_and_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let dirs = Dirs::under(&tmp.path().join("xdg"));
        // A dotfiles repository, linked into home.
        let repo = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("zshenv"), "export EDITOR=vi\n").unwrap();
        std::fs::set_permissions(repo.join("zshenv"), std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(repo.join("zshenv"), home.join(".zshenv")).unwrap();

        let hook = Hook::new(&dirs, &home, home.join(".zshenv"));
        assert_eq!(hook.status(true, None).state, State::Off);
        hook.enable().unwrap();
        hook.enable().unwrap();
        assert!(home.join(".zshenv").is_symlink(), "the link stays a link");
        let text = std::fs::read_to_string(repo.join("zshenv")).unwrap();
        assert_eq!(text.matches("&& source").count(), 1, "{text}");
        assert_eq!(std::fs::metadata(repo.join("zshenv")).unwrap().permissions().mode() & 0o777, 0o640);
        let status = hook.status(true, None);
        assert_eq!(status.state, State::On);
        assert_eq!(status.file, "~/.zshenv");
        assert!(status.backup.is_some());

        hook.disable().unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("zshenv")).unwrap(), "export EDITOR=vi\n");
        assert_eq!(backups(&dirs).len(), 2);
        let first = std::fs::read_to_string(&backups(&dirs)[0]).unwrap();
        assert_eq!(first, "export EDITOR=vi\n", "the backup is the file before goatty changed it");

        // No file yet: created (0644); off again leaves it empty, not gone.
        let fresh = Hook::new(&dirs, &home, home.join("zd/.zshenv"));
        std::fs::create_dir_all(home.join("zd")).unwrap();
        fresh.enable().unwrap();
        assert_eq!(std::fs::metadata(home.join("zd/.zshenv")).unwrap().permissions().mode() & 0o777, 0o644);
        fresh.disable().unwrap();
        assert_eq!(std::fs::read_to_string(home.join("zd/.zshenv")).unwrap(), "");

        // A changed line: neither on nor off touches the file.
        let changed = "[[ -r ~/x/entry.zsh ]] && source ~/x/entry.zsh # goatty\n";
        std::fs::write(home.join("zd/.zshenv"), changed).unwrap();
        assert!(fresh.enable().unwrap_err().to_string().contains("line 1"));
        assert!(fresh.disable().unwrap_err().to_string().contains("didn't write"));
        assert_eq!(std::fs::read_to_string(home.join("zd/.zshenv")).unwrap(), changed);
    }

    #[test]
    fn the_file_follows_zdotdir() {
        let home = Path::new("/home/u");
        let goatty = Path::new("/home/u/.local/share/goatty/current/zsh");
        let env = |z: &str| vec![("ZDOTDIR".to_owned(), z.to_owned())];
        // Without zsh: from the environment, never goatty's own directory.
        assert_eq!(zshenv_file(None, home, &[], goatty), Path::new("/home/u/.zshenv"));
        assert_eq!(zshenv_file(None, home, &env("/home/u/.config/zsh"), goatty), Path::new("/home/u/.config/zsh/.zshenv"));
        assert_eq!(zshenv_file(None, home, &env(&goatty.to_string_lossy()), goatty), Path::new("/home/u/.zshenv"));
        // zsh agrees (when it's here).
        if let Some(zsh) = crate::launch::which("zsh") {
            let tmp = tempfile::tempdir().unwrap();
            let h = tmp.path();
            assert_eq!(zshenv_file(Some(&zsh), h, &[], goatty), h.join(".zshenv"));
            let zd = h.join("zd");
            assert_eq!(zshenv_file(Some(&zsh), h, &env(&zd.to_string_lossy()), goatty), zd.join(".zshenv"));
            assert_eq!(zshenv_file(Some(&zsh), h, &env(&goatty.to_string_lossy()), goatty), h.join(".zshenv"));
        }
    }
}
