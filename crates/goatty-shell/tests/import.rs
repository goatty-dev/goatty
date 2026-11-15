//! Importing the user's shell setup, run for real: zsh and bash with
//! startup files in a sandboxed `$HOME`, harvested the way goattyd does.
//!
//! Skipped (with a note) when zsh or bash isn't installed; CI installs both.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use goatty_config::{HostSettings, ImportFrom};
use goatty_shell::Dirs;
use goatty_shell::import::{self, AliasKind, Context, Kind, Prepared, Record};

fn find(bin: &str) -> Option<String> {
    let found = goatty_shell::launch::which(bin);
    if found.is_none() {
        eprintln!("{bin} not installed; skipping");
    }
    found
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    sysbin: PathBuf,
    dirs: Dirs,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let sysbin = tmp.path().join("sysbin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sysbin).unwrap();
        for tool in
            ["zsh", "bash", "sleep", "cat", "mkdir", "grep", "sed", "mv", "rm", "dircolors", "lesspipe", "id", "uname", "tty", "tput"]
        {
            if let Some(p) = ["/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(tool)).find(|p| p.exists()) {
                std::os::unix::fs::symlink(p, sysbin.join(tool)).unwrap();
            }
        }
        let dirs = Dirs::under(&tmp.path().join("xdg"));
        Self { _tmp: tmp, home, sysbin, dirs }
    }

    fn file(&self, rel: &str, contents: &str) {
        let p = self.home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    fn ctx(&self, login: &str) -> Context {
        let env = vec![
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            ("PATH".into(), self.sysbin.to_string_lossy().into_owned()),
            ("USER".into(), "tester".into()),
            ("GOATTY_SOCKET".into(), "/nonexistent/goattyd.sock".into()),
        ];
        Context::new(self.home.clone(), Some(self.sysbin.join(login).to_string_lossy().into_owned()), env, &self.dirs.zsh_dir())
    }

    fn harvest(&self, login: &str, from: ImportFrom, timeout: Duration) -> Result<import::Harvest, import::Failure> {
        let ctx = self.ctx(login);
        let src = import::source(from, &ctx).unwrap();
        import::harvest(&src, &ctx, &self.dirs.import_dir(), timeout, None)
    }
}

fn var<'a>(h: &'a import::Harvest, name: &str) -> Option<&'a str> {
    h.vars.iter().find(|v| v.name == name).map(|v| v.value.as_str())
}

fn alias<'a>(h: &'a import::Harvest, name: &str) -> Option<(&'a str, AliasKind)> {
    h.aliases.iter().find(|a| a.name == name).map(|a| (a.value.as_str(), a.kind))
}

