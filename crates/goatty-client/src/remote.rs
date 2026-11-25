//! goattyd on other hosts: finding it, installing a matching one, and
//! telling apart "not installed" and "wrong version".
//!
//! Like VS Code's remote server, goatty brings its daemon along: every goatty
//! build knows the exact goattyd it wants on a host,
//! `~/.local/share/goatty/bin/goattyd-<version>-p<protocol>`, and can upload
//! it over the same ssh (after the user said yes). Nothing outside that
//! directory changes on the host: no PATH, no dotfiles, no sudo.
//!
//! The connect script tells the client what it found on stderr in one line,
//! `goatty-remote: <os> <arch> <goattyd path or ->`, before it starts the
//! proxy. That line is how the client learns the host's platform (to pick a
//! binary) without another ssh round trip.
//!
//! **Windows hosts.** Win32-OpenSSH runs remote commands with cmd.exe (or
//! PowerShell, when the admin made that the default shell), not sh. When the
//! connect script fails the way those shells fail ("'sh' is not
//! recognized", exit code 9009), or sh turns out to be Git Bash's, the host
//! is marked Windows ([`SshTarget::windows`]) and asked again, the Windows
//! way:
//!
//! * goattyd lives in `AppData\Local\goatty\bin` under the directory the
//!   session starts in (the user's home: [`WINDOWS_BIN_DIR`]), as
//!   `goattyd-<version>-p<protocol>.exe`,
//! * the connect command is just that relative path plus `proxy`, which
//!   cmd.exe and PowerShell both run as is,
//! * the probe, install, and the not-installed check are PowerShell scripts
//!   passed as `-EncodedCommand` (base64), so no quoting rule of either
//!   shell applies.
//!
//! Only this build's installed goattyd (or `--remote-daemon`) is used there:
//! there is no search through `PATH`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use goatty_proto::PROTOCOL_VERSION;

use crate::handshake::VersionMismatch;
use crate::transport::{SshTarget, sh_quote};

/// Where installed daemons live on a host, relative to `$HOME`.
pub const BIN_DIR: &str = ".local/share/goatty/bin";

/// Where installed daemons live on a Windows host, relative to the directory
/// its SSH sessions start in (the user's profile, so this is
/// `%LOCALAPPDATA%\goatty\bin` normally).
pub const WINDOWS_BIN_DIR: &str = r"AppData\Local\goatty\bin";

const MARKER: &str = "goatty-remote:";

/// The file name of the goattyd this build installs and prefers on hosts.
pub fn binary_name() -> String {
    format!("goattyd-{}-p{PROTOCOL_VERSION}", env!("CARGO_PKG_VERSION"))
}

/// [`binary_name`] on a Windows host.
pub fn windows_binary_name() -> String {
    format!("{}.exe", binary_name())
}

/// This build's goattyd on a Windows host, relative to where sessions start.
fn windows_installed() -> String {
    format!(r".\{WINDOWS_BIN_DIR}\{}", windows_binary_name())
}

/// The command that runs goattyd's proxy on a Windows host: this build's
/// installed goattyd, or `remote_daemon`. Free of quotes, spaces and other
/// characters cmd.exe or PowerShell treat specially, so both run it
/// unchanged.
pub(crate) fn windows_proxy_command(remote_daemon: Option<&str>) -> Result<String> {
    Ok(format!("{} proxy", windows_program(remote_daemon)?))
}

fn windows_program(remote_daemon: Option<&str>) -> Result<String> {
    match remote_daemon {
        None => Ok(windows_installed()),
        Some(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || r"\/:._-+~".contains(c)) => Ok(p.to_string()),
        Some(p) => bail!("--remote-daemon on a Windows host must be a path without spaces or special characters: {p}"),
    }
}

