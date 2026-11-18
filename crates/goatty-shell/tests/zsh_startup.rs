//! The managed zsh, run for real in a sandboxed `$HOME`: startup order,
//! the user's files, ZDOTDIR hand-back, feature detection, `goatty-setup`,
//! and the OSC 133 / OSC 7 stream through a PTY.
//!
//! Skipped (with a note) when zsh isn't installed; CI installs it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use goatty_config::{Effective, HostSettings, ZshSettings};
use goatty_shell::{Dirs, Launch, Launcher};
use goatty_term::TermSize;
use goatty_term::pty::{Pty, ShellCommand};

fn zsh() -> Option<String> {
    let found = goatty_shell::launch::which("zsh");
    if found.is_none() {
        eprintln!("zsh not installed; skipping");
    }
    found
}

const BASIC_UTILS: &[&str] = &[
    "awk",
    "basename",
    "cat",
    "chmod",
    "cp",
    "cut",
    "date",
    "dircolors",
    "dirname",
    "env",
    "getent",
    "grep",
    "head",
    "id",
    "ln",
    "locale",
    "ls",
    "mkdir",
    "mktemp",
    "mv",
    "rm",
    "sed",
    "sort",
    "stat",
    "tail",
    "touch",
    "tput",
    "tr",
    "uname",
    "wc",
];

struct Sandbox {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    dirs: Dirs,
    zsh: String,
}

impl Sandbox {
    fn new(zsh: String) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // macOS's temp dir is behind a symlink (/var → /private/var); the
        // shell sees the real path, so HOME must be it for `~` to show.
        let home = tmp.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(home.join("userbin")).unwrap();
        // PATH holds zsh and basic utilities (compinit needs mv, grep, ...)
        // but not the host's starship/fzf/fd, which would change the
        // results. Some distros' /etc/profile still appends /usr/bin.
        let sysbin = home.join("sysbin");
        std::fs::create_dir_all(&sysbin).unwrap();
        std::os::unix::fs::symlink(&zsh, sysbin.join("zsh")).unwrap();
        for tool in BASIC_UTILS {
            if let Some(p) = ["/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(tool)).find(|p| p.exists()) {
                std::os::unix::fs::symlink(p, sysbin.join(tool)).unwrap();
            }
        }
        let dirs = Dirs::under(&tmp.path().join("xdg"));
        // A fake starship on a PATH entry that only the user's .zprofile adds.
        exe(&home.join("userbin/starship"), "#!/bin/sh\n[ \"$1\" = init ] && echo 'PROMPT=\"fake-starship> \"'\n");
        // A fake goattyd: records what goatty-setup asks for, then fails so
        // goatty-setup doesn't go on to exec a new shell.
        exe(&home.join("goattyd"), "#!/bin/sh\necho \"goattyd $*\" >> \"$HOME/trace\"\nexit 1\n");
        Self { _tmp: tmp, home, dirs, zsh }
    }

    fn file(&self, rel: &str, contents: &str) {
        let p = self.home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    fn launch(&self, zsh: ZshSettings, orig_zdotdir: Option<&Path>) -> Launch {
        let mut settings = HostSettings::default();
        settings.shell.zsh = zsh;
        self.launch_with(settings, orig_zdotdir)
    }

    fn launcher(&self) -> Launcher {
        // Imports run the sandbox's zsh in the sandbox's home, never the real one.
        let ctx = goatty_shell::import::Context::new(self.home.clone(), Some(self.zsh.clone()), self.base_env(), &self.dirs.zsh_dir());
        Launcher::new(Some(self.dirs.clone()), Some(self.home.join("goattyd"))).with_import_context(ctx)
    }

    /// The import (on by default) is off here, so the other tests' traces
    /// only show goatty's shell: [`Self::launch_importing`] keeps it.
    fn launch_with(&self, mut settings: HostSettings, orig_zdotdir: Option<&Path>) -> Launch {
        settings.shell.import.enabled = false;
        self.launch_inner(settings, orig_zdotdir)
    }

    fn launch_importing(&self, mut settings: HostSettings) -> Launch {
        settings.shell.import.enabled = true;
        self.launch_inner(settings, None)
    }

    fn launch_inner(&self, mut settings: HostSettings, orig_zdotdir: Option<&Path>) -> Launch {
        settings.shell.program = Some(self.zsh.clone());
        let mut l = self.launcher().launch(&Effective::plain(settings));
        assert!(l.managed);
        // Don't inherit the test runner's ZDOTDIR.
        l.env.retain(|(k, _)| k != "GOATTY_ORIG_ZDOTDIR");
        if let Some(o) = orig_zdotdir {
            l.env.push(("GOATTY_ORIG_ZDOTDIR".into(), o.to_string_lossy().into_owned()));
        }
        l
    }

    /// A clean environment, as goattyd would hand to a new session.
    fn base_env(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            ("PATH".into(), self.home.join("sysbin").to_string_lossy().into_owned()),
            ("TERM".into(), "xterm-256color".into()),
            ("USER".into(), "tester".into()),
        ]
    }

    /// `zsh -l -i -c script` with the managed config.
    fn run(&self, launch: &Launch, script: &str) -> Output {
        Command::new(&launch.program)
            .args(["-l", "-i", "-c", script])
            .env_clear()
            .envs(self.base_env())
            .envs(launch.env.iter().cloned())
            .current_dir(&self.home)
            .output()
            .unwrap()
    }

    fn trace(&self) -> Vec<String> {
        std::fs::read_to_string(self.home.join("trace")).unwrap_or_default().lines().map(str::to_owned).collect()
    }
}

/// Default settings with the syntax highlighting plugin off (the host's
/// distro copy would otherwise load in some tests and not in others).
fn no_highlighting() -> HostSettings {
    let mut s = HostSettings::default();
    s.edit_plugin("syntax-highlighting", |p| p.enabled = false);
    s
}

/// Whether the host has a distro copy at any of `builtin_id`'s `system_paths`.
fn has_system_copy(builtin_id: &str) -> bool {
    goatty_shell::builtins::get(builtin_id)
        .unwrap()
        .manifest()
        .packages
        .iter()
        .flat_map(|p| &p.system_paths)
        .any(|p| Path::new(p).is_file())
}

fn exe(p: &Path, body: &str) {
    // Write and chmod in a short-lived child process so the test binary never
    // holds a write fd to the script while other test threads fork (ETXTBSY).
    let mut child =
        Command::new("sh").args(["-c", "cat > \"$1\" && chmod 0755 \"$1\"", "sh"]).arg(p).stdin(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "writing fake binary {} failed: {status}", p.display());
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// User startup files that log themselves to `$HOME/trace`.
const TRACE: &str = "print -r -- \"$NAME:${ZDOTDIR-unset}\" >> $HOME/trace\n";

fn traced(name: &str, extra: &str) -> String {
    format!("{}{extra}", TRACE.replace("$NAME", name))
}

#[test]
fn generated_files_parse() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh.clone());
    sb.launch(ZshSettings::default(), None);
    for f in [
        ".zshenv",
        ".zprofile",
        ".zshrc",
        "integration.zsh",
        "rc-guard.zsh",
        "settings.zsh",
        "aliases.zsh",
        "plugins-early.zsh",
        "plugins-line-editor.zsh",
        "plugins-prompt.zsh",
        "plugins.zsh",
        "plugins-last.zsh",
    ] {
        let out = Command::new(&zsh).arg("-n").arg(sb.dirs.zsh_dir().join(f)).output().unwrap();
        assert!(out.status.success(), "{f}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn default_startup_sources_user_env_not_rc() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    sb.file(".zshenv", &traced("zshenv", "export FROM_ZSHENV=1\n"));
    sb.file(".zprofile", &traced("zprofile", "path=($HOME/userbin $path)\n"));
    sb.file(".zshrc", &traced("zshrc", ""));
    sb.file(".zlogin", &traced("zlogin", ""));
    std::fs::create_dir_all(&sb.dirs.config).unwrap();
    std::fs::write(sb.dirs.user_zshrc(), TRACE.replace("$NAME", "additions")).unwrap();

    let launch = sb.launch_with(no_highlighting(), None);
    let out = sb.run(
        &launch,
        "print -r -- \"Z=${ZDOTDIR-unset} E=$FROM_ZSHENV\"; goatty-status; zsh -c 'print -r -- nested=${ZDOTDIR-unset}'; goatty-setup",
    );
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");

    // The user's files ran with their own (unset) ZDOTDIR; .zshrc didn't;
    // goatty's additions file did; .zlogin came from the user's dir after
    // goatty's .zshrc handed ZDOTDIR back.
    let trace = sb.trace();
    assert_eq!(&trace[..4], ["zshenv:unset", "zprofile:unset", "additions:unset", "zlogin:unset"], "{trace:?}");
    assert!(!trace.iter().any(|l| l.starts_with("zshrc")), "{trace:?}");

    assert!(text.contains("Z=unset E=1"), "{text}");
    assert!(text.contains("nested=unset"), "nested shells are plain zsh: {text}");
    let status = goatty_shell::doctor::parse_status(&text);
    let state = |n: &str| status.iter().find(|f| f.name == n).map(|f| f.state.clone());
    use goatty_shell::doctor::State;
    // Found through the PATH entry the user's .zprofile added.
    assert_eq!(state("starship"), Some(State::Ok(sb.home.join("userbin/starship").to_string_lossy().into_owned())));
    if matches!(state("fzf"), Some(State::Ok(_))) && has_system_copy("fzf-tab") {
        assert!(matches!(state("fzf-tab"), Some(State::Ok(_))));
    } else {
        assert_eq!(state("fzf-tab"), Some(State::Missing));
    }
    assert_eq!(state("syntax-highlighting"), Some(State::Off));
    assert!(matches!(state("integration"), Some(State::Ok(_))));

    // goatty-setup asks for exactly the missing default tools, never the
    // optional helpers (fd, bat, eza).
    let setup = trace.iter().find(|l| l.starts_with("goattyd ")).expect("goatty-setup ran goattyd");
    // Alphabetical, like `goatty-status`. autosuggestions is missing on
    // hosts without the distro plugin (CI), fzf where /usr/bin isn't
    // appended to PATH or has no fzf.
    let want: Vec<&str> = ["autosuggestions", "fzf", "fzf-tab"].into_iter().filter(|t| state(t) == Some(State::Missing)).collect();
    assert_eq!(setup, &format!("goattyd shell install -- {}", want.join(" ")));
}

/// goatty's completion functions are on `fpath`, and one added after the
/// completion dump was built is still found (the dump is dropped).
#[test]
fn completions_of_goatty_and_its_tools_load() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    use goatty_shell::completions;
    completions::write(&sb.dirs, "goattyd", "#compdef goattyd\n_arguments '1:cmd:(list stop)'\n").unwrap();
    let launch = sb.launch_with(no_highlighting(), None);
    let probe = "print -r -- \"goattyd=${_comps[goattyd]-none} demo=${_comps[demo]-none}\"";
    let out = sb.run(&launch, probe);
    assert!(String::from_utf8_lossy(&out.stderr).trim().is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).contains("goattyd=_goattyd demo=none"), "{}", stdout(&out));
    assert!(std::fs::read_dir(&sb.dirs.cache).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("zcompdump")));

    completions::write(&sb.dirs, "demo", "#compdef demo\n_files\n").unwrap();
    let out = sb.run(&launch, probe);
    assert!(stdout(&out).contains("goattyd=_goattyd demo=_demo"), "{}", stdout(&out));
}

