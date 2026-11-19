//! macOS launchd user agent (`~/Library/LaunchAgents/dev.goatty.goattyd.plist`).
//!
//! Uses modern `launchctl` domain subcommands (`bootstrap`, `bootout`,
//! `enable`, `kill`, `print`) targeting `gui/<uid>`, with on-demand socket
//! activation (`Sockets` -> `Listeners` -> `SockPathName`) claimed by
//! `goattyd serve` via `launch_activate_socket(3)`.
//!
//! Logs go to `~/Library/Logs/goatty/goattyd.log`, and the socket lives at
//! `~/Library/Caches/goatty/goattyd.sock` (persistent across reboots for
//! launchd, excluded from Time Machine, not TCC-protected, and well under
//! macOS's 104-byte `sun_path` limit).
//!
//! `goattyd restart` keeps sessions via in-place `execve` (§5c); launchd has
//! no fd store, so a crash or `launchctl kickstart -k` ends sessions.

#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use anyhow::{Context, Result, bail};

#[cfg(target_os = "macos")]
use super::{Installed, Status};

pub const LAUNCHD_LABEL: &str = "dev.goatty.goattyd";
pub const LAUNCHD_PLIST: &str = "dev.goatty.goattyd.plist";
/// Dictionary key under `<key>Sockets</key>`, passed to `launch_activate_socket`.
pub const LAUNCHD_SOCKET_NAME: &str = "Listeners";

/// Marker comment so we never overwrite or remove a user-authored plist.
const HEADER: &str = "<!-- Written by `goattyd service install`; `goattyd service uninstall` removes it. -->";

/// Octal `0600` in decimal: XML plists have no octal integer syntax
/// (`launchd.plist(5)`).
const SOCKET_MODE_DECIMAL: u32 = 0o600;

/// macOS `sockaddr_un.sun_path` is 104 bytes including NUL terminator.
#[cfg(target_os = "macos")]
const MAX_SOCKET_PATH_BYTES: usize = 103;

#[cfg(target_os = "macos")]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute())
}

/// `~/Library/LaunchAgents`.
#[cfg(target_os = "macos")]
pub fn unit_dir() -> Option<PathBuf> {
    Some(home_dir()?.join("Library/LaunchAgents"))
}

/// `~/Library/Logs/goatty/goattyd.log`.
#[cfg(target_os = "macos")]
pub fn log_path() -> Option<PathBuf> {
    Some(home_dir()?.join("Library/Logs/goatty/goattyd.log"))
}

/// The socket the LaunchAgent listens on: `~/Library/Caches/goatty/goattyd.sock`.
#[cfg(target_os = "macos")]
pub fn unit_socket_path() -> Option<PathBuf> {
    Some(home_dir()?.join("Library/Caches/goatty/goattyd.sock"))
}

/// The LaunchAgent plist is installed (whether or not it's bootstrapped).
#[cfg(target_os = "macos")]
pub fn installed() -> bool {
    unit_dir().is_some_and(|d| d.join(LAUNCHD_PLIST).is_file())
}

/// The goattyd binary the installed LaunchAgent runs.
#[cfg(target_os = "macos")]
pub fn service_program() -> Option<PathBuf> {
    let text = std::fs::read_to_string(unit_dir()?.join(LAUNCHD_PLIST)).ok()?;
    plist_program(&text)
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        let Some(semi) = rest.find(';') else {
            out.push('&');
            break;
        };
        let entity = &rest[..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether `text` carries the marker written by `goattyd service install`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_goatty_plist(text: &str) -> bool {
    text.contains(HEADER)
}

/// Render the macOS LaunchAgent XML property list.
pub fn render_launchd_plist(exe: &Path, socket_path: &Path, log_path: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let sock = xml_escape(&socket_path.to_string_lossy());
    let log = xml_escape(&log_path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
{HEADER}
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCHD_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>serve</string>
	</array>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>Sockets</key>
	<dict>
		<key>{LAUNCHD_SOCKET_NAME}</key>
		<dict>
			<key>SockPathName</key>
			<string>{sock}</string>
			<key>SockPathMode</key>
			<integer>{SOCKET_MODE_DECIMAL}</integer>
		</dict>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

/// Extract the `<string>` entries inside `<key>ProgramArguments</key><array>…</array>`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_program_args(plist: &str) -> Option<Vec<String>> {
    let after_key = plist.split_once("<key>ProgramArguments</key>")?.1;
    let after_array = after_key.split_once("<array>")?.1;
    let body = after_array.split_once("</array>")?.0;
    let mut args = Vec::new();
    let mut rest = body;
    while let Some((_, after_open)) = rest.split_once("<string>") {
        let (val, after_close) = after_open.split_once("</string>")?;
        args.push(xml_unescape(val));
        rest = after_close;
    }
    (!args.is_empty()).then_some(args)
}

/// The executable path from a plist's `ProgramArguments`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_program(plist: &str) -> Option<PathBuf> {
    plist_program_args(plist)?.into_iter().next().map(PathBuf::from)
}

/// Human-readable command line from a plist's `ProgramArguments`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_exec_display(plist: &str) -> Option<String> {
    let args = plist_program_args(plist)?;
    let formatted: Vec<String> = args
        .into_iter()
        .map(|a| {
            if a.chars().any(|c| c.is_whitespace() || c == '"') {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                a
            }
        })
        .collect();
    Some(formatted.join(" "))
}

/// Parsed live state from `launchctl print gui/<uid>/<label>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct LaunchdState {
    socket_active: bool,
    service_active: bool,
    pid: Option<u32>,
}

