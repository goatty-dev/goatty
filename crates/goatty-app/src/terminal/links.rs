//! What a link under the mouse points at, and opening it.
//!
//! `goatty_term` finds links in the grid ([`Link`]); this module decides
//! what they mean on the session's host: a relative path is resolved against
//! the shell's working directory (locally on disk, or on a remote goattyd via
//! `StatPath`) and only counts if it exists; remote files can be opened via
//! a configured remote editor or fetched as a read-only local copy (#110);
//! and schemes other than the everyday ones (http, https, mailto, ftp, file)
//! ask before launching another application.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use goatty_client::RemotePathInfo;
use goatty_config::{LinkSettings, RemoteFileOpen};
use goatty_term::{Link, LinkKind};

/// Stale remote file cache directories older than this are pruned at startup.
const STALE_CACHE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// What clicking a link does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Open in the browser / mail client.
    Url(String),
    /// A URL with a scheme that launches some other application: ask first.
    Other(String),
    /// A file or directory on this machine.
    File { path: PathBuf, line: Option<u32>, column: Option<u32> },
    /// A file or directory on a remote host.
    Remote {
        path: String,
        host: String,
        line: Option<u32>,
        column: Option<u32>,
        is_dir: bool,
        size: Option<u64>,
        /// True when the path lives on the active session's daemon host (so
        /// `ssh_host` and `ReadFile` refer to that same host), rather than a
        /// third-party `file://other-host/…` URI.
        on_session_host: bool,
        /// True when the active session's daemon supports `ReadFile` and the
        /// target is a regular file on the session's host.
        can_fetch: bool,
    },
}

impl Target {
    /// What the hint at the bottom of the pane shows.
    pub fn describe(&self) -> String {
        match self {
            Target::Url(u) | Target::Other(u) => u.clone(),
            Target::File { path, line, column } => format_with_pos(&path.display().to_string(), *line, *column),
            Target::Remote { path, host, line, column, .. } => format!("{host}:{}", format_with_pos(path, *line, *column)),
        }
    }
}

fn format_with_pos(path: &str, line: Option<u32>, column: Option<u32>) -> String {
    let mut s = path.to_string();
    if let Some(l) = line {
        s.push_str(&format!(":{l}"));
        if let Some(c) = column {
            s.push_str(&format!(":{c}"));
        }
    }
    s
}

/// Result of consulting the client's cached remote `StatPath` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteLookup {
    /// Resolved on the daemon to an existing regular file or directory.
    Found(RemotePathInfo),
    /// Resolved on the daemon as missing or non-regular (e.g. FIFO/socket).
    Missing,
    /// Not yet queried; the caller should request `StatPath` asynchronously.
    Pending,
}

/// Where the session runs, for resolving paths.
pub struct Place<'a> {
    /// The shell's working directory (OSC 7), local sessions only.
    pub cwd: Option<&'a Path>,
    pub home: Option<&'a Path>,
    /// `Some(label)` when the session runs on another host.
    pub remote_host: Option<&'a str>,
    /// The session host's name (local hostname locally, daemon hostname remotely).
    pub hostname: Option<&'a str>,
    /// Does a local path exist? (`Path::exists` outside tests.)
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// `Some` when the remote session speaks protocol v13 (`StatPath`).
    pub remote_stat: Option<&'a dyn Fn(&str) -> RemoteLookup>,
    /// Whether `file://` URIs on the session's host can be fetched via `ReadFile`.
    pub remote_can_fetch: bool,
}

const EVERYDAY_SCHEMES: &[&str] = &["http", "https", "mailto", "ftp"];

/// What `link` points at, or `None` if it isn't really a link here (a path
/// that doesn't exist, a relative path with no known directory).
pub fn resolve(link: &Link, place: &Place<'_>) -> Option<Target> {
    match link.kind {
        LinkKind::Hyperlink | LinkKind::Url => resolve_uri(&link.target, place),
        LinkKind::Path => resolve_path(&link.target, link.line, link.column, place),
    }
}