#[test]
fn zdotdir_set_by_user_zshenv_and_user_rc() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let xdg_zsh = sb.home.join(".config/zsh");
    // The common pattern: ~/.zshenv moves everything else to ~/.config/zsh.
    sb.file(".zshenv", &traced("zshenv", "export ZDOTDIR=$HOME/.config/zsh\n"));
    sb.file(".config/zsh/.zprofile", &traced("zprofile", ""));
    sb.file(".config/zsh/.zshrc", &traced("zshrc", "user_rc_var=1\n"));
    sb.file(".zprofile", &traced("wrong-zprofile", ""));

    let launch = sb.launch(ZshSettings { user_rc: true, ..Default::default() }, None);
    let out = sb.run(&launch, "print -r -- \"Z=$ZDOTDIR rc=$user_rc_var\"; zsh -c 'print -r -- nested=$ZDOTDIR'");
    let text = stdout(&out);
    let xdg = xdg_zsh.to_string_lossy();
    assert_eq!(sb.trace(), ["zshenv:unset".to_string(), format!("zprofile:{xdg}"), format!("zshrc:{xdg}")]);
    assert!(text.contains(&format!("Z={xdg} rc=1")), "user rc vars are global; ZDOTDIR restored: {text}");
    assert!(text.contains(&format!("nested={xdg}")), "{text}");
}

#[test]
fn orig_zdotdir_restored_without_user_env() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let mine = sb.home.join("myzsh");
    sb.file("myzsh/.zshenv", &traced("zshenv", ""));
    sb.file("myzsh/.zlogin", &traced("zlogin", ""));
    let launch = sb.launch(ZshSettings { user_env: false, ..Default::default() }, Some(&mine));
    let out = sb.run(&launch, "print -r -- Z=$ZDOTDIR");
    assert!(stdout(&out).contains(&format!("Z={}", mine.display())), "{}", stdout(&out));
    // .zshenv skipped (user_env off); .zlogin is zsh's own doing, from the
    // restored ZDOTDIR.
    assert_eq!(sb.trace(), [format!("zlogin:{}", mine.display())]);
}

/// Run an interactive session in a PTY, typing `input`, and return the output.
fn pty_session(sb: &Sandbox, launch: &Launch, input: &[&str]) -> String {
    let mut env = sb.base_env();
    env.extend(launch.env.iter().cloned());
    let mut cmd = ShellCommand::new(launch.program.clone(), launch.args.clone(), env);
    cmd.cwd = Some(sb.home.clone());
    let (mut pty, mut reader) = Pty::spawn(&cmd, TermSize::new(120, 40)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut out = Vec::new();
    let wait_for = |needle: &[u8], out: &mut Vec<u8>| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !out.windows(needle.len()).any(|w| w == needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(chunk) => out.extend(chunk),
                Err(_) => panic!("timed out waiting for {:?} in:\n{}", String::from_utf8_lossy(needle), String::from_utf8_lossy(out)),
            }
        }
    };
    // Each prompt ends with OSC 133;B.
    let mut prompts = 1;
    wait_for(b"\x1b]133;B", &mut out);
    for line in input {
        pty.write(format!("{line}\r").as_bytes()).unwrap();
        prompts += 1;
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.windows(7).filter(|w| w == b"\x1b]133;B").count() < prompts {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(chunk) => out.extend(chunk),
                Err(_) => panic!("no prompt after {line:?}:\n{}", String::from_utf8_lossy(&out)),
            }
        }
    }
    pty.write(b"exit\r").unwrap();
    while let Ok(chunk) = rx.recv_timeout(Duration::from_secs(5)) {
        out.extend(chunk);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn integration_marks_prompts_and_reports_cwd() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    std::fs::create_dir_all(sb.home.join("a b")).unwrap();
    let launch = sb.launch(ZshSettings::default(), None);
    let out = pty_session(&sb, &launch, &["cd 'a b'", "false"]);

    assert!(out.contains("\x1b]133;A"), "prompt start mark");
    assert!(out.contains("\x1b]133;C"), "command start mark");
    assert!(out.contains("\x1b]133;D;0\x07"), "cd succeeded");
    assert!(out.contains("\x1b]133;D;1\x07"), "false failed");
    let cwd = format!("{}/a%20b\x1b\\", sb.home.display());
    assert!(out.contains("\x1b]7;file://") && out.contains(&cwd), "OSC 7 with a percent-encoded path; got:\n{out:?}");
    // fzf-tab is missing and the hint isn't dismissed.
    assert!(out.contains("goatty-setup"), "missing-tools hint");
}

#[test]
fn transient_prompt_shrinks_accepted_prompts() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let mut settings = HostSettings::default();
    settings.prompt.transient = true;
    settings.prompt.character.error = "✗".into();
    let launch = sb.launch_with(settings, None);
    let mut sh = PtyShell::start(&sb, &launch);
    // Accepting a line redraws its prompt as the bold prompt character
    // (marked as a prompt), colored by the status that prompt showed.
    let after_false = sh.prompts() + 2;
    sh.pty.write(b"false\r").unwrap();
    sh.wait_for(b"\x1b]133;A\x07\x1b[1m\x1b[32m\xe2\x9d\xaf");
    sh.wait_for(b"\x1b]133;D;1\x07");
    sh.wait_for_prompts(after_false);
    let after_true = sh.prompts() + 2;
    sh.pty.write(b"true\r").unwrap();
    sh.wait_for("\x1b[31m✗".as_bytes());
    sh.wait_for(b"\x1b]133;D;0\x07");
    // The next prompt is the full one again (it shows the directory).
    sh.wait_for_prompts(after_true);
    let out = sh.text();
    let last = &out[out.rfind("\x1b]133;D;0").unwrap()..];
    assert!(last.contains('~'), "the full prompt is back: {last:?}");
    assert!(!out.contains("command not found") && !out.contains("no such"), "{out:?}");
    let _ = sh.pty.write(b"exit\r");
}

#[test]
fn dismissed_hint_stays_quiet() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    std::fs::create_dir_all(&sb.dirs.state).unwrap();
    std::fs::write(sb.dirs.hint_marker(), "").unwrap();
    let launch = sb.launch(ZshSettings::default(), None);
    let out = pty_session(&sb, &launch, &[]);
    assert!(!out.contains("goatty-setup"), "{out:?}");
}

#[test]
fn hint_stays_quiet_in_sessions_of_a_window_that_offers_the_setup() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let mut launch = sb.launch(ZshSettings::default(), None);
    goatty_shell::setup::gui_offers_setup(&mut launch);
    let out = pty_session(&sb, &launch, &["echo gui=${GOATTY_GUI_SETUP-unset}"]);
    assert!(!out.contains("goatty-setup"), "{out:?}");
    assert!(out.contains("gui=unset"), "programs don't inherit the marker: {out:?}");
}

#[test]
fn hint_stays_quiet_once_the_gui_offered_the_install() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    std::fs::create_dir_all(&sb.dirs.state).unwrap();
    std::fs::write(sb.dirs.offer_marker(), "").unwrap();
    let launch = sb.launch(ZshSettings::default(), None);
    let out = pty_session(&sb, &launch, &[]);
    assert!(!out.contains("goatty-setup"), "{out:?}");
}

/// An interactive managed shell in a PTY, driven prompt by prompt.
struct PtyShell {
    pty: Pty,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    out: Vec<u8>,
}

impl PtyShell {
    fn start(sb: &Sandbox, launch: &Launch) -> Self {
        let mut env = sb.base_env();
        env.extend(launch.env.iter().cloned());
        let mut cmd = ShellCommand::new(launch.program.clone(), launch.args.clone(), env);
        cmd.cwd = Some(sb.home.clone());
        let (pty, mut reader) = Pty::spawn(&cmd, TermSize::new(120, 40)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut sh = Self { pty, rx, out: Vec::new() };
        sh.wait_for_prompts(1);
        sh.wait_for_idle();
        sh
    }

    fn prompts(&self) -> usize {
        self.out.windows(7).filter(|w| w == b"\x1b]133;B").count()
    }

    /// Wait until `n` prompts have been shown in total.
    fn wait_for_prompts(&mut self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.prompts() < n {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(chunk) => self.out.extend(chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let exit = self.pty.try_wait();
                    panic!(
                        "pty closed while waiting for prompt {n} (exit={exit:?}, prompts={}):\n{:?}",
                        self.prompts(),
                        String::from_utf8_lossy(&self.out)
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let exit = self.pty.try_wait();
                    panic!(
                        "waiting for prompt {n} (timeout, exit={exit:?}, prompts={}):\n{:?}",
                        self.prompts(),
                        String::from_utf8_lossy(&self.out)
                    );
                }
            }
        }
    }

    /// Wait until zsh has entered zle and is blocked reading input for the
    /// latest prompt, avoiding the brief window right after `133;B` where
    /// `zlecore` resets `errflag` before reading.
    fn wait_for_idle(&mut self) {
        let start = self.out.windows(7).rposition(|w| w == b"\x1b]133;B").unwrap_or(0);
        self.wait_for_from(start, b"\x1b[?2004h");
        if let Some(pid) = self.pty.pid() {
            let wchan = format!("/proc/{pid}/wchan");
            if std::path::Path::new(&wchan).exists() {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if let Ok(w) = std::fs::read_to_string(&wchan) {
                        let w = w.trim();
                        if !w.is_empty() && w != "0" {
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    fn wait_for(&mut self, needle: &[u8]) {
        self.wait_for_from(0, needle);
    }

    fn wait_for_from(&mut self, start: usize, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.out[start..].windows(needle.len()).any(|w| w == needle) {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(chunk) => self.out.extend(chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "pty closed ({:?}) while waiting for {:?}:\n{}",
                    self.pty.try_wait(),
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&self.out)
                ),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("waiting for {:?}:\n{}", String::from_utf8_lossy(needle), String::from_utf8_lossy(&self.out))
                }
            }
        }
    }

    /// Type `line` + Enter and wait for the next idle prompt.
    fn run(&mut self, line: &str) {
        let n = self.prompts() + 1;
        self.pty.write(format!("{line}\r").as_bytes()).unwrap();
        self.wait_for_prompts(n);
        self.wait_for_idle();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
}

#[test]
fn reloads_on_request() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let launch = sb.launch(ZshSettings::default(), None);
    let svc = goatty_shell::HostService::new(Launcher::new(Some(sb.dirs.clone()), None), None);
    let mut sh = PtyShell::start(&sb, &launch);
    let pid = sh.pty.pid().unwrap();
    svc.track_shell(pid);

    // Idle at an empty prompt: reloads right away (same pid: exec).
    sh.run("x=1");
    let n = sh.prompts();
    assert_eq!(svc.reload_shells(), 1);
    sh.wait_for_prompts(n + 1);
    sh.wait_for_idle();
    sh.run("print -r -- X=${x:-unset} P=$$");
    assert!(sh.text().contains(&format!("X=unset P={pid}")), "{}", sh.text());

    // Busy: reloads once the command is done.
    sh.run("y=1");
    let n = sh.prompts();
    let offset = sh.out.len();
    sh.pty.write(b"while [[ ! -e $HOME/busy-done ]]; do /bin/sleep 0.02; done\r").unwrap();
    sh.wait_for_from(offset, b"\x1b]133;C");
    assert_eq!(svc.reload_shells(), 1);
    assert_eq!(sh.prompts(), n, "no reload while a command is running");
    sb.file("busy-done", "");
    sh.wait_for_prompts(n + 1);
    sh.wait_for_idle();
    sh.run("print -r -- Y=${y:-unset}");
    assert!(sh.text().contains("Y=unset"), "{}", sh.text());

    // Something typed but not entered: left alone until Enter. The signal
    // still arrives quickly; give slow runners a moment instead of a fixed
    // sleep, and assert it stayed untouched after that.
    sh.run("z=1");
    let n = sh.prompts();
    let offset = sh.out.len();
    sh.pty.write(b"print -r -- typed").unwrap();
    sh.wait_for_from(offset, b"typed");
    assert_eq!(svc.reload_shells(), 1);
    let deadline = Instant::now() + Duration::from_secs(3);
    while sh.prompts() == n && Instant::now() < deadline {
        match sh.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => sh.out.extend(chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("pty closed ({:?}) while typing:\n{}", sh.pty.try_wait(), sh.text());
            }
        }
    }
    assert_eq!(sh.prompts(), n, "no reload while typing");
    sh.run("");
    sh.run("print -r -- Z=${z:-unset}");
    assert!(sh.text().contains("Z=unset"), "{}", sh.text());

    // Background jobs: only a reminder, the shell keeps its state.
    sh.run("w=1; /bin/sleep 30 &");
    let n = sh.prompts();
    assert_eq!(svc.reload_shells(), 1);
    sh.wait_for_prompts(n + 1);
    sh.wait_for_idle();
    sh.run("print -r -- W=${w:-unset}; kill %1");
    let text = sh.text();
    assert!(text.contains("once your jobs are done") && text.contains("W=1"), "{text}");

    // Gone: no longer signaled.
    sh.pty.write(b"exit\r").unwrap();
    while sh.rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
    let _ = sh.pty.try_wait();
    svc.untrack_shell(pid);
    assert_eq!(svc.reload_shells(), 0);
}

/// Installed plugins load in a real managed shell: aliases with setting
/// arguments, variables, a generated config file, the plugin's own code
/// (seeing its settings), and `goatty-status` reporting them, including a
/// plugin whose required command is missing.
#[test]
fn plugins_load_in_a_real_shell() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let plugin = |id: &str, toml: &str, files: &[(&str, &str)]| {
        let dir = sb.home.join("plugins-src").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), toml).unwrap();
        for (rel, text) in files {
            std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
            std::fs::write(dir.join(rel), text).unwrap();
        }
        dir
    };
    let meow = plugin(
        "meow",
        r#"
schema = 1
id = "meow"
name = "Meow"
version = "1.0.0"
summary = "cat, with options"
publisher = "tests"
[requires]
commands = ["cat"]
[[setting]]
id = "numbers"
label = "Number lines"
type = "bool"
default = true
arg = "-n"
[[setting]]
id = "mode"
label = "Mode"
type = "choice"
default = "purr"
choices = [{ value = "purr", label = "Purr" }, { value = "hiss", label = "Hiss" }]
[[alias]]
name = "meow"
command = "cat"
args = ["{args}"]
[[env]]
name = "MEOW_MODE"
value = "it's {mode}"
[[file]]
name = "meowrc"
env = "MEOW_CONFIG"
lines = ["mode={mode}", "{numbers.arg}"]
[zsh]
init = "zsh/init.zsh"
[permissions]
shell-code = "defines the greet function the plugin is about"
"#,
        &[("zsh/init.zsh", "typeset -g _meow=\"${goatty_settings[mode]}:$+goatty_settings\"\ngreet() { print -r -- \"greet:$_meow\" }\n")],
    );
    let ghost = plugin(
        "ghost",
        "schema = 1\nid = \"ghost\"\nname = \"Ghost\"\nversion = \"1.0.0\"\nsummary = \"needs a missing command\"\npublisher = \"tests\"\n[requires]\ncommands = [\"ghost-cmd\"]\n[[alias]]\nname = \"boo\"\ncommand = \"ghost-cmd\"\n",
        &[],
    );
    let none = goatty_plugin::fetch::Fixed::default();
    let p = goatty_shell::plugins::Plugins { dirs: &sb.dirs, fetch: &none, builtin: &[] };
    for dir in [&meow, &ghost] {
        let c = goatty_shell::plugins::Candidate::local(dir).unwrap();
        let plan = p.plan(&c, &HostSettings::default()).unwrap();
        p.install(&c, &plan.digest, &HostSettings::default()).unwrap();
    }

    let launch = sb.launch_with(no_highlighting(), None);
    let out = sb.run(
        &launch,
        "print -r -- \"alias=$aliases[meow] mode=$MEOW_MODE boo=${+aliases[boo]}\"; command cat -- \"$MEOW_CONFIG\"; greet; print -r -- after=${+goatty_settings}; goatty-status",
    );
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");
    assert!(text.contains("alias=cat -n mode=it's purr boo=0"), "{text}");
    assert!(text.contains("mode=purr\n-n\n"), "generated config file: {text}");
    assert!(text.contains("greet:purr:1"), "init code saw its settings: {text}");
    assert!(text.contains("after=0"), "settings are unset after the init code: {text}");
    let status = goatty_shell::doctor::parse_status(&text);
    let state = |n: &str| status.iter().find(|f| f.name == n).map(|f| f.state.clone());
    use goatty_shell::doctor::State;
    assert!(matches!(state("plugin:meow"), Some(State::Ok(d)) if d.ends_with("/cat")), "{status:?}");
    assert_eq!(state("plugin:ghost"), Some(State::Missing));
}

