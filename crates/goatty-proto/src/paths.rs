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

fn runtime_dir_from(macos: bool, env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    if let Some(xdg) = env("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("goatty");
    }
    if macos && let Some(home) = env("HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        return home.join("Library/Caches/goatty");
    }
    let user =
        env("UID").or_else(|| env("USER")).and_then(|v| v.into_string().ok()).filter(|s| !s.is_empty()).unwrap_or_else(|| "user".into());
    temp_dir.join(format!("goatty-{user}")).join("goatty")
}

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GOATTY_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    runtime_dir().join("goattyd.sock")
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
}
