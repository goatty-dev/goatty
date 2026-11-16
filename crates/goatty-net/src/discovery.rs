//! Which tailnet peers to offer as goatty hosts: online nodes (of the same
//! Tailscale user unless asked otherwise) on an OS goattyd runs on, probed
//! on goatty's port. Peers that don't answer as a goatty gateway are left
//! out. Nothing polls: callers run this when a list opens.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::gateway::{self, Probe};
use crate::link::{self, Deadline, Mode};
use crate::tailscale::{Node, Status, Tailscale};

/// How long a probe may take: connect, greeting and answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A peer that answered as a goatty gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub node: Node,
    pub probe: Probe,
}

/// Whether discovery can tell this user's own devices from others'.
/// A tagged node belongs to the tailnet, not to a person, so from one
/// every peer counts.
pub fn owner_known(status: &Status) -> bool {
    !status.me.tagged
}

/// The peers worth probing: online, on an OS goattyd runs on, and the same
/// Tailscale user's (when that can be told) unless `all_users`.
pub fn candidates(status: &Status, all_users: bool) -> Vec<&Node> {
    let mine_only = !all_users && owner_known(status);
    status
        .peers
        .iter()
        .filter(|p| p.online && p.addr().is_some())
        .filter(|p| !matches!(p.os.as_str(), "android" | "iOS" | "windows" | "tvOS"))
        .filter(|p| !mine_only || (p.user_id == status.me.user_id && !p.tagged))
        .collect()
}

/// Probe `addr:port`.
pub fn probe(ts: &Tailscale, mode: Mode, addr: &str, port: u16) -> Result<Probe> {
    let l = link::open(ts, mode, addr, port, PROBE_TIMEOUT)?;
    let _deadline = Deadline::start(&l.closer, PROBE_TIMEOUT);
    let mut r = std::io::BufReader::new(l.reader);
    let mut w = l.writer;
    let p = gateway::probe(&mut r, &mut w);
    l.closer.close();
    p
}

/// Probe every candidate on `port` at once; the ones that answered, in the
/// status's order.
pub fn discover(ts: &Tailscale, status: &Status, port: u16, all_users: bool) -> Vec<Peer> {
    let mode = Mode::for_tun(status.tun);
    let handles: Vec<_> = candidates(status, all_users)
        .into_iter()
        .cloned()
        .map(|node| {
            let ts = ts.clone();
            std::thread::spawn(move || {
                let addr = node.addr()?.to_string();
                match probe(&ts, mode, &addr, port) {
                    Ok(probe) => Some(Peer { node, probe }),
                    Err(e) => {
                        log::debug!("tailnet: {} doesn't answer on {port}: {e:#}", node.name);
                        None
                    }
                }
            })
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tailscale::{State, tests::STATUS};

    #[test]
    fn candidates_are_online_capable_and_mine() {
        let State::Running(mut s) = Status::parse(STATUS).unwrap() else { panic!() };
        let names = |s: &Status, all| candidates(s, all).iter().map(|n| n.name.clone()).collect::<Vec<_>>();
        // phone: android; server: offline and someone else's.
        assert_eq!(names(&s, false), ["Laptop"]);
        s.peers.iter_mut().for_each(|p| p.online = true);
        assert_eq!(names(&s, false), ["Laptop"]);
        assert_eq!(names(&s, true), ["Laptop", "server"]);
        // From a tagged node, nobody's devices can be told apart.
        s.me.tagged = true;
        assert!(!owner_known(&s));
        assert_eq!(names(&s, false), ["Laptop", "server"]);
    }
}