/// The built-in plugins in a real shell (#98, #165): fzf, then fzf-tab
/// (so Tab is fzf-tab's), then autosuggestions; fzf's options file, fd as
/// its file list and the previews; fzf-tab found through a managed copy;
/// a setting reaching its plugin's init; the fallback prompt with Starship
/// turned off (reported as off).
#[test]
fn builtin_plugins_load_in_their_slots() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let sysbin = sb.home.join("sysbin");
    exe(
        &sysbin.join("fzf"),
        "#!/bin/sh
[ \"$1\" = --zsh ] && echo 'fzf_w() { :; }; zle -N fzf_w; bindkey \"^R\" fzf_w; bindkey \"^T\" fzf_w; bindkey \"^I\" fzf_w'\n",
    );
    exe(&sysbin.join("fd"), "#!/bin/sh\n");
    exe(&sysbin.join("bat"), "#!/bin/sh\n");
    // goatty's own copies: autosuggestions laid out like the old tool
    // installer did, fzf-tab like a plugin install does.
    let managed = |pkg: &str, root: &str, file: &str, code: &str| {
        let root = sb.dirs.tools_dir().join(pkg).join(root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(file), code).unwrap();
        std::fs::create_dir_all(sb.dirs.plugins_dir()).unwrap();
        std::os::unix::fs::symlink(&root, sb.dirs.plugins_dir().join(pkg)).unwrap();
    };
    managed(
        "autosuggestions",
        "0.7.1/zsh-autosuggestions-0.7.1",
        "zsh-autosuggestions.zsh",
        "typeset -g fake_suggest=\"${(j:,:)ZSH_AUTOSUGGEST_STRATEGY}\"\n",
    );
    managed("fzf-tab", "1.3.0/fzf-tab-1.3.0", "fzf-tab.plugin.zsh", "ftb_w() { :; }; zle -N ftb_w; bindkey '^I' ftb_w\n");

    let mut settings = no_highlighting();
    settings.edit_plugin("autosuggestions", |p| {
        p.settings.insert("strategy".into(), "history".into());
    });
    settings.edit_plugin("starship", |p| p.enabled = false);
    let launch = sb.launch_with(settings, None);
    let out = pty_session(
        &sb,
        &launch,
        &[
            "bindkey '^R'; bindkey '^T'; bindkey '^I'",
            "[[ $PROMPT == *'%F{blue}%~%f'* ]] && p=fallback; print -r -- strat=${(j:,:)ZSH_AUTOSUGGEST_STRATEGY} suggest=$fake_suggest prompt=$p",
            "goatty-status",
        ],
    );
    // The variables in a clean environment (the PTY inherits the test's).
    let vars = stdout(&sb.run(
        &launch,
        "print -r -- \"cmd=[$FZF_DEFAULT_COMMAND] alt=[$FZF_ALT_C_COMMAND] topts=[$FZF_CTRL_T_OPTS]\"; [[ -r $FZF_DEFAULT_OPTS_FILE ]] && print -r -- rc=${FZF_DEFAULT_OPTS_FILE:t}; \
         zstyle -L ':completion:*' menu; zstyle -L ':fzf-tab:complete:*:*' fzf-preview",
    ));
    let host_tab = has_system_copy("fzf-tab");
    assert!(out.contains("\"^R\" fzf_w") && out.contains("\"^T\" fzf_w"), "{out}");
    if !host_tab {
        assert!(out.contains("\"^I\" ftb_w"), "fzf-tab loads after fzf and takes Tab: {out}");
    }
    let host_copy = has_system_copy("autosuggestions");
    assert!(out.contains("strat=history "), "the plugin's setting reached it: {out}");
    if !host_copy {
        assert!(out.contains("suggest=history "), "the managed copy loaded after its options were set: {out}");
    }
    assert!(out.contains("prompt=fallback"), "goatty's fallback prompt: {out}");
    let fd = "fd --strip-cwd-prefix --hidden --follow --exclude .git";
    assert!(vars.contains(&format!("cmd=[{fd} --type f] alt=[{fd} --type d]")), "fd lists the files: {vars}");
    assert!(vars.contains("topts=['--preview=bat --color=always"), "Ctrl+T previews files with bat: {vars}");
    assert!(vars.contains("rc=fzfrc"), "goatty's fzf options: {vars}");
    assert!(vars.contains("zstyle ':completion:*' menu no"), "fzf-tab replaces the menu: {vars}");
    assert!(vars.contains("elif [[ -f $realpath ]]; then bat --color=always"), "file previews with bat: {vars}");
    assert!(out.contains("starship\toff"), "{out}");
    assert!(out.contains("syntax-highlighting\toff"), "{out}");
    assert!(out.contains("fzf\tok\t") && out.contains("fzf-tab\tok\t"), "{out}");
    assert!(out.contains(&format!("helper:fd\tok\t{}", sysbin.join("fd").display())), "{out}");
    assert!(out.contains("helper:bat\tok\t"), "{out}");
    assert!(!out.contains("atuin"), "{out}");
}

/// fzf's variables: yours win, set before goatty's plugins load (here in
/// the environment, like a .zshenv export); goatty's own from the shell a
/// goatty-reload replaced are replaced again; fd missing and previews off
/// fall back to fzf's own directory walker.
#[test]
fn fzf_keeps_your_variables_and_replaces_its_own() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    exe(&sb.home.join("sysbin/fzf"), "#!/bin/sh\n");
    let mut settings = no_highlighting();
    settings.edit_plugin("fzf", |p| {
        p.settings.insert("previews".into(), false.into());
        p.settings.insert("hidden_files".into(), false.into());
    });
    settings.edit_plugin("fzf-tab", |p| p.enabled = false);
    let mut launch = sb.launch_with(settings, None);
    launch.env.extend([
        ("FZF_DEFAULT_COMMAND".to_string(), "goatty-old".to_string()),
        ("_goatty_fzf_FZF_DEFAULT_COMMAND".into(), "goatty-old".into()),
        ("FZF_DEFAULT_OPTS_FILE".into(), String::new()),
    ]);
    let out = sb.run(
        &launch,
        "print -r -- \"cmd=[$FZF_DEFAULT_COMMAND] file=[${FZF_DEFAULT_OPTS_FILE-unset}] t=[$FZF_CTRL_T_OPTS] c=[$FZF_ALT_C_OPTS]\"; goatty-status",
    );
    let text = stdout(&out);
    assert!(String::from_utf8_lossy(&out.stderr).trim().is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    // On hosts whose /etc/profile appends /usr/bin, the host's fd shows up.
    if text.contains("helper:fd\tmissing") {
        assert!(text.contains("cmd=[]"), "goatty's old value is gone, fzf walks the directory itself: {text}");
        assert!(text.contains("t=['--walker=file,follow' '--walker-skip=.git'] c=['--walker=dir,follow' '--walker-skip=.git']"), "{text}");
    } else {
        assert!(text.contains("cmd=[fd --strip-cwd-prefix --follow --exclude .git --type f]"), "no --hidden: {text}");
    }
    assert!(text.contains("file=[]"), "your options file (even empty) wins: {text}");
    assert!(!text.contains("helper:bat") && !text.contains("helper:eza"), "previews are off: {text}");
}