/// An oh-my-zsh-style `.zshrc`: a framework with a theme, plugins' aliases
/// and a global alias, an installer's PATH line, a sourced env file, noise
/// on stdout and stderr. The harvest sees what the user's terminal would.
#[test]
fn zsh_harvest_sees_what_the_zshrc_sets_up() {
    if find("zsh").is_none() {
        return;
    }
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.home.join(".cargo/bin")).unwrap();
    sb.file(".zshenv", "export FROM_ZSHENV=1\n");
    sb.file(".cargo/env", "case \":$PATH:\" in *\":$HOME/.cargo/bin:\"*) ;; *) export PATH=\"$HOME/.cargo/bin:$PATH\" ;; esac\n");
    sb.file(
        ".oh-my-zsh/oh-my-zsh.sh",
        "PROMPT='omz> '\nalias gst='git status'\nalias -g G='| grep'\nalias -s pdf=zathura\nexport LESS=-R\nexport ZSH_CACHE_DIR=$ZSH/cache\n",
    );
    sb.file(
        ".zshrc",
        "export ZSH=\"$HOME/.oh-my-zsh\"\nsource $ZSH/oh-my-zsh.sh\n. \"$HOME/.cargo/env\"\nexport EDITOR=nvim\nexport MULTI=$'a\\nb'\n\
         alias ll='ls -l'\nalias -- -='cd -'\n[[ -n $GOATTY_IMPORT ]] && export SAW_MARKER=1\necho 'welcome!'\necho 'a warning' >&2\n\
         typeset -i COUNT=3; export COUNT\n",
    );
    let h = sb.harvest("zsh", ImportFrom::Auto, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(h.source.kind, Kind::Zsh);
    assert_eq!(var(&h, "EDITOR"), Some("nvim"));
    assert_eq!(var(&h, "MULTI"), Some("a\nb"));
    assert_eq!(var(&h, "FROM_ZSHENV"), Some("1"), "the whole startup: .zshenv too");
    assert_eq!(var(&h, "LESS"), Some("-R"), "what the framework exports");
    assert_eq!(var(&h, "SAW_MARKER"), Some("1"), "GOATTY_IMPORT tells the .zshrc");
    assert_eq!(var(&h, "COUNT"), Some("3"), "exported integers");
    for skipped in ["ZSH", "ZSH_CACHE_DIR", "PWD", "SHLVL", "HOME", "PATH", "GOATTY_IMPORT", "GOATTY_SOCKET", "DISABLE_AUTO_UPDATE"] {
        assert_eq!(var(&h, skipped), None, "{skipped}: {:?}", h.vars);
    }
    // First: the system's profile.d may add folders of its own after it.
    assert_eq!(h.added_path().first().copied(), Some(&*sb.home.join(".cargo/bin").to_string_lossy()), "{:?}", h.path);
    assert!(h.path.contains(&sb.sysbin.to_string_lossy().into_owned()), "{:?}", h.path);
    assert_eq!(alias(&h, "gst"), Some(("git status", AliasKind::Regular)));
    assert_eq!(alias(&h, "ll"), Some(("ls -l", AliasKind::Regular)));
    assert_eq!(alias(&h, "-"), Some(("cd -", AliasKind::Regular)));
    assert_eq!(alias(&h, "G"), Some(("| grep", AliasKind::Global)));
    assert_eq!(alias(&h, "pdf"), Some(("zathura", AliasKind::Suffix)));
    let own = |name: &str| h.aliases.iter().find(|a| a.name == name).map(|a| a.own);
    assert_eq!((own("ll"), own("gst")), (Some(true), Some(false)), "the .zshrc's own, not the framework's");
    assert!(h.stderr.contains("a warning"), "{}", h.stderr);
    assert!(!h.stderr.contains("welcome"), "stdout isn't kept: {}", h.stderr);
}

