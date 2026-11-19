//! Where the daemon socket lives on a host.

use std::ffi::OsString;
use std::path::PathBuf;

/// Directory for the local daemon socket and ephemeral runtime files.
///
/// Uses `$XDG_RUNTIME_DIR/goatty` when set. Otherwise, on macOS uses
/// `~/Library/Caches/goatty` (persistent across reboots for launchd's
/// `SockPathName`, excluded from Time Machine, shared across GUI/terminal/SSH,
/// and well under macOS's 104-byte `sun_path` limit); on other platforms falls
/// back to `<temp_dir>/goatty-<uid>/goatty`.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_from(cfg!(target_os = "macos"), |k| std::env::var_os(k), std::env::temp_dir())
}

fn fallback_runtime_dir(env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    let user =
        env("UID").or_else(|| env("USER")).and_then(|v| v.into_string().ok()).filter(|s| !s.is_empty()).unwrap_or_else(|| "user".into());
    temp_dir.join(format!("goatty-{user}")).join("goatty")
}

fn runtime_dir_from(macos: bool, env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    if let Some(xdg) = env("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("goatty");
    }
    if macos && let Some(home) = env("HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        return home.join("Library/Caches/goatty");
    }
    fallback_runtime_dir(env, temp_dir)
}

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GOATTY_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    runtime_dir().join("goattyd.sock")
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
    let home = env("HOME").map(PathBuf::from).filter(|p| p.is_absolute())?;
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
    fn runtime_dir_prefers_xdg_on_all_platforms() {
        let env = env_of(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("HOME", "/Users/alice")]);
        assert_eq!(runtime_dir_from(false, &env, PathBuf::from("/tmp")), PathBuf::from("/run/user/1000/goatty"));
        assert_eq!(runtime_dir_from(true, &env, PathBuf::from("/tmp")), PathBuf::from("/run/user/1000/goatty"));
    }

    #[test]
    fn runtime_dir_uses_library_caches_on_macos() {
        let env = env_of(&[("HOME", "/Users/alice"), ("USER", "alice")]);
        assert_eq!(runtime_dir_from(true, &env, PathBuf::from("/var/folders/xx/T")), PathBuf::from("/Users/alice/Library/Caches/goatty"));
        assert_eq!(runtime_dir_from(false, &env, PathBuf::from("/tmp")), PathBuf::from("/tmp/goatty-alice/goatty"));
    }

    #[test]
    fn runtime_dir_falls_back_when_home_missing_or_relative() {
        let env = env_of(&[("HOME", "relative"), ("USER", "alice")]);
        assert_eq!(runtime_dir_from(true, &env, PathBuf::from("/tmp")), PathBuf::from("/tmp/goatty-alice/goatty"));
    }

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
