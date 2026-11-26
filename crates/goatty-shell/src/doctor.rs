//! Diagnostics: ask a real managed shell what it loaded.
//!
//! What goattyd sees in its own `PATH` isn't what the shell sees: the
//! user's `.zprofile` usually adds directories. So the probe starts the
//! managed zsh exactly like a session would (login + interactive, no TTY),
//! runs `goatty-status`, and reports each feature's state plus how long the
//! startup took.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::launch::Launch;

const SENTINEL: &str = "<<goatty-status>>";

/// The terminal variables a session's shell gets (`goatty_term::pty::TERMINAL_ENV`,
/// kept equal by a test: this crate doesn't depend on `goatty-term`). goattyd,
/// started by a service manager or over SSH, may have no `TERM` at all, and
/// startup files that run `tput` then print errors only the probe sees (#316).
const TERMINAL_ENV: [(&str, &str); 4] = [
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", "goatty"),
    ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Loaded; the detail is the file or binary used.
    Ok(String),
    /// Enabled but its tool isn't installed.
    Missing,
    /// Turned off in the settings.
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feature {
    pub name: String,
    pub state: State,
}

#[derive(Debug, Clone)]
pub struct Probe {
    /// Wall time from spawn to exit: startup files plus `goatty-status`.
    pub elapsed: Duration,
    pub features: Vec<Feature>,
    /// Anything the startup printed to stderr (errors in startup files).
    pub stderr: String,
}

impl Probe {
    pub fn get(&self, name: &str) -> Option<&State> {
        self.features.iter().find(|f| f.name == name).map(|f| &f.state)
    }

    pub fn missing(&self) -> impl Iterator<Item = &str> {
        self.features.iter().filter(|f| f.state == State::Missing).map(|f| f.name.as_str())
    }
}

/// Start the managed shell from `launch` and collect `goatty-status`.
pub fn probe(launch: &Launch, timeout: Duration) -> Result<Probe> {
    let ran = run(launch, "goatty-status", timeout)?;
    Ok(Probe { elapsed: ran.elapsed, features: parse_status(&ran.report), stderr: ran.stderr })
}

/// What a managed shell printed after its startup.
pub struct Ran {
    pub elapsed: Duration,
    /// What `command` printed.
    pub report: String,
    pub stderr: String,
}

/// Start the managed shell from `launch` (login + interactive, no TTY), run
/// `command` once the startup is done, and collect what it printed.
pub fn run(launch: &Launch, command: &str, timeout: Duration) -> Result<Ran> {
    if !launch.managed {
        bail!("{} doesn't run goatty's managed configuration", launch.program);
    }
    let start = Instant::now();
    let mut child = shell_command(launch, command)?.spawn().with_context(|| format!("start {}", launch.program))?;
    let read_all = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = r.read_to_end(&mut s);
            String::from_utf8_lossy(&s).into_owned()
        })
    };
    let out = read_all(Box::new(child.stdout.take().context("stdout")?));
    let err = read_all(Box::new(child.stderr.take().context("stderr")?));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the shell didn't finish starting within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let elapsed = start.elapsed();
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some((_, report)) = stdout.split_once(SENTINEL) else {
        bail!("the shell exited ({status}) before reporting its status\n{}", stderr.trim_end());
    };
    Ok(Ran { elapsed, report: report.to_owned(), stderr })
}

/// The managed shell from `launch`, set up like a session's: it runs
/// `command` after its startup, with the output piped.
fn shell_command(launch: &Launch, command: &str) -> Result<Command> {
    let mut cmd = Command::new(&launch.program);
    if crate::launch::is_powershell(&launch.program) {
        let init_cmd = launch
            .args
            .windows(2)
            .find(|w| w[0].eq_ignore_ascii_case("-Command"))
            .map(|w| w[1].as_str())
            .context("managed PowerShell launch is missing -Command init")?;
        let script = format!("{init_cmd}; Write-Output '{SENTINEL}'; {command}");
        cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script]);
    } else {
        let script = format!("builtin print -r -- '{SENTINEL}'; {command}");
        cmd.args(["-l", "-i", "-c", &script]);
    }
    let cwd = crate::dirs::Dirs::home_dir().unwrap_or_else(|| if cfg!(windows) { ".".into() } else { "/".into() });
    // A session's terminal variables first, then the launch's.
    cmd.envs(TERMINAL_ENV)
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

/// Parse `goatty-status` lines: `name<TAB>ok<TAB>detail`, `name<TAB>missing`, `name<TAB>off`.
pub fn parse_status(text: &str) -> Vec<Feature> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let name = parts.next()?.trim();
            let state = match parts.next()? {
                "ok" => State::Ok(parts.next().unwrap_or_default().to_owned()),
                "missing" => State::Missing,
                "off" => State::Off,
                _ => return None,
            };
            (!name.is_empty()).then(|| Feature { name: name.to_owned(), state })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_gets_a_sessions_terminal_variables() {
        let launch =
            Launch { program: "zsh".into(), args: Vec::new(), env: vec![("COLORTERM".into(), "from-launch".into())], managed: true };
        let cmd = shell_command(&launch, "goatty-status").unwrap();
        // Later settings win, as in `Command::spawn`.
        let env =
            |name: &str| cmd.get_envs().filter(|(k, _)| *k == name).last().and_then(|(_, v)| v).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(env("TERM").as_deref(), Some("xterm-256color"));
        assert_eq!(env("TERM_PROGRAM").as_deref(), Some("goatty"));
        assert_eq!(env("COLORTERM").as_deref(), Some("from-launch"), "the launch's own variables win");
    }

    #[test]
    fn terminal_env_matches_a_sessions() {
        assert_eq!(TERMINAL_ENV, goatty_term::pty::TERMINAL_ENV);
    }

    #[test]
    fn parses_status_lines() {
        let f = parse_status("\nfzf-tab\tmissing\nfzf\toff\nstarship\tok\t/usr/bin/starship\nnoise line\nx\tweird\n");
        assert_eq!(
            f,
            vec![
                Feature { name: "fzf-tab".into(), state: State::Missing },
                Feature { name: "fzf".into(), state: State::Off },
                Feature { name: "starship".into(), state: State::Ok("/usr/bin/starship".into()) },
            ]
        );
    }
}