/// The user's aliases from host.json (#88): defined in a real shell,
/// quoted intact, a switched-off one left out, one with a plugin alias's
/// name replacing it, and a plugin alias the user switched off missing.
#[test]
fn user_aliases_in_a_real_shell() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let dir = sb.home.join("plugins-src/kitty");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "schema = 1\nid = \"kitty\"\nname = \"Kitty\"\nversion = \"1.0.0\"\nsummary = \"s\"\npublisher = \"tests\"\n[requires]\ncommands = [\"cat\"]\n\
         [[alias]]\nname = \"meow\"\ncommand = \"cat\"\n[[alias]]\nname = \"purr\"\ncommand = \"cat\"\nargs = [\"-n\"]\n[[alias]]\nname = \"hiss\"\ncommand = \"cat\"\n",
    )
    .unwrap();
    let none = goatty_plugin::fetch::Fixed::default();
    let p = goatty_shell::plugins::Plugins { dirs: &sb.dirs, fetch: &none, builtin: &[] };
    let c = goatty_shell::plugins::Candidate::local(&dir).unwrap();
    let plan = p.plan(&c, &HostSettings::default()).unwrap();
    p.install(&c, &plan.digest, &HostSettings::default()).unwrap();

    let mut settings = no_highlighting();
    let alias = |command: &str, group: &str, enabled: bool| goatty_config::UserAlias {
        group: group.into(),
        enabled,
        ..goatty_config::UserAlias::new(command)
    };
    settings.aliases.insert("say".into(), alias(r#"print -r -- "it's ok" | cat"#, "Fun", true));
    settings.aliases.insert("meow".into(), alias("print -r -- mine", "", true));
    settings.aliases.insert("nope".into(), alias("print -r -- nope", "Fun", false));
    settings.aliases.insert("..".into(), alias("cd ..", "", true));
    settings.plugins.entry("kitty".into()).or_default().disabled_aliases.insert("hiss".into());
    let launch = sb.launch_with(settings, None);
    let out =
        sb.run(&launch, "say; meow; print -r -- \"purr=$aliases[purr] hiss=${+aliases[hiss]} nope=${+aliases[nope]} dots=$aliases[..]\"");
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");
    assert!(text.contains("it's ok\nmine\n"), "{text}");
    assert!(text.contains("purr=cat -n hiss=0 nope=0 dots=cd .."), "{text}");
}

#[test]
fn added_folders_join_path_in_order() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    std::fs::create_dir_all(sb.home.join("my bin")).unwrap();
    std::fs::create_dir_all(sb.home.join("opt/it's")).unwrap();
    exe(&sb.home.join("my bin/hello-goatty"), "#!/bin/sh\necho hello from my bin\n");
    let mut settings = no_highlighting();
    let opt = sb.home.join("opt/it's").to_string_lossy().into_owned();
    settings.shell.path = vec!["~/my bin".into(), "~/missing".into(), opt.clone()];
    let launch = sb.launch_with(settings, None);
    let out = sb.run(&launch, "hello-goatty; print -rl -- $path");
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");
    assert!(text.starts_with("hello from my bin\n"), "{text}");
    let path: Vec<&str> = text.lines().skip(1).collect();
    let mine = sb.home.join("my bin").to_string_lossy().into_owned();
    let at = |d: &str| path.iter().position(|p| *p == d).unwrap_or_else(|| panic!("{d} not in {path:?}"));
    assert!(at(&mine) < at(&opt), "in order: {path:?}");
    assert!(at(&opt) < at(&sb.dirs.bin_dir().to_string_lossy()), "before goatty's own tools: {path:?}");
    assert!(!path.iter().any(|p| p.ends_with("missing")), "missing folders are skipped: {path:?}");
}

/// The PATH page's marks: each folder is attributed to the startup step
/// that added it.
#[test]
fn path_marks_tell_where_folders_came_from() {
    use goatty_shell::path_report::{COMMAND, attribute, parse_marks};
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    for d in ["env-bin", "profile-bin", "mine", "additions-bin"] {
        std::fs::create_dir_all(sb.home.join(d)).unwrap();
    }
    sb.file(".zshenv", "path+=(~/env-bin)\n");
    sb.file(".zprofile", "path+=(~/profile-bin)\n");
    std::fs::create_dir_all(sb.dirs.user_zshrc().parent().unwrap()).unwrap();
    std::fs::write(sb.dirs.user_zshrc(), "path+=(~/additions-bin)\n").unwrap();
    let mut settings = no_highlighting();
    // ~/profile-bin is on PATH already: the setting changes nothing.
    settings.shell.path = vec!["~/mine".into(), "~/profile-bin".into()];
    let launch = sb.launch_with(settings, None);
    let out = sb.run(&launch, COMMAND);
    let text = stdout(&out);
    let mut steps = parse_marks(&text).unwrap_or_else(|| panic!("no marks: {text}"));
    steps.insert(0, ("inherited".into(), sb.home.join("sysbin").to_string_lossy().into_owned()));
    let found = attribute(&steps);
    let step = |d: &Path| {
        let d = d.to_string_lossy();
        found.iter().find(|(f, _)| *f == d).map(|(_, s)| *s).unwrap_or_else(|| panic!("{d} not in {found:?}"))
    };
    assert_eq!(step(&sb.home.join("sysbin")), "inherited");
    assert_eq!(step(&sb.home.join("env-bin")), "zshenv");
    assert_eq!(step(&sb.home.join("profile-bin")), "zprofile");
    assert_eq!(step(&sb.home.join("mine")), "settings");
    assert_eq!(step(&sb.dirs.bin_dir()), "tools");
    assert_eq!(step(&sb.home.join("additions-bin")), "additions");
}

/// A reload request that arrives while the prompt is being drawn (after
/// precmd, before the line editor reads input) must still reload an idle
/// shell right away, not wait for the next command. The user's additions
/// slow the prompt down so the signal reliably lands in that window.
#[test]
fn reloads_when_signaled_while_the_prompt_draws() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    std::fs::create_dir_all(&sb.dirs.config).unwrap();
    std::fs::write(sb.dirs.user_zshrc(), "setopt prompt_subst\nPROMPT='$(/bin/sleep 0.6)'$PROMPT\n").unwrap();
    let launch = sb.launch_with(no_highlighting(), None);
    let svc = goatty_shell::HostService::new(Launcher::new(Some(sb.dirs.clone()), None), None);
    let mut sh = PtyShell::start(&sb, &launch);
    let pid = sh.pty.pid().unwrap();
    svc.track_shell(pid);

    sh.run("x=1");
    let n = sh.prompts();
    sh.pty.write(b"print -r -- drawing-next\r").unwrap();
    sh.wait_for(b"drawing-next\r\n");
    // The next prompt is now being drawn (its $(sleep) runs for 0.6 s).
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(sh.prompts(), n, "the prompt is still being drawn");
    assert_eq!(svc.reload_shells(), 1);
    // That prompt, then the reloaded shell's.
    sh.wait_for_prompts(n + 2);
    sh.wait_for_idle();
    sh.run("print -r -- X=${x:-unset} P=$$");
    assert!(sh.text().contains(&format!("X=unset P={pid}")), "{}", sh.text());
    sh.pty.write(b"exit\r").unwrap();
}

/// The import of the user's shell setup in a real managed shell: the PATH
/// folders and variables the user's `.zshrc` adds (harvested in its own
/// process) are there, its prompt framework never ran, its aliases aren't
/// applied (they're offered once instead), skipped names stay out, and
/// PATH folders goatty's shell has already keep their place.
#[test]
fn imported_setup_applies_in_the_managed_shell() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    for d in ["tools/bin", "late/bin", "early/bin"] {
        std::fs::create_dir_all(sb.home.join(d)).unwrap();
    }
    exe(&sb.home.join("tools/bin/my-tool"), "#!/bin/sh\necho my-tool ran\n");
    sb.file(".oh-my-zsh/oh-my-zsh.sh", "print -r -- OMZ-LOADED >> $HOME/trace\nalias gst='git status'\nalias -g G='| grep'\n");
    // .zprofile puts early/bin after the system folders; the .zshrc moves it
    // to the front, which goatty's shell leaves as .zprofile had it.
    sb.file(".zprofile", "path+=($HOME/early/bin)\n");
    sb.file(
        ".zshrc",
        &traced(
            "zshrc",
            "source $HOME/.oh-my-zsh/oh-my-zsh.sh\nexport PATH=\"$HOME/tools/bin:$HOME/early/bin:$PATH\"\nexport EDITOR=nvim\nexport SECRET=x\n\
             alias ll='ls -l'\n",
        ),
    );
    let mut settings = no_highlighting();
    settings.shell.import.skip_vars = vec!["SECRET".into()];
    settings.shell.path = vec!["~/late/bin".into()];
    let launch = sb.launch_importing(settings.clone());
    // The harvest ran the .zshrc (with oh-my-zsh) once, in its own process.
    assert_eq!(sb.trace(), ["zshrc:unset", "OMZ-LOADED"]);
    std::fs::remove_file(sb.home.join("trace")).unwrap();

    let record = goatty_shell::import::Record::load(&sb.dirs);
    let h = record.harvest.expect("harvested");
    let mine: Vec<(&str, bool)> = h.aliases.iter().map(|a| (a.name.as_str(), a.own)).collect();
    assert!(mine.contains(&("ll", true)) && mine.contains(&("gst", false)), "own aliases told apart: {mine:?}");
    let offer: Vec<&str> = goatty_shell::import::offer_aliases(&h, &settings).iter().map(|a| a.name.as_str()).collect();
    assert_eq!(offer.first(), Some(&"ll"), "{offer:?}");
    assert!(!offer.contains(&"G"), "no global aliases: {offer:?}");

    let out = sb.run(
        &launch,
        "my-tool; print -r -- \"E=$EDITOR S=${SECRET-unset} gst=${+aliases[gst]} G=${+galiases[G]} ll=${+aliases[ll]}\"; \
         print -rl -- $path; goatty-status",
    );
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");
    assert!(sb.trace().is_empty(), "neither the .zshrc nor oh-my-zsh ran in goatty's shell: {:?}", sb.trace());
    assert!(text.contains("my-tool ran\n"), "{text}");
    assert!(text.contains("E=nvim S=unset gst=0 G=0 ll=0"), "{text}");
    let path: Vec<&str> = text.lines().filter(|l| l.starts_with('/')).collect();
    let at = |d: &Path| path.iter().position(|p| Path::new(p) == d).unwrap_or_else(|| panic!("{} not in {path:?}", d.display()));
    assert!(at(&sb.home.join("tools/bin")) < at(&sb.home.join("sysbin")), "added folders in front: {path:?}");
    assert!(at(&sb.home.join("sysbin")) < at(&sb.home.join("early/bin")), ".zprofile's place kept: {path:?}");
    assert!(at(&sb.home.join("tools/bin")) < at(&sb.home.join("late/bin")), "{path:?}");
    assert!(at(&sb.home.join("late/bin")) < at(&sb.dirs.bin_dir()), "goatty's tools still last: {path:?}");
    assert_eq!(path.iter().filter(|p| Path::new(p) == sb.home.join("sysbin")).count(), 1, "{path:?}");
    let status = goatty_shell::doctor::parse_status(&text);
    let import = status.iter().find(|f| f.name == "import").map(|f| f.state.clone());
    assert_eq!(import, Some(goatty_shell::doctor::State::Ok("~/.zprofile, ~/.zshrc".into())), "{status:?}");

    // PATH page: imported folders are attributed to the .zshrc.
    let marks = stdout(&sb.run(&launch, goatty_shell::path_report::COMMAND));
    let steps = goatty_shell::path_report::parse_marks(&marks).unwrap();
    let found = goatty_shell::path_report::attribute(&steps);
    let tools = sb.home.join("tools/bin").to_string_lossy().into_owned();
    assert_eq!(found.iter().find(|(d, _)| *d == tools).map(|(_, s)| *s), Some("imported"), "{found:?}");

    // Off: nothing of it.
    let launch = sb.launch_with(settings, None);
    let text = stdout(&sb.run(&launch, "print -r -- \"E=${EDITOR-unset}\""));
    assert!(text.contains("E=unset"), "{text}");
}

