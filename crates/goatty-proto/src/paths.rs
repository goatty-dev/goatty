//! Where the daemon socket / named pipe and user home live on a host.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn is_abs(windows: bool, path: &Path) -> bool {
    if windows {
        let Some(s) = path.to_str() else {
            return path.is_absolute();
        };
        let b = s.as_bytes();
        (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/')) || s.starts_with(r"\\")
    } else {
        path.is_absolute()
    }
}

/// The current user's home directory.
///
/// On Windows, checks `USERPROFILE`, then `HOMEDRIVE`+`HOMEPATH`, then `HOME`.
/// On Unix, checks `HOME`.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(cfg!(windows), |k| std::env::var_os(k))
}

pub fn home_dir_from(windows: bool, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if windows {
        if let Some(p) = env("USERPROFILE").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(p));
        }
        if let (Some(drive), Some(path)) = (env("HOMEDRIVE").filter(|v| !v.is_empty()), env("HOMEPATH").filter(|v| !v.is_empty())) {
            let mut combined = drive;
            combined.push(path);
            return Some(PathBuf::from(combined));
        }
    }
    env("HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Directory for the local daemon socket / runtime metadata and ephemeral files.
///
/// Uses `$XDG_RUNTIME_DIR/goatty` when set. Otherwise:
/// * **Windows:** `%LOCALAPPDATA%\goatty\run` (falling back to `<home>\AppData\Local\goatty\run`),
/// * **macOS:** `~/Library/Caches/goatty` (persistent across reboots for launchd's
///   `SockPathName`, excluded from Time Machine, shared across GUI/terminal/SSH,
///   and well under macOS's 104-byte `sun_path` limit),
/// * **other platforms:** `<temp_dir>/goatty-<uid>/goatty`.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_from(cfg!(target_os = "macos"), cfg!(windows), |k| std::env::var_os(k), std::env::temp_dir())
}

fn fallback_runtime_dir(env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    let user = env("UID")
        .or_else(|| env("USER"))
        .or_else(|| env("USERNAME"))
        .and_then(|v| v.into_string().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "user".into());
    temp_dir.join(format!("goatty-{user}")).join("goatty")
}

fn runtime_dir_from(macos: bool, windows: bool, env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    if let Some(xdg) = env("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("goatty");
    }
    if windows {
        if let Some(local) = env("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| is_abs(true, p)) {
            return local.join("goatty").join("run");
        }
        if let Some(home) = home_dir_from(true, &env).filter(|p| is_abs(true, p)) {
            return home.join("AppData").join("Local").join("goatty").join("run");
        }
    }
    if macos && let Some(home) = env("HOME").map(PathBuf::from).filter(|p| is_abs(false, p)) {
        return home.join("Library/Caches/goatty");
    }
    fallback_runtime_dir(env, temp_dir)
}

/// Default Windows Named Pipe path for this user's daemon (`\\.\pipe\goattyd-<user-or-hash>`).
pub fn default_windows_pipe_path() -> PathBuf {
    default_windows_pipe_path_from(|k| std::env::var_os(k))
}

fn default_windows_pipe_path_from(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let user = env("USERNAME").or_else(|| env("USER")).and_then(|v| v.into_string().ok()).filter(|s| !s.is_empty());
    let domain = env("USERDOMAIN").and_then(|v| v.into_string().ok()).filter(|s| !s.is_empty());
    let home = home_dir_from(true, &env).map(|p| p.to_string_lossy().into_owned());

    // Include a short hash of domain/home alongside the sanitized username so
    // two accounts with the same leaf name (or non-ASCII usernames) never collide.
    let mut h = Sha256::new();
    if let Some(d) = &domain {
        h.update(d.to_ascii_lowercase().as_bytes());
    }
    h.update(b"\0");
    if let Some(u) = &user {
        h.update(u.to_ascii_lowercase().as_bytes());
    }
    h.update(b"\0");
    if let Some(hm) = &home {
        h.update(hm.to_ascii_lowercase().as_bytes());
    }
    let digest = h.finalize();
    let short_hash: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();

    let safe_user: String =
        user.as_deref().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(32).collect();
    let suffix = if safe_user.is_empty() { short_hash } else { format!("{safe_user}-{short_hash}") };
    PathBuf::from(format!(r"\\.\pipe\goattyd-{suffix}"))
}

