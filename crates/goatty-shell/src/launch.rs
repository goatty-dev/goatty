//! Which shell a new session runs, and with what environment.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use goatty_config::{Effective, HostSettings, store};

use crate::dirs::Dirs;
use crate::generation::{self, Activated, FileDiff};
use crate::zsh::{self, GeneratedFile, ZshConfig};

/// A shell to spawn: what goattyd and `goatty --local` hand to the PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    /// Added to the inherited environment.
    pub env: Vec<(String, String)>,
    /// Runs goatty's managed zsh configuration.
    pub managed: bool,
}

/// Builds [`Launch`]es and the generations behind them.
#[derive(Debug, Clone)]
pub struct Launcher {
    dirs: Option<Dirs>,
    daemon_exe: Option<PathBuf>,
    /// Where imports of the user's shell setup come from.
    import: ImportFrom,
    /// Whether a Nerd Font is installed ([`crate::fonts`]).
    nerd_font: NerdFont,
}

/// How [`Launcher`] knows whether a Nerd Font is installed.
#[derive(Debug, Clone, Copy)]
enum NerdFont {
    /// Given: [`Launcher::new`] assumes none, so tests don't depend on the
    /// machine's fonts ([`Launcher::assume_nerd_font`]).
    Given(bool),
    /// Ask fontconfig (goattyd).
    Probe,
}

/// Whose shell [`Launcher`] imports from.
#[derive(Debug, Clone)]
enum ImportFrom {
    /// Nobody's: a launcher built with [`Launcher::new`] (tests, tools)
    /// never runs the developer's real login shell.
    Nobody,
    /// This process's home, login shell and environment (goattyd).
    Env,
    Given(crate::import::Context),
}

impl Launcher {
    /// `dirs`: where the managed environment lives (`None`: no `$HOME`, so
    /// no managed zsh). `daemon_exe`: the goattyd that `goatty-setup` runs.
    /// It imports nobody's shell setup ([`Self::from_env`] and
    /// [`Self::with_import_context`] do).
    pub fn new(dirs: Option<Dirs>, daemon_exe: Option<PathBuf>) -> Self {
        Self { dirs, daemon_exe, import: ImportFrom::Nobody, nerd_font: NerdFont::Given(false) }
    }

    /// Import the user's shell setup from `ctx` instead of this process's
    /// home and environment (tests, sandboxes).
    pub fn with_import_context(mut self, ctx: crate::import::Context) -> Self {
        self.import = ImportFrom::Given(ctx);
        self
    }

    /// Whether a Nerd Font is installed, instead of asking fontconfig
    /// (tests, sandboxes).
    pub fn assume_nerd_font(mut self, installed: bool) -> Self {
        self.nerd_font = NerdFont::Given(installed);
        self
    }

    /// Where the user's shell setup is imported from (`None` without
    /// `$HOME`, or for a launcher that imports nothing).
    pub fn import_context(&self) -> Option<crate::import::Context> {
        match &self.import {
            ImportFrom::Given(c) => Some(c.clone()),
            ImportFrom::Env => crate::import::Context::from_env(&self.dirs.as_ref()?.zsh_dir()),
            ImportFrom::Nobody => None,
        }
    }

    /// Bring the import of the user's shell setup up to date, if it's on
    /// (may run their shell: call off the UI thread).
    pub fn prepare_import(&self, settings: &HostSettings) {
        if !settings.shell.import.enabled {
            return;
        }
        let (Some(dirs), Some(ctx)) = (&self.dirs, self.import_context()) else { return };
        match crate::import::prepare(dirs, settings, &ctx) {
            Ok(p) => log::debug!("import: {p:?}"),
            Err(e) => log::warn!("import: {e:#}"),
        }
    }

    /// The user's dirs, importing their shell setup from this process's
    /// environment.
    pub fn from_env(daemon_exe: Option<PathBuf>) -> Self {
        Self { import: ImportFrom::Env, nerd_font: NerdFont::Probe, ..Self::new(Dirs::from_env(), daemon_exe) }
    }

