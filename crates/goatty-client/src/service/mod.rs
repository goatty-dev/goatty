//! goattyd as a user service (opt-in: `goattyd service install`).
//!
//! * **Linux (`systemd`)**: two units in `~/.config/systemd/user/` —
//!   `goattyd.socket` listens on `%t/goatty/goattyd.sock` and `goattyd.service`
//!   runs `goattyd serve` with `LISTEN_FDS` and systemd's fd store so sessions
//!   survive crashes and `systemctl --user restart` (#131).
//! * **macOS (`launchd`)**: a LaunchAgent in
//!   `~/Library/LaunchAgents/dev.goatty.goattyd.plist` with a `Sockets` entry
//!   listening on `~/Library/Caches/goatty/goattyd.sock` (claimed via
//!   `launch_activate_socket`) and logs under `~/Library/Logs/goatty/`.
//!   `goattyd restart` keeps sessions via re-exec, while crashes or
//!   `launchctl kickstart -k` end them because launchd has no fd store (#224).
//!
//! Without the service installed, clients start a detached daemon themselves
//! (`setsid`).

mod launchd;
mod systemd;

use std::path::{Path, PathBuf};

use anyhow::Result;

pub use launchd::{LAUNCHD_LABEL, LAUNCHD_PLIST, LAUNCHD_SOCKET_NAME, render_launchd_plist};
pub use systemd::{SERVICE_UNIT, SOCKET_UNIT, render_service, render_socket};

/// Current state of the user service.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub unit_dir: Option<PathBuf>,
    pub installed: bool,
    /// The program/command of the installed service.
    pub exec_start: Option<String>,
    pub socket_active: bool,
    pub service_active: bool,
    /// systemd-logind lingering state (Linux only; `None` on macOS).
    pub linger: Option<bool>,
    /// The installed service keeps sessions across daemon crashes and
    /// supervisor restarts (systemd fd store on Linux; `false` on macOS where
    /// launchd has no fd store and only `goattyd restart` keeps sessions).
    pub keeps_sessions: bool,
    /// Log file path when the service logs to a file (macOS launchd).
    pub log_path: Option<PathBuf>,
}

/// What [`install`] did.
#[derive(Debug, Clone)]
pub struct Installed {
    pub unit_dir: PathBuf,
    /// The socket is listening now. `false` when a daemon started outside
    /// the service manager still owns the socket path: the service takes over
    /// once it stops.
    pub started: bool,
    pub linger: Option<bool>,
}

/// Name of the managed service shown in CLI messages.
pub fn service_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        LAUNCHD_LABEL
    }
    #[cfg(not(target_os = "macos"))]
    {
        SERVICE_UNIT
    }
}

/// Where the service definition files live (`~/.config/systemd/user` on Linux,
/// `~/Library/LaunchAgents` on macOS).
pub fn unit_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::unit_dir()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::unit_dir()
    }
}

/// Log file path for the service, when it logs to a file (macOS launchd).
pub fn log_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::log_path()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Human-readable hint for where to look when the service fails to come up.
pub fn logs_hint() -> String {
    #[cfg(target_os = "macos")]
    {
        launchd::log_path().map(|p| p.display().to_string()).unwrap_or_else(|| "~/Library/Logs/goatty/goattyd.log".into())
    }
    #[cfg(not(target_os = "macos"))]
    {
        "`journalctl --user -u goattyd`".into()
    }
}

/// The socket path the user service listens on.
pub fn unit_socket_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::unit_socket_path()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::unit_socket_path()
    }
}

/// The service definition is installed (whether or not it's running).
pub fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        launchd::installed()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::installed()
    }
}

/// The goattyd binary the installed service runs.
pub fn service_program() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::service_program()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::service_program()
    }
}

/// Whether the managed daemon process is currently running under the user
/// service manager.
pub fn service_active() -> bool {
    #[cfg(target_os = "macos")]
    {
        launchd::service_active()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::service_active()
    }
}

/// Start the installed socket listener (clients do this instead of spawning a
/// detached daemon when the service is installed).
pub fn start_socket() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::start_socket()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::start_socket()
    }
}

/// Stop the running daemon through the service manager (`systemctl --user stop`
/// on Linux, `launchctl kill` / `bootout` on macOS).
pub fn stop_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::stop_service()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::stop_service()
    }
}

/// Whether logind keeps this user's services running without a login session
/// (Linux only; always `None` on macOS).
pub fn linger() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::linger()
    }
}

/// Inspect the installed service and its live state.
pub fn status() -> Status {
    #[cfg(target_os = "macos")]
    {
        launchd::status()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::status()
    }
}

/// Install and enable the user service for `exe` (`goattyd`), and start its
/// socket listener unless another daemon is already serving `socket_path`.
pub fn install(exe: &Path, socket_path: &Path) -> Result<Installed> {
    #[cfg(target_os = "macos")]
    {
        launchd::install(exe, socket_path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::install(exe, socket_path)
    }
}

/// Disable and remove the user service.
pub fn uninstall() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        launchd::uninstall()
    }
    #[cfg(not(target_os = "macos"))]
    {
        systemd::uninstall()
    }
}