/// Parse `launchctl print gui/<uid>/dev.goatty.goattyd` output.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_launchctl_print(out: &str) -> LaunchdState {
    let mut state = LaunchdState::default();
    let mut depth: usize = 0;
    let mut in_sockets = false;
    let mut sockets_depth: usize = 0;

    for raw in out.lines() {
        let line = raw.trim();
        let opens = line.ends_with("= {") || line.ends_with("=> {") || line == "{";
        let closes = line == "}";

        if depth == 1 {
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                match k {
                    "state" | "job state" if v == "running" => {
                        state.service_active = true;
                    }
                    "pid" => {
                        if let Ok(pid) = v.parse::<u32>() {
                            state.pid = Some(pid);
                            state.service_active = true;
                        }
                    }
                    "sockets" if opens => {
                        in_sockets = true;
                        sockets_depth = depth + 1;
                    }
                    _ => {}
                }
            }
        } else if in_sockets
            && depth == sockets_depth
            && let Some((k, _)) = line.split_once('=')
            && k.trim().trim_matches('"') == LAUNCHD_SOCKET_NAME
        {
            state.socket_active = true;
        }

        if opens {
            depth += 1;
        } else if closes {
            depth = depth.saturating_sub(1);
            if in_sockets && depth < sockets_depth {
                in_sockets = false;
            }
        }
    }
    state
}

#[cfg(target_os = "macos")]
fn domain() -> String {
    // SAFETY: getuid is a plain, thread-safe syscall that cannot fail.
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}

