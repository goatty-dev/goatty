//! The `Hello` exchange every client performs first, and version
//! negotiation (see the compatibility notes in `goatty_proto`).

use std::fmt;
use std::io::Read;

use anyhow::{Context, Result};
use goatty_proto::{ClientMsg, Frame, HostInfo, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, ServerMsg, encode, read_msg};

/// The client's greeting frame, announcing protocol `version`.
pub(crate) fn hello(client: &str, version: u32) -> Result<Frame> {
    Ok(encode(&ClientMsg::Hello { version, client: client.into() })?)
}

/// A daemon that answered, with what was agreed.
#[derive(Debug, Clone)]
pub(crate) struct Greeting {
    pub host: HostInfo,
    pub pid: u32,
    /// The protocol version both sides use on this connection.
    pub version: u32,
}

/// The daemon speaks no protocol version this client does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMismatch {
    /// The daemon's protocol version, when it said.
    pub daemon_protocol: Option<u32>,
    /// The daemon's goatty version (newer daemons tell).
    pub daemon_version: Option<String>,
    /// The daemon is newer than this client (update goatty here), rather
    /// than older (update or restart the daemon).
    pub daemon_is_newer: bool,
}

impl fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let daemon = match (&self.daemon_version, self.daemon_protocol) {
            (Some(v), Some(p)) => format!("goattyd {v} (protocol v{p})"),
            (None, Some(p)) => format!("goattyd with protocol v{p}"),
            _ => "an incompatible goattyd".into(),
        };
        let range = format!("v{MIN_PROTOCOL_VERSION}–v{PROTOCOL_VERSION}");
        if self.daemon_is_newer {
            write!(f, "{daemon} is newer than this goatty (protocol {range}): update goatty on this computer")
        } else {
            write!(f, "{daemon} is older than this goatty supports (protocol {range}): update goattyd on that host")
        }
    }
}

impl std::error::Error for VersionMismatch {}

/// What a greeting turned out to be.
#[derive(Debug)]
pub(crate) enum Outcome {
    Ok(Greeting),
    /// A daemon from before negotiation (exact match only) that speaks
    /// `version`, which we support: reconnect and announce that.
    RetryWith(u32),
}

/// Read and check the daemon's greeting to a `Hello` that announced
/// `announced`.
///
/// `Hello` is variant 0 with `version` first in every protocol version, so
/// even an old daemon's greeting decodes far enough to tell. Anything else:
/// not a (compatible) goattyd.
pub(crate) fn read_greeting(reader: &mut impl Read, via: &str, announced: u32) -> Result<Outcome> {
    read_greeting_from(reader, via, announced, MIN_PROTOCOL_VERSION)
}

/// [`read_greeting`], accepting daemons down to protocol `min` (restarting
/// or stopping an older daemon, `ADMIN_MIN_PROTOCOL_VERSION`).
pub(crate) fn read_greeting_from(reader: &mut impl Read, via: &str, announced: u32, min: u32) -> Result<Outcome> {
    let reply: ServerMsg = read_msg(reader).with_context(|| format!("no valid greeting from goattyd via {via}"))?;
    check_greeting_from(reply, announced, min)
}

#[cfg(test)]
fn check_greeting(reply: ServerMsg, announced: u32) -> Result<Outcome> {
    check_greeting_from(reply, announced, MIN_PROTOCOL_VERSION)
}

fn check_greeting_from(reply: ServerMsg, announced: u32, min: u32) -> Result<Outcome> {
    match reply {
        ServerMsg::Hello { version, daemon_pid, host } => {
            if (min..=announced.min(PROTOCOL_VERSION)).contains(&version) {
                Ok(Outcome::Ok(Greeting { host, pid: daemon_pid, version }))
            } else {
                Err(VersionMismatch {
                    daemon_protocol: Some(version),
                    daemon_version: Some(host.daemon_version).filter(|v| !v.is_empty()),
                    daemon_is_newer: version > announced,
                }
                .into())
            }
        }
        ServerMsg::Error { message, .. } => match legacy_mismatch(&message) {
            Some(v) if (min..announced).contains(&v) => Ok(Outcome::RetryWith(v)),
            Some(v) => Err(VersionMismatch { daemon_protocol: Some(v), daemon_version: None, daemon_is_newer: v > announced }.into()),
            None => Err(anyhow::anyhow!("goattyd: {message}")),
        },
        other => Err(anyhow::anyhow!("unexpected greeting {other:?}")),
    }
}

