//! Generations: built, validated, atomically activated shell configurations.
//!
//! Applying host settings is a build-and-activate step (plan §6):
//!
//! 1. render the startup files from the settings ([`crate::zsh::render`]),
//! 2. compare with the active generation; identical → nothing to do,
//! 3. write them into a staging directory with the settings snapshot,
//! 4. validate (`zsh -n` on every startup file),
//! 5. rename the staging directory to `generations/<n>` and switch the
//!    `current` symlink to it (rename over the old link: atomic),
//! 6. keep the newest [`KEEP`] generations for rollback.
//!
//! Sessions get `ZDOTDIR=<data>/current/zsh`, so a new session always sees
//! a complete generation. Running shells already read theirs.
//!
//! A rollback switches `current` back and returns the generation, whose
//! `host.json` the caller restores as the settings file, so the next build
//! reproduces it instead of undoing the rollback.
//!
//! Each generation's `info.json` ([`Info`]) records when it was built, the
//! generation it was built on, what changed since then and its name
//! ([`crate::history`]).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::history::{self, Changes};
use crate::zsh::GeneratedFile;

/// Generations kept (the active one is never removed).
pub const KEEP: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub number: u32,
    pub path: PathBuf,
    pub created_unix: u64,
    pub active: bool,
}

impl Generation {
    pub fn zsh_dir(&self) -> PathBuf {
        self.path.join("zsh")
    }

    pub fn pwsh_dir(&self) -> PathBuf {
        self.path.join("pwsh")
    }

    /// The generation's managed shell directory (`pwsh` on Windows, `zsh` on Unix).
    pub fn shell_dir(&self) -> PathBuf {
        if cfg!(windows) { self.pwsh_dir() } else { self.zsh_dir() }
    }

    /// Whether this generation has its entry startup file.
    pub fn is_complete(&self) -> bool {
        if cfg!(windows) { self.pwsh_dir().join("init.ps1").is_file() } else { self.zsh_dir().join(".zshrc").is_file() }
    }

    /// The host settings this generation was built from.
    pub fn settings_json(&self) -> Option<String> {
        std::fs::read_to_string(self.path.join("host.json")).ok()
    }
}

/// A generation's `info.json`. `created` stays the first field: goatty
/// before #55 read it with a string search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    pub created: u64,
    /// The goatty version that built it.
    pub goatty: String,
    /// The generation that was active when it was built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Changes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The `info.json` in generation directory `dir` (defaults if unreadable).
pub fn read_info(dir: &Path) -> Info {
    std::fs::read_to_string(dir.join("info.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Replace `info.json` in `dir` (atomically: written next to it, renamed).
pub(crate) fn write_info(dir: &Path, info: &Info) -> Result<()> {
    let tmp = dir.join(format!(".info.tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(info)? + "\n")?;
    std::fs::rename(&tmp, dir.join("info.json")).with_context(|| format!("write {}", dir.join("info.json").display()))
}

/// Hold the generations lock: builders in other processes (goattyd,
/// `goatty --local`, the CLI) wait.
pub(crate) fn lock(dirs: &Dirs) -> Result<File> {
    let lock = File::create(dirs.generations_dir().join(".lock"))?;
    lock.lock()?;
    Ok(lock)
}

/// The active generation's number.
pub fn current(dirs: &Dirs) -> Option<u32> {
    #[cfg(unix)]
    let target = std::fs::read_link(dirs.current_link()).ok()?;
    #[cfg(windows)]
    let target = crate::winlink::read_junction(&dirs.current_link()).ok()?;
    target.file_name()?.to_str()?.parse().ok()
}

/// All generations, oldest first.
pub fn list(dirs: &Dirs) -> Vec<Generation> {
    let active = current(dirs);
    let mut v: Vec<Generation> = std::fs::read_dir(dirs.generations_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let number: u32 = e.file_name().to_str()?.parse().ok()?;
            let path = e.path();
            let created_unix = read_info(&path).created;
            Some(Generation { number, path, created_unix, active: active == Some(number) })
        })
        .collect();
    v.sort_by_key(|g| g.number);
    v
}

pub(crate) fn normalize_generated_text<'a>(name: &str, text: &'a str) -> &'a str {
    if name.ends_with(".ps1") { text.strip_prefix('\u{feff}').unwrap_or(text) } else { text }
}

fn write_generated_file(path: &Path, f: &GeneratedFile) -> std::io::Result<()> {
    // Windows PowerShell 5.1 decodes `.ps1` files without a UTF-8 BOM using the
    // system ANSI code page, corrupting non-ASCII paths and text. Prepend a
    // UTF-8 BOM to `.ps1` files only so PS 5.1 and PS 7 decode identically.
    if f.name.ends_with(".ps1") {
        let body = normalize_generated_text(&f.name, &f.contents);
        let mut bytes = Vec::with_capacity(3 + body.len());
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        bytes.extend_from_slice(body.as_bytes());
        std::fs::write(path, bytes)
    } else {
        std::fs::write(path, &f.contents)
    }
}

pub(crate) fn read_generated_text(path: &Path, name: &str) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(normalize_generated_text(name, &text).to_owned())
}

fn read_files(dir: &Path, like: &[GeneratedFile]) -> Option<Vec<String>> {
    like.iter().map(|f| read_generated_text(&dir.join(&f.name), &f.name).ok()).collect()
}

/// What [`activate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activated {
    pub generation: u32,
    /// `false`: the active generation already matched.
    pub created: bool,
}