/// "Also use ~/.zshrc" runs it guarded: frameworks and prompts it sources
/// are skipped (and listed), the rest runs at top level, compinit and
/// `starship init` do nothing, and a prompt it sets stays when goatty has
/// none.
#[test]
fn guarded_zshrc_skips_frameworks_and_keeps_the_rest() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    sb.file(".cache/p10k-instant-prompt-tester.zsh", "print -r -- P10K-LOADED >> $HOME/trace\n");
    sb.file(".oh-my-zsh/oh-my-zsh.sh", "print -r -- OMZ-LOADED >> $HOME/trace\nPROMPT='omz> '\n");
    sb.file(".zsh/mine.zsh", "typeset -A mine=(a 1)\nprint -r -- \"args=$#\" >> $HOME/trace\n");
    sb.file(".zsh/with-args.zsh", "print -r -- \"with=$*\" >> $HOME/trace\nsource ~/.zsh/nested.zsh\n");
    // Sourced without arguments: sees the arguments of the file that sourced it.
    sb.file(".zsh/nested.zsh", "print -r -- \"nested=$*\" >> $HOME/trace\n");
    sb.file(
        ".zshrc",
        "source \"$HOME/.cache/p10k-instant-prompt-tester.zsh\"\nexport ZSH=$HOME/.oh-my-zsh\nsource -- $ZSH/oh-my-zsh.sh\n\
         . ~/.zsh/mine.zsh\nsource ~/.zsh/with-args.zsh one two\nautoload -Uz compinit && compinit\neval \"$(starship init zsh)\"\n\
         source ~/.zinit/zinit.zsh\nzinit light zsh-users/zsh-completions\nomz update\n(( ! ${+functions[p10k]} )) || p10k finalize\n\
         export FROM_RC=1\nalias foo=bar\nprint -r -- rc-done >> $HOME/trace\n",
    );
    let mut settings = no_highlighting();
    settings.shell.zsh.user_rc = true;
    let launch = sb.launch_with(settings.clone(), None);
    let out = sb.run(
        &launch,
        "print -r -- \"rc=$FROM_RC foo=$aliases[foo] mine=$mine[a] source=${+aliases[source]} stub=${+functions[starship]} zinit=${+functions[zinit]}\"; print -r -- \"P=$PROMPT\"; goatty-status",
    );
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");
    assert_eq!(sb.trace(), ["args=0", "with=one two", "nested=one two", "rc-done"], "frameworks skipped, the rest ran with its arguments");
    assert!(text.contains("rc=1 foo=bar mine=1 source=0 stub=0 zinit=0"), "typesets stay global, the guard and stubs are gone: {text}");
    assert!(!text.contains("compinit: initialization aborted"), "{text}");
    assert!(!text.contains("P=omz> "), "goatty's prompt plugin, not oh-my-zsh's: {text}");
    let status = goatty_shell::doctor::parse_status(&text);
    let off: Vec<&str> = status.iter().filter(|f| f.name.starts_with("rc:")).map(|f| f.name.as_str()).collect();
    assert_eq!(off, ["rc:oh-my-zsh", "rc:powerlevel10k", "rc:starship", "rc:zinit"], "{status:?}");

    // Without a prompt plugin, the .zshrc's own prompt stays.
    sb.file(".zshrc", "PROMPT='mine> '\n");
    settings.edit_plugin("starship", |p| p.enabled = false);
    let launch = sb.launch_with(settings, None);
    let text = stdout(&sb.run(&launch, "print -r -- \"P=$PROMPT\""));
    assert!(text.contains("P=mine> "), "{text}");
}

/// Where zsh keeps `compinit` defined after goatty ran it (Ubuntu's), a
/// guarded `.zshrc` calling it doesn't run it again (a second run asks
/// about insecure directories without a terminal and resets completion),
/// and the definition is back afterwards.
#[test]
fn guarded_zshrc_never_runs_compinit_twice() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    // A compinit that stays defined, like Ubuntu's.
    sb.file(".zshenv", "compinit() { print -r -- compinit-ran >> $HOME/trace }\ncompdef() { : }\n");
    sb.file(".zshrc", "autoload -Uz compinit\ncompinit\nprint -r -- rc-done >> $HOME/trace\n");
    let mut settings = no_highlighting();
    settings.edit_plugin("fzf-tab", |p| p.enabled = false);
    settings.shell.zsh.user_rc = true;
    let launch = sb.launch_with(settings, None);
    let out = sb.run(&launch, "print -r -- \"defined=${+functions[compinit]} body=${functions[compinit]}\"");
    let text = stdout(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "startup printed errors:\n{stderr}");
    assert_eq!(sb.trace(), ["compinit-ran", "rc-done"], "goatty's run only");
    assert!(text.contains("defined=1") && text.contains("compinit-ran"), "the user's definition is back: {text}");
}

/// Outdated managed tools (#45): detected in pure zsh when the active copy
/// lives under `tools/<id>/<old>/`, shown in the startup hint even if an
/// earlier GUI offered the setup, hidden under `--dismiss` or `GOATTY_GUI_SETUP`,
/// ignored when shadowed by a system copy, and passed to `goatty-setup`.
#[test]
fn outdated_managed_tools_hint_and_setup() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);

    // Seed an outdated managed binary (starship) and an outdated managed plugin script (fzf-tab),
    // plus a current managed script (autosuggestions) and a system fzf that shadows an old managed fzf.
    let starship_dir = sb.dirs.tools_dir().join("starship/1.0.0");
    std::fs::create_dir_all(&starship_dir).unwrap();
    exe(&starship_dir.join("starship"), "#!/bin/sh\n[ \"$1\" = init ] && echo 'PROMPT=\"managed-starship> \"'\n");
    std::fs::create_dir_all(sb.dirs.bin_dir()).unwrap();
    std::os::unix::fs::symlink(starship_dir.join("starship"), sb.dirs.bin_dir().join("starship")).unwrap();

    let tab_dir = sb.dirs.tools_dir().join("fzf-tab/1.0.0/fzf-tab-1.0.0");
    std::fs::create_dir_all(&tab_dir).unwrap();
    std::fs::write(tab_dir.join("fzf-tab.plugin.zsh"), "ftb_w() { :; }; zle -N ftb_w\n").unwrap();
    std::fs::create_dir_all(sb.dirs.plugins_dir()).unwrap();
    std::os::unix::fs::symlink(&tab_dir, sb.dirs.plugins_dir().join("fzf-tab")).unwrap();

    let current_as = goatty_shell::builtins::get("autosuggestions").unwrap().manifest().packages[0].version.clone();
    let as_root = sb.dirs.tools_dir().join(format!("autosuggestions/{current_as}/zsh-autosuggestions-{current_as}"));
    std::fs::create_dir_all(&as_root).unwrap();
    std::fs::write(as_root.join("zsh-autosuggestions.zsh"), "true\n").unwrap();
    std::os::unix::fs::symlink(&as_root, sb.dirs.plugins_dir().join("autosuggestions")).unwrap();

    // System fzf on PATH shadows an old managed fzf -> fzf must NOT be flagged outdated.
    let old_fzf_dir = sb.dirs.tools_dir().join("fzf/0.10.0");
    std::fs::create_dir_all(&old_fzf_dir).unwrap();
    exe(&old_fzf_dir.join("fzf"), "#!/bin/sh\n");
    std::os::unix::fs::symlink(old_fzf_dir.join("fzf"), sb.dirs.bin_dir().join("fzf")).unwrap();
    exe(&sb.home.join("sysbin/fzf"), "#!/bin/sh\n");

    // Remove the fake starship from userbin (which Sandbox::new added for .zprofile tests)
    // and ensure only managed starship is used, and disable syntax-highlighting so nothing else is missing.
    let _ = std::fs::remove_file(sb.home.join("userbin/starship"));
    // Also mark shell-setup-offered to verify outdated hints still surface after earlier GUI offers.
    std::fs::create_dir_all(&sb.dirs.state).unwrap();
    std::fs::write(sb.dirs.offer_marker(), "").unwrap();

    let launch = sb.launch_with(no_highlighting(), None);
    // Derive expectations from the sandbox child's environment (the runner's PATH,
    // e.g. ~/.cargo/bin, isn't inherited, while /etc/profile may add /usr/bin).
    let status_out = sb.run(&launch, "goatty-status");
    let status = goatty_shell::doctor::parse_status(&stdout(&status_out));
    let is_managed = |id: &str| {
        status.iter().find(|f| f.name == id).is_some_and(|f| match &f.state {
            goatty_shell::doctor::State::Ok(detail) => goatty_shell::setup::is_managed_detail(&sb.dirs, detail),
            _ => false,
        })
    };
    let mut expected_outdated = Vec::new();
    if is_managed("fzf-tab") {
        expected_outdated.push("fzf-tab");
    }
    if is_managed("starship") {
        expected_outdated.push("starship");
    }
    let out = pty_session(&sb, &launch, &["goatty-setup"]);
    if !expected_outdated.is_empty() {
        assert!(out.contains(&format!("outdated: {}", expected_outdated.join(", "))), "expected outdated hint:\n{out}");
        assert!(out.contains("to update"), "expected update verb:\n{out}");
        assert!(out.contains("goatty-setup"), "expected goatty-setup hint:\n{out}");
        let trace = sb.trace();
        let setup = trace.iter().find(|l| l.starts_with("goattyd ")).expect("goatty-setup ran goattyd");
        assert_eq!(setup, &format!("goattyd shell install -- {}", expected_outdated.join(" ")));
    }
    assert!(!out.contains("not installed:"), "missing tools suppressed by offer marker & everything else installed:\n{out}");

    // Directly exercise `_goatty_outdated_tools` in real zsh for both a managed binary
    // symlink (`bin/starship -> tools/starship/1.0.0/starship`), a managed script symlink,
    // a current managed copy, and a system copy shadowing a managed copy.
    let direct = stdout(&sb.run(
        &launch,
        "typeset -gA _goatty_state; \
         _goatty_state[starship]=\"ok\t$_goatty_bin/starship\"; \
         _goatty_state[fzf-tab]=\"ok\t$_goatty_pkgs/fzf-tab/fzf-tab.plugin.zsh\"; \
         _goatty_state[autosuggestions]=\"ok\t$_goatty_pkgs/autosuggestions/zsh-autosuggestions.zsh\"; \
         _goatty_state[fzf]=\"ok\t/usr/bin/fzf\"; \
         _goatty_outdated_tools; \
         print -r -- \"out=${(j:,:)reply}\"",
    ));
    assert!(direct.contains("out=fzf-tab,starship\n"), "direct _goatty_outdated_tools check failed:\n{direct}");

    // Even when startup state shows nothing missing or outdated (e.g. goattyd was
    // upgraded with a new pin while this shell stayed open), `goatty-setup` with
    // no arguments delegates discovery to the current daemon.
    let _ = std::fs::remove_file(sb.home.join("trace"));
    let _ = sb.run(
        &launch,
        "typeset -gA _goatty_state; \
         _goatty_state[fzf]=ok; _goatty_state[fzf-tab]=ok; _goatty_state[autosuggestions]=ok; _goatty_state[starship]=ok; \
         goatty-setup",
    );
    assert_eq!(sb.trace(), ["goattyd shell install"]);

    // Quiet in GUI sessions that offer setup themselves.
    let mut gui_launch = sb.launch_with(no_highlighting(), None);
    goatty_shell::setup::gui_offers_setup(&mut gui_launch);
    let gui_out = pty_session(&sb, &gui_launch, &[]);
    assert!(!gui_out.contains("outdated:"), "{gui_out:?}");

    // Quiet when dismissed (`goatty-setup --dismiss`).
    std::fs::write(sb.dirs.hint_marker(), "").unwrap();
    let dismissed_out = pty_session(&sb, &launch, &[]);
    assert!(!dismissed_out.contains("outdated:"), "{dismissed_out:?}");
}