/// `powershell … -EncodedCommand <script>`: runs `script` from cmd.exe and
/// from PowerShell alike, as the base64 has no character either treats
/// specially.
fn powershell(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!("powershell -NoLogo -NoProfile -NonInteractive -EncodedCommand {}", base64(&utf16))
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// The PowerShell script that reports a Windows host's platform and this
/// build's goattyd there, as the connect script does (`goatty-remote: Windows
/// <arch> <path or ->`, on stderr).
fn windows_probe_script() -> String {
    format!(
        "$ProgressPreference='SilentlyContinue'\n\
         $a=$env:PROCESSOR_ARCHITEW6432;if(-not $a){{$a=$env:PROCESSOR_ARCHITECTURE}}\n\
         $p=Join-Path (Get-Location).Path '{WINDOWS_BIN_DIR}\\{}'\n\
         if(-not(Test-Path -LiteralPath $p -PathType Leaf)){{$p='-'}}\n\
         [Console]::Error.WriteLine(\"{MARKER} Windows $a $p\")\n",
        windows_binary_name()
    )
}

/// Ask a Windows host for its platform and this build's goattyd.
pub(crate) fn probe_windows(target: &SshTarget, interactive: bool) -> Result<Probe> {
    let out = target
        .command_with(&powershell(&windows_probe_script()), interactive)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("couldn't run {}", target.program))?;
    let stderr: Vec<String> = String::from_utf8_lossy(&out.stderr).lines().map(String::from).collect();
    parse_probe(&stderr).with_context(|| format!("asking {} about Windows failed: {}", target.host, last_words(&out.stderr, &out.stdout)))
}

/// The remote command failed without saying anything goatty understands,
/// but ssh itself worked (it exits with 255 on its own errors, else with the
/// remote command's status). That's what the connect script looks like to a
/// Windows shell: cmd.exe stumbles over its redirections or doesn't find
/// `sh` (9009), PowerShell doesn't find `sh` either, in whatever language
/// the host speaks.
fn remote_command_failed(exit: Option<i32>) -> bool {
    exit.is_some_and(|code| code != 0 && code != 255)
}

/// Shell snippet run on the remote host: use this build's installed
/// goattyd, else one in `PATH`, `~/.local/bin` or `~/.cargo/bin`
/// (non-interactive SSH often lacks those in PATH). Plain POSIX sh, since
/// the user's login shell may be fish.
pub(crate) fn proxy_script(remote_daemon: Option<&str>) -> String {
    let candidates = match remote_daemon {
        Some(p) => sh_quote(p),
        None => format!("\"$HOME/{BIN_DIR}/{}\" goattyd \"$HOME/.local/bin/goattyd\" \"$HOME/.cargo/bin/goattyd\"", binary_name()),
    };
    format!(
        "p=\"$(uname -s) $(uname -m)\"; \
         for d in {candidates}; do if command -v \"$d\" >/dev/null 2>&1; then echo \"{MARKER} $p $d\" >&2; exec \"$d\" proxy; fi; done; \
         echo \"{MARKER} $p -\" >&2; \
         echo 'goatty: goattyd not found on remote host (install it or pass --remote-daemon PATH)' >&2; exit 127"
    )
}

/// A host's OS and CPU as `uname -s` / `uname -m` print them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

impl Platform {
    /// The goattyd build for this platform (`x86_64-linux`,
    /// `x86_64-windows`), if goatty has one.
    pub fn target(&self) -> Option<&'static str> {
        let windows = self.is_windows();
        if !windows && !self.os.eq_ignore_ascii_case("linux") {
            return None;
        }
        match (self.arch.to_ascii_lowercase().as_str(), windows) {
            ("x86_64" | "amd64", false) => Some("x86_64-linux"),
            ("aarch64" | "arm64", false) => Some("aarch64-linux"),
            ("x86_64" | "amd64", true) => Some("x86_64-windows"),
            ("aarch64" | "arm64", true) => Some("aarch64-windows"),
            _ => None,
        }
    }

    /// Windows, as the Windows probe reports it, or as Git Bash, MSYS2 and
    /// Cygwin's `uname -s` do (`MINGW64_NT-10.0-19045`).
    pub fn is_windows(&self) -> bool {
        let os = self.os.to_ascii_uppercase();
        os == "WINDOWS" || ["MINGW", "MSYS", "CYGWIN"].iter().any(|p| os.starts_with(p))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.os, self.arch)
    }
}

/// What the connect script reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub platform: Platform,
    /// The goattyd it started, `None` when it found none.
    pub daemon: Option<String>,
}

