//! This host on the user's tailnet (#15): goattyd's `goatty_net::host::Host`.
//!
//! The host is made only when the tailnet is used: on start when the user
//! made this host available before (`tailnet/state.json` exists), else on
//! the first `Net` request. Connections that authenticated at the gateway
//! as a paired device are served like any other connection
//! (`client::handle`). goattyd binds no network socket: tailscaled forwards
//! the tailnet port into the gateway socket.

use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use goatty_net::Paths;
use goatty_net::api::{Reply, Request};
use goatty_net::gateway::HostInfo;
use goatty_net::host::Host;
use goatty_net::tailscale::Tailscale;
use goatty_proto::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, ReqId, ServerMsg};

use crate::server::Daemon;

fn paths() -> Option<Paths> {
    goatty_shell::Dirs::from_env().map(|d| Paths::new(d.tailnet_dir()))
}

fn make(daemon: &Arc<Daemon>) -> Result<Arc<Host>> {
    let paths = paths().context("no $HOME: nowhere to keep the device key")?;
    let info = HostInfo {
        name: crate::server::host_info().hostname,
        goatty: env!("CARGO_PKG_VERSION").into(),
        protocol: (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
    };
    let host = Arc::new(Host::new(Tailscale::system(), paths, goatty_net::gateway_socket(&daemon.socket), info)?);
    let weak: Weak<Daemon> = Arc::downgrade(daemon);
    let h = host.clone();
    // `start` talks to tailscale: never on the request's or the accept path.
    std::thread::Builder::new().name("tailnet-start".into()).spawn(move || {
        h.start(Box::new(move |stream, device| {
            let Some(daemon) = weak.upgrade() else { return };
            log::info!("tailnet: serving {}", device.name);
            if let Err(e) = crate::client::handle(daemon, stream) {
                log::debug!("tailnet client error: {e:#}");
            }
        }));
    })?;
    Ok(host)
}

/// The host, made now if needed.
fn host(daemon: &Arc<Daemon>) -> Result<Arc<Host>> {
    let mut net = daemon.net.lock();
    if let Some(h) = net.as_ref() {
        return Ok(h.clone());
    }
    let h = make(daemon)?;
    *net = Some(h.clone());
    Ok(h)
}

/// On start: serve the tailnet again if the user made this host available
/// (or used it before, so a leftover route gets cleaned up).
pub(crate) fn start(daemon: &Arc<Daemon>) {
    if paths().is_some_and(|p| p.state().exists())
        && let Err(e) = host(daemon)
    {
        log::error!("tailnet: {e:#}");
    }
}

/// The socket watchdog's tick: listen again if the gateway socket vanished.
pub(crate) fn check(daemon: &Arc<Daemon>) {
    let h = daemon.net.lock().clone();
    if let Some(h) = h {
        h.check();
    }
}

/// Answer a `Net` request (`goatty_net::api::Request` as JSON).
pub(crate) fn request(daemon: &Arc<Daemon>, req: ReqId, json: &str) -> ServerMsg {
    let reply = match serde_json::from_str::<Request>(json) {
        Err(e) => Reply::Error { message: format!("bad tailnet request: {e}") },
        Ok(request) => match host(daemon) {
            Ok(h) => h.request(request),
            Err(e) => Reply::Error { message: format!("{e:#}") },
        },
    };
    match serde_json::to_string(&reply) {
        Ok(reply) => ServerMsg::Net { req, reply },
        Err(e) => ServerMsg::Error { req: Some(req), message: e.to_string() },
    }
}