fn resolve_uri(uri: &str, place: &Place<'_>) -> Option<Target> {
    let (scheme, rest) = uri.split_once(':')?;
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    if EVERYDAY_SCHEMES.contains(&scheme.as_str()) {
        return Some(Target::Url(uri.to_string()));
    }
    if scheme != "file" {
        return Some(Target::Other(uri.to_string()));
    }
    // file://host/path or file:/path
    let after = rest.strip_prefix("//").unwrap_or(rest);
    let (host, path) = match after.find('/') {
        Some(i) if rest.starts_with("//") => (&after[..i], &after[i..]),
        _ if rest.starts_with("//") => return None,
        _ => ("", after),
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let path = normalize_file_uri_path(percent_decode(path)?);
    let local_host = host.is_empty() || host.eq_ignore_ascii_case("localhost") || place.hostname.is_some_and(|h| same_host(h, host));
    match place.remote_host {
        Some(remote) => Some(Target::Remote {
            path,
            host: if local_host { remote.to_string() } else { host.to_string() },
            line: None,
            column: None,
            is_dir: false,
            size: None,
            on_session_host: local_host,
            can_fetch: local_host && place.remote_can_fetch,
        }),
        None if local_host => Some(Target::File { path: PathBuf::from(path), line: None, column: None }),
        None => Some(Target::Remote {
            path,
            host: host.to_string(),
            line: None,
            column: None,
            is_dir: false,
            size: None,
            on_session_host: false,
            can_fetch: false,
        }),
    }
}

fn same_host(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    match (a.split_once('.'), b.split_once('.')) {
        (None, Some((b_short, _))) => a.eq_ignore_ascii_case(b_short),
        (Some((a_short, _)), None) => a_short.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// Normalize RFC 8089 Windows `file:///C:/...` paths by stripping the leading
/// `/` before an ASCII drive letter (`"/C:/Users/me"` -> `"C:/Users/me"`).
fn normalize_file_uri_path(path: String) -> String {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' && (b.len() == 3 || b[3] == b'/') {
        path[1..].to_string()
    } else {
        path
    }
}

pub(crate) fn is_windows_drive_path(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/')
}

/// Candidate strings sent to the daemon's `StatPath` for a detected `LinkKind::Path`.
/// Strips `git diff` `a/` and `b/` prefixes as fallback candidates.
pub fn remote_candidates(text: &str) -> Vec<String> {
    if let Some(rest) = text.strip_prefix("a/").or_else(|| text.strip_prefix("b/"))
        && !rest.is_empty()
    {
        vec![text.to_string(), rest.to_string()]
    } else {
        vec![text.to_string()]
    }
}

fn resolve_path(text: &str, line: Option<u32>, column: Option<u32>, place: &Place<'_>) -> Option<Target> {
    if let Some(host) = place.remote_host {
        if let Some(stat) = place.remote_stat {
            if is_windows_drive_path(text) {
                return None;
            }
            return match stat(text) {
                RemoteLookup::Found(info) => Some(Target::Remote {
                    path: info.path,
                    host: host.to_string(),
                    line,
                    column,
                    is_dir: info.is_dir,
                    size: Some(info.size),
                    on_session_host: true,
                    can_fetch: !info.is_dir && place.remote_can_fetch,
                }),
                RemoteLookup::Missing | RemoteLookup::Pending => None,
            };
        }
        // Pre-v13 daemon fallback: only unambiguous paths that can't be misread.
        return (text.starts_with('/') || text.starts_with("~/") || is_windows_drive_path(text)).then(|| Target::Remote {
            path: text.to_string(),
            host: host.to_string(),
            line,
            column,
            is_dir: false,
            size: None,
            on_session_host: true,
            can_fetch: false,
        });
    }
    let candidates: Vec<PathBuf> = if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        vec![place.home?.join(rest)]
    } else if Path::new(text).is_absolute() || is_windows_drive_path(text) {
        vec![PathBuf::from(text)]
    } else {
        let cwd = place.cwd?;
        let mut v = vec![cwd.join(text)];
        // `git diff` prefixes: a/src/x.rs, b/src/x.rs.
        if let Some(rest) = text.strip_prefix("a/").or_else(|| text.strip_prefix("b/")) {
            v.push(cwd.join(rest));
        }
        v
    };
    let path = candidates.into_iter().map(|p| normalize(&p)).find(|p| (place.exists)(p))?;
    Some(Target::File { path, line, column })
}

/// How clicking a [`Target::Remote`] should act under the current settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    /// Launch the user's configured remote editor (`links.remote_file_opener`).
    OpenInEditor { host: String, argv: Vec<String> },
    /// Fetch a read-only local copy over the session `Connection`.
    Fetch { max_bytes: u64, prompt_if_larger: bool },
    /// Copy the remote path to the clipboard.
    Copy,
}