/// Map a socket path to a Windows Named Pipe name.
///
/// Paths that already begin with `\\.\pipe\` (case-insensitive) are kept as-is.
/// Any other path (such as a `GOATTY_SOCKET` sandbox file path) is mapped
/// deterministically to `\\.\pipe\goatty-<sha256>`.
pub fn windows_pipe_name(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with(r"\\.\pipe\") {
        return raw.into_owned();
    }
    // Normalize path separators and case so `C:/foo/d.sock` and `c:\foo\d.sock`
    // map to the same pipe name.
    let normalized = lower.replace('/', "\\");
    let digest = Sha256::digest(normalized.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\goatty-{hex}")
}

/// The local daemon's IPC endpoint path.
///
/// Respects `GOATTY_SOCKET` on all platforms. On Windows without `GOATTY_SOCKET`,
/// uses `\\.\pipe\goattyd-<user-or-hash>` (unless `XDG_RUNTIME_DIR` is set, in
/// which case `<runtime_dir>\goattyd.sock` is returned and mapped to a pipe by
/// `windows_pipe_name`). On Unix, uses `<runtime_dir>/goattyd.sock`.
pub fn socket_path() -> PathBuf {
    socket_path_from(cfg!(target_os = "macos"), cfg!(windows), |k| std::env::var_os(k), std::env::temp_dir())
}

fn socket_path_from(macos: bool, windows: bool, env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    if let Some(p) = env("GOATTY_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if windows && env("XDG_RUNTIME_DIR").is_none_or(|v| v.is_empty()) {
        return default_windows_pipe_path_from(env);
    }
    runtime_dir_from(macos, windows, env, temp_dir).join("goattyd.sock")
}

/// Previous default socket path on macOS (`$TMPDIR/goatty-<user>/goatty/goattyd.sock`),
/// returned only when `socket_path()` moved to `~/Library/Caches/goatty/goattyd.sock`
/// (i.e. macOS with neither `GOATTY_SOCKET` nor `XDG_RUNTIME_DIR` set and an absolute `$HOME`).
///
/// Clients check this path before spawning a new daemon so an already-running
/// pre-upgrade macOS daemon and its sessions stay reachable across the upgrade.
pub fn legacy_socket_path() -> Option<PathBuf> {
    legacy_socket_path_from(cfg!(target_os = "macos"), |k| std::env::var_os(k), std::env::temp_dir())
}

fn legacy_socket_path_from(macos: bool, env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> Option<PathBuf> {
    if !macos {
        return None;
    }
    if env("GOATTY_SOCKET").is_some_and(|v| !v.is_empty()) || env("XDG_RUNTIME_DIR").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let home = env("HOME").map(PathBuf::from).filter(|p| is_abs(false, p))?;
    let current = home.join("Library/Caches/goatty").join("goattyd.sock");
    let legacy = fallback_runtime_dir(env, temp_dir).join("goattyd.sock");
    (legacy != current).then_some(legacy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn home_dir_handles_windows_fallbacks() {
        let env = env_of(&[("USERPROFILE", r"C:\Users\alice"), ("HOME", "/ignored")]);
        assert_eq!(home_dir_from(true, &env), Some(PathBuf::from(r"C:\Users\alice")));

        let env = env_of(&[("HOMEDRIVE", "C:"), ("HOMEPATH", r"\Users\bob")]);
        assert_eq!(home_dir_from(true, &env), Some(PathBuf::from(r"C:\Users\bob")));

        let env = env_of(&[("HOME", r"C:\Users\carol")]);
        assert_eq!(home_dir_from(true, &env), Some(PathBuf::from(r"C:\Users\carol")));

        let env = env_of(&[("USERPROFILE", r"C:\Users\alice")]);
        assert_eq!(home_dir_from(false, &env), None);
    }

    #[test]
    fn runtime_dir_prefers_xdg_on_all_platforms() {
        let env =
            env_of(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("HOME", "/Users/alice"), ("LOCALAPPDATA", r"C:\Users\alice\AppData\Local")]);
        assert_eq!(runtime_dir_from(false, false, &env, PathBuf::from("/tmp")), PathBuf::from("/run/user/1000/goatty"));
        assert_eq!(runtime_dir_from(true, false, &env, PathBuf::from("/tmp")), PathBuf::from("/run/user/1000/goatty"));
        assert_eq!(runtime_dir_from(false, true, &env, PathBuf::from(r"C:\Temp")), PathBuf::from("/run/user/1000/goatty"));
    }

    #[test]
    fn runtime_dir_uses_localappdata_on_windows() {
        let env = env_of(&[("LOCALAPPDATA", r"C:\Users\alice\AppData\Local"), ("USERNAME", "alice")]);
        assert_eq!(
            runtime_dir_from(false, true, &env, PathBuf::from(r"C:\Temp")),
            PathBuf::from(r"C:\Users\alice\AppData\Local").join("goatty").join("run")
        );

        let env = env_of(&[("USERPROFILE", r"C:\Users\alice")]);
        assert_eq!(
            runtime_dir_from(false, true, &env, PathBuf::from(r"C:\Temp")),
            PathBuf::from(r"C:\Users\alice").join("AppData").join("Local").join("goatty").join("run")
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_uses_library_caches_on_macos() {
        let env = env_of(&[("HOME", "/Users/alice"), ("USER", "alice")]);
        assert_eq!(
            runtime_dir_from(true, false, &env, PathBuf::from("/var/folders/xx/T")),
            PathBuf::from("/Users/alice/Library/Caches/goatty")
        );
        assert_eq!(runtime_dir_from(false, false, &env, PathBuf::from("/tmp")), PathBuf::from("/tmp/goatty-alice/goatty"));
    }

    #[test]
    fn runtime_dir_falls_back_when_home_missing_or_relative() {
        let env = env_of(&[("HOME", "relative"), ("USER", "alice")]);
        assert_eq!(runtime_dir_from(true, false, &env, PathBuf::from("/tmp")), PathBuf::from("/tmp/goatty-alice/goatty"));
    }

    #[test]
    fn windows_pipe_name_preserves_pipes_and_hashes_file_paths() {
        assert_eq!(windows_pipe_name(Path::new(r"\\.\pipe\goattyd-alice")), r"\\.\pipe\goattyd-alice");
        assert_eq!(windows_pipe_name(Path::new(r"\\.\PIPE\Custom")), r"\\.\PIPE\Custom");

        let a = windows_pipe_name(Path::new(r"C:\Temp\goatty-test\d.sock"));
        let b = windows_pipe_name(Path::new("c:/temp/goatty-test/d.sock"));
        let c = windows_pipe_name(Path::new(r"C:\Temp\other\d.sock"));
        assert!(a.starts_with(r"\\.\pipe\goatty-"), "{a}");
        assert_eq!(a.len(), r"\\.\pipe\goatty-".len() + 64);
        assert_eq!(a, b, "case and slash differences map to the same pipe");
        assert_ne!(a, c);
    }

    #[test]
    fn default_windows_socket_is_a_named_pipe() {
        let env = env_of(&[("USERNAME", "Alice"), ("USERPROFILE", r"C:\Users\Alice")]);
        let sock = socket_path_from(false, true, &env, PathBuf::from(r"C:\Temp"));
        let s = sock.to_string_lossy();
        assert!(s.starts_with(r"\\.\pipe\goattyd-Alice-"), "{s}");

        let with_override = env_of(&[("GOATTY_SOCKET", r"C:\Temp\test\d.sock"), ("USERNAME", "Alice")]);
        assert_eq!(socket_path_from(false, true, &with_override, PathBuf::from(r"C:\Temp")), PathBuf::from(r"C:\Temp\test\d.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_socket_path_only_applies_to_default_macos_path() {
        let env = env_of(&[("HOME", "/Users/alice"), ("USER", "alice")]);
        assert_eq!(
            legacy_socket_path_from(true, &env, PathBuf::from("/var/folders/xx/T")),
            Some(PathBuf::from("/var/folders/xx/T/goatty-alice/goatty/goattyd.sock"))
        );
        assert_eq!(legacy_socket_path_from(false, &env, PathBuf::from("/tmp")), None);

        let with_xdg = env_of(&[("XDG_RUNTIME_DIR", "/run/user/501"), ("HOME", "/Users/alice"), ("USER", "alice")]);
        assert_eq!(legacy_socket_path_from(true, &with_xdg, PathBuf::from("/var/folders/xx/T")), None);

        let with_override = env_of(&[("GOATTY_SOCKET", "/tmp/custom.sock"), ("HOME", "/Users/alice"), ("USER", "alice")]);
        assert_eq!(legacy_socket_path_from(true, &with_override, PathBuf::from("/var/folders/xx/T")), None);

        let relative_home = env_of(&[("HOME", "relative"), ("USER", "alice")]);
        assert_eq!(legacy_socket_path_from(true, &relative_home, PathBuf::from("/var/folders/xx/T")), None);
    }
}