/// Build and activate a generation from `files` and the settings snapshot
/// `settings_json`, unless the active one already matches both.
pub fn activate(dirs: &Dirs, files: &[GeneratedFile], settings_json: &str) -> Result<Activated> {
    let gens = dirs.generations_dir();
    std::fs::create_dir_all(&gens).with_context(|| format!("create {}", gens.display()))?;
    let _lock = lock(dirs)?;

    let shell_subdir = if cfg!(windows) { "pwsh" } else { "zsh" };
    if let Some(n) = current(dirs) {
        let path = gens.join(n.to_string());
        let same_files = read_files(&path.join(shell_subdir), files)
            .is_some_and(|old| old.iter().zip(files).all(|(o, f)| o == normalize_generated_text(&f.name, &f.contents)));
        let same_settings = std::fs::read_to_string(path.join("host.json")).is_ok_and(|s| s == settings_json);
        if same_files && same_settings {
            return Ok(Activated { generation: n, created: false });
        }
    }

    let number = list(dirs).last().map(|g| g.number).unwrap_or(0) + 1;
    let staging = gens.join(format!(".build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| -> Result<()> {
        let sdir = staging.join(shell_subdir);
        std::fs::create_dir_all(&sdir)?;
        for f in files {
            let path = sdir.join(&f.name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_generated_file(&path, f)?;
        }
        std::fs::write(staging.join("host.json"), settings_json)?;
        let version = env!("CARGO_PKG_VERSION");
        let base = current(dirs).and_then(|n| list(dirs).into_iter().find(|g| g.number == n));
        let tree = files.iter().map(|f| (f.name.clone(), normalize_generated_text(&f.name, &f.contents).to_owned())).collect();
        let info = Info {
            created: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            goatty: version.to_owned(),
            parent: base.as_ref().map(|b| b.number),
            changes: Some(base.map(|b| history::compare(&b, settings_json, &tree, version)).unwrap_or_default()),
            name: None,
        };
        write_info(&staging, &info)?;
        validate(&sdir, files)?;
        std::fs::rename(&staging, gens.join(number.to_string()))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    switch(dirs, number)?;
    prune(dirs);
    Ok(Activated { generation: number, created: true })
}

/// Syntax-check every generated shell script (`zsh -n` on Unix, PowerShell's
/// AST parser `[System.Management.Automation.Language.Parser]::ParseFile` on
/// Windows). Skipped when the shell isn't installed.
fn validate(sdir: &Path, files: &[GeneratedFile]) -> Result<()> {
    let zsh_files: Vec<&GeneratedFile> = files.iter().filter(|f| f.name.ends_with(".zsh") || f.name.starts_with(".z")).collect();
    if !zsh_files.is_empty()
        && let Some(zsh) = crate::launch::which("zsh")
    {
        for f in zsh_files {
            let out = Command::new(&zsh).arg("-n").arg(sdir.join(&f.name)).stdin(Stdio::null()).output().context("run zsh -n")?;
            if !out.status.success() {
                bail!("{} doesn't parse: {}", f.name, String::from_utf8_lossy(&out.stderr).trim());
            }
        }
    }
    let ps_files: Vec<&GeneratedFile> = files.iter().filter(|f| f.name.ends_with(".ps1")).collect();
    if !ps_files.is_empty()
        && let Some(pwsh) = crate::launch::find_powershell()
    {
        let mut script = String::from("$failed = $false\n");
        for f in &ps_files {
            let full = sdir.join(&f.name);
            let q_full = goatty_plugin::powershell::quote(&full.to_string_lossy());
            let q_name = goatty_plugin::powershell::quote(&f.name);
            script.push_str(&format!(
                "$tokens = $null; $e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile({q_full}, [ref]$tokens, [ref]$e)\n\
                 if ($e -and $e.Count -gt 0) {{ foreach ($err in $e) {{ [Console]::Error.WriteLine({q_name} + ' doesn''t parse: ' + $err.Message) }}; $failed = $true }}\n"
            ));
        }
        script.push_str("if ($failed) { exit 1 }\n");
        let mut cmd = Command::new(&pwsh);
        cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script]).stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let out = cmd.output().context("validate PowerShell syntax")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let msg = err.trim();
            if msg.is_empty() {
                bail!("PowerShell syntax validation failed ({})", out.status);
            }
            bail!("{msg}");
        }
    }
    Ok(())
}

