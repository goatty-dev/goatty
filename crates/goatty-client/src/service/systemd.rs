//! Linux systemd user units (`goattyd.socket` + `goattyd.service`).
//!
//! Two units in `~/.config/systemd/user/`:
//!
//! * `goattyd.socket` listens on `%t/goatty/goattyd.sock` (the default
//!   socket path) and starts the daemon on the first connection,
//! * `goattyd.service` runs `goattyd serve`, which takes the socket from
//!   systemd (`LISTEN_FDS`).
//!
//! The service has no `Requires=goattyd.socket`, so stopping or removing the
//! socket never takes running sessions with it; only stopping the service
//! (or `goattyd stop`) does.
//!
//! Sessions outlive the daemon process (#131): goattyd keeps every PTY in
//! the service's fd store (`FileDescriptorStoreMax=`, `NotifyAccess=main`),
//! and `KillMode=process` leaves the shells alone when the daemon process
//! ends.

use std::path::{Path, PathBuf};
#[cfg(not(target_os = "macos"))]
use std::process::{Command, Stdio};

#[cfg(not(target_os = "macos"))]
use anyhow::{Context, Result, bail};

#[cfg(not(target_os = "macos"))]
use super::{Installed, Status};

pub const SOCKET_UNIT: &str = "goattyd.socket";
pub const SERVICE_UNIT: &str = "goattyd.service";

/// First line of the units we write, so we never overwrite someone else's.
const HEADER: &str = "# Written by `goattyd service install`; `goattyd service uninstall` removes it.";

/// `$XDG_CONFIG_HOME/systemd/user`.
#[cfg(not(target_os = "macos"))]
pub fn unit_dir() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("systemd/user"))
}

/// The socket the units listen on: `$XDG_RUNTIME_DIR/goatty/goattyd.sock`.
#[cfg(not(target_os = "macos"))]
pub fn unit_socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(|r| PathBuf::from(r).join("goatty/goattyd.sock"))
}

/// The socket unit is installed (whether or not it's running).
#[cfg(not(target_os = "macos"))]
pub fn installed() -> bool {
    unit_dir().is_some_and(|d| d.join(SOCKET_UNIT).is_file())
}

/// Quote a word for a systemd `ExecStart=` line.
fn systemd_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The program in an `ExecStart=` line we wrote (the reverse of
/// [`systemd_quote`] for its first word).
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn exec_program(exec_start: &str) -> Option<PathBuf> {
    let rest = exec_start.strip_prefix('"')?;
    let (mut out, mut chars) = (String::new(), rest.chars());
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(PathBuf::from(out)),
            '\\' => out.push(chars.next()?),
            '%' | '$' => {
                chars.next().filter(|n| *n == c)?;
                out.push(c);
            }
            c => out.push(c),
        }
    }
    None
}

/// The goattyd the installed service runs.
#[cfg(not(target_os = "macos"))]
pub fn service_program() -> Option<PathBuf> {
    let text = std::fs::read_to_string(unit_dir()?.join(SERVICE_UNIT)).ok()?;
    text.lines().find_map(|l| l.strip_prefix("ExecStart=")).and_then(exec_program)
}

pub fn render_socket() -> String {
    format!(
        "{HEADER}
[Unit]
Description=goatty session daemon socket
Documentation=https://github.com/goatty-dev/goatty/blob/main/docs/remote-architecture.md

[Socket]
ListenStream=%t/goatty/goattyd.sock
SocketMode=0600
DirectoryMode=0700

[Install]
WantedBy=sockets.target
"
    )
}

pub fn render_service(exe: &Path) -> String {
    format!(
        "{HEADER}
[Unit]
Description=goatty session daemon
Documentation=https://github.com/goatty-dev/goatty/blob/main/docs/remote-architecture.md
After={SOCKET_UNIT}

[Service]
ExecStart={} serve
# goattyd keeps each session's terminal in the fd store, and the shells stay
# when the daemon process ends: a crash or `systemctl --user restart` keeps
# the sessions. `systemctl --user stop` empties the store, which closes the
# terminals and ends the shells (SIGHUP), like `goattyd stop`.
NotifyAccess=main
FileDescriptorStoreMax=4096
KillMode=process
Restart=on-failure
RestartSec=2
",
        systemd_quote(&exe.to_string_lossy())
    )
}

