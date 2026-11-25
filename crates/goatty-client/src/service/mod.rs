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
//! * **Windows (Scheduled Task)**: a per-user Scheduled Task (`\goattyd`) with
//!   a `LogonTrigger`, unlimited runtime, allowed on battery, hidden window,
//!   and logs in `%LOCALAPPDATA%\goatty\goattyd.log`. No administrator
//!   privileges are needed; stopping or restarting ends running sessions
//!   because Windows has no `execve` and ConPTY handles cannot be handed off.
//!
//! Without the service installed, clients start a detached daemon themselves.

mod launchd;
mod systemd;
mod windows;

use std::path::{Path, PathBuf};

use anyhow::Result;

pub use launchd::{LAUNCHD_LABEL, LAUNCHD_PLIST, LAUNCHD_SOCKET_NAME, render_launchd_plist};
pub use systemd::{SERVICE_UNIT, SOCKET_UNIT, render_service, render_socket};
pub use windows::{WINDOWS_TASK_NAME, render_task_xml};

/// Current state of the user service.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub unit_dir: Option<PathBuf>,
    pub installed: bool,
    /// The program/command of the installed service.
    pub exec_start: Option<String>,
    pub socket_active: bool,
    pub service_active: bool,
    /// systemd-logind lingering state (Linux only; `None` on macOS/Windows).
    pub linger: Option<bool>,
    /// The installed service keeps sessions across daemon crashes and
    /// supervisor restarts (systemd fd store on Linux; `false` on macOS and
    /// Windows).
    pub keeps_sessions: bool,
    /// Log file path when the service logs to a file (macOS launchd, Windows
    /// Scheduled Task).
    pub log_path: Option<PathBuf>,
}

/// What [`install`] did.
#[derive(Debug, Clone)]
pub struct Installed {
    pub unit_dir: PathBuf,
    /// The service/listener is active now. `false` when a daemon started
    /// outside the service manager still owns the endpoint: the service takes
    /// over once it stops.
    pub started: bool,
    pub linger: Option<bool>,
}

/// Name of the managed service shown in CLI messages.
pub fn service_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        LAUNCHD_LABEL
    }
    #[cfg(windows)]
    {
        WINDOWS_TASK_NAME
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        SERVICE_UNIT
    }
}

/// Where the service definition files live (`~/.config/systemd/user` on Linux,
/// `~/Library/LaunchAgents` on macOS, `%LOCALAPPDATA%\goatty` on Windows).
pub fn unit_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::unit_dir()
    }
    #[cfg(windows)]
    {
        windows::unit_dir()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        systemd::unit_dir()
    }
}

/// Log file path for the service, when it logs to a file (macOS launchd,
/// Windows Scheduled Task).
pub fn log_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::log_path()
    }
    #[cfg(windows)]
    {
        windows::log_path()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    #[cfg(windows)]
    {
        windows::log_path().map(|p| p.display().to_string()).unwrap_or_else(|| r"%LOCALAPPDATA%\goatty\goattyd.log".into())
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        "`journalctl --user -u goattyd`".into()
    }
}

/// The socket/pipe path the user service listens on.
pub fn unit_socket_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::unit_socket_path()
    }
    #[cfg(windows)]
    {
        windows::unit_socket_path()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    #[cfg(windows)]
    {
        windows::installed()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    #[cfg(windows)]
    {
        windows::service_program()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    #[cfg(windows)]
    {
        windows::service_active()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        systemd::service_active()
    }
}

/// Start the installed user service (clients do this instead of spawning a
/// detached daemon when the service is installed).
pub fn start_socket() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::start_socket()
    }
    #[cfg(windows)]
    {
        windows::start_socket()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        systemd::start_socket()
    }
}

/// Stop the running daemon through the service manager (`systemctl --user stop`
/// on Linux, `launchctl kill` / `bootout` on macOS, `schtasks /End` on Windows).
pub fn stop_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::stop_service()
    }
    #[cfg(windows)]
    {
        windows::stop_service()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        systemd::stop_service()
    }
}

/// Whether logind keeps this user's services running without a login session
/// (Linux only; always `None` on macOS and Windows).
pub fn linger() -> Option<bool> {
    #[cfg(any(target_os = "macos", windows))]
    {
        None
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    #[cfg(windows)]
    {
        windows::status()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        systemd::status()
    }
}

/// Install and enable the user service for `exe` (`goattyd`), and start it
/// unless another daemon is already serving `socket_path`.
pub fn install(exe: &Path, socket_path: &Path) -> Result<Installed> {
    #[cfg(target_os = "macos")]
    {
        launchd::install(exe, socket_path)
    }
    #[cfg(windows)]
    {
        windows::install(exe, socket_path)
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    #[cfg(windows)]
    {
        windows::uninstall()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        systemd::uninstall()
    }
}
