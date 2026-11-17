//! Client side of the goatty protocol, independent of any UI.
//!
//! * [`Control`] is a blocking request/response client for CLIs and scripts
//!   (`goattyd list`). It never starts a daemon and never reconnects.
//! * [`Connection`] is one link to one goattyd (local or remote) carrying any
//!   number of attached sessions ("panes"). It owns three threads:
//!
//!   * **reader**: decodes frames, feeds each pane's [`PaneSink`], sends acks,
//!     and transparently reconnects + reattaches when the link drops,
//!   * **writer**: the only thread that writes to the transport, so the UI
//!     thread never blocks on a congested or dead link,
//!   * **heartbeat** (remote only): measures RTT and tears down links that
//!     have gone silent, which triggers the reconnect.
//! * [`transport`] turns an [`Endpoint`] (local socket, `ssh host`, any
//!   stdio command) into a byte stream.
//! * [`remote`] finds, installs and restarts goattyd on SSH hosts and
//!   explains failed connects ([`RemoteProblem`]); [`askpass`] decodes ssh's
//!   prompts for the GUI.
//! * [`local`] tells whether the local goattyd is an older build than the
//!   installed one, and stops it (any protocol version) to restart it.
//! * [`service`] knows about goattyd's systemd user units (starting the
//!   socket instead of a detached daemon when they're installed).
//! * [`ssh_config`] lists the hosts in `~/.ssh/config` for the GUI's host list.
//! * [`tailnet`] connects to paired hosts on the user's tailnet, discovers
//!   and pairs new ones (`goatty_net` does the Tailscale and gateway parts).

pub mod askpass;
mod connection;
pub mod control;
mod handshake;
pub mod local;
pub mod remote;
pub mod service;
pub mod ssh_config;
pub mod tailnet;
pub mod transport;

pub use connection::{Connection, HostSettingsApplied, HostSettingsReply};
pub use control::Control;
pub use goatty_proto::{RemotePathInfo, TopLineCause};

/// This computer's host name (`gethostname`, else `/etc/hostname`); empty
/// when neither works.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for writes of the length passed.
    let from_libc = (unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0)
        .then(|| String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(buf.len())]).trim().to_string());
    from_libc
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .unwrap_or_default()
}
pub use handshake::VersionMismatch;
pub use local::LocalProblem;
pub use remote::RemoteProblem;
pub use transport::{Endpoint, SshTarget, TailnetTarget, connect_or_spawn_daemon};

/// Receives one session's terminal stream. Called on the connection's
/// reader thread; keep it fast (feed a VT, wake the UI).
pub trait PaneSink: Send + Sync + 'static {
    /// VT bytes in stream order: attach snapshots, live output and resync
    /// snapshots alike. Feeding them into a terminal reproduces the session.
    fn feed(&self, bytes: &[u8]);
    fn event(&self, ev: PaneEvent);
    /// In stream order: the session's top screen row is host line `line`
    /// (protocol v11; `goatty_term::Terminal::set_top_line`).
    fn top_line(&self, _line: u64, _cause: TopLineCause) {}
    /// The answer to [`Connection::fetch_history`], or `None` if it failed
    /// (the link dropped, or the daemon refused). Called on the reader
    /// thread, in order with `feed`.
    fn history(&self, _history: Option<History>) {}
}

/// Scrollback from the daemon ([`Connection::fetch_history`]): the rows
/// from host line `first` up to `before`, as VT text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub before: u64,
    pub first: u64,
    /// The oldest line the daemon still has.
    pub oldest: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum PaneEvent {
    /// Flow control skipped `skipped` bytes; the screen is current again.
    Resynced { skipped: u64 },
    /// The link dropped; the session keeps running on the host.
    Disconnected(String),
    /// Automatic reconnects can't continue without asking the user (a
    /// password, a key passphrase, a host key): [`Connection::reconnect`]
    /// tries again with prompts.
    ///
    /// [`Connection::reconnect`]: crate::Connection::reconnect
    NeedsReconnect(String),
    /// Reattached after a reconnect (a full snapshot was fed).
    Reconnected,
    /// The session's process exited.
    Exited(Option<u32>),
    /// The session can't be reattached (daemon restarted, or gave up).
    Lost(String),
    /// The foreground program changed (`zsh`, `nvim`, …).
    Process(Option<String>),
    /// The session's working directory on the daemon host changed.
    Cwd(Option<String>),
}