#[cfg(not(target_os = "macos"))]
fn systemctl(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("couldn't run systemctl (is systemd installed?)")?;
    if !out.status.success() {
        bail!("systemctl --user {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(not(target_os = "macos"))]
fn is_active(unit: &str) -> bool {
    systemctl(&["is-active", "--quiet", unit]).is_ok()
}

#[cfg(not(target_os = "macos"))]
pub fn service_active() -> bool {
    is_active(SERVICE_UNIT)
}

#[cfg(not(target_os = "macos"))]
pub fn start_socket() -> Result<()> {
    systemctl(&["start", SOCKET_UNIT]).map(drop)
}

#[cfg(not(target_os = "macos"))]
pub fn stop_service() -> Result<()> {
    systemctl(&["stop", SERVICE_UNIT]).map(drop)
}

#[cfg(not(target_os = "macos"))]
pub fn linger() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("loginctl").args(["show-user", &user, "--property=Linger", "--value"]).stdin(Stdio::null()).output().ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn status() -> Status {
    let unit_dir = unit_dir();
    let installed = installed();
    let service = unit_dir.as_ref().and_then(|d| std::fs::read_to_string(d.join(SERVICE_UNIT)).ok());
    let exec_start = service.as_ref().and_then(|s| s.lines().find_map(|l| l.strip_prefix("ExecStart=").map(String::from)));
    let keeps_sessions = service.as_ref().is_some_and(|s| keeps_sessions(s));
    Status {
        unit_dir,
        installed,
        exec_start,
        socket_active: is_active(SOCKET_UNIT),
        service_active: is_active(SERVICE_UNIT),
        linger: linger(),
        keeps_sessions,
        log_path: None,
    }
}

/// A service unit that has the fd store and leaves the shells running.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn keeps_sessions(unit: &str) -> bool {
    let has = |line: &str| unit.lines().any(|l| l.trim() == line);
    unit.lines().any(|l| l.starts_with("FileDescriptorStoreMax=")) && has("NotifyAccess=main") && has("KillMode=process")
}

#[cfg(not(target_os = "macos"))]
pub fn install(exe: &Path, socket_path: &Path) -> Result<Installed> {
    let unit_socket = unit_socket_path().context("XDG_RUNTIME_DIR is not set (no systemd user session?)")?;
    if socket_path != unit_socket {
        bail!("goattyd uses {} (GOATTY_SOCKET?), but the systemd socket unit listens on {}", socket_path.display(), unit_socket.display());
    }
    systemctl(&["show-environment"]).context("no systemd user manager is running for this user")?;
    let dir = unit_dir().context("$HOME is not set")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    for (name, text) in [(SOCKET_UNIT, render_socket()), (SERVICE_UNIT, render_service(exe))] {
        let path = dir.join(name);
        if let Ok(existing) = std::fs::read_to_string(&path)
            && !existing.starts_with(HEADER)
        {
            bail!("{} exists and wasn't written by goatty; remove it first", path.display());
        }
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SOCKET_UNIT])?;
    // A daemon started the old way (setsid) owns the socket path; binding the
    // unit's socket over it would orphan it. Let it finish first.
    let foreign = !is_active(SERVICE_UNIT) && std::os::unix::net::UnixStream::connect(socket_path).is_ok();
    if !foreign {
        start_socket()?;
    }
    Ok(Installed { unit_dir: dir, started: !foreign, linger: linger() })
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> Result<bool> {
    let dir = unit_dir().context("$HOME is not set")?;
    let mut removed = false;
    if dir.join(SOCKET_UNIT).exists() {
        let _ = systemctl(&["disable", SOCKET_UNIT]);
        let _ = systemctl(&["stop", SOCKET_UNIT]);
    }
    for name in [SOCKET_UNIT, SERVICE_UNIT] {
        let path = dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.starts_with(HEADER) => {
                std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
                removed = true;
            }
            Ok(_) => bail!("{} wasn't written by goatty; leaving it alone", path.display()),
            Err(_) => {}
        }
    }
    if removed {
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_are_well_formed() {
        let socket = render_socket();
        assert!(socket.starts_with(HEADER));
        assert!(socket.contains("ListenStream=%t/goatty/goattyd.sock\nSocketMode=0600\nDirectoryMode=0700"));
        let service = render_service(Path::new("/home/a b/100%/$x/goattyd"));
        assert!(service.contains("ExecStart=\"/home/a b/100%%/$$x/goattyd\" serve\n"), "{service}");
        // Stopping the socket must not stop the sessions.
        assert!(!service.contains("Requires="));
        assert!(keeps_sessions(&service));
        assert!(!keeps_sessions("[Service]\nExecStart=goattyd serve\nRestart=on-failure\n"));
        assert_eq!(systemd_quote(r#"a"b\c"#), r#""a\"b\\c""#);
        let exec = service.lines().find_map(|l| l.strip_prefix("ExecStart=")).unwrap();
        assert_eq!(exec_program(exec).unwrap(), Path::new("/home/a b/100%/$x/goattyd"));
        assert_eq!(exec_program(&format!("{} serve", systemd_quote(r#"/a"b\c"#))).unwrap(), Path::new(r#"/a"b\c"#));
        assert_eq!(exec_program("/usr/bin/goattyd serve"), None);
    }
}