    pub fn dirs(&self) -> Option<&Dirs> {
        self.dirs.as_ref()
    }

    fn render(&self, dirs: &Dirs, settings: &HostSettings) -> Vec<GeneratedFile> {
        let (plugins, _) = crate::plugins::active(dirs, settings);
        if cfg!(windows) {
            crate::powershell::render(&crate::powershell::PowerShellConfig {
                settings,
                dirs,
                daemon_exe: self.daemon_exe.as_deref(),
                plugins: &plugins,
            })
        } else {
            let nerd_font = settings.prompt.elsewhere() == goatty_config::IconsElsewhere::Auto
                && match self.nerd_font {
                    NerdFont::Given(b) => b,
                    NerdFont::Probe => crate::fonts::installed_for(&crate::starship::render(settings)),
                };
            zsh::render(&ZshConfig { settings, dirs, daemon_exe: self.daemon_exe.as_deref(), plugins: &plugins, nerd_font })
        }
    }

    /// Build and activate the generation for `host` (a no-op when the
    /// active one matches). The snapshot stored with it is the file's
    /// settings, without environment overrides, so a rollback restores
    /// exactly what was saved.
    pub fn apply(&self, host: &Effective<HostSettings>) -> Result<Activated> {
        let dirs = self.dirs.as_ref().context("no $HOME")?;
        let files = self.render(dirs, &host.value);
        generation::activate(dirs, &files, &store::to_json(&host.file.value, &host.file.unknown))
    }

    /// How the generated files would change if `settings` were applied.
    pub fn preview(&self, settings: &HostSettings) -> Vec<FileDiff> {
        match &self.dirs {
            Some(dirs) => generation::diff(dirs, &self.render(dirs, settings)),
            None => Vec::new(),
        }
    }