#[cfg(target_os = "macos")]
fn target() -> String {
    format!("{}/{LAUNCHD_LABEL}", domain())
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Result<String> {
    let out = Command::new("launchctl").args(args).stdin(Stdio::null()).output().context("couldn't run launchctl")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("launchctl {}: {detail}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(target_os = "macos")]
fn query_launchd() -> Option<LaunchdState> {
    let out = launchctl(&["print", &target()]).ok()?;
    Some(parse_launchctl_print(&out))
}

#[cfg(target_os = "macos")]
fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(|| format!("chmod 0700 {}", dir.display()))?;
    Ok(())
}

/// Wait for `launchctl bootout` asynchronous teardown to complete before
/// re-bootstrapping (avoids macOS `Bootstrap failed: 5: Input/output error`).
#[cfg(target_os = "macos")]
fn wait_unloaded(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while query_launchd().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Enable and bootstrap the plist, retrying briefly on transient teardown races.
#[cfg(target_os = "macos")]
fn bootstrap_plist(plist_path: &Path) -> Result<()> {
    let dom = domain();
    let tgt = target();
    let _ = launchctl(&["enable", &tgt]);
    let plist_str = plist_path.to_string_lossy();
    let mut last_err = None;
    for attempt in 0..3 {
        match launchctl(&["bootstrap", &dom, &plist_str]) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
    Err(last_err.expect("at least one bootstrap attempt"))
}

#[cfg(target_os = "macos")]
pub fn start_socket() -> Result<()> {
    let dir = unit_dir().context("$HOME is not set")?;
    let plist_path = dir.join(LAUNCHD_PLIST);
    anyhow::ensure!(plist_path.is_file(), "{} is not installed", plist_path.display());

    let sock = unit_socket_path().context("$HOME is not set")?;
    if let Some(parent) = sock.parent() {
        ensure_private_dir(parent)?;
    }
    if let Some(log_dir) = log_path().and_then(|p| p.parent().map(Path::to_path_buf)) {
        ensure_private_dir(&log_dir)?;
    }

    // If the job is already loaded in launchd, the socket file may have been
    // removed (e.g. cache cleanup); bootout + bootstrap recreates it.
    if query_launchd().is_some() {
        let _ = launchctl(&["bootout", &target()]);
        wait_unloaded(Duration::from_secs(5));
    }
    bootstrap_plist(&plist_path)
}

#[cfg(target_os = "macos")]
pub fn stop_service() -> Result<()> {
    let tgt = target();
    let Some(before) = query_launchd() else { return Ok(()) };
    if !before.service_active {
        return Ok(());
    }
    let old_pid = before.pid;
    launchctl(&["kill", "SIGTERM", &tgt])?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut respawned = false;
    loop {
        match query_launchd() {
            None => return Ok(()),
            Some(st) if !st.service_active => return Ok(()),
            Some(st) if old_pid.is_some() && st.pid.is_some() && st.pid != old_pid => {
                respawned = true;
                break;
            }
            Some(_) if Instant::now() >= deadline => break,
            Some(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    if !respawned {
        let _ = launchctl(&["kill", "SIGKILL", &tgt]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match query_launchd() {
                None => return Ok(()),
                Some(st) if !st.service_active => return Ok(()),
                Some(st) if old_pid.is_some() && st.pid.is_some() && st.pid != old_pid => {
                    respawned = true;
                    break;
                }
                Some(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    // If launchd respawned it (e.g. a user-edited KeepAlive plist) or SIGKILL
    // didn't settle it, bootout guarantees it stops without respawning.
    if respawned || query_launchd().is_some_and(|st| st.service_active) {
        launchctl(&["bootout", &tgt])?;
        wait_unloaded(Duration::from_secs(5));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn status() -> Status {
    let unit_dir = unit_dir();
    let installed = installed();
    let plist = unit_dir.as_ref().and_then(|d| std::fs::read_to_string(d.join(LAUNCHD_PLIST)).ok());
    let exec_start = plist.as_deref().and_then(plist_exec_display);
    let live = query_launchd().unwrap_or_default();
    Status {
        unit_dir,
        installed,
        exec_start,
        socket_active: live.socket_active,
        service_active: live.service_active,
        linger: None,
        keeps_sessions: false,
        log_path: log_path(),
    }
}

#[cfg(target_os = "macos")]
pub fn install(exe: &Path, socket_path: &Path) -> Result<Installed> {
    let unit_socket = unit_socket_path().context("$HOME is not set")?;
    if socket_path != unit_socket {
        bail!(
            "goattyd uses {} (GOATTY_SOCKET or XDG_RUNTIME_DIR?), but the launchd agent listens on {}",
            socket_path.display(),
            unit_socket.display()
        );
    }
    let sock_len = socket_path.as_os_str().as_encoded_bytes().len();
    anyhow::ensure!(
        sock_len <= MAX_SOCKET_PATH_BYTES,
        "socket path {} is {} bytes (macOS Unix sockets allow at most {MAX_SOCKET_PATH_BYTES})",
        socket_path.display(),
        sock_len
    );

    let dom = domain();
    launchctl(&["print-disabled", &dom]).with_context(|| format!("no launchd GUI session ({dom}) is available for this user"))?;

    let dir = unit_dir().context("$HOME is not set")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    if let Some(sock_dir) = socket_path.parent() {
        ensure_private_dir(sock_dir)?;
    }
    let log = log_path().context("$HOME is not set")?;
    if let Some(log_dir) = log.parent() {
        ensure_private_dir(log_dir)?;
    }

    let plist_path = dir.join(LAUNCHD_PLIST);
    let rendered = render_launchd_plist(exe, socket_path, &log);
    let existing = match std::fs::read_to_string(&plist_path) {
        Ok(text) => {
            if !is_goatty_plist(&text) {
                bail!("{} exists and wasn't written by goatty; remove it first", plist_path.display());
            }
            Some(text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", plist_path.display())),
    };
    let unchanged = existing.as_deref() == Some(rendered.as_str());

    std::fs::write(&plist_path, &rendered).with_context(|| format!("write {}", plist_path.display()))?;
    // Apple recommends 0644 (never group/world-writable) for LaunchAgent plists.
    std::fs::set_permissions(&plist_path, std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod 0644 {}", plist_path.display()))?;

    let live = query_launchd();
    let already_loaded = live.is_some();
    // A daemon started outside launchd (setsid) owns the socket path;
    // bootstrapping over it would orphan it. Let it finish first.
    let foreign = !already_loaded && std::os::unix::net::UnixStream::connect(socket_path).is_ok();
    if foreign {
        return Ok(Installed { unit_dir: dir, started: false, linger: None });
    }

    if already_loaded && unchanged && live.as_ref().is_some_and(|s| s.socket_active) && socket_path.exists() {
        let _ = launchctl(&["enable", &target()]);
        return Ok(Installed { unit_dir: dir, started: true, linger: None });
    }

    if already_loaded {
        let _ = launchctl(&["bootout", &target()]);
        wait_unloaded(Duration::from_secs(5));
    }
    // Remove a stale socket left behind by an earlier non-launchd crash.
    if socket_path.exists() && std::os::unix::net::UnixStream::connect(socket_path).is_err() {
        let _ = std::fs::remove_file(socket_path);
    }
    bootstrap_plist(&plist_path)?;
    Ok(Installed { unit_dir: dir, started: true, linger: None })
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<bool> {
    let dir = unit_dir().context("$HOME is not set")?;
    let path = dir.join(LAUNCHD_PLIST);
    match std::fs::read_to_string(&path) {
        Ok(text) if is_goatty_plist(&text) => {
            let _ = launchctl(&["bootout", &target()]);
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            Ok(true)
        }
        Ok(_) => bail!("{} wasn't written by goatty; leaving it alone", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_well_formed_and_round_trips_program() {
        let exe = Path::new("/Users/a & b/bin/<goatty'd>\"x\"/goattyd");
        let sock = Path::new("/Users/a & b/Library/Caches/goatty/goattyd.sock");
        let log = Path::new("/Users/a & b/Library/Logs/goatty/goattyd.log");
        let plist = render_launchd_plist(exe, sock, log);

        assert!(is_goatty_plist(&plist));
        assert!(plist.contains("<key>Label</key>\n\t<string>dev.goatty.goattyd</string>"));
        assert!(plist.contains("<key>ProcessType</key>\n\t<string>Interactive</string>"));
        assert!(plist.contains("<key>Listeners</key>"));
        assert!(plist.contains("<key>SockPathMode</key>\n\t\t\t<integer>384</integer>"));
        assert!(plist.contains("&amp;"), "escapes ampersands: {plist}");
        assert!(plist.contains("&lt;goatty&apos;d&gt;&quot;x&quot;"), "escapes XML chars: {plist}");
        assert!(!plist.contains("<key>KeepAlive</key>"), "on-demand socket activation should not force KeepAlive");
        assert_eq!(plist_program(&plist).unwrap(), exe);
        assert_eq!(plist_exec_display(&plist).unwrap(), r#""/Users/a & b/bin/<goatty'd>\"x\"/goattyd" serve"#);
    }

    #[test]
    fn xml_unescape_handles_entities_and_unknowns() {
        assert_eq!(xml_unescape("a &amp; b &lt;c&gt; &quot;d&quot; &apos;e&apos;"), "a & b <c> \"d\" 'e'");
        assert_eq!(xml_unescape("&#65;&#x42;"), "AB");
        assert_eq!(xml_unescape("keep &unknown; and &dangling"), "keep &unknown; and &dangling");
    }

    #[test]
    fn parses_launchctl_print_running() {
        let sample = r#"
gui/501/dev.goatty.goattyd = {
	active count = 1
	path = /Users/alice/Library/LaunchAgents/dev.goatty.goattyd.plist
	type = LaunchAgent
	state = running

	program = /Users/alice/.local/bin/goattyd
	arguments = {
		/Users/alice/.local/bin/goattyd
		serve
	}
	pid = 48291
	last exit code = (never exited)

	sockets = {
		"Listeners" = {
			type = stream
			path = /Users/alice/Library/Caches/goatty/goattyd.sock
			mode = 140600
			sockets = {
				7 (no bytes to read)
			}
			active = 1
			passive = 1
		}
	}

	spawn type = interactive (4)
	job state = running
}
"#;
        assert_eq!(parse_launchctl_print(sample), LaunchdState { socket_active: true, service_active: true, pid: Some(48291) });
    }

    #[test]
    fn parses_launchctl_print_idle_socket_listening() {
        let sample = r#"
gui/501/dev.goatty.goattyd = {
	active count = 0
	path = /Users/alice/Library/LaunchAgents/dev.goatty.goattyd.plist
	state = not running

	program = /Users/alice/bin/{braces}/goattyd
	arguments = {
		/Users/alice/bin/{braces}/goattyd
		serve
	}

	sockets = {
		"Listeners" = {
			type = stream
			path = /Users/alice/Library/Caches/goatty/goattyd.sock
			sockets = {
				6 (no bytes to read)
			}
			active = 0
			passive = 1
		}
	}

	job state = waiting
}
"#;
        assert_eq!(parse_launchctl_print(sample), LaunchdState { socket_active: true, service_active: false, pid: None });
    }

    #[test]
    fn parses_launchctl_print_without_listeners_socket() {
        let sample = r#"
gui/501/dev.goatty.goattyd = {
	active count = 0
	state = not running
	sockets = {}
}
"#;
        assert_eq!(parse_launchctl_print(sample), LaunchdState { socket_active: false, service_active: false, pid: None });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rendered_plist_passes_plutil_lint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAUNCHD_PLIST);
        let plist = render_launchd_plist(
            Path::new("/Users/test & dev/bin/goattyd"),
            Path::new("/Users/test & dev/Library/Caches/goatty/goattyd.sock"),
            Path::new("/Users/test & dev/Library/Logs/goatty/goattyd.log"),
        );
        std::fs::write(&path, plist).unwrap();
        let out = Command::new("plutil").args(["-lint"]).arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "plutil -lint failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