/// What the last command printed: the text after its OSC 133;C mark.
fn last_output(sh: &PtyShell) -> String {
    let t = sh.text();
    let start = t.rfind("\x1b]133;C").unwrap_or(0);
    t[start..].to_string()
}

#[test]
fn expansions_rewrite_words_on_space_and_enter() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    // The built-in Basics pack brings the rules; autosuggestions and syntax
    // highlighting load too when the host has them, and wrap the widgets.
    let launch = sb.launch_with(HostSettings::default(), None);
    let mut sh = PtyShell::start(&sb, &launch);

    // Space expands `| first 2 lines` into `| head -n 2` before it runs.
    sh.run("printf 'L%s\\n' 1 2 3 4 | first 2 lines ");
    let out = last_output(&sh);
    assert!(out.contains("L2") && !out.contains("L3"), "{out:?}");
    assert!(sh.text().contains("→  | head -n 2"), "the message says what happened: {:?}", sh.text());

    // Enter on a line ending in a rule expands it and stops; Enter again runs it.
    let before = sh.prompts();
    sh.pty.write(b"printf 'L%s\\n' 1 2 3 | last 1 lines\r").unwrap();
    sh.wait_for(b"| tail -n 1");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(sh.prompts(), before, "not run yet");
    sh.run("");
    let out = last_output(&sh);
    assert!(out.contains("L3") && !out.contains("L1"), "{out:?}");

    // Backspace right after an expansion brings back what was typed.
    sh.pty.write(b"echo | sorted \x7f").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    sh.run("x");
    assert!(last_output(&sh).contains("sortedx"), "{:?}", last_output(&sh));

    // Inside quotes nothing changes.
    sh.run("echo 'a | first 2 lines b'");
    assert!(last_output(&sh).contains("a | first 2 lines b"), "{:?}", last_output(&sh));

    // Pasted text is never rewritten, not by a Space or an Enter right after it.
    sh.run("\x1b[200~echo pasted | sorted\x1b[201~");
    assert!(last_output(&sh).contains("sorted"), "{:?}", last_output(&sh));
    sh.run("\x1b[200~echo pasted | sorted\x1b[201~ ");
    assert!(last_output(&sh).contains("sorted"), "{:?}", last_output(&sh));

    // A placeholder at the end: the word before it is the key.
    sh.run("printf 'L%s\\n' apple banana | only lines with ban ");
    let out = last_output(&sh);
    assert!(out.contains("Lbanana") && !out.contains("Lapple"), "{out:?}");
    let _ = sh.pty.write(b"exit\r");
}

#[test]
fn expansions_can_be_turned_off() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let mut s = HostSettings::default();
    s.shell.expansions = false;
    let launch = sb.launch_with(s, None);
    let out = stdout(&sb.run(&launch, "print -r -- ${_goatty_state[expansions]} ${widgets[goatty-expand-space]-none} $#_goatty_exp_re"));
    assert_eq!(out.trim(), "off none 0");
    let launch = sb.launch_with(HostSettings::default(), None);
    let out = stdout(&sb.run(&launch, "print -r -- ${_goatty_state[expansions]%%$'\\t'*} ${widgets[goatty-expand-space]}"));
    assert_eq!(out.trim(), "ok user:goatty-expand-space");
}

/// goatty's right-click menu (#186) reads and changes the typed line
/// through private key sequences bound to zle widgets.
#[test]
fn command_line_widgets_report_and_change_the_line() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let launch = sb.launch_with(no_highlighting(), None);
    let mut sh = PtyShell::start(&sb, &launch);
    assert!(sh.text().contains("\x1b]7337;zle=1\x07"), "the prompt says the widgets are there");
    // An "editor" that leaves the file as it is.
    sh.run("export VISUAL=true");

    // The typed line comes back percent-encoded, multibyte and all.
    let start = sh.out.len();
    sh.pty.write("echo 'a%b' ✓".as_bytes()).unwrap();
    sh.pty.write(b"\x1b[7337;1~").unwrap();
    sh.wait_for_from(start, b"\x1b]7337;line=echo %27a%25b%27 %E2%9C%93\x07");

    // sudo goes in front, and comes off again; the line stays as typed.
    sh.pty.write(b"\x1b[7337;2~\x1b[7337;1~").unwrap();
    sh.wait_for_from(start, b"\x1b]7337;line=sudo echo %27a%25b%27 %E2%9C%93\x07");
    sh.pty.write(b"\x1b[7337;2~\x1b[7337;1~").unwrap();
    let again = sh.out.len();
    sh.wait_for_from(again, b"\x1b]7337;line=echo %27a%25b%27 %E2%9C%93\x07");

    // Taking sudo away with the cursor at the end keeps it at the end.
    sh.pty.write(b"\x1b[7337;2~X\x1b[7337;1~").unwrap();
    let again = sh.out.len();
    sh.wait_for_from(again, b"\x1b]7337;line=sudo echo %27a%25b%27 %E2%9C%93X\x07");
    sh.pty.write(b"\x1b[7337;2~Y\x1b[7337;1~").unwrap();
    let again = sh.out.len();
    sh.wait_for_from(again, b"\x1b]7337;line=echo %27a%25b%27 %E2%9C%93XY\x07");

    // The editor owns the terminal while it runs: goatty hears so.
    let start = sh.out.len();
    sh.pty.write(b"\x1b[7337;3~").unwrap();
    sh.wait_for_from(start, b"\x1b]7337;zle=0\x07");
    sh.wait_for_from(start, b"\x1b]7337;zle=1\x07");
    sh.wait_for_idle();
    sh.pty.write(b"\x1b[7337;1~").unwrap();
    sh.wait_for_from(start, b"\x1b]7337;line=echo %27a%25b%27 %E2%9C%93XY\x07");

    // Clearing empties it; on an empty line, sudo takes the last command.
    sh.pty.write(b"\x1b[7337;4~\x1b[7337;1~").unwrap();
    sh.wait_for_from(again, b"\x1b]7337;line=\x07");
    sh.run("true marker");
    let start = sh.out.len();
    sh.pty.write(b"\x1b[7337;2~\x1b[7337;1~").unwrap();
    sh.wait_for_from(start, b"\x1b]7337;line=sudo true marker\x07");
    sh.pty.write(b"\x1b[7337;4~").unwrap();
    let _ = sh.pty.write(b"exit\r");
}

fn cached_entries(init_dir: &Path, prefix: &str, zwc: bool) -> Vec<PathBuf> {
    let want_prefix = format!("{prefix}-");
    let want_suffix = if zwc { ".zsh.zwc" } else { ".zsh" };
    let Ok(rd) = std::fs::read_dir(init_dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            let rest = s.strip_prefix(&want_prefix)?;
            let tag = rest.strip_suffix(want_suffix)?;
            (!tag.is_empty() && !tag.contains('-')).then(|| e.path())
        })
        .collect();
    paths.sort();
    paths
}

#[test]
fn cached_tool_inits_hit_cache_and_preserve_global_scope() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    // Keep only our fake starship and fzf on PATH.
    let _ = std::fs::remove_file(sb.home.join("userbin/starship"));
    let sysbin = sb.home.join("sysbin");
    exe(
        &sysbin.join("starship"),
        "#!/bin/sh
echo \"starship:$*\" >> \"$HOME/init-calls\"
case \"$1\" in
  init)
    cat <<'EOF'
typeset starship_scoped=1
PROMPT='fake-starship-v1> '
PROMPT2=\"$(starship prompt --continuation)\"
EOF
    ;;
  prompt)
    if [ \"${STARSHIP_SHELL:-}\" = zsh ]; then
      printf '%%{\\033[90m%%}cont-v1>%%{\\033[0m%%} '
    else
      printf '\\033[90mcont-v1>\\033[0m '
    fi
    ;;
esac
",
    );
    exe(
        &sysbin.join("fzf"),
        "#!/bin/sh
echo \"fzf:$*\" >> \"$HOME/init-calls\"
if [ \"$1\" = --zsh ]; then
  cat <<'EOF'
typeset fzf_scoped=1
fzf_v=1
EOF
fi
",
    );

    let mut settings = no_highlighting();
    settings.edit_plugin("fzf-tab", |p| p.enabled = false);
    settings.edit_plugin("autosuggestions", |p| p.enabled = false);
    let launch = sb.launch_with(settings, None);

    let first = pty_session(
        &sb,
        &launch,
        &["print -r -- \"starship_scoped=$starship_scoped fzf_scoped=$fzf_scoped fzf_v=$fzf_v p2=[${(V)PROMPT2}]\""],
    );
    assert!(
        first.contains("starship_scoped=1 fzf_scoped=1 fzf_v=1 p2=[%{\\e[90m%}cont-v1>%{\\e[0m%} ]")
            || first.contains("starship_scoped=1 fzf_scoped=1 fzf_v=1 p2=[%{^[[90m%}cont-v1>%{^[[0m%} ]"),
        "{first}"
    );
    let calls_after_first = std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default();
    assert_eq!(calls_after_first.lines().filter(|l| l.starts_with("starship:init")).count(), 1, "{calls_after_first}");
    assert_eq!(calls_after_first.lines().filter(|l| l.starts_with("starship:prompt --continuation")).count(), 1, "{calls_after_first}");
    assert_eq!(calls_after_first.lines().filter(|l| l.starts_with("fzf:--zsh")).count(), 1, "{calls_after_first}");

    let init_dir = sb.dirs.init_cache_dir();
    for (prefix, zwc) in [("starship", false), ("starship", true), ("fzf", false), ("fzf", true)] {
        let files = cached_entries(&init_dir, prefix, zwc);
        assert_eq!(files.len(), 1, "expected 1 {prefix} cache file (zwc={zwc}), got {files:?}");
        let meta = std::fs::metadata(&files[0]).unwrap();
        assert!(meta.len() > 0, "{} is empty", files[0].display());
    }

    // Second startup hits both caches without spawning starship or fzf.
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let second = pty_session(
        &sb,
        &launch,
        &["print -r -- \"starship_scoped=$starship_scoped fzf_scoped=$fzf_scoped fzf_v=$fzf_v p2=[${(V)PROMPT2}]\""],
    );
    assert!(
        second.contains("starship_scoped=1 fzf_scoped=1 fzf_v=1 p2=[%{\\e[90m%}cont-v1>%{\\e[0m%} ]")
            || second.contains("starship_scoped=1 fzf_scoped=1 fzf_v=1 p2=[%{^[[90m%}cont-v1>%{^[[0m%} ]"),
        "{second}"
    );
    let calls_after_second = std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default();
    assert!(calls_after_second.trim().is_empty(), "expected cache hit with no subprocess spawns, got:\n{calls_after_second}");
}

#[test]
fn cached_tool_inits_invalidate_on_updates_and_config_changes() {
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh);
    let _ = std::fs::remove_file(sb.home.join("userbin/starship"));

    // Symlink in sysbin pointing into versioned directories (like bin/starship -> tools/starship/<ver>/starship)
    // so it always stays ahead of any host /usr/bin/starship on PATH.
    let v1_dir = sb.dirs.tools_dir().join("starship/1.0.0");
    std::fs::create_dir_all(&v1_dir).unwrap();
    exe(
        &v1_dir.join("starship"),
        "#!/bin/sh
echo \"v1:$*\" >> \"$HOME/init-calls\"
case \"$1\" in
  init) echo 'PROMPT=\"starship-v1> \"'; echo 'PROMPT2=\"$(starship prompt --continuation)\"' ;;
  prompt) printf 'cont-v1> ' ;;
