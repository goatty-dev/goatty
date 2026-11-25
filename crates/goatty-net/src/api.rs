//! What the GUI and `goattyd net` ask a goattyd about its availability on
//! the tailnet, as JSON (`Net`, protocol v12). New operations need no
//! protocol bump.

use serde::{Deserialize, Serialize};

use crate::gateway::PairingWindow;
use crate::pairing::Device;
use crate::tailscale::{Permission, State};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Make this host available on the tailnet (on `port`, default: the
    /// one it had, else [`crate::DEFAULT_PORT`]).
    Enable {
        port: Option<u16>,
    },
    /// Remove goatty's Serve route. Paired devices stay paired.
    Disable,
    /// Accept pairing requests for `secs` (default 5 minutes).
    OpenPairing {
        secs: Option<u64>,
    },
    ClosePairing,
    /// Confirm or reject a waiting pairing request.
    Decide {
        id: u32,
        accept: bool,
    },
    /// Forget the devices whose key starts with `key` (8+ hex digits).
    Revoke {
        key: String,
    },
    /// Disable and forget every paired device.
    Leave,
}

/// Every request is answered with the status after it, or an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Box<NetStatus>),
    Error { message: String },
}

impl Reply {
    pub fn into_result(self) -> anyhow::Result<NetStatus> {
        match self {
            Reply::Status(s) => Ok(*s),
            Reply::Error { message } => Err(anyhow::anyhow!(message)),
        }
    }
}

/// This host's availability on the tailnet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    pub tailscale: State,
    /// The user wants this host available.
    pub available: bool,
    pub port: u16,
    pub route: RouteStatus,
    /// The gateway socket is listening.
    pub listening: bool,
    pub pairing: Option<PairingWindow>,
    pub devices: Vec<Device>,
    /// This host's public key (hex), for comparing by eye.
    pub key: String,
}

/// goatty's Serve route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum RouteStatus {
    /// None (availability is off).
    Off,
    /// In place: tailnet connections reach the gateway.
    Active,
    /// Tailscale isn't usable (see `tailscale`).
    NoTailscale,
    /// The port serves something else, which goatty leaves alone.
    PortTaken {
        what: String,
    },
    /// Tailscale doesn't let this user change Serve: run one of these.
    Permission(Permission),
    Failed {
        message: String,
    },
}

impl RouteStatus {
    /// One line for the CLI and the settings page.
    pub fn describe(&self, port: u16) -> String {
        match self {
            RouteStatus::Off => "not available on the tailnet".into(),
            RouteStatus::Active => format!("available on the tailnet, port {port}"),
            RouteStatus::NoTailscale => "waiting for Tailscale".into(),
            RouteStatus::PortTaken { what } => {
                format!("port {port} already serves {what}; goatty leaves it alone: pick another port")
            }
            RouteStatus::Permission(p) => format!("Tailscale doesn't let goatty add its route: {}", p.summary()),
            RouteStatus::Failed { message } => message.clone(),
        }
    }
}