impl Probe {
    /// The script ran this build's own installed goattyd.
    pub fn runs_ours(&self) -> bool {
        self.daemon.as_deref().is_some_and(|d| {
            d.ends_with(&format!("/{BIN_DIR}/{}", binary_name()))
                || d.to_ascii_lowercase().ends_with(&format!(r"\{WINDOWS_BIN_DIR}\{}", windows_binary_name()).to_ascii_lowercase())
        })
    }
}

/// Find the connect script's report in a transport's stderr.
pub fn parse_probe(lines: &[String]) -> Option<Probe> {
    lines.iter().rev().find_map(|l| {
        let rest = l.strip_prefix(MARKER)?.trim_start();
        let mut parts = rest.splitn(3, ' ');
        let os = parts.next()?.to_string();
        let arch = parts.next()?.to_string();
        let daemon = parts.next()?.trim();
        Some(Probe { platform: Platform { os, arch }, daemon: (daemon != "-").then(|| daemon.to_string()) })
    })
}

/// Why connecting to goattyd on a host failed, when goatty can fix it.
#[derive(Debug, Clone)]
pub enum RemoteProblem {
    /// No goattyd on the host.
    NotInstalled { host: String, platform: Platform },
    /// A goattyd answered (or the installed one is running), but it speaks
    /// no protocol version we do.
    Incompatible { host: String, mismatch: VersionMismatch, probe: Option<Probe> },
}

impl fmt::Display for RemoteProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteProblem::NotInstalled { host, platform } => write!(f, "goattyd isn't installed on {host} ({platform})"),
            RemoteProblem::Incompatible { host, mismatch, .. } => write!(f, "{host}: {mismatch}"),
        }
    }
}

impl std::error::Error for RemoteProblem {}

/// A goattyd binary for some target on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundled {
    pub path: PathBuf,
    /// The goattyd next to this goatty, used because the host has the same
    /// CPU: dynamically linked in development builds, so it may not run on
    /// a host with an older C library (the install checks that it runs).
    pub this_build: bool,
}

/// The goattyd to upload for `target`. Release archives carry static
/// builds in `remote/goattyd-<target>` next to the `goatty` binary
/// (packages may use `../lib/goatty/remote/`).
pub fn find_bundled(target: &str) -> Option<Bundled> {
    let exe = std::env::current_exe().ok()?;
    find_bundled_near(exe.parent()?, target)
}

fn find_bundled_near(dir: &Path, target: &str) -> Option<Bundled> {
    let exe = if target.ends_with("-windows") { ".exe" } else { "" };
    let name = format!("goattyd-{target}{exe}");
    for candidate in [dir.join("remote").join(&name), dir.join("../lib/goatty/remote").join(&name)] {
        if candidate.is_file() {
            return Some(Bundled { path: candidate, this_build: false });
        }
    }
    let own = dir.join(format!("goattyd{}", std::env::consts::EXE_SUFFIX));
    (own.is_file() && target == format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS))
        .then_some(Bundled { path: own, this_build: true })
}

fn install_script(size: u64) -> String {
    let name = binary_name();
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"set -e
d="$HOME/{BIN_DIR}"; n="{name}"
mkdir -p "$d"
t="$d/.$n.$$"
trap 'rm -f "$t"' EXIT
cat > "$t"
s=$(($(wc -c < "$t")))
[ "$s" -eq {size} ] || {{ echo "upload incomplete ($s of {size} bytes)" >&2; exit 1; }}
chmod 755 "$t"
v=$("$t" --version 2>&1) || {{ echo "the uploaded goattyd doesn't run on this host: $v" >&2; exit 1; }}
case "$v" in "goattyd {version}"*) ;; *) echo "unexpected goattyd version: $v" >&2; exit 1;; esac
mv -f "$t" "$d/$n"
echo "installed $d/$n"
if [ -L "$d/goattyd" ] || [ ! -e "$d/goattyd" ]; then ln -sf "$n" "$d/.goattyd.$$" && mv -f "$d/.goattyd.$$" "$d/goattyd"; fi
if [ -e /proc/self/exe ]; then
  pd=$(cd "$d" && pwd -P)
  for f in "$d"/goattyd-*; do
    b=${{f##*/}}
    [ -f "$f" ] && [ "$b" != "$n" ] || continue
    keep=
    for e in /proc/[0-9]*/exe; do
      [ "$(readlink "$e" 2>/dev/null)" = "$pd/$b" ] && {{ keep=1; break; }}
    done
    [ -n "$keep" ] || {{ rm -f "$f" && echo "removed $f"; }} || :
  done
fi
"#
    )
}