esac
",
    );
    let starship_link = sb.home.join("sysbin/starship");
    std::os::unix::fs::symlink(v1_dir.join("starship"), &starship_link).unwrap();

    // fzf in sysbin so we can also test replacing a binary file directly.
    let fzf_path = sb.home.join("sysbin/fzf");
    exe(
        &fzf_path,
        "#!/bin/sh
echo \"fzf1:$*\" >> \"$HOME/init-calls\"
[ \"$1\" = --zsh ] && echo 'fzf_v=1'
",
    );

    let mut settings = no_highlighting();
    settings.edit_plugin("fzf-tab", |p| p.enabled = false);
    settings.edit_plugin("autosuggestions", |p| p.enabled = false);
    let launch = sb.launch_with(settings.clone(), None);

    let out1 = pty_session(&sb, &launch, &["print -r -- \"p=[$PROMPT] p2=[$PROMPT2] fzf=$fzf_v\""]);
    assert!(out1.contains("starship-v1> ") && out1.contains("p2=[cont-v1> ] fzf=1"), "{out1}");

    // 1) Update via symlink target switch (1.0.0 -> 2.0.0).
    let v2_dir = sb.dirs.tools_dir().join("starship/2.0.0");
    std::fs::create_dir_all(&v2_dir).unwrap();
    exe(
        &v2_dir.join("starship"),
        "#!/bin/sh
echo \"v2:$*\" >> \"$HOME/init-calls\"
case \"$1\" in
  init) echo 'PROMPT=\"starship-v2> \"'; echo 'PROMPT2=\"$(starship prompt --continuation)\"' ;;
  prompt) printf 'cont-v2> ' ;;
esac
",
    );
    let tmp_link = sb.home.join("sysbin/.starship.tmp");
    std::os::unix::fs::symlink(v2_dir.join("starship"), &tmp_link).unwrap();
    std::fs::rename(&tmp_link, &starship_link).unwrap();

    // 2) Update fzf in-place on disk with new content/size.
    exe(
        &fzf_path,
        "#!/bin/sh
echo \"fzf2:$*\" >> \"$HOME/init-calls\"
[ \"$1\" = --zsh ] && echo 'fzf_v=2_updated_binary'
",
    );

    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out2 = pty_session(&sb, &launch, &["print -r -- \"p=[$PROMPT] p2=[$PROMPT2] fzf=$fzf_v\""]);
    assert!(out2.contains("starship-v2> ") && out2.contains("p2=[cont-v2> ] fzf=2_updated_binary"), "{out2}");
    let calls = std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default();
    assert!(calls.contains("v2:init") && calls.contains("fzf2:--zsh"), "{calls}");

    // 3) Changing starship config invalidates the Starship cache even without binary update.
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let mut s_cfg = settings.clone();
    s_cfg.prompt.character.success = ">>".into();
    let launch_cfg = sb.launch_with(s_cfg, None);
    let out3 = pty_session(&sb, &launch_cfg, &["print -r -- \"p=[$PROMPT] p2=[$PROMPT2]\""]);
    assert!(out3.contains("starship-v2> "), "{out3}");
    let calls_cfg = std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default();
    assert!(calls_cfg.contains("v2:init"), "config generation change should re-run starship init: {calls_cfg}");

    // 4) PATH shadowing by an earlier binary invalidates cache.
    let shadow_dir = sb.home.join("shadowbin");
    std::fs::create_dir_all(&shadow_dir).unwrap();
    exe(
        &shadow_dir.join("starship"),
        "#!/bin/sh
echo \"shadow:$*\" >> \"$HOME/init-calls\"
case \"$1\" in
  init) echo 'PROMPT=\"starship-shadow> \"'; echo 'PROMPT2=\"$(starship prompt --continuation)\"' ;;
  prompt) printf 'cont-shadow> ' ;;
esac
",
    );
    sb.file(".zshenv", "path=($HOME/shadowbin $path)\n");
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out4 = pty_session(&sb, &launch_cfg, &["print -r -- \"p=[$PROMPT] p2=[$PROMPT2]\""]);
    assert!(out4.contains("starship-shadow> ") && out4.contains("p2=[cont-shadow> ]"), "{out4}");

    // 5) prefer_user_config=true watches $HOME/.config/starship.toml (even when XDG_CONFIG_HOME points elsewhere)
    //    and regenerates even on a same-size, same-second edit.
    let user_cfg_dir = sb.home.join(".config");
    let xdg_cfg_dir = sb.home.join("other-xdg-config");
    std::fs::create_dir_all(&user_cfg_dir).unwrap();
    std::fs::create_dir_all(&xdg_cfg_dir).unwrap();
    let user_toml = user_cfg_dir.join("starship.toml");
    std::fs::write(&user_toml, "continuation_prompt = 'u1> '\n").unwrap();
    std::fs::write(xdg_cfg_dir.join("starship.toml"), "continuation_prompt = 'xdg> '\n").unwrap();
    exe(
        &shadow_dir.join("starship"),
        "#!/bin/sh
echo \"usercfg:$*\" >> \"$HOME/init-calls\"
case \"$1\" in
  init) echo 'PROMPT=\"starship-user> \"'; echo 'PROMPT2=\"$(starship prompt --continuation)\"' ;;
  prompt) grep -q 'u2>' \"$HOME/.config/starship.toml\" && printf 'u2> ' || printf 'u1> ' ;;
esac
",
    );
    let mut s_user = settings.clone();
    s_user.edit_plugin("starship", |p| {
        p.settings.insert("prefer_user_config".into(), true.into());
    });
    let mut launch_user = sb.launch_with(s_user, None);
    launch_user.env.push(("XDG_CONFIG_HOME".into(), xdg_cfg_dir.to_string_lossy().into_owned()));
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out5a = pty_session(&sb, &launch_user, &["print -r -- \"p2=[$PROMPT2]\""]);
    assert!(out5a.contains("p2=[u1> ]"), "{out5a}");
    // Same byte length as 'u1> ' to verify subsecond timestamps catch same-size edits too.
    std::fs::write(&user_toml, "continuation_prompt = 'u2> '\n").unwrap();
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out5b = pty_session(&sb, &launch_user, &["print -r -- \"p2=[$PROMPT2]\""]);
    assert!(out5b.contains("p2=[u2> ]"), "{out5b}");
    let calls_u = std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default();
    assert!(calls_u.contains("usercfg:init"), "editing ~/.config/starship.toml should re-run starship init: {calls_u}");

    // 6) TERM=dumb uses separate starship-dumb cache and doesn't poison TERM=xterm-256color PROMPT2.
    let mut launch_dumb = launch_user.clone();
    launch_dumb.env.push(("TERM".into(), "dumb".into()));
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out_dumb = sb.run(&launch_dumb, "print -r -- \"p2=[$PROMPT2]\"");
    assert!(String::from_utf8_lossy(&out_dumb.stderr).trim().is_empty());
    let init_dir = sb.dirs.init_cache_dir();
    assert_eq!(cached_entries(&init_dir, "starship-dumb", false).len(), 1);
    assert!(!cached_entries(&init_dir, "starship", false).is_empty());
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out_xterm = pty_session(&sb, &launch_user, &["print -r -- \"p2=[$PROMPT2]\""]);
    assert!(out_xterm.contains("p2=[u2> ]"), "{out_xterm}");
    assert!(
        std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default().trim().is_empty(),
        "TERM=dumb run must not clobber TERM=xterm-256color cache"
    );

    // 7) ZSH_VERSION mismatch in cache header invalidates cache.
    let star_caches = cached_entries(&init_dir, "starship", false);
    let star_cache = star_caches
        .iter()
        .find(|p| std::fs::read_to_string(p).is_ok_and(|c| c.contains("PROMPT2='u2> '")))
        .expect("missing active starship cache entry for u2");
    let contents = std::fs::read_to_string(star_cache).unwrap();
    let mut lines = contents.lines();
    let first_line = lines.next().unwrap();
    let rest: Vec<&str> = lines.collect();
    let stale_hdr = first_line.replacen("v2|", "v2|0.0.0-stale|", 1);
    std::fs::write(star_cache, format!("{stale_hdr}\n{}\n", rest.join("\n"))).unwrap();
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out7 = pty_session(&sb, &launch_user, &["print -r -- \"p2=[$PROMPT2]\""]);
    assert!(out7.contains("p2=[u2> ]"), "{out7}");
    assert!(
        std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default().contains("usercfg:init"),
        "header version mismatch must re-cache"
    );
}

#[test]
fn cached_tool_init_fallbacks_and_hostile_shell_options() {
    use std::os::unix::fs::PermissionsExt;
    let Some(zsh) = zsh() else { return };
    let sb = Sandbox::new(zsh.clone());
    let _ = std::fs::remove_file(sb.home.join("userbin/starship"));
    let sysbin = sb.home.join("sysbin");
    exe(
        &sysbin.join("starship"),
        "#!/bin/sh
case \"$1\" in
  init)
    echo 'PROMPT=\"starship-ok> \"'
    echo 'PROMPT2=\"$(starship prompt --continuation)\"'
    ;;
  prompt)
    printf 'cont-strict> '
    ;;