    /// The login shell for a new session. zsh (on Unix) or PowerShell (on
    /// Windows) gets goatty's managed configuration when `shell.managed` is on:
    /// the generation for these settings is built if needed. If that fails
    /// (say, validation), the last good generation is used, and with none, a
    /// plain shell. Other shells start as they are.
    pub fn launch(&self, host: &Effective<HostSettings>) -> Launch {
        let shell = &host.value.shell;
        let program = resolve_program(shell.program.as_deref());
        let args = if cfg!(windows) && !is_zsh(&program) { Vec::new() } else { vec!["-l".into()] };
        let mut launch = Launch { program, args, env: Vec::new(), managed: false };
        #[cfg(windows)]
        {
            if !(shell.managed && is_powershell(&launch.program)) {
                return launch;
            }
            let Some(dirs) = &self.dirs else {
                log::warn!("no user profile directory: starting plain PowerShell");
                return launch;
            };
            let started = SystemTime::now();
            if let Err(e) = self.apply(host) {
                log::warn!("couldn't build the PowerShell configuration: {e:#}");
            }
            touch_stamp(dirs, started);
            let init = dirs.pwsh_dir().join("init.ps1");
            if !init.is_file() {
                log::warn!("no usable PowerShell configuration generation: starting plain PowerShell");
                return launch;
            }
            let dot_cmd = format!(". {}", goatty_plugin::powershell::quote(&init.to_string_lossy()));
            launch.args = vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-NoExit".into(),
                "-Command".into(),
                dot_cmd,
            ];
            launch.managed = true;
            launch
        }
        #[cfg(not(windows))]
        {
            if !(shell.managed && is_zsh(&launch.program)) {
                return launch;
            }
            let Some(dirs) = &self.dirs else {
                log::warn!("no $HOME: starting plain zsh");
                return launch;
            };
            let started = SystemTime::now();
            if let Err(e) = self.apply(host) {
                log::warn!("couldn't build the shell configuration: {e:#}");
            }
            self.prepare_import(&host.value);
            touch_stamp(dirs, started);
            if !dirs.zsh_dir().join(".zshrc").is_file() {
                log::warn!("no usable shell configuration generation: starting plain zsh");
                return launch;
            }
            let zdotdir = dirs.zsh_dir();
            launch.env.push(("ZDOTDIR".to_owned(), zdotdir.to_string_lossy().into_owned()));
            // The user's own ZDOTDIR, for .zshenv to put back. Ignore our own
            // (a daemon started from inside a goatty shell mid-startup).
            if let Some(orig) = std::env::var_os("ZDOTDIR").filter(|o| Path::new(o) != zdotdir) {
                launch.env.push(("GOATTY_ORIG_ZDOTDIR".to_owned(), orig.to_string_lossy().into_owned()));
            }
            launch.managed = true;
            launch
        }
    }

    /// Bring the configuration and the import of the user's shell setup up
    /// to date, as a new session would, and note when
    /// ([`Dirs::refresh_stamp`]): `goattyd shell refresh`, which shells
    /// outside goatty ask for when a startup file changed
    /// ([`crate::everywhere`]). `started`: taken before `host` was read, so
    /// a change to `host.json` after that still counts as newer. May run the
    /// user's shell: off the UI thread.
    pub fn refresh(&self, host: &Effective<HostSettings>, started: SystemTime) -> Result<()> {
        let dirs = self.dirs.as_ref().context("$HOME is not set, so goatty has nowhere to keep its shell environment")?;
        // Also with goatty's zsh off: entry.zsh then says so.
        let built = self.apply(host).map(|_| ());
        self.prepare_import(&host.value);
        touch_stamp(dirs, started);
        built
    }

    /// Launch a direct program (`argv`, no shell) using this host's managed
    /// `PATH` (imported folders, daemon `PATH`, `shell.path`, and goatty
    /// plugin binaries in `bin/`), imported shell variables, and active
    /// plugins' environment/config variables (#241).
    pub fn launch_command(&self, host: &Effective<HostSettings>, mut argv: Vec<String>, cwd: Option<&str>) -> Result<Launch> {
        anyhow::ensure!(!argv.is_empty(), "nothing to run");
        let raw_program = argv.remove(0);
        let mut env_map = std::collections::BTreeMap::<String, String>::new();
        let (path, has_dirs) = match &self.dirs {
            Some(dirs) => {
                let settings = &host.value;
                if settings.shell.import.enabled {
                    let record = crate::import::Record::load(dirs);
                    if let Some(h) = &record.harvest {
                        for var in &h.vars {
                            if var.name == "PATH"
                                || var.name == "ZDOTDIR"
                                || var.name.starts_with("GOATTY_")
                                || settings.shell.import.skip_vars.iter().any(|s| s == &var.name)
                            {
                                continue;
                            }
                            env_map.insert(var.name.clone(), var.value.clone());
                        }
                    }
                }
                let (plugins, _) = crate::plugins::active(dirs, settings);
                let needs_gen = plugins.iter().any(|a| a.manifest.files.iter().any(|f| f.env.is_some()));
                let entry_file = if cfg!(windows) { dirs.pwsh_dir().join("init.ps1") } else { dirs.zsh_dir().join(".zshrc") };
                if needs_gen && !entry_file.is_file() {
                    let _ = self.apply(host);
                }
                let home = Dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
                for a in &plugins {
                    let mut values = a.values.clone();
                    if cfg!(windows) {
                        goatty_plugin::powershell::expand_home(&a.manifest, &mut values, home.as_deref());
                    } else {
                        goatty_plugin::zsh::expand_home(&a.manifest, &mut values, home.as_deref());
                    }
                    for e in a.manifest.env.iter().filter(|e| goatty_plugin::manifest::applies(&e.when, &values)) {
                        let v = goatty_plugin::template::expand_inline(&e.value, &a.manifest, &values, None);
                        if !v.is_empty() {
                            env_map.insert(e.name.clone(), v);
                        }
                    }
                    for f in a.manifest.files.iter().filter(|f| goatty_plugin::manifest::applies(&f.when, &values)) {
                        if let Some(var) = &f.env {
                            let file_path = dirs.shell_dir().join("plugins").join(&a.manifest.id).join(&f.name);
                            if file_path.is_file() {
                                env_map.insert(var.clone(), file_path.to_string_lossy().into_owned());
                            }
                        }
                    }
                }
                let managed = crate::snippets::managed_path(dirs, settings);
                env_map.insert("PATH".into(), managed.clone());
                (managed, true)
            }
            None => (std::env::var("PATH").unwrap_or_default(), false),
        };
        let resolved = crate::snippets::find_in_with_cwd(&path, &raw_program, cwd)
            .with_context(|| format!("{raw_program:?} isn't installed on this host (not found on PATH)"))?;
        let env = if has_dirs { env_map.into_iter().collect() } else { Vec::new() };
        Ok(Launch { program: resolved.to_string_lossy().into_owned(), args: argv, env, managed: false })
    }
}