/// Upload `binary` to the host as this build's goattyd (see
/// [`binary_name`]), after checking it arrived whole and runs there, and
/// point the unversioned `goattyd` symlink in [`BIN_DIR`] at it, so
/// `goattyd` works in the host's managed zsh (which has that folder on
/// `PATH`). A `goattyd` there that isn't a symlink is left alone.
/// Afterwards, older `goattyd-*` builds in [`BIN_DIR`] are deleted unless a
/// running process uses them (found through `/proc`; hosts without it keep
/// everything).
///
/// On a Windows host ([`SshTarget::is_windows`], or a `.exe` to upload) it
/// goes to [`WINDOWS_BIN_DIR`] as [`windows_binary_name`], with the same
/// checks; older builds there are deleted unless they run (Windows doesn't
/// let a running program's file go).
pub fn install(target: &SshTarget, binary: &Path) -> Result<()> {
    let file = std::fs::File::open(binary).with_context(|| format!("open {}", binary.display()))?;
    let size = file.metadata()?.len();
    let windows = target.is_windows() || binary.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    let command = if windows { powershell(&windows_install_script(size)) } else { format!("sh -c {}", sh_quote(&install_script(size))) };
    log::info!("installing {} on {} ({size} bytes)", binary_name(), target.host);
    let out = target
        .command(&command)
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("couldn't run {}", target.program))?;
    if !out.status.success() {
        bail!("installing goattyd on {} failed: {}", target.host, last_words(&out.stderr, &out.stdout));
    }
    Ok(())
}

/// Stop whatever goattyd owns the daemon socket on the host, using this
/// build's installed goattyd (`goattyd stop --force` works on daemons of any
/// version). Its sessions end.
pub fn stop_daemon(target: &SshTarget) -> Result<()> {
    let out = target
        .command(&daemon_command(target, "stop --force"))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("couldn't run {}", target.program))?;
    if !out.status.success() {
        bail!("stopping goattyd on {} failed: {}", target.host, last_words(&out.stderr, &out.stdout));
    }
    Ok(())
}

/// Restart the host's goattyd with this build's installed one
/// (`goattyd restart --force`): its sessions carry over when the running
/// daemon can do that (protocol v10 on), else it's stopped and they end.
pub fn restart_daemon(target: &SshTarget) -> Result<()> {
    let out = target
        .command(&daemon_command(target, "restart --force"))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("couldn't run {}", target.program))?;
    if !out.status.success() {
        bail!("restarting goattyd on {} failed: {}", target.host, last_words(&out.stderr, &out.stdout));
    }
    Ok(())
}

/// This build's installed goattyd on the host with `args`.
fn daemon_command(target: &SshTarget, args: &str) -> String {
    if target.is_windows() {
        format!("{} {args}", windows_installed())
    } else {
        format!("sh -c {}", sh_quote(&format!("exec \"$HOME/{BIN_DIR}/{}\" {args}", binary_name())))
    }
}

/// [`install_script`] for a Windows host, in PowerShell: the upload comes
/// on stdin as raw bytes.
fn windows_install_script(size: u64) -> String {
    let name = windows_binary_name();
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
try {{
  $d=Join-Path (Get-Location).Path '{WINDOWS_BIN_DIR}'
  $n='{name}'
  $null=New-Item -ItemType Directory -Force -Path $d
  # Ends in .exe: PowerShell runs nothing else as a program.
  $t=Join-Path $d ".$PID.$n"
  try {{
    $i=[Console]::OpenStandardInput()
    $f=[IO.File]::Create($t)
    try {{ $i.CopyTo($f) }} finally {{ $f.Close() }}
    $s=(Get-Item -LiteralPath $t).Length
    if ($s -ne {size}) {{ throw "upload incomplete ($s of {size} bytes)" }}
    $v=(& $t --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {{ throw "the uploaded goattyd doesn't run on this host: $v" }}
    if (-not $v.StartsWith('goattyd {version}')) {{ throw "unexpected goattyd version: $v" }}
    Move-Item -Force -LiteralPath $t -Destination (Join-Path $d $n)
  }} finally {{
    if (Test-Path -LiteralPath $t) {{ Remove-Item -Force -LiteralPath $t }}
  }}
  "installed $(Join-Path $d $n)"
  Get-ChildItem -LiteralPath $d -Filter 'goattyd-*.exe' | Where-Object {{ $_.Name -ne $n }} | ForEach-Object {{
    try {{ Remove-Item -Force -LiteralPath $_.FullName; "removed $($_.FullName)" }} catch {{ }}
  }}
}} catch {{
  # Plain text: PowerShell would report the error as CLIXML here.
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
}}
"#
    )
}

