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

impl Dirs {
    /// From the process environment. `None` without `$HOME`.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// From variables read through `var` (injectable for tests). Relative
    /// XDG paths are invalid per the spec and ignored.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let home = PathBuf::from(var("HOME").filter(|h| !h.is_empty())?);
        let xdg = |name: &str, fallback: &str| {
            var(name).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(fallback)).join("goatty")
        };
        Some(Self {
            data: xdg("XDG_DATA_HOME", ".local/share"),
            config: xdg("XDG_CONFIG_HOME", ".config"),
            cache: xdg("XDG_CACHE_HOME", ".cache"),
            state: xdg("XDG_STATE_HOME", ".local/state"),
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
    pub fn generations_dir(&self) -> PathBuf {
        self.data.join("generations")
    }
    /// Symlink to the active generation.
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

    #[test]
    fn xdg_defaults_and_overrides() {
        let d = Dirs::from_vars(|k| (k == "HOME").then(|| "/home/u".into())).unwrap();
        assert_eq!(d.data, Path::new("/home/u/.local/share/goatty"));
        assert_eq!(d.state, Path::new("/home/u/.local/state/goatty"));
        assert_eq!(d.zsh_dir(), Path::new("/home/u/.local/share/goatty/current/zsh"));
        assert_eq!(d.user_zshrc(), Path::new("/home/u/.config/goatty/zshrc"));

        let d = Dirs::from_vars(|k| match k {
            "HOME" => Some("/home/u".into()),
            "XDG_DATA_HOME" => Some("/data".into()),
            "XDG_CACHE_HOME" => Some("relative/ignored".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(d.data, Path::new("/data/goatty"));
        assert_eq!(d.cache, Path::new("/home/u/.cache/goatty"));
        assert!(Dirs::from_vars(|_| None).is_none());
    }
}