/// Note that the configuration and the import were current as of `at`
/// (the check's start, so a file changed while it ran still counts as newer).
pub(crate) fn touch_stamp(dirs: &Dirs, at: SystemTime) {
    let path = dirs.refresh_stamp();
    let touched = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::File::options().create(true).truncate(false).write(true).open(&path))
        .and_then(|f| f.set_modified(at));
    if let Err(e) = touched {
        log::warn!("couldn't note the shell refresh in {}: {e}", path.display());
    }
}

/// Find PowerShell on this host: PowerShell 7 (`pwsh.exe`) first, then
/// Windows PowerShell 5.1 (`powershell.exe`). On non-Windows hosts checks
/// `pwsh` on PATH.
pub fn find_powershell() -> Option<String> {
    #[cfg(windows)]
    {
        if let Some(p) = which("pwsh.exe") {
            return Some(p);
        }
        for env_var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(env_var) {
                let candidate = PathBuf::from(pf).join("PowerShell").join("7").join("pwsh.exe");
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
        if let Some(p) = which("powershell.exe") {
            return Some(p);
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            let candidate = PathBuf::from(root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        which("pwsh")
    }
}

/// `program` if given; on Unix, else zsh if installed, else `$SHELL`, else `/bin/sh`;
/// on Windows, else `pwsh.exe`, else `powershell.exe`, else `%COMSPEC%` / `cmd.exe`.
pub fn resolve_program(program: Option<&str>) -> String {
    if let Some(p) = program.filter(|p| !p.is_empty()) {
        return p.to_owned();
    }
    #[cfg(windows)]
    {
        find_powershell().or_else(|| std::env::var("COMSPEC").ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        which("zsh").or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "/bin/sh".into())
    }
}

/// `shell.program` names a shell that isn't on this host: new sessions
/// would fail to start. `None` (the default shell) is always fine.
pub fn program_issue(program: Option<&str>) -> Option<goatty_config::Issue> {
    let p = program.map(str::trim).filter(|p| !p.is_empty())?;
    let is_path = p.contains('/') || (cfg!(windows) && p.contains('\\'));
    let found = if is_path { Path::new(p).is_file() } else { which(p).is_some() };
    (!found).then(|| goatty_config::Issue::new("shell.program", format!("{p:?} isn't installed on this host; new sessions won't start")))
}

pub fn is_zsh(program: &str) -> bool {
    Path::new(program).file_name().is_some_and(|n| n == "zsh")
}

/// Whether `program` is PowerShell (`pwsh`, `pwsh.exe`, `powershell`, or
/// `powershell.exe`, matched case-insensitively on the basename).
pub fn is_powershell(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    matches!(name.to_ascii_lowercase().as_str(), "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe")
}

/// Whether `program` is legacy Windows PowerShell 5.1 (`powershell` / `powershell.exe`).
pub fn is_windows_powershell(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    matches!(name.to_ascii_lowercase().as_str(), "powershell" | "powershell.exe")
}

/// Whether `program` is the shell managed by goatty on this host (`zsh` on
/// Unix, PowerShell on Windows).
pub fn is_managed_shell(program: &str) -> bool {
    if cfg!(windows) { is_powershell(program) } else { is_zsh(program) }
}

/// First `bin` in `$PATH`.
pub fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    #[cfg(windows)]
    {
        let has_ext = Path::new(bin).extension().is_some();
        let exts: Vec<String> = if has_ext {
            Vec::new()
        } else {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
                .split(';')
                .filter(|e| !e.is_empty())
                .map(|e| e.to_ascii_lowercase())
                .collect()
        };
        for dir in std::env::split_paths(&path) {
            let direct = dir.join(bin);
            if direct.is_file() {
                return Some(direct.to_string_lossy().into_owned());
            }
            for ext in &exts {
                let candidate = dir.join(format!("{bin}{ext}"));
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(program: &str) -> HostSettings {
        let mut h = HostSettings::default();
        h.shell.program = Some(program.into());
        h
    }

    #[test]
    fn missing_programs_are_reported() {
        assert!(program_issue(None).is_none());
        assert!(program_issue(Some(" ")).is_none());
        if cfg!(windows) {
            assert!(program_issue(Some("cmd.exe")).is_none(), "found on PATH");
            assert_eq!(program_issue(Some(r"C:\nonexistent\fish.exe")).unwrap().path, "shell.program");
        } else {
            assert!(program_issue(Some("/bin/sh")).is_none());
            assert!(program_issue(Some("sh")).is_none(), "found on PATH");
            assert_eq!(program_issue(Some("/nonexistent/fish")).unwrap().path, "shell.program");
        }
        assert!(program_issue(Some("goatty-no-such-shell")).is_some());
    }

    #[test]
    fn zsh_and_powershell_detection() {
        assert!(is_zsh("/usr/bin/zsh"));
        assert!(is_zsh("zsh"));
        assert!(!is_zsh("/bin/zsh5"));
        assert!(!is_zsh("/bin/bash"));

        assert!(is_powershell("pwsh"));
        assert!(is_powershell("pwsh.exe"));
        assert!(is_powershell(r"C:\Program Files\PowerShell\7\PWSH.EXE"));
        assert!(is_powershell(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"));
        assert!(is_windows_powershell("powershell.exe"));
        assert!(!is_windows_powershell("pwsh.exe"));
        assert!(!is_powershell("cmd.exe"));
        assert!(!is_powershell("/bin/bash"));
    }

    #[test]
    fn unmanaged_shells_start_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let l = Launcher::new(Some(Dirs::under(tmp.path())), None);
        if cfg!(windows) {
            let launch = l.launch(&Effective::plain(host("cmd.exe")));
            assert_eq!(launch, Launch { program: "cmd.exe".into(), args: vec![], env: vec![], managed: false });
        } else {
            let launch = l.launch(&Effective::plain(host("/bin/sh")));
            assert_eq!(launch, Launch { program: "/bin/sh".into(), args: vec!["-l".into()], env: vec![], managed: false });
        }
        assert!(!tmp.path().join("data").exists(), "nothing written for other shells");
    }

    #[cfg(unix)]
    #[test]
    fn managed_zsh_uses_the_current_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let l = Launcher::new(Some(dirs.clone()), None);
        let mut h = Effective::plain(host("/usr/bin/zsh"));
        let launch = l.launch(&h);
        assert!(launch.managed);
        assert_eq!(launch.env[0], ("ZDOTDIR".into(), dirs.zsh_dir().to_string_lossy().into_owned()));
        assert!(dirs.zsh_dir().join(".zshrc").is_file());
        assert_eq!(generation::current(&dirs), Some(1));

        // Same settings: same generation. Changed: preview shows it, the next launch builds it.
        l.launch(&h);
        assert_eq!(generation::current(&dirs), Some(1));
        h.value.history.size = 777;
        h.file.value.history.size = 777;
        let diff = l.preview(&h.value);
        assert_eq!(diff.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["settings.zsh"]);
        l.launch(&h);
        assert_eq!(generation::current(&dirs), Some(2));
        let snapshot = generation::list(&dirs).pop().unwrap().settings_json().unwrap();
        assert!(snapshot.contains("777"), "{snapshot}");

        let mut off = h.clone();
        off.value.shell.managed = false;
        assert!(!l.launch(&off).managed);
        assert!(!Launcher::new(None, None).launch(&h).managed, "no HOME: plain zsh");
    }

    #[cfg(windows)]
    #[test]
    fn managed_powershell_uses_the_current_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let l = Launcher::new(Some(dirs.clone()), None);
        let mut h = Effective::plain(host("pwsh.exe"));
        let launch = l.launch(&h);
        assert!(launch.managed);
        assert!(launch.args.iter().any(|a| a == "-NoProfile"));
        assert!(launch.args.iter().any(|a| a == "-NoExit"));
        assert!(launch.args.iter().any(|a| a.contains("init.ps1")));
        assert!(dirs.pwsh_dir().join("init.ps1").is_file());
        assert_eq!(generation::current(&dirs), Some(1));

        l.launch(&h);
        assert_eq!(generation::current(&dirs), Some(1));
        h.value.history.size = 777;
        h.file.value.history.size = 777;
        let diff = l.preview(&h.value);
        assert_eq!(diff.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["settings.ps1"]);
        l.launch(&h);
        assert_eq!(generation::current(&dirs), Some(2));

        let mut off = h.clone();
        off.value.shell.managed = false;
        assert!(!l.launch(&off).managed);
        assert!(!Launcher::new(None, None).launch(&h).managed);
    }

    #[cfg(unix)]
    #[test]
    fn launch_command_finds_managed_bin_and_shell_path_and_sets_env() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        std::fs::create_dir_all(dirs.bin_dir()).unwrap();
        let plugin_tool = dirs.bin_dir().join("goatty-test-tool");
        std::fs::write(&plugin_tool, "#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&plugin_tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let extra_dir = tmp.path().join("extra-bin");
        std::fs::create_dir_all(&extra_dir).unwrap();
        let extra_tool = extra_dir.join("goatty-extra-tool");
        std::fs::write(&extra_tool, "#!/bin/sh\necho extra\n").unwrap();
        std::fs::set_permissions(&extra_tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut h = HostSettings::default();
        h.shell.path.push(extra_dir.to_string_lossy().into_owned());
        let eff = Effective::plain(h);
        let l = Launcher::new(Some(dirs.clone()), None);

        let launch = l.launch_command(&eff, vec!["goatty-test-tool".into(), "--flag".into()], None).unwrap();
        assert_eq!(launch.program, plugin_tool.to_string_lossy());
        assert_eq!(launch.args, ["--flag"]);
        assert!(!launch.managed);
        let path_val = launch.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap();
        assert!(path_val.contains(dirs.bin_dir().to_str().unwrap()), "{path_val}");
        assert!(path_val.contains(extra_dir.to_str().unwrap()), "{path_val}");

        let launch2 = l.launch_command(&eff, vec!["goatty-extra-tool".into()], None).unwrap();
        assert_eq!(launch2.program, extra_tool.to_string_lossy());

        // Relative path resolves against cwd.
        let launch3 = l.launch_command(&eff, vec!["./goatty-extra-tool".into()], Some(extra_dir.to_str().unwrap())).unwrap();
        assert_eq!(std::fs::canonicalize(&launch3.program).unwrap(), std::fs::canonicalize(&extra_tool).unwrap());

        // Missing tool reports a clear error.
        let err = l.launch_command(&eff, vec!["goatty-no-such-program-xyz".into()], None).unwrap_err();
        assert!(err.to_string().contains("not found on PATH"), "{err}");
    }
}