esac
",
    );
    exe(
        &sysbin.join("fzf"),
        "#!/bin/sh
[ \"$1\" = --zsh ] && echo 'fzf_ok=1'
",
    );
    // Hostile user options, function shadows, and aliases (defined after functions)
    // must not break caching or sourcing.
    sb.file(
        ".zshenv",
        "setopt noclobber shwordsplit\n\
         function read { return 1 }\n\
         function zstat { return 1 }\n\
         alias mv='mv -i' rm='rm -i' mkdir='false' eval='false' source='false' read='false' zstat='false'\n",
    );
    let mut settings = no_highlighting();
    settings.edit_plugin("fzf-tab", |p| p.enabled = false);
    settings.edit_plugin("autosuggestions", |p| p.enabled = false);
    let launch = sb.launch_with(settings.clone(), None);

    // 1a) Verify cold-cache generation, zcompile, and _goatty_starship_post PROMPT2 rewriting
    //     all succeed under ksharrays + nounset + errexit with zero stderr.
    let init_dir = sb.dirs.init_cache_dir();
    let _ = std::fs::remove_dir_all(&init_dir);
    let strict_out = sb.run(
        &launch,
        "command rm -rf -- \"${_goatty_cache}/init\"\n\
         builtin eval \"$(command sed -n '/^_goatty_cached_init()/,/^}/p' \"$_goatty_dir/.zshrc\")\"\n\
         setopt ksharrays nounset errexit\n\
         typeset -A goatty_settings=()\n\
         builtin unset STARSHIP_CONFIG PROMPT PROMPT2\n\
         builtin source -- \"$_goatty_dir/plugins/starship/zsh/init.zsh\"\n\
         print -r -- \"strict-ok:$PROMPT|p2=$PROMPT2\"",
    );
    assert!(strict_out.status.success(), "strict opts failed: {}", String::from_utf8_lossy(&strict_out.stderr));
    assert!(
        String::from_utf8_lossy(&strict_out.stderr).trim().is_empty(),
        "strict stderr: {}",
        String::from_utf8_lossy(&strict_out.stderr)
    );
    assert!(stdout(&strict_out).contains("strict-ok:starship-ok> |p2=cont-strict> "), "{}", stdout(&strict_out));
    assert_eq!(cached_entries(&init_dir, "starship", false).len(), 1);

    // 1b) Normal run populates/hits cache cleanly under hostile options, aliases, and function shadows,
    //     and zcompile -U does not bake builder aliases into compiled .zwc entries.
    let _ = std::fs::remove_dir_all(&init_dir);
    sb.file(
        ".zshenv",
        "setopt noclobber shwordsplit\n\
         function read { return 1 }\n\
         function zstat { return 1 }\n\
         probe_cmd() { builtin print -r -- GOOD_FN }\n\
         alias mv='mv -i' rm='rm -i' mkdir='false' eval='false' source='false' read='false' zstat='false' probe_cmd='echo BAD_ALIAS'\n",
    );
    exe(
        &sysbin.join("starship"),
        "#!/bin/sh
[ \"$1\" = init ] && echo 'PROMPT=\"starship-ok:$(probe_cmd)> \"'
",
    );
    let out1 = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(String::from_utf8_lossy(&out1.stderr).trim().is_empty(), "stderr: {}", String::from_utf8_lossy(&out1.stderr));
    assert_eq!(cached_entries(&init_dir, "starship", false).len(), 1);
    // Drop the alias for the second (cached .zwc) run: compiled code must resolve probe_cmd to GOOD_FN, not BAD_ALIAS.
    sb.file(
        ".zshenv",
        "setopt noclobber shwordsplit\n\
         function read { return 1 }\n\
         function zstat { return 1 }\n\
         alias mv='mv -i' rm='rm -i' mkdir='false' eval='false' source='false' read='false' zstat='false'\n\
         probe_cmd() { builtin print -r -- GOOD_FN }\n",
    );
    let out1_cached = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(stdout(&out1_cached).contains("p=[starship-ok:GOOD_FN> ]"), "{}", stdout(&out1_cached));

    // 2) Failing tool (exit 1) and empty stdout (exit 0) do not cache broken state or delete valid entries.
    let valid_before = cached_entries(&init_dir, "starship", false);
    assert_eq!(valid_before.len(), 1);
    exe(&sysbin.join("starship"), "#!/bin/sh\nexit 1\n");
    let out_broken = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(!stdout(&out_broken).contains("starship-ok:"), "stale cache used after failure: {}", stdout(&out_broken));
    assert!(valid_before[0].exists(), "failing run must not remove an earlier valid cache entry");
    assert_eq!(cached_entries(&init_dir, "starship", false).len(), 1, "failing run must not write a new entry");

    exe(&sysbin.join("starship"), "#!/bin/sh\nexit 0\n");
    let out_empty = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(!stdout(&out_empty).contains("starship-ok:"), "stale cache used after empty exit 0: {}", stdout(&out_empty));
    assert_eq!(cached_entries(&init_dir, "starship", false).len(), 1, "empty output must not write a new entry");

    // 3) Syntactically invalid tool output (fails zcompile) is never promoted into the cache file.
    let _ = std::fs::remove_dir_all(&init_dir);
    exe(
        &sysbin.join("starship"),
        "#!/bin/sh
[ \"$1\" = init ] && echo 'if then broken'
",
    );
    let _ = sb.run(&launch, "true");
    assert!(cached_entries(&init_dir, "starship", false).is_empty(), "syntax-broken output must not be cached");

    // 4) Non-directory at init/ pathname forces cache creation to fail regardless of runner privileges,
    //    and falls back cleanly to in-memory eval with zero stderr.
    exe(
        &sysbin.join("starship"),
        "#!/bin/sh
echo \"ro:$*\" >> \"$HOME/init-calls\"
[ \"$1\" = init ] && echo 'PROMPT=\"starship-ro> \"'
",
    );
    let _ = std::fs::remove_dir_all(&init_dir);
    std::fs::write(&init_dir, b"not a directory").unwrap();
    let out_ro_parent = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    let out_ro_pty = pty_session(&sb, &launch, &["print -r -- \"p=[$PROMPT] fzf=$fzf_ok\""]);
    std::fs::remove_file(&init_dir).unwrap();
    assert!(
        String::from_utf8_lossy(&out_ro_parent.stderr).trim().is_empty(),
        "unwritable cache path leaked stderr: {}",
        String::from_utf8_lossy(&out_ro_parent.stderr)
    );
    assert!(stdout(&out_ro_parent).contains("starship-ro> "), "{}", stdout(&out_ro_parent));
    assert!(out_ro_pty.contains("starship-ro> ") && out_ro_pty.contains("fzf=1"), "{out_ro_pty}");

    // 5) Active advisory zsystem flock falls back to eval without hanging or failing;
    //    killing the lock holder releases the kernel lock cleanly for subsequent runs.
    use std::io::BufRead;
    std::fs::create_dir_all(&init_dir).unwrap();
    std::fs::set_permissions(&init_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock_path = init_dir.join(".lock-starship");
    let mut holder = Command::new(&zsh)
        .env_clear()
        .envs(sb.base_env())
        .envs(launch.env.iter().cloned())
        .current_dir(&sb.home)
        .args([
            "-fc",
            "zmodload -F zsh/system b:zsystem && { : >>! \"$1\"; } && zsystem flock -t 0 -f fd -- \"$1\" && printf 'locked\\n' && IFS= read -r _",
            "_",
            lock_path.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "locked");

    let out_locked = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(String::from_utf8_lossy(&out_locked.stderr).trim().is_empty());
    assert!(stdout(&out_locked).contains("starship-ro> "));
    assert!(cached_entries(&init_dir, "starship", false).is_empty(), "active lock prevented write");

    let _ = holder.kill();
    let _ = holder.wait();
    let out_unlocked = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(String::from_utf8_lossy(&out_unlocked.stderr).trim().is_empty());
    assert!(stdout(&out_unlocked).contains("starship-ro> "));
    assert_eq!(cached_entries(&init_dir, "starship", false).len(), 1, "cache populated once lock released");

    // 6) Security: group/other-writable init dir, non-sticky 0o777 ancestor dir, or unsafe .zwc
    //    is never written to or sourced.
    let _ = std::fs::remove_dir_all(&init_dir);
    std::fs::create_dir_all(&init_dir).unwrap();
    std::fs::set_permissions(&init_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let out_unsafe_dir = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(String::from_utf8_lossy(&out_unsafe_dir.stderr).trim().is_empty());
    assert!(stdout(&out_unsafe_dir).contains("starship-ro> "));
    assert!(cached_entries(&init_dir, "starship", false).is_empty(), "group/other-writable dir must not be used");

    let unsafe_parent = sb.home.join("shared-nonsticky");
    std::fs::create_dir_all(&unsafe_parent).unwrap();
    std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o777)).unwrap();
    let unsafe_xdg = unsafe_parent.join("xdg");
    let unsafe_dirs = goatty_shell::Dirs::under(&unsafe_xdg);
    let mut unsafe_settings = settings.clone();
    unsafe_settings.shell.import.enabled = false;
    unsafe_settings.shell.program = Some(zsh.clone());
    let unsafe_ctx = goatty_shell::import::Context::new(sb.home.clone(), Some(zsh.clone()), sb.base_env(), &unsafe_dirs.zsh_dir());
    let mut unsafe_launch = Launcher::new(Some(unsafe_dirs.clone()), Some(sb.home.join("goattyd")))
        .with_import_context(unsafe_ctx)
        .launch(&Effective::plain(unsafe_settings));
    assert!(unsafe_launch.managed);
    unsafe_launch.env.retain(|(k, _)| k != "GOATTY_ORIG_ZDOTDIR");
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out_unsafe_parent = sb.run(&unsafe_launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(String::from_utf8_lossy(&out_unsafe_parent.stderr).trim().is_empty());
    assert!(stdout(&out_unsafe_parent).contains("starship-ro> "));
    assert!(!unsafe_dirs.init_cache_dir().exists(), "non-sticky 0777 ancestor must block cache dir creation");
    // Even if init_cache_dir() itself is pre-created with 0700 under the 0777 non-sticky ancestor, reject it.
    std::fs::create_dir_all(unsafe_dirs.init_cache_dir()).unwrap();
    std::fs::set_permissions(unsafe_dirs.init_cache_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _ = sb.run(&unsafe_launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(
        cached_entries(&unsafe_dirs.init_cache_dir(), "starship", false).is_empty(),
        "non-sticky 0777 ancestor must block cache writes even when init/ is 0700"
    );
    std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o700)).unwrap();

    std::fs::set_permissions(&init_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _ = sb.run(&launch, "true");
    let zwc = cached_entries(&init_dir, "starship", true);
    assert_eq!(zwc.len(), 1);
    std::fs::set_permissions(&zwc[0], std::fs::Permissions::from_mode(0o664)).unwrap();
    // Overwrite the .zsh body with a sentinel (making it newer than .zwc too): if the
    // unsafe .zwc were ignored on cache hit, zsh would source the sentinel instead of regenerating.
    let zsh_cache = cached_entries(&init_dir, "starship", false);
    let mut lines: Vec<String> = std::fs::read_to_string(&zsh_cache[0]).unwrap().lines().map(str::to_string).collect();
    lines[1] = "PROMPT=\"from-stale-body> \"".into();
    std::fs::write(&zsh_cache[0], format!("{}\n", lines.join("\n"))).unwrap();
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out_unsafe_zwc = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(stdout(&out_unsafe_zwc).contains("starship-ro> "), "unsafe .zwc mode must reject cache hit: {}", stdout(&out_unsafe_zwc));
    assert!(
        std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default().contains("ro:init"),
        "unsafe .zwc must force starship regeneration"
    );

    // 7) Missing .zwc with a syntax-broken .zsh body (retaining a valid key header) is rejected and regenerated.
    let zsh_cache2 = cached_entries(&init_dir, "starship", false);
    let zwc2 = cached_entries(&init_dir, "starship", true);
    assert_eq!(zsh_cache2.len(), 1);
    assert_eq!(zwc2.len(), 1);
    std::fs::remove_file(&zwc2[0]).unwrap();
    let hdr_only = std::fs::read_to_string(&zsh_cache2[0]).unwrap().lines().next().unwrap().to_string();
    std::fs::write(&zsh_cache2[0], format!("{hdr_only}\nif then broken\n")).unwrap();
    let _ = std::fs::remove_file(sb.home.join("init-calls"));
    let out_missing_zwc = sb.run(&launch, "print -r -- \"p=[$PROMPT]\"");
    assert!(
        String::from_utf8_lossy(&out_missing_zwc.stderr).trim().is_empty(),
        "missing .zwc leaked stderr: {}",
        String::from_utf8_lossy(&out_missing_zwc.stderr)
    );
    assert!(stdout(&out_missing_zwc).contains("starship-ro> "));
    assert!(
        std::fs::read_to_string(sb.home.join("init-calls")).unwrap_or_default().contains("ro:init"),
        "missing .zwc must trigger regeneration instead of sourcing broken .zsh"
    );
    assert_eq!(cached_entries(&init_dir, "starship", true).len(), 1, "missing .zwc must be rebuilt");

    // 8) fzf init warnings emitted at source time are preserved on cache hits.
    let _ = std::fs::remove_dir_all(&init_dir);
    exe(
        &sysbin.join("fzf"),
        "#!/bin/sh
[ \"$1\" = --zsh ] && printf '%s\\n' 'fzf_ok=warn' 'builtin print -u2 -- \"fzf-setup-warning\"'
",
    );
    let out_fzf_cold = pty_session(&sb, &launch, &["print -r -- \"fzf=$fzf_ok\""]);
    assert!(out_fzf_cold.contains("fzf=warn") && out_fzf_cold.contains("fzf-setup-warning"), "{out_fzf_cold}");
    let out_fzf_warm = pty_session(&sb, &launch, &["print -r -- \"fzf=$fzf_ok\""]);
    assert!(out_fzf_warm.contains("fzf=warn") && out_fzf_warm.contains("fzf-setup-warning"), "{out_fzf_warm}");
}