/// The last few lines a command printed, for an error message. A backtrace
/// (goattyd's errors carry one when the host sets `RUST_BACKTRACE`) is cut
/// off, so the message itself is what's left.
fn last_words(stderr: &[u8], stdout: &[u8]) -> String {
    let text = if stderr.iter().any(|b| !b.is_ascii_whitespace()) { stderr } else { stdout };
    let text = String::from_utf8_lossy(text);
    let lines: Vec<&str> = text
        .lines()
        .take_while(|l| !l.trim_start().to_ascii_lowercase().starts_with("stack backtrace:"))
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() { "no output".into() } else { lines[lines.len().saturating_sub(3)..].join(" / ") }
}

/// The host turned out to run Windows and has this build's goattyd: connect
/// again, now the Windows way.
#[derive(Debug)]
pub(crate) struct RetryAsWindows;

impl fmt::Display for RetryAsWindows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the host runs Windows; connecting the Windows way")
    }
}

impl std::error::Error for RetryAsWindows {}

/// Classify a failed connect to `target` from the error, what the transport
/// printed on stderr and how it exited. Finds out about Windows hosts (with
/// one more ssh run, the Windows probe): the error is then
/// [`RetryAsWindows`] when this build's goattyd is there.
pub(crate) fn diagnose(target: &SshTarget, err: anyhow::Error, stderr: &[String], exit: Option<i32>, interactive: bool) -> anyhow::Error {
    let host = &target.host;
    let probe = parse_probe(stderr);
    if target.is_windows() {
        return diagnose_windows(target, err, stderr, exit, interactive);
    }
    if let Some(mismatch) = err.downcast_ref::<VersionMismatch>() {
        return RemoteProblem::Incompatible { host: host.into(), mismatch: mismatch.clone(), probe }.into();
    }
    // sh is Git Bash's, or no sh at all: ask the Windows way. A host that
    // doesn't answer that either keeps the original error.
    let sure = probe.as_ref().is_some_and(|p| p.platform.is_windows());
    if sure || (probe.is_none() && remote_command_failed(exit)) {
        match probe_windows(target, interactive) {
            Ok(found) => {
                log::info!("{host} runs Windows ({})", found.platform.arch);
                target.set_windows();
                return match found {
                    Probe { daemon: Some(_), .. } => RetryAsWindows.into(),
                    Probe { platform, daemon: None } => RemoteProblem::NotInstalled { host: host.into(), platform }.into(),
                };
            }
            Err(e) if sure => return with_stderr(err, stderr).context(format!("{e:#}")),
            Err(e) => log::debug!("{host} isn't a Windows host either: {e:#}"),
        }
    }
    if let Some(Probe { platform, daemon: None }) = probe {
        return RemoteProblem::NotInstalled { host: host.into(), platform }.into();
    }
    with_stderr(err, stderr)
}

/// [`diagnose`] for a host known to run Windows: its connect command runs
/// only this build's goattyd (or `--remote-daemon`), so a missing one shows
/// as the shell not finding it.
fn diagnose_windows(target: &SshTarget, err: anyhow::Error, stderr: &[String], exit: Option<i32>, interactive: bool) -> anyhow::Error {
    let host = &target.host;
    if let Some(mismatch) = err.downcast_ref::<VersionMismatch>() {
        // What answered was started by the goattyd the command ran.
        let daemon = windows_program(target.remote_daemon.as_deref()).ok();
        let platform = Platform { os: "Windows".into(), arch: String::new() };
        let probe = Some(Probe { platform, daemon });
        return RemoteProblem::Incompatible { host: host.into(), mismatch: mismatch.clone(), probe }.into();
    }
    if target.remote_daemon.is_none() && remote_command_failed(exit) {
        match probe_windows(target, interactive) {
            Ok(Probe { platform, daemon: None }) => return RemoteProblem::NotInstalled { host: host.into(), platform }.into(),
            Ok(_) => {}
            Err(e) => return with_stderr(err, stderr).context(format!("{e:#}")),
        }
    }
    with_stderr(err, stderr)
}

