//! This host on the user's tailnet (#15): goattyd's `goatty_net::host::Host`.
//!
//! The host is made only when the tailnet is used: on start when the user
//! made this host available before (`tailnet/state.json` exists), else on
//! the first `Net` request. Connections that authenticated at the gateway
//! as a paired device are served like any other connection
//! (`client::handle`). On Unix goattyd binds no network socket: tailscaled
//! forwards the tailnet port into the gateway socket. On Windows the gateway
//! listens on a loopback port (Serve can't forward into a pipe) and each
//! authenticated connection is bridged into a named pipe pair ([`bridge`]),
//! as connections are named pipes there.

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
            #[cfg(unix)]
            let stream = goatty_client::LocalStream::from(stream);
            #[cfg(not(unix))]
            let stream = match bridge(stream) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("tailnet: couldn't serve {}: {e:#}", device.name);
                    return;
                }
            };
            if let Err(e) = crate::client::handle(daemon, stream) {
                log::debug!("tailnet client error: {e:#}");
            }
        }));
    })?;
    Ok(host)
}

/// Windows: relay an authenticated gateway connection (loopback TCP) through
/// a connected named pipe pair, and return the end `client::handle` serves.
/// Either side closing ends both.
///
/// A peer that stops reading must not hold both threads forever (writes to
/// it would block, and its silence keeps the reader waiting): a write that
/// makes no progress for `STALLED_WRITE` ends the bridge. Flow control
/// keeps a live peer's backlog far below what that takes to drain.
#[cfg(not(unix))]
fn bridge(tcp: std::net::TcpStream) -> Result<goatty_client::LocalStream> {
    use std::io::{Read, Write};
    const STALLED_WRITE: std::time::Duration = std::time::Duration::from_secs(60);
    tcp.set_write_timeout(Some(STALLED_WRITE))?;
    let (ours, theirs) = goatty_client::LocalStream::pair().context("named pipe pair")?;
    fn pump(mut from: impl Read, mut to: impl Write) {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
                        break;
                    }
                }
            }
        }
    }
    let (tcp_r, pipe_w) = (tcp.try_clone()?, ours.try_clone()?);
    let (tcp_w, pipe_r) = (tcp, ours);
    std::thread::Builder::new().name("gateway-in".into()).spawn(move || {
        pump(&tcp_r, &pipe_w);
        let _ = pipe_w.shutdown(std::net::Shutdown::Both);
        let _ = tcp_r.shutdown(std::net::Shutdown::Both);
    })?;
    std::thread::Builder::new().name("gateway-out".into()).spawn(move || {
        pump(&pipe_r, &tcp_w);
        let _ = tcp_w.shutdown(std::net::Shutdown::Both);
        let _ = pipe_r.shutdown(std::net::Shutdown::Both);
    })?;
    Ok(theirs)
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
