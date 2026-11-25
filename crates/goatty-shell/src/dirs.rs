//! Where goatty keeps its managed shell environment (XDG base directories).
//!
//! ```text
//! $XDG_DATA_HOME/goatty/          (~/.local/share/goatty)
//!   generations/<n>/              one built shell configuration each: zsh/ (the ZDOTDIR),
//!                                 host.json (the settings it came from), info.json
//!   current -> generations/<n>    the active one (switched atomically)
//!   tools/<tool>/<version>/       unpacked managed installs
//!   bin/<exe>                     symlinks to the active managed binaries
//!   plugins/<tool>                symlinks to the active managed zsh plugins
//!   share/zsh/site-functions/     completion functions: goatty's own, managed tools' (on fpath)
//!   installed/<plugin>/           installed goatty plugins (verified copies of their files)
//!   installed/lock.json           which plugins are installed, from where, what was approved
//!   registries/<name>/            verified snapshots of plugin registries
//!   tailnet/                      device.key (0600), devices.json, hosts.json, state.json (goatty-net)
//! $XDG_CONFIG_HOME/goatty/zshrc   the user's own additions (goatty never writes it)
//! $XDG_CONFIG_HOME/goatty/registries.json   plugin registries added besides the official one
//! $XDG_CACHE_HOME/goatty/         completion dump, init/ (cached tool init scripts), downloads
//! $XDG_STATE_HOME/goatty/         small flags (the dismissed setup hint, the GUI offered the setup)
//!   import/                       what the user's own shell sets up (harvest.json, import.zsh)
//!   shell-refreshed               when the configuration and the import were last brought up to date
//!   zshenv-backups/               the user's .zshenv before goatty added or removed its line
//! ```

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub data: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
}

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

