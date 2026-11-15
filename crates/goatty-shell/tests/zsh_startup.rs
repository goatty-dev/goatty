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
        let home = tmp.path().join("home");
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

    fn launch_with(&self, mut settings: HostSettings, orig_zdotdir: Option<&Path>) -> Launch {
        settings.shell.program = Some(self.zsh.clone());
        let mut l = Launcher::new(Some(self.dirs.clone()), Some(self.home.join("goattyd"))).launch(&Effective::plain(settings));
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