/// Point `current` at generation `number` (relative symlink on Unix, NTFS
/// directory junction on Windows).
#[cfg(unix)]
fn switch(dirs: &Dirs, number: u32) -> Result<()> {
    let link = dirs.current_link();
    let tmp = dirs.data.join(format!(".current.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(Path::new("generations").join(number.to_string()), &tmp)?;
    std::fs::rename(&tmp, &link).with_context(|| format!("activate generation {number}"))?;
    Ok(())
}

#[cfg(windows)]
fn switch(dirs: &Dirs, number: u32) -> Result<()> {
    let target = dirs.generations_dir().join(number.to_string());
    crate::winlink::replace_junction(&target, &dirs.current_link()).with_context(|| format!("activate generation {number}"))
}

fn prune(dirs: &Dirs) {
    let gens = list(dirs);
    let excess = gens.len().saturating_sub(KEEP);
    for g in gens.iter().filter(|g| !g.active).take(excess) {
        let _ = std::fs::remove_dir_all(&g.path);
    }
}

/// Make generation `to` (default: the one before the active one) active.
/// The caller restores its settings snapshot ([`Generation::settings_json`]).
pub fn rollback(dirs: &Dirs, to: Option<u32>) -> Result<Generation> {
    let gens = list(dirs);
    let active = current(dirs);
    let target = match to {
        Some(n) => gens.iter().find(|g| g.number == n).with_context(|| format!("no generation {n}"))?,
        None => {
            let a = active.context("no active generation")?;
            gens.iter().rev().find(|g| g.number < a).context("no older generation to roll back to")?
        }
    };
    if !target.is_complete() {
        bail!("generation {} is incomplete", target.number);
    }
    // Locked like a build, so a concurrent build doesn't switch underneath.
    let _lock = lock(dirs)?;
    switch(dirs, target.number)?;
    Ok(Generation { active: true, ..target.clone() })
}

/// A changed file, as a unified diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub name: String,
    pub unified: String,
}

/// How `files` differ from the active generation (all of them are new when
/// there's none).
pub fn diff(dirs: &Dirs, files: &[GeneratedFile]) -> Vec<FileDiff> {
    let dir = dirs.shell_dir();
    files
        .iter()
        .filter_map(|f| {
            let old = read_generated_text(&dir.join(&f.name), &f.name).unwrap_or_default();
            let new_text = normalize_generated_text(&f.name, &f.contents);
            (old != new_text).then(|| FileDiff {
                name: f.name.clone(),
                unified: similar::TextDiff::from_lines(&old, new_text)
                    .unified_diff()
                    .context_radius(2)
                    .header(&format!("active/{}", f.name), &format!("new/{}", f.name))
                    .to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_name() -> &'static str {
        if cfg!(windows) { "init.ps1" } else { ".zshrc" }
    }

    fn files(rc: &str) -> Vec<GeneratedFile> {
        vec![GeneratedFile::new(entry_name(), rc), GeneratedFile::new("starship.toml", "x = 1\n")]
    }

    #[test]
    fn activate_is_idempotent_and_versioned() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        assert_eq!(current(&dirs), None);
        let one = if cfg!(windows) { "$a = 1\n" } else { "a=1\n" };
        let two = if cfg!(windows) { "$a = 2\n" } else { "a=2\n" };
        assert_eq!(diff(&dirs, &files(one)).len(), 2, "everything is new");

        assert_eq!(activate(&dirs, &files(one), "{}").unwrap(), Activated { generation: 1, created: true });
        assert_eq!(activate(&dirs, &files(one), "{}").unwrap(), Activated { generation: 1, created: false });
        let entry_path = dirs.shell_dir().join(entry_name());
        if cfg!(windows) {
            let raw = std::fs::read(&entry_path).unwrap();
            assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), ".ps1 files must include UTF-8 BOM for PS 5.1");
            assert!(!std::fs::read(dirs.shell_dir().join("starship.toml")).unwrap().starts_with(&[0xEF, 0xBB, 0xBF]));
        }
        assert_eq!(read_generated_text(&entry_path, entry_name()).unwrap(), one);
        assert!(diff(&dirs, &files(one)).is_empty());

        let d = diff(&dirs, &files(two));
        assert_eq!(d.len(), 1);
        assert!(
            d[0].unified.contains(&format!("-{}", one.trim())) && d[0].unified.contains(&format!("+{}", two.trim())),
            "{}",
            d[0].unified
        );

        // A settings-only change is a new generation too (rollback target).
        assert_eq!(activate(&dirs, &files(one), "{\"x\":1}").unwrap().generation, 2);
        assert_eq!(activate(&dirs, &files(two), "{\"x\":1}").unwrap().generation, 3);
        assert_eq!(read_generated_text(&dirs.shell_dir().join(entry_name()), entry_name()).unwrap(), two);
        assert_eq!(list(&dirs).iter().filter(|g| g.active).map(|g| g.number).collect::<Vec<_>>(), [3]);
        assert!(list(&dirs)[0].created_unix > 0);
    }

    #[test]
    fn rollback_switches_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let one = if cfg!(windows) { "$a = 1\n" } else { "a=1\n" };
        let two = if cfg!(windows) { "$a = 2\n" } else { "a=2\n" };
        let three = if cfg!(windows) { "$a = 3\n" } else { "a=3\n" };
        assert!(rollback(&dirs, None).is_err());
        activate(&dirs, &files(one), "one").unwrap();
        activate(&dirs, &files(two), "two").unwrap();
        let g = rollback(&dirs, None).unwrap();
        assert_eq!(g.number, 1);
        assert_eq!(g.settings_json().as_deref(), Some("one"));
        assert_eq!(current(&dirs), Some(1));
        assert_eq!(read_generated_text(&dirs.shell_dir().join(entry_name()), entry_name()).unwrap(), one);
        assert!(rollback(&dirs, None).is_err(), "nothing older than 1");
        assert_eq!(rollback(&dirs, Some(2)).unwrap().number, 2);
        assert!(rollback(&dirs, Some(9)).is_err());
        // Building after a rollback continues the numbering.
        assert_eq!(activate(&dirs, &files(three), "three").unwrap().generation, 3);
    }

    #[test]
    fn prunes_old_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        for i in 0..(KEEP + 4) {
            let line = if cfg!(windows) { format!("$a = {i}\n") } else { format!("a={i}\n") };
            activate(&dirs, &files(&line), "{}").unwrap();
        }
        let gens = list(&dirs);
        assert_eq!(gens.len(), KEEP);
        assert_eq!(gens.last().unwrap().number as usize, KEEP + 4);
        rollback(&dirs, Some(gens[0].number)).unwrap();
        let last = if cfg!(windows) { "$a = 'new'\n" } else { "new\n" };
        activate(&dirs, &files(last), "{}").unwrap();
        let after = list(&dirs);
        assert_eq!(after.len(), KEEP);
        assert!(after.last().unwrap().active);
    }

    #[test]
    fn invalid_config_is_not_activated() {
        if cfg!(windows) {
            if crate::launch::find_powershell().is_none() {
                return;
            }
        } else if crate::launch::which("zsh").is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let good = if cfg!(windows) { "$a = 1\n" } else { "a=1\n" };
        let bad = if cfg!(windows) { "if ( {\n" } else { "if then fi (\n" };
        activate(&dirs, &files(good), "{}").unwrap();
        let err = activate(&dirs, &files(bad), "{}").unwrap_err();
        assert!(err.to_string().contains(&format!("{} doesn't parse", entry_name())), "{err}");
        assert_eq!(current(&dirs), Some(1));
        assert_eq!(list(&dirs).len(), 1, "no half-built generation left");
    }

    #[test]
    fn bom_prefixed_ps1_plugin_file_is_idempotent_across_activate_diff_and_history() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let entry = if cfg!(windows) { "$a = 1\n" } else { "a=1\n" };
        let files =
            vec![GeneratedFile::new(entry_name(), entry), GeneratedFile::new("plugins/demo/init.ps1", "\u{feff}$x = 'caf\u{00e9}'\n")];
        assert_eq!(activate(&dirs, &files, "{}").unwrap(), Activated { generation: 1, created: true });
        // Second activate with the same BOM-prefixed `.ps1` must not create a new generation.
        assert_eq!(activate(&dirs, &files, "{}").unwrap(), Activated { generation: 1, created: false });
        assert!(diff(&dirs, &files).is_empty(), "diff must normalize BOM on both sides");
        // Stored file must have exactly one UTF-8 BOM, never doubled.
        let stored = std::fs::read(dirs.shell_dir().join("plugins/demo/init.ps1")).unwrap();
        assert!(stored.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert!(!stored[3..].starts_with(&[0xEF, 0xBB, 0xBF]));
        // Changing only settings must record 0 changed files in history.
        let g2 = activate(&dirs, &files, r#"{"x":1}"#).unwrap();
        assert_eq!(g2, Activated { generation: 2, created: true });
        let info = read_info(&dirs.generations_dir().join("2"));
        assert_eq!(info.changes.unwrap().files, 0);
    }
}
