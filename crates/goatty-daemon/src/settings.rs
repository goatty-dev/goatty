//! Host settings over the protocol: the settings window edits a host's
//! `host.json` through its daemon, so a remote host keeps its own. The
//! logic is `goatty_shell::HostService`; this maps it to wire messages.

use goatty_proto::{FileDiff, GenerationInfo, ReqId, ServerMsg};
use goatty_shell::service::HostSnapshot;

use crate::server::Daemon;

fn snapshot_msg(req: ReqId, s: HostSnapshot) -> ServerMsg {
    ServerMsg::HostSettings { req, json: s.json, issues: s.issues, env_overrides: s.env_overrides, generation: s.generation }
}

fn error(req: ReqId, e: anyhow::Error) -> ServerMsg {
    ServerMsg::Error { req: Some(req), message: format!("{e:#}") }
}

pub(crate) fn get(daemon: &Daemon, req: ReqId) -> ServerMsg {
    snapshot_msg(req, daemon.host.snapshot())
}

/// On a host that follows the shared config, an applied change to a shared
/// setting becomes this host's override (#313): an older GUI, or one
/// editing only this host, keeps what it set.
pub(crate) fn set(daemon: &Daemon, req: ReqId, json: &str, apply: bool) -> ServerMsg {
    match daemon.host.set_captured(json, apply) {
        Ok(o) => ServerMsg::HostSettingsApplied {
            req,
            issues: o.issues,
            diff: o.diff.into_iter().map(|d| FileDiff { name: d.name, unified: d.unified }).collect(),
            applied: o.applied,
            generation: o.generation,
        },
        Err(e) => error(req, e),
    }
}

pub(crate) fn generations(daemon: &Daemon, req: ReqId) -> ServerMsg {
    let list = daemon
        .host
        .generations()
        .into_iter()
        .map(|g| GenerationInfo { number: g.number, created_unix: g.created_unix, active: g.active })
        .collect();
    ServerMsg::Generations { req, list }
}

pub(crate) fn rollback(daemon: &Daemon, req: ReqId, to: Option<u32>) -> ServerMsg {
    let before = daemon.host.settings().file.value;
    let mut result = daemon.host.rollback(to);
    if let Ok(snap) = &mut result
        && let Err(e) = daemon.host.capture(&before, &goatty_config::store::parse::<goatty_config::HostSettings>(&snap.json).value)
    {
        log::warn!("couldn't keep the rollback in the shared config: {e:#}");
        snap.issues.push(format!(
            "rolled back on this host, but couldn't keep it as this host's override in the shared config ({e:#}); the next shared update may undo it"
        ));
    }
    match result {
        Ok(s) => snapshot_msg(req, s),
        Err(e) => error(req, e),
    }
}

/// A plugin operation: JSON in, JSON out (`goatty_plugin::api`).
pub(crate) fn plugin(daemon: &Daemon, req: ReqId, request: &str) -> ServerMsg {
    let parsed: Result<goatty_plugin::api::Request, _> = serde_json::from_str(request);
    let result = match parsed {
        Ok(r) => daemon.host.plugin_request(r),
        Err(e) => Err(anyhow::anyhow!("unknown plugin request ({e}); is goattyd older than this goatty?")),
    };
    match result.and_then(|r| serde_json::to_string(&r).map_err(Into::into)) {
        Ok(reply) => ServerMsg::Plugin { req, reply },
        Err(e) => error(req, e),
    }
}

/// A shell setup or diagnostics operation: JSON in, JSON out (`goatty_shell::setup`).
pub(crate) fn shell(daemon: &Daemon, req: ReqId, request: &str) -> ServerMsg {
    let parsed: Result<goatty_shell::setup::Request, _> = serde_json::from_str(request);
    let result = match parsed {
        Ok(r) => daemon.host.shell_request(r),
        Err(e) => Err(anyhow::anyhow!("unknown shell request ({e}); is goattyd older than this goatty?")),
    };
    match result.and_then(|r| serde_json::to_string(&r).map_err(Into::into)) {
        Ok(reply) => ServerMsg::Shell { req, reply },
        Err(e) => error(req, e),
    }
}

/// A shared-config operation: JSON in, JSON out (`goatty_shell::shared::api`, v18).
pub(crate) fn shared(daemon: &Daemon, req: ReqId, request: &str) -> ServerMsg {
    let parsed: Result<goatty_shell::shared::api::Request, _> = serde_json::from_str(request);
    let result = match parsed {
        Ok(r) => daemon.host.shared_request(r),
        Err(e) => Err(anyhow::anyhow!("unknown shared-config request ({e}); is goattyd older than this goatty?")),
    };
    match result.and_then(|r| serde_json::to_string(&r).map_err(Into::into)) {
        Ok(reply) => ServerMsg::Shared { req, reply },
        Err(e) => error(req, e),
    }
}