/// The daemon version in a v2–v5 daemon's mismatch error
/// (`"protocol mismatch: daemon v5, client v6"`).
fn legacy_mismatch(message: &str) -> Option<u32> {
    let rest = message.strip_prefix("protocol mismatch: daemon v")?;
    rest.split(',').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(version: u32, daemon_version: &str) -> ServerMsg {
        ServerMsg::Hello { version, daemon_pid: 7, host: HostInfo { daemon_version: daemon_version.into(), ..Default::default() } }
    }

    fn mismatch(r: Result<Outcome>) -> VersionMismatch {
        r.unwrap_err().downcast::<VersionMismatch>().unwrap()
    }

    #[test]
    fn accepts_any_version_in_range() {
        for v in MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION {
            let Outcome::Ok(g) = check_greeting(hello(v, "0.1.0"), PROTOCOL_VERSION).unwrap() else { panic!() };
            assert_eq!((g.version, g.pid), (v, 7));
        }
    }

    #[test]
    fn tells_older_from_newer() {
        let m = mismatch(check_greeting(hello(MIN_PROTOCOL_VERSION - 1, "0.0.1"), PROTOCOL_VERSION));
        assert!(!m.daemon_is_newer);
        assert_eq!(m.daemon_version.as_deref(), Some("0.0.1"));
        assert!(m.to_string().contains("update goattyd on that host"), "{m}");
        // A daemon whose oldest version is newer than ours answers with its newest.
        let m = mismatch(check_greeting(hello(PROTOCOL_VERSION + 4, "9.0.0"), PROTOCOL_VERSION));
        assert!(m.daemon_is_newer);
        assert!(m.to_string().contains("update goatty on this computer"), "{m}");
    }

    #[test]
    fn older_daemons_can_still_be_restarted() {
        let old = hello(MIN_PROTOCOL_VERSION - 1, "0.1.0");
        assert!(check_greeting(old.clone(), PROTOCOL_VERSION).is_err());
        let Outcome::Ok(g) = check_greeting_from(old, PROTOCOL_VERSION, goatty_proto::ADMIN_MIN_PROTOCOL_VERSION).unwrap() else { panic!() };
        assert_eq!(g.version, MIN_PROTOCOL_VERSION - 1);
    }

    #[test]
    fn legacy_exact_match_daemons_get_a_retry() {
        // Exact-match daemons (v5 and older) are older than any client
        // speaks now; the retry logic itself, with a lower minimum:
        let err = |d: u32| ServerMsg::Error { req: None, message: format!("protocol mismatch: daemon v{d}, client v{PROTOCOL_VERSION}") };
        let check = |reply, announced| check_greeting_from(reply, announced, 4);
        assert!(matches!(check(err(5), PROTOCOL_VERSION).unwrap(), Outcome::RetryWith(5)));
        // Too old to speak to at all.
        assert!(!mismatch(check(err(3), PROTOCOL_VERSION)).daemon_is_newer);
        assert!(!mismatch(check_greeting(err(5), PROTOCOL_VERSION)).daemon_is_newer);
        // Retrying with that version and getting the same answer again doesn't loop.
        assert!(check(err(5), 5).is_err());
        let other = ServerMsg::Error { req: None, message: "nope".into() };
        assert_eq!(check_greeting(other, PROTOCOL_VERSION).unwrap_err().to_string(), "goattyd: nope");
    }
}