/// Debian's `.bashrc` returns early when not interactive and a login bash
/// reads it only through `.profile`; both ways work, and it runs once.
#[test]
fn bash_harvest_reads_bashrc_once() {
    if find("bash").is_none() {
        return;
    }
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.home.join(".local/bin")).unwrap();
    let bashrc = "case $- in *i*) ;; *) return;; esac\nexport RUNS=$((RUNS+1))\nalias ll='ls -alF'\nexport PATH=\"$HOME/.local/bin:$PATH\"\n\
                  alias ls='ls --color=auto'\nexport EDITOR=vim\n";
    sb.file(".bashrc", bashrc);
    // A .profile that doesn't mention it: goatty sources it like a terminal would.
    sb.file(".profile", "export FROM_PROFILE=1\n");
    let h = sb.harvest("bash", ImportFrom::Auto, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(h.source.kind, Kind::Bash);
    assert_eq!(var(&h, "FROM_PROFILE"), Some("1"));
    assert_eq!(var(&h, "RUNS"), Some("1"));
    assert_eq!(var(&h, "EDITOR"), Some("vim"));
    assert_eq!(alias(&h, "ll"), Some(("ls -alF", AliasKind::Regular)));
    assert_eq!(h.added_path().first().copied(), Some(&*sb.home.join(".local/bin").to_string_lossy()), "{:?}", h.path);
    assert!(!h.stderr.contains("job control"), "bash's noise without a terminal is dropped: {}", h.stderr);

    // Debian's .profile sources .bashrc itself: still once.
    sb.file(".profile", "export FROM_PROFILE=1\nif [ -n \"$BASH_VERSION\" ] && [ -f \"$HOME/.bashrc\" ]; then . \"$HOME/.bashrc\"; fi\n");
    let h = sb.harvest("bash", ImportFrom::Auto, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(var(&h, "RUNS"), Some("1"));
}

/// `set -o noclobber` in the startup doesn't stop the dump from writing
/// into the (already created, private) file.
#[test]
fn noclobber_in_the_startup_is_fine() {
    let sb = Sandbox::new();
    if find("bash").is_some() {
        sb.file(".bashrc", "set -o noclobber\nexport FROM_BASHRC=1\n");
        let h = sb.harvest("bash", ImportFrom::Bash, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
        assert_eq!(var(&h, "FROM_BASHRC"), Some("1"));
    }
    if find("zsh").is_some() {
        sb.file(".zshrc", "setopt no_clobber\nexport FROM_ZSHRC=1\n");
        let h = sb.harvest("zsh", ImportFrom::Zsh, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
        assert_eq!(var(&h, "FROM_ZSHRC"), Some("1"));
    }
}

/// A zsh login shell whose user only ever configured bash: Automatic takes
/// bash's files; choosing zsh takes zsh's.
#[test]
fn auto_falls_back_to_bash_files() {
    if find("zsh").is_none() || find("bash").is_none() {
        return;
    }
    let sb = Sandbox::new();
    sb.file(".bashrc", "export FROM_BASHRC=1\n");
    let h = sb.harvest("zsh", ImportFrom::Auto, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!((h.source.kind, var(&h, "FROM_BASHRC")), (Kind::Bash, Some("1")));
    let h = sb.harvest("zsh", ImportFrom::Zsh, Duration::from_secs(20)).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!((h.source.kind, var(&h, "FROM_BASHRC")), (Kind::Zsh, None));
}

/// A startup that waits forever is killed (with what it started), one that
/// replaces itself (`exec tmux`) or exits never reports: both are failures
/// that say why.
#[test]
fn hanging_and_exiting_startups_fail_cleanly() {
    if find("zsh").is_none() {
        return;
    }
    let sb = Sandbox::new();
    sb.file(".zshrc", "sleep 300 &\nsleep 300\n");
    let start = Instant::now();
    let f = sb.harvest("zsh", ImportFrom::Zsh, Duration::from_secs(1)).unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(10), "killed at the timeout: {:?}", start.elapsed());
    assert!(f.message.contains("didn't finish"), "{}", f.message);

    sb.file(".zshrc", "exec sleep 0\n");
    let f = sb.harvest("zsh", ImportFrom::Zsh, Duration::from_secs(20)).unwrap_err();
    assert!(f.message.contains("exited before"), "{}", f.message);
    sb.file(".zshrc", "exit 3\n");
    assert!(sb.harvest("zsh", ImportFrom::Zsh, Duration::from_secs(20)).is_err());
    let leftovers: Vec<_> = std::fs::read_dir(sb.dirs.import_dir()).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(leftovers.iter().all(|n| n == ".lock"), "temporary files are removed: {leftovers:?}");
}

/// `prepare` harvests once, then uses the cache until a startup file
/// changes; a failure keeps the previous harvest and isn't retried for
/// every new tab.
#[test]
fn prepare_caches_until_a_startup_file_changes() {
    if find("zsh").is_none() {
        return;
    }
    let sb = Sandbox::new();
    let ctx = sb.ctx("zsh");
    let mut settings = HostSettings::default();
    settings.shell.import.enabled = true;
    sb.file(".zshrc", "export EDITOR=nvim\n");
    assert_eq!(import::prepare(&sb.dirs, &settings, &ctx).unwrap(), Prepared::Harvested);
    let text = std::fs::read_to_string(sb.dirs.import_file()).unwrap();
    assert!(text.contains("'EDITOR' 'nvim'"), "{text}");
    assert_eq!(import::prepare(&sb.dirs, &settings, &ctx).unwrap(), Prepared::Cached);

    // mtime granularity: make sure the change is visible.
    std::thread::sleep(Duration::from_millis(20));
    sb.file(".zshrc", "export EDITOR=hx\n");
    assert_eq!(import::prepare(&sb.dirs, &settings, &ctx).unwrap(), Prepared::Harvested);
    assert!(std::fs::read_to_string(sb.dirs.import_file()).unwrap().contains("'EDITOR' 'hx'"));

    std::thread::sleep(Duration::from_millis(20));
    sb.file(".zshrc", "export EDITOR=broken\nexit 1\n");
    assert_eq!(import::prepare(&sb.dirs, &settings, &ctx).unwrap(), Prepared::Failed);
    let record = Record::load(&sb.dirs);
    assert!(record.failure.is_some());
    assert_eq!(record.harvest.as_ref().and_then(|h| var(h, "EDITOR")), Some("hx"), "the last good harvest stays");
    assert!(std::fs::read_to_string(sb.dirs.import_file()).unwrap().contains("'EDITOR' 'hx'"));
    assert_eq!(import::prepare(&sb.dirs, &settings, &ctx).unwrap(), Prepared::Cached, "not retried until something changes");

    let status = import::status(&sb.dirs, &settings, true, &ctx, false);
    assert!(status.enabled && status.failure.is_some() && status.harvest.is_some());
    assert_eq!(status.files.iter().filter(|f| f.exists).map(|f| f.path.as_str()).collect::<Vec<_>>(), ["~/.zshrc"]);
}