#[allow(clippy::too_many_arguments)]
pub fn plan_remote_action(
    links: &LinkSettings,
    ssh_host: Option<&str>,
    remote_user: Option<&str>,
    remote_hostname: Option<&str>,
    path: &str,
    line: Option<u32>,
    column: Option<u32>,
    on_session_host: bool,
    can_fetch: bool,
) -> RemoteAction {
    let opener = if on_session_host && path.starts_with('/') {
        ssh_host.and_then(|host| {
            links
                .remote_file_command(host, remote_user.unwrap_or(""), remote_hostname.unwrap_or(""), path, line, column)
                .map(|argv| (host.to_string(), argv))
        })
    } else {
        None
    };

    match links.remote_files {
        RemoteFileOpen::Copy => RemoteAction::Copy,
        RemoteFileOpen::Fetch => {
            if can_fetch {
                RemoteAction::Fetch { max_bytes: links.remote_max_bytes(), prompt_if_larger: true }
            } else {
                RemoteAction::Copy
            }
        }
        RemoteFileOpen::Auto | RemoteFileOpen::RemoteOpener => {
            if let Some((host, argv)) = opener {
                RemoteAction::OpenInEditor { host, argv }
            } else if can_fetch {
                RemoteAction::Fetch { max_bytes: links.remote_max_bytes(), prompt_if_larger: true }
            } else {
                RemoteAction::Copy
            }
        }
    }
}

/// Human-readable binary byte size (e.g. `10 MiB`, `24.5 MiB`, `512 B`).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_unit(bytes: u64, unit: u64, label: &str) -> String {
    if bytes.is_multiple_of(unit) { format!("{} {label}", bytes / unit) } else { format!("{:.1} {label}", bytes as f64 / unit as f64) }
}

/// Drop `.` and resolve `..` lexically (no symlink lookups).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// `%20` → ' '. `None` for malformed escapes or non-UTF-8 results.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// This machine's host name, if the OS tells us cheaply.
pub fn hostname() -> Option<String> {
    Some(goatty_client::hostname()).filter(|s| !s.is_empty())
}