fn home_dir_for(windows: bool, var: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if windows {
        if let Some(p) = var("USERPROFILE").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(p));
        }
        if let (Some(drive), Some(path)) = (var("HOMEDRIVE").filter(|v| !v.is_empty()), var("HOMEPATH").filter(|v| !v.is_empty())) {
            return Some(PathBuf::from(format!("{drive}{path}")));
        }
    }
    var("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

impl Dirs {
    /// The user's home directory (`USERPROFILE` / `HOMEDRIVE`+`HOMEPATH` / `HOME`
    /// on Windows; `HOME` on Unix).
    pub fn home_dir() -> Option<PathBuf> {
        home_dir_for(cfg!(windows), &|k| std::env::var(k).ok())
    }

    /// From the process environment. `None` without a home directory (or Windows AppData dirs).
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// From variables read through `var` (injectable for tests). Relative
    /// XDG paths are invalid per the spec and ignored.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        Self::from_vars_for(cfg!(windows), var)
    }

    fn from_vars_for(windows: bool, var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let xdg = |name: &str| var(name).map(PathBuf::from).filter(|p| is_abs(windows, p)).map(|p| p.join("goatty"));
        if windows {
            let home = home_dir_for(true, &var);
            let local_app = var("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|p| is_abs(true, p))
                .or_else(|| home.as_ref().map(|h| h.join("AppData").join("Local")));
            let app_data = var("APPDATA")
                .map(PathBuf::from)
                .filter(|p| is_abs(true, p))
                .or_else(|| home.as_ref().map(|h| h.join("AppData").join("Roaming")));
            return Some(Self {
                data: xdg("XDG_DATA_HOME").or_else(|| local_app.as_ref().map(|p| p.join("goatty")))?,
                config: xdg("XDG_CONFIG_HOME").or_else(|| app_data.as_ref().map(|p| p.join("goatty")))?,
                cache: xdg("XDG_CACHE_HOME").or_else(|| local_app.as_ref().map(|p| p.join("goatty").join("cache")))?,
                state: xdg("XDG_STATE_HOME").or_else(|| local_app.as_ref().map(|p| p.join("goatty").join("state")))?,
            });
        }
        let home = home_dir_for(false, &var)?;
        Some(Self {
            data: xdg("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share").join("goatty")),
            config: xdg("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config").join("goatty")),
            cache: xdg("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache").join("goatty")),
            state: xdg("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state").join("goatty")),
        })
    }

    /// All four under one root (tests, sandboxes).
    pub fn under(root: &Path) -> Self {
        Self { data: root.join("data"), config: root.join("config"), cache: root.join("cache"), state: root.join("state") }
    }

    /// The active generation's ZDOTDIR (through the `current` link, so a
    /// path written into the config stays valid across generations).
    pub fn zsh_dir(&self) -> PathBuf {
        self.current_link().join("zsh")
    }
    /// The active generation's PowerShell directory (through the `current` junction).
    pub fn pwsh_dir(&self) -> PathBuf {
        self.current_link().join("pwsh")
    }
    /// The active generation's shell directory (`pwsh` on Windows, `zsh` on Unix).
    pub fn shell_dir(&self) -> PathBuf {
        if cfg!(windows) { self.pwsh_dir() } else { self.zsh_dir() }
    }
    pub fn generations_dir(&self) -> PathBuf {
        self.data.join("generations")
    }
    /// Symlink (Unix) or NTFS junction (Windows) to the active generation.
    pub fn current_link(&self) -> PathBuf {
        self.data.join("current")
    }
    pub fn tools_dir(&self) -> PathBuf {
        self.data.join("tools")
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.data.join("bin")
    }
    pub fn plugins_dir(&self) -> PathBuf {
        self.data.join("plugins")
    }
    /// Cached tool init scripts (`_goatty_cache/init`).
    pub fn init_cache_dir(&self) -> PathBuf {
        self.cache.join("init")
    }
    /// Zsh completion functions goatty provides ([`crate::completions`]).
    pub fn site_functions_dir(&self) -> PathBuf {
        self.data.join("share/zsh/site-functions")
    }
    /// PowerShell completion scripts goatty provides ([`crate::completions`]).
    pub fn pwsh_completions_dir(&self) -> PathBuf {
        self.data.join("share/pwsh/completions")
    }
    /// Sentinel file bumped by `reload_shells()` on Windows so running
    /// PowerShell prompts pick up configuration reloads without `SIGURG`.
    pub fn reload_sentinel(&self) -> PathBuf {
        self.state.join("shell-reload")
    }
    /// Installed goatty plugins (Stage 5): `installed/<id>/` + `lock.json`.
    pub fn installed_dir(&self) -> PathBuf {
        self.data.join("installed")
    }
    /// Verified registry snapshots: `registries/<name>/`.
    pub fn registries_dir(&self) -> PathBuf {
        self.data.join("registries")
    }
    /// Registries the user added (the official one is built in).
    pub fn registries_file(&self) -> PathBuf {
        self.config.join("registries.json")
    }
    /// The host settings (`goatty_config::host_file()` for the real dirs).
    pub fn host_file(&self) -> PathBuf {
        self.config.join("host.json")
    }
    /// Touched (with the time the check started) whenever the
    /// configuration and the import were brought up to date: hooked shells
    /// outside goatty compare their startup files against it (`entry.zsh`).
    pub fn refresh_stamp(&self) -> PathBuf {
        self.state.join("shell-refreshed")
    }
    /// The files goatty added its line to (one path a line), so turning it
    /// off finds them after `ZDOTDIR` moved ([`crate::everywhere`]).
    pub fn zshenv_record(&self) -> PathBuf {
        self.state.join("zshenv-hooked")
    }
    /// Copies of the user's `.zshenv` from before goatty added or removed
    /// its line ([`crate::everywhere`]).
    pub fn zshenv_backups(&self) -> PathBuf {
        self.state.join("zshenv-backups")
    }
    /// The user's own zsh additions, sourced late by the managed `.zshrc`.
    pub fn user_zshrc(&self) -> PathBuf {
        self.config.join("zshrc")
    }
    /// The user's own PowerShell additions (`%APPDATA%\goatty\profile.ps1`),
    /// dot-sourced late by the managed `init.ps1`.
    pub fn user_ps_profile(&self) -> PathBuf {
        self.config.join("profile.ps1")
    }
    /// goatty over the tailnet (`goatty_net::Paths`): the device key, paired
    /// devices, pinned hosts, whether this host is available.
    pub fn tailnet_dir(&self) -> PathBuf {
        self.data.join("tailnet")
    }
    /// What goatty imported from the user's own shell ([`crate::import`]).
    pub fn import_dir(&self) -> PathBuf {
        self.state.join("import")
    }
    /// The last harvest and failure, as JSON.
    pub fn import_record(&self) -> PathBuf {
        self.import_dir().join("harvest.json")
    }
    /// The harvest as zsh, sourced by the managed `.zshrc`.
    pub fn import_file(&self) -> PathBuf {
        self.import_dir().join("import.zsh")
    }
    /// Exists once the user dismissed the "tools missing" hint.
    pub fn hint_marker(&self) -> PathBuf {
        self.state.join("shell-hint-dismissed")
    }
    /// Exists once a GUI offered to install the missing tools: the hint in
    /// new shells stays quiet (the GUI asks instead).
    pub fn offer_marker(&self) -> PathBuf {
        self.state.join("shell-setup-offered")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn xdg_defaults_and_overrides() {
        let d = Dirs::from_vars_for(false, |k| (k == "HOME").then(|| "/home/u".into())).unwrap();
        assert_eq!(d.data, Path::new("/home/u/.local/share/goatty"));
        assert_eq!(d.state, Path::new("/home/u/.local/state/goatty"));
        assert_eq!(d.zsh_dir(), Path::new("/home/u/.local/share/goatty/current/zsh"));
        assert_eq!(d.user_zshrc(), Path::new("/home/u/.config/goatty/zshrc"));

        let d = Dirs::from_vars_for(false, |k| match k {
            "HOME" => Some("/home/u".into()),
            "XDG_DATA_HOME" => Some("/data".into()),
            "XDG_CACHE_HOME" => Some("relative/ignored".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(d.data, Path::new("/data/goatty"));
        assert_eq!(d.cache, Path::new("/home/u/.cache/goatty"));
        assert!(Dirs::from_vars_for(false, |_| None).is_none());
    }

    #[test]
    fn windows_defaults_and_xdg_overrides() {
        let d = Dirs::from_vars_for(true, |k| match k {
            "APPDATA" => Some(r"C:\Users\u\AppData\Roaming".into()),
            "LOCALAPPDATA" => Some(r"C:\Users\u\AppData\Local".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(d.config, PathBuf::from(r"C:\Users\u\AppData\Roaming").join("goatty"));
        assert_eq!(d.data, PathBuf::from(r"C:\Users\u\AppData\Local").join("goatty"));
        assert_eq!(d.cache, PathBuf::from(r"C:\Users\u\AppData\Local").join("goatty").join("cache"));
        assert_eq!(d.state, PathBuf::from(r"C:\Users\u\AppData\Local").join("goatty").join("state"));

        let d = Dirs::from_vars_for(true, |k| match k {
            "USERPROFILE" => Some(r"C:\Users\u".into()),
            "XDG_CONFIG_HOME" => Some(r"D:\cfg".into()),
            "XDG_CACHE_HOME" => Some("relative/ignored".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(d.config, PathBuf::from(r"D:\cfg").join("goatty"));
        assert_eq!(d.cache, PathBuf::from(r"C:\Users\u").join("AppData").join("Local").join("goatty").join("cache"));
        assert!(Dirs::from_vars_for(true, |_| None).is_none());
    }
}