/// `err` with the last lines a transport command printed on stderr.
pub(crate) fn with_stderr(err: anyhow::Error, stderr: &[String]) -> anyhow::Error {
    let said: Vec<&str> = stderr.iter().map(|l| l.as_str()).filter(|l| !l.starts_with(MARKER) && !l.trim().is_empty()).collect();
    match said.len() {
        0 => err,
        n => err.context(said[n.saturating_sub(3)..].join(" / ")),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::{Read, Write};

    use super::*;

    #[cfg(unix)]
    fn drain(mut r: impl Read) -> String {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn probe_lines_parse() {
        let p =
            parse_probe(&lines(&["Warning: Permanently added 'box'", "goatty-remote: Linux x86_64 /home/a b/.cargo/bin/goattyd"])).unwrap();
        assert_eq!(p.platform, Platform { os: "Linux".into(), arch: "x86_64".into() });
        assert_eq!(p.daemon.as_deref(), Some("/home/a b/.cargo/bin/goattyd"));
        assert!(!p.runs_ours());
        let p = parse_probe(&lines(&["goatty-remote: Linux aarch64 -"])).unwrap();
        assert_eq!(p.daemon, None);
        assert_eq!(p.platform.target(), Some("aarch64-linux"));
        let ours = format!("goatty-remote: Linux x86_64 /home/u/{BIN_DIR}/{}", binary_name());
        assert!(parse_probe(&lines(&[&ours])).unwrap().runs_ours());
        assert!(parse_probe(&lines(&["ssh: connect to host box port 22: Connection refused"])).is_none());
        let win = format!(r"goatty-remote: Windows AMD64 C:\Users\A B\appdata\local\goatty\bin\{}", windows_binary_name());
        let p = parse_probe(&lines(&[&win])).unwrap();
        assert!(p.platform.is_windows() && p.runs_ours(), "{p:?}");
    }

    #[test]
    fn targets() {
        let t = |os: &str, arch: &str| Platform { os: os.into(), arch: arch.into() }.target();
        assert_eq!(t("Linux", "x86_64"), Some("x86_64-linux"));
        assert_eq!(t("Linux", "arm64"), Some("aarch64-linux"));
        assert_eq!(t("Linux", "armv7l"), None);
        assert_eq!(t("Darwin", "arm64"), None);
        assert_eq!(t("Windows", "AMD64"), Some("x86_64-windows"));
        assert_eq!(t("Windows", "ARM64"), Some("aarch64-windows"));
        assert_eq!(t("MINGW64_NT-10.0-19045", "x86_64"), Some("x86_64-windows"));
        assert_eq!(t("Windows", "x86"), None);
        assert_eq!(t("FreeBSD", "amd64"), None);
    }

    #[test]
    fn diagnose_tells_what_happened() {
        let eof = || anyhow::anyhow!("no valid greeting");
        let t = SshTarget::new("box");
        let e = diagnose(&t, eof(), &lines(&["goatty-remote: Linux x86_64 -", "goatty: goattyd not found"]), Some(127), false);
        assert!(matches!(e.downcast_ref::<RemoteProblem>(), Some(RemoteProblem::NotInstalled { .. })));
        let m = VersionMismatch { daemon_protocol: Some(3), daemon_version: None, daemon_is_newer: false };
        let e = diagnose(&t, m.into(), &lines(&["goatty-remote: Linux x86_64 /usr/bin/goattyd"]), Some(1), false);
        let Some(RemoteProblem::Incompatible { probe: Some(p), .. }) = e.downcast_ref::<RemoteProblem>() else { panic!("{e:#}") };
        assert_eq!(p.daemon.as_deref(), Some("/usr/bin/goattyd"));
        let e = diagnose(&t, eof(), &lines(&["user@box: Permission denied (publickey)."]), Some(255), false);
        assert_eq!(format!("{e:#}"), "user@box: Permission denied (publickey).: no valid greeting");
        assert!(!t.is_windows());
    }

    /// A fake `ssh` for a Windows host: the connect script fails as it does
    /// in cmd.exe, the PowerShell probe reports `probe_line`.
    #[cfg(unix)]
    fn fake_windows_ssh(dir: &Path, probe_line: &str) -> SshTarget {
        use std::os::unix::fs::PermissionsExt;
        let ssh = dir.join("ssh");
        let script = format!(
            "#!/bin/sh\nfor a; do last=$a; done\ncase \"$last\" in powershell*) echo '{probe_line}' >&2; exit 0;; esac\n\
             echo 'The system cannot find the path specified.' >&2; exit 1\n"
        );
        std::fs::write(&ssh, script).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        SshTarget { program: ssh.display().to_string(), ..SshTarget::new("winbox") }
    }

    #[cfg(unix)]
    #[test]
    fn windows_hosts_are_found_out() {
        let dir = tempfile::tempdir().unwrap();
        let said = lines(&["The system cannot find the path specified."]);
        let eof = || anyhow::anyhow!("no valid greeting");

        // Not installed: the probe tells the platform, the host is marked.
        let t = fake_windows_ssh(dir.path(), "goatty-remote: Windows ARM64 -");
        let e = diagnose(&t, eof(), &said, Some(1), false);
        let Some(RemoteProblem::NotInstalled { platform, .. }) = e.downcast_ref::<RemoteProblem>() else { panic!("{e:#}") };
        assert_eq!(platform.target(), Some("aarch64-windows"));
        assert!(t.is_windows() && t.clone().is_windows(), "clones share it");

        // Installed: connect again, the Windows way.
        let installed = format!(r"goatty-remote: Windows AMD64 C:\Users\a b\{WINDOWS_BIN_DIR}\{}", windows_binary_name());
        let t = fake_windows_ssh(dir.path(), &installed);
        assert!(diagnose(&t, eof(), &said, Some(1), false).downcast_ref::<RetryAsWindows>().is_some());

        // Known Windows host without goattyd: asked again for the platform.
        let t = fake_windows_ssh(dir.path(), "goatty-remote: Windows AMD64 -");
        t.set_windows();
        let e = diagnose(&t, eof(), &lines(&["'.\\AppData' is not recognized"]), Some(9009), false);
        assert!(matches!(e.downcast_ref::<RemoteProblem>(), Some(RemoteProblem::NotInstalled { .. })), "{e:#}");
        // An old daemon behind ours: restart it.
        let m = VersionMismatch { daemon_protocol: Some(3), daemon_version: None, daemon_is_newer: false };
        let e = diagnose(&t, m.into(), &[], Some(1), false);
        let Some(RemoteProblem::Incompatible { probe: Some(p), .. }) = e.downcast_ref::<RemoteProblem>() else { panic!("{e:#}") };
        assert!(p.runs_ours());

        // A POSIX host whose command failed for another reason keeps its error.
        let t = SshTarget { program: "false".into(), ..SshTarget::new("box") };
        let e = diagnose(&t, eof(), &lines(&["fatal: unrecognized command"]), Some(128), false);
        assert!(!t.is_windows());
        assert_eq!(format!("{e:#}"), "fatal: unrecognized command: no valid greeting");
    }

    #[test]
    fn windows_commands_need_no_quoting() {
        let ours = windows_proxy_command(None).unwrap();
        assert_eq!(ours, format!(r".\{WINDOWS_BIN_DIR}\{} proxy", windows_binary_name()));
        assert!(!ours.contains(['"', '\'', '&', '|', '%', '$', ';', '(', ')']) && ours.matches(' ').count() == 1, "{ours}");
        assert_eq!(windows_proxy_command(Some(r"C:\tools\goattyd.exe")).unwrap(), r"C:\tools\goattyd.exe proxy");
        assert!(windows_proxy_command(Some(r"C:\Program Files\goattyd.exe")).is_err());
        assert!(windows_proxy_command(Some("a&b")).is_err());
        let ps = powershell("Write-Output 'hi'");
        let b64 = ps.rsplit(' ').next().unwrap();
        assert!(b64.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)), "{b64}");
        // Under cmd.exe's 8191-character limit.
        assert!(powershell(&windows_install_script(1 << 30)).len() < 8000);
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn scripts_run_in_posix_sh() {
        // Quoted paths survive, and the scripts parse in plain sh.
        let s = proxy_script(Some("/opt/it's/goattyd"));
        assert!(s.contains("'/opt/it'\\''s/goattyd'"));
        #[cfg(unix)]
        for script in [proxy_script(None), proxy_script(Some("/x y")), install_script(10)] {
            let out = std::process::Command::new("sh").args(["-n", "-c", &script]).output().unwrap();
            assert!(out.status.success(), "{script}\n{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    #[cfg(unix)]
    fn find_tool(name: &str) -> PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .chain(["/usr/bin", "/bin"].into_iter().map(PathBuf::from))
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{name} not found in PATH or /usr/bin:/bin"))
    }

    #[cfg(unix)]
    #[test]
    fn proxy_script_reports_a_missing_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let sysbin = dir.path().join("sysbin");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&sysbin).unwrap();
        for tool in ["sh", "uname"] {
            std::os::unix::fs::symlink(find_tool(tool), sysbin.join(tool)).unwrap();
        }
        let mut out = std::process::Command::new(sysbin.join("sh"))
            .args(["-c", &proxy_script(None)])
            .env("HOME", &home)
            .env("PATH", &sysbin)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = drain(out.stderr.take().unwrap());
        assert_eq!(out.wait().unwrap().code(), Some(127));
        let probe = parse_probe(&err.lines().map(String::from).collect::<Vec<_>>()).unwrap();
        assert_eq!(probe.daemon, None);
        assert!(!probe.platform.os.is_empty() && !probe.platform.arch.is_empty());
    }

    #[test]
    fn bundled_binaries_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let here = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
        assert_eq!(find_bundled_near(dir.path(), "sparc-linux"), None);
        assert_eq!(find_bundled_near(dir.path(), &here), None);
        std::fs::create_dir(dir.path().join("remote")).unwrap();
        std::fs::write(dir.path().join("remote/goattyd-sparc-linux"), "x").unwrap();
        let b = find_bundled_near(dir.path(), "sparc-linux").unwrap();
        assert!(!b.this_build && b.path.ends_with("remote/goattyd-sparc-linux"));
        // The own goattyd stands in for a host with this computer's CPU.
        let own_name = format!("goattyd{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.path().join(&own_name), "x").unwrap();
        let own = find_bundled_near(dir.path(), &here).unwrap();
        assert!(own.this_build && own.path.ends_with(&own_name));
        assert_eq!(find_bundled_near(dir.path(), "mips-linux"), None);
        std::fs::write(dir.path().join("remote/goattyd-x86_64-windows.exe"), "x").unwrap();
        assert!(find_bundled_near(dir.path(), "x86_64-windows").unwrap().path.ends_with("goattyd-x86_64-windows.exe"));
        assert_eq!(last_words(b"\n", b"a\nb\nc\nd\n"), "b / c / d");
        let with_backtrace = b"Error: not a goattyd\n\nStack backtrace:\n   0: main\n   1: _start\n";
        assert_eq!(last_words(with_backtrace, b""), "Error: not a goattyd");
    }

    #[cfg(unix)]
    fn run_install(home: &Path) {
        let fake = format!("#!/bin/sh\necho goattyd {}\n", env!("CARGO_PKG_VERSION"));
        let mut child = std::process::Command::new("sh")
            .args(["-c", &install_script(fake.len() as u64)])
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(fake.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[cfg(unix)]
    #[test]
    fn install_links_an_unversioned_goattyd() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(BIN_DIR);
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("goattyd-0.0.1-p1", bin.join("goattyd")).unwrap();
        run_install(dir.path());
        assert_eq!(std::fs::read_link(bin.join("goattyd")).unwrap(), PathBuf::from(binary_name()));
        let out = std::process::Command::new(bin.join("goattyd")).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("goattyd "));

        // A goattyd the user put there themselves stays.
        std::fs::remove_file(bin.join("goattyd")).unwrap();
        std::fs::write(bin.join("goattyd"), "mine").unwrap();
        run_install(dir.path());
        assert_eq!(std::fs::read_to_string(bin.join("goattyd")).unwrap(), "mine");
    }
}