/// Resolve `program` using Windows `PATHEXT` rules (so `.cmd`/`.bat` openers
/// like VS Code's `code.cmd` work with `Command::new`). Pure helper so tests
/// can exercise it on every OS.
#[cfg_attr(not(windows), allow(dead_code))]
fn resolve_windows_program_in(program: &str, path_env: Option<&std::ffi::OsStr>, pathext_env: Option<&str>) -> PathBuf {
    let direct = Path::new(program);
    let has_sep = program.contains('/') || program.contains('\\');
    let has_ext = direct.extension().is_some();
    let exts: Vec<String> = if has_ext {
        Vec::new()
    } else {
        pathext_env.unwrap_or(".COM;.EXE;.BAT;.CMD").split(';').filter(|e| !e.is_empty()).map(|e| e.to_ascii_lowercase()).collect()
    };
    if has_sep {
        if direct.is_file() {
            return direct.to_path_buf();
        }
        for ext in &exts {
            let candidate = PathBuf::from(format!("{program}{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
        return direct.to_path_buf();
    }
    if let Some(path) = path_env {
        for dir in std::env::split_paths(path) {
            let exact = dir.join(program);
            if exact.is_file() {
                return exact;
            }
            for ext in &exts {
                let candidate = dir.join(format!("{program}{ext}"));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    direct.to_path_buf()
}

/// Run a file opener command (argv from `LinkSettings::file_command` or
/// `LinkSettings::remote_file_command`) detached from goatty, reaping it in
/// the background.
pub fn spawn_opener(argv: &[String]) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let (program, args) = argv.split_first().ok_or_else(|| std::io::Error::other("empty command"))?;
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        let path_env = std::env::var_os("PATH");
        let pathext_env = std::env::var("PATHEXT").ok();
        let resolved = resolve_windows_program_in(program, path_env.as_deref(), pathext_env.as_deref());
        let mut c = Command::new(resolved);
        c.creation_flags(CREATE_NO_WINDOW);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new(program);
    let mut child = cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::Builder::new().name("link-opener".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(())
}

// ---- Read-only local cache for fetched remote files -------------------------

fn remote_cache_root() -> PathBuf {
    goatty_proto::runtime_dir().join("remote")
}

fn session_cache_dir() -> PathBuf {
    remote_cache_root().join(format!("p{}", std::process::id()))
}

fn fnv1a_64(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in (part.len() as u64).to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        for &b in *part {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn safe_basename(remote_path: &str) -> &str {
    remote_path.rsplit(['/', '\\']).next().filter(|n| !n.is_empty() && *n != "." && *n != ".." && !n.contains('\0')).unwrap_or("file")
}

fn set_dir_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_file_readonly(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    }
    #[cfg(not(unix))]
    {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Write `data` into a read-only (`0o444`) temporary file preserving the
/// basename/extension of `remote_path`, under `<runtime>/goatty/remote/p<pid>/<hash16>/<basename>`.
/// `endpoint_key` must uniquely identify the endpoint (separate from the display host label).
pub fn save_remote_copy(endpoint_key: &str, remote_path: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    save_remote_copy_in(&session_cache_dir(), endpoint_key, remote_path, data)
}

fn save_remote_copy_in(session_dir: &Path, endpoint_key: &str, remote_path: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    fs::create_dir_all(session_dir)?;
    set_dir_private(session_dir)?;

    let hash = fnv1a_64(&[endpoint_key.as_bytes(), remote_path.as_bytes()]);
    let bucket = session_dir.join(format!("{hash:016x}"));
    fs::create_dir_all(&bucket)?;
    set_dir_private(&bucket)?;

    let name = safe_basename(remote_path);
    let dest = bucket.join(name);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = bucket.join(format!(".goatty-tmp.{seq}.{}", std::process::id()));

    let write_res = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(data)?;
        file.flush()?;
        drop(file);
        set_file_readonly(&tmp)?;
        // Windows refuses to overwrite a read-only destination via `rename`.
        #[cfg(windows)]
        if dest.exists()
            && let Ok(meta) = fs::metadata(&dest)
        {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = fs::set_permissions(&dest, perms);
            }
        }
        fs::rename(&tmp, &dest)?;
        Ok(())
    })();

    if let Err(e) = write_res {
        #[cfg(windows)]
        if let Ok(meta) = fs::metadata(&tmp) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = fs::set_permissions(&tmp, perms);
            }
        }
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(dest)
}

#[cfg(windows)]
fn clear_readonly_recursive(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
        if meta.is_dir()
            && let Ok(entries) = fs::read_dir(path)
        {
            for entry in entries.flatten() {
                clear_readonly_recursive(&entry.path());
            }
        }
    }
}

fn remove_cache_dir(path: &Path) {
    #[cfg(windows)]
    clear_readonly_recursive(path);
    let _ = fs::remove_dir_all(path);
}

/// Remove this process's cached remote file copies on clean exit.
pub fn cleanup_remote_cache() {
    remove_cache_dir(&session_cache_dir());
}

/// Remove leftover remote cache directories from previous runs older than 24 hours.
pub fn cleanup_stale_remote_cache() {
    cleanup_stale_remote_cache_in(&remote_cache_root(), STALE_CACHE_AGE, std::process::id(), SystemTime::now());
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: `kill(pid, 0)` sends no signal and only probes whether `pid` exists.
        let rc = unsafe { libc::kill(raw_pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, FALSE, GetLastError, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

        // SAFETY: plain Win32 call; returned handle is validated and closed.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if handle.is_null() {
            // SAFETY: `GetLastError` has no preconditions.
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        }
        let mut exit_code = 0u32;
        // SAFETY: `handle` is open and `exit_code` is a valid out-pointer.
        let ok = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) };
        // SAFETY: `handle` is owned exclusively by this scope.
        unsafe {
            CloseHandle(handle);
        }
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn cleanup_stale_remote_cache_in(root: &Path, max_age: Duration, current_pid: u32, now: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        let Some(pid_str) = name_str.strip_prefix('p') else { continue };
        let Ok(pid) = pid_str.parse::<u32>() else { continue };
        if pid == current_pid || is_pid_alive(pid) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(newest) = newest_mtime(&path, 3)
            && now.duration_since(newest).is_ok_and(|age| age >= max_age)
        {
            remove_cache_dir(&path);
        }
    }
}

fn newest_mtime(path: &Path, depth: u8) -> Option<SystemTime> {
    let mut latest = fs::metadata(path).ok()?.modified().ok()?;
    if depth == 0 || !path.is_dir() {
        return Some(latest);
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Some(t) = newest_mtime(&entry.path(), depth - 1)
                && t > latest
            {
                latest = t;
            }
        }
    }
    Some(latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: LinkKind, target: &str) -> Link {
        Link { kind, target: target.into(), line: None, column: None, spans: Vec::new() }
    }

    fn local<'a>(exists: &'a dyn Fn(&Path) -> bool) -> Place<'a> {
        Place {
            cwd: Some(Path::new("/work/proj")),
            home: Some(Path::new("/home/me")),
            remote_host: None,
            hostname: Some("box"),
            exists,
            remote_stat: None,
            remote_can_fetch: false,
        }
    }

    fn file(p: &str) -> Option<Target> {
        Some(Target::File { path: PathBuf::from(p), line: None, column: None })
    }

    #[test]
    fn urls_by_scheme() {
        let all = |_: &Path| true;
        let place = local(&all);
        let url = |u: &str| resolve(&link(LinkKind::Url, u), &place);
        assert_eq!(url("https://x.org/a"), Some(Target::Url("https://x.org/a".into())));
        assert_eq!(url("MAILTO:me@x.org"), Some(Target::Url("MAILTO:me@x.org".into())));
        assert_eq!(url("ssh://host"), Some(Target::Other("ssh://host".into())));
        let osc8 = |u: &str| resolve(&link(LinkKind::Hyperlink, u), &place);
        assert_eq!(osc8("vscode://file/x"), Some(Target::Other("vscode://file/x".into())));
        assert_eq!(osc8("javascript alert"), None);
        assert_eq!(osc8("file:///tmp/a%20b.txt"), file("/tmp/a b.txt"));
        assert_eq!(osc8("file:///C:/Users/me/my%20file.rs"), file("C:/Users/me/my file.rs"));
        assert_eq!(osc8("file://localhost/C:/Users/me/a.rs"), file("C:/Users/me/a.rs"));
        assert_eq!(osc8("file://localhost/etc/hosts"), file("/etc/hosts"));
        assert_eq!(osc8("file://box/etc/hosts?x#y"), file("/etc/hosts"));
        assert_eq!(osc8("file:/etc/hosts"), file("/etc/hosts"));
        assert_eq!(
            osc8("file://other/etc/hosts"),
            Some(Target::Remote {
                path: "/etc/hosts".into(),
                host: "other".into(),
                line: None,
                column: None,
                is_dir: false,
                size: None,
                on_session_host: false,
                can_fetch: false,
            })
        );
        assert_eq!(osc8("file:///bad%zz"), None);
    }

    #[test]
    fn local_paths_must_exist() {
        let exists = |p: &Path| {
            ["/work/proj/src/main.rs", "/home/me/notes.md", "/etc/hosts", "/work/x", r"C:\Users\me\main.rs"]
                .iter()
                .any(|e| Path::new(e) == p)
        };
        let place = local(&exists);
        let path = |t: &str| resolve(&link(LinkKind::Path, t), &place);
        assert_eq!(path("src/main.rs"), file("/work/proj/src/main.rs"));
        assert_eq!(path("./src/main.rs"), file("/work/proj/src/main.rs"));
        assert_eq!(path("../x"), file("/work/x"));
        assert_eq!(path("b/src/main.rs"), file("/work/proj/src/main.rs"), "git diff prefix");
        assert_eq!(path("~/notes.md"), file("/home/me/notes.md"));
        #[cfg(unix)]
        assert_eq!(path("/etc/hosts"), file("/etc/hosts"));
        assert_eq!(path("src/nope.rs"), None);
        let mut with_pos = link(LinkKind::Path, "src/main.rs");
        (with_pos.line, with_pos.column) = (Some(3), Some(9));
        let t = resolve(&with_pos, &place).unwrap();
        let expected_main = normalize(&Path::new("/work/proj").join("src/main.rs"));
        assert_eq!(t.describe(), format!("{}:3:9", expected_main.display()));

        let no_cwd = Place { cwd: None, ..local(&exists) };
        assert_eq!(resolve(&link(LinkKind::Path, "src/main.rs"), &no_cwd), None, "relative needs a directory");
        #[cfg(unix)]
        assert_eq!(resolve(&link(LinkKind::Path, "/etc/hosts"), &no_cwd), file("/etc/hosts"));
        assert_eq!(resolve(&link(LinkKind::Path, r"C:\Users\me\main.rs"), &no_cwd), file(r"C:\Users\me\main.rs"));
    }

    #[test]
    fn remote_paths_resolve_via_stat_and_fallback() {
        let never = |_: &Path| -> bool { panic!("no local checks for remote sessions") };

        // Pre-v13 daemon fallback (`remote_stat == None`): unambiguous paths only.
        let v12 = Place { remote_host: Some("me@server"), hostname: Some("server"), ..local(&never) };
        assert_eq!(
            resolve(&link(LinkKind::Path, "/var/log/syslog"), &v12),
            Some(Target::Remote {
                path: "/var/log/syslog".into(),
                host: "me@server".into(),
                line: None,
                column: None,
                is_dir: false,
                size: None,
                on_session_host: true,
                can_fetch: false,
            })
        );
        assert_eq!(resolve(&link(LinkKind::Path, "src/main.rs"), &v12), None);

        // v13 daemon with `StatPath`: resolves relative, git diff, and rejects missing/plain words.
        let stat = |text: &str| match text {
            "src/main.rs" | "a/src/main.rs" => {
                RemoteLookup::Found(RemotePathInfo { path: "/srv/repo/src/main.rs".into(), is_dir: false, size: 42 })
            }
            "src" => RemoteLookup::Found(RemotePathInfo { path: "/srv/repo/src".into(), is_dir: true, size: 4096 }),
            "Node.js" | "example.com" => RemoteLookup::Missing,
            _ => RemoteLookup::Pending,
        };
        let v13 = Place {
            remote_host: Some("me@server"),
            hostname: Some("server"),
            remote_stat: Some(&stat),
            remote_can_fetch: true,
            ..local(&never)
        };
        let mut with_pos = link(LinkKind::Path, "a/src/main.rs");
        (with_pos.line, with_pos.column) = (Some(10), Some(4));
        let target = resolve(&with_pos, &v13).expect("resolved via stat");
        assert_eq!(
            target,
            Target::Remote {
                path: "/srv/repo/src/main.rs".into(),
                host: "me@server".into(),
                line: Some(10),
                column: Some(4),
                is_dir: false,
                size: Some(42),
                on_session_host: true,
                can_fetch: true,
            }
        );
        assert_eq!(target.describe(), "me@server:/srv/repo/src/main.rs:10:4");
        assert_eq!(
            resolve(&link(LinkKind::Path, "src"), &v13),
            Some(Target::Remote {
                path: "/srv/repo/src".into(),
                host: "me@server".into(),
                line: None,
                column: None,
                is_dir: true,
                size: Some(4096),
                on_session_host: true,
                can_fetch: false,
            }),
            "directories cannot be fetched as files"
        );
        assert_eq!(resolve(&link(LinkKind::Path, "Node.js"), &v13), None);
        assert_eq!(resolve(&link(LinkKind::Path, "pending.rs"), &v13), None);

        // `file://server/...` matches session's remote hostname and is fetchable.
        let osc8 = resolve(&link(LinkKind::Hyperlink, "file://server/srv/x"), &v13);
        assert_eq!(
            osc8,
            Some(Target::Remote {
                path: "/srv/x".into(),
                host: "me@server".into(),
                line: None,
                column: None,
                is_dir: false,
                size: None,
                on_session_host: true,
                can_fetch: true,
            })
        );
        // `file://other-box/...` stays copy-only and does not inherit `on_session_host`.
        let third = resolve(&link(LinkKind::Hyperlink, "file://other-box/srv/x"), &v13);
        assert_eq!(
            third,
            Some(Target::Remote {
                path: "/srv/x".into(),
                host: "other-box".into(),
                line: None,
                column: None,
                is_dir: false,
                size: None,
                on_session_host: false,
                can_fetch: false,
            })
        );
        assert_eq!(resolve(&link(LinkKind::Url, "https://x.org"), &v13), Some(Target::Url("https://x.org".into())));
    }

    #[test]
    fn remote_candidates_strip_git_prefixes() {
        assert_eq!(remote_candidates("a/src/main.rs"), vec!["a/src/main.rs", "src/main.rs"]);
        assert_eq!(remote_candidates("b/src/main.rs"), vec!["b/src/main.rs", "src/main.rs"]);
        assert_eq!(remote_candidates("src/main.rs"), vec!["src/main.rs"]);
        assert_eq!(remote_candidates("a/"), vec!["a/"]);
    }

    #[test]
    fn remote_action_planning_respects_mode_host_and_dir() {
        let mut links = LinkSettings::default();
        // Default Auto with no opener -> Fetch when can_fetch.
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/srv/a.rs", Some(3), Some(2), true, true),
            RemoteAction::Fetch { max_bytes: 10 * 1024 * 1024, prompt_if_larger: true }
        );
        // Auto on a directory with no opener -> Copy.
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/srv", None, None, true, false),
            RemoteAction::Copy
        );

        // Configure remote editor opener.
        links.remote_file_opener = Some("code --remote ssh-remote+{host} --goto {path}:{line}:{column}".into());
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/srv/a.rs", Some(7), None, true, true),
            RemoteAction::OpenInEditor {
                host: "prod-box".into(),
                argv: vec!["code".into(), "--remote".into(), "ssh-remote+prod-box".into(), "--goto".into(), "/srv/a.rs:7:1".into()],
            }
        );
        // Works for directories too when opener is configured.
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/srv", None, None, true, false),
            RemoteAction::OpenInEditor {
                host: "prod-box".into(),
                argv: vec!["code".into(), "--remote".into(), "ssh-remote+prod-box".into(), "--goto".into(), "/srv:1:1".into()],
            }
        );
        // `--via` / Command endpoints have `ssh_host == None` -> falls back to Fetch for files.
        assert_eq!(
            plan_remote_action(&links, None, Some("alice"), Some("prod"), "/srv/a.rs", None, None, true, true),
            RemoteAction::Fetch { max_bytes: 10 * 1024 * 1024, prompt_if_larger: true }
        );
        // Third-party `file://other-host/` never launches our session's SSH opener.
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/etc/hosts", None, None, false, false),
            RemoteAction::Copy
        );
        // Unexpanded `~/` never goes to remote editor opener; falls back to Fetch or Copy.
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "~/note.md", None, None, true, true),
            RemoteAction::Fetch { max_bytes: 10 * 1024 * 1024, prompt_if_larger: true }
        );
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "~/note.md", None, None, true, false),
            RemoteAction::Copy
        );

        // Explicit Fetch mode ignores opener.
        links.remote_files = RemoteFileOpen::Fetch;
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/srv/a.rs", None, None, true, true),
            RemoteAction::Fetch { max_bytes: 10 * 1024 * 1024, prompt_if_larger: true }
        );

        // Explicit Copy mode always copies.
        links.remote_files = RemoteFileOpen::Copy;
        assert_eq!(
            plan_remote_action(&links, Some("prod-box"), Some("alice"), Some("prod"), "/srv/a.rs", None, None, true, true),
            RemoteAction::Copy
        );
    }

    #[test]
    fn remote_copy_cache_preserves_extension_readonly_and_cleans_stale() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("p999999");
        let saved = save_remote_copy_in(&sess, "prod-box", "/srv/repo/config.json", br#"{"ok":true}"#).unwrap();
        assert_eq!(saved.file_name().and_then(|n| n.to_str()), Some("config.json"));
        assert_eq!(fs::read(&saved).unwrap(), br#"{"ok":true}"#);
        assert!(fs::metadata(&saved).unwrap().permissions().readonly());

        // Overwriting an existing read-only copy atomically succeeds.
        let updated = save_remote_copy_in(&sess, "prod-box", "/srv/repo/config.json", br#"{"ok":false}"#).unwrap();
        assert_eq!(updated, saved);
        assert_eq!(fs::read(&saved).unwrap(), br#"{"ok":false}"#);

        // Distinct endpoint identities with the same remote path do not collide.
        let other = save_remote_copy_in(&sess, "ssh:prod-box-2", "/srv/repo/config.json", br#"{"host":2}"#).unwrap();
        assert_ne!(other, saved);
        assert_eq!(fs::read(&saved).unwrap(), br#"{"ok":false}"#);
        assert_eq!(fs::read(&other).unwrap(), br#"{"host":2}"#);

        // Live PID directory is kept even when `now` is far ahead and `current_pid` differs.
        let live = dir.path().join(format!("p{}", std::process::id()));
        fs::create_dir_all(&live).unwrap();
        let dead_pid = (900_000..=u32::MAX).find(|&p| p != 999_999 && !is_pid_alive(p)).unwrap();
        let old = dir.path().join(format!("p{dead_pid}"));
        fs::create_dir_all(&old).unwrap();
        let future = SystemTime::now() + Duration::from_secs(48 * 60 * 60);
        cleanup_stale_remote_cache_in(dir.path(), Duration::from_secs(24 * 60 * 60), 999_999, future);
        assert!(!old.exists(), "stale dead-pid cache dir removed");
        assert!(sess.exists(), "current process dir kept");
        assert!(live.exists(), "live process dir kept");
    }

    #[test]
    fn same_host_matches_short_and_fqdn() {
        assert!(same_host("box", "box"));
        assert!(same_host("Box", "box.example.com"));
        assert!(same_host("box.example.com", "BOX"));
        assert!(!same_host("box.a.com", "box.b.com"));
        assert!(!same_host("other", "box.example.com"));
    }

    #[test]
    fn safe_basename_sanitizes_special_names() {
        assert_eq!(safe_basename("/srv/repo/main.rs"), "main.rs");
        assert_eq!(safe_basename(""), "file");
        assert_eq!(safe_basename("/"), "file");
        assert_eq!(safe_basename("."), "file");
        assert_eq!(safe_basename(".."), "file");
        assert_eq!(safe_basename("/a/.."), "file");
        assert_eq!(safe_basename("bad\0name"), "file");
    }

    #[test]
    fn format_bytes_formats_cleanly() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10 MiB");
        assert_eq!(format_bytes(25 * 1024 * 1024 + 512 * 1024), "25.5 MiB");
    }

    #[test]
    fn windows_opener_resolves_pathext() {
        let dir = tempfile::tempdir().unwrap();
        let code_cmd = dir.path().join("code.cmd");
        fs::write(&code_cmd, "@echo off\r\n").unwrap();
        let path_env = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_windows_program_in("code", Some(path_env.as_os_str()), Some(".EXE;.CMD;.BAT")), code_cmd);
        assert_eq!(resolve_windows_program_in(dir.path().join("code").to_str().unwrap(), None, Some(".EXE;.CMD")), code_cmd);
        assert_eq!(
            resolve_windows_program_in("missing-tool", Some(path_env.as_os_str()), Some(".EXE;.CMD")),
            PathBuf::from("missing-tool")
        );
    }

    #[test]
    fn windows_local_drive_path_resolves() {
        let exists = |p: &Path| p == Path::new(r"C:\Users\me\main.rs");
        let place = local(&exists);
        assert_eq!(resolve(&link(LinkKind::Path, r"C:\Users\me\main.rs"), &place), file(r"C:\Users\me\main.rs"));
        assert!(is_pid_alive(std::process::id()));
        assert!(!is_pid_alive(0));
    }
}
