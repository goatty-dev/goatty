//! Where host settings live: the connected goattyd (local or remote), or,
//! with `--local`, this process. All calls block; run them off the UI thread.

use std::sync::Arc;

use anyhow::{Result, bail};
use goatty_client::Connection;
use goatty_shell::HostService;
use goatty_shell::{history, setup};
use gpui::Global;

#[derive(Clone)]
pub enum HostLink {
    Daemon(Arc<Connection>),
    Local(Arc<HostService>),
}

/// The settings window's host link.
pub struct HostLinkGlobal(pub Option<HostLink>);

impl Global for HostLinkGlobal {}

pub struct Snapshot {
    pub json: String,
    pub issues: Vec<String>,
    pub env_overrides: Vec<String>,
    pub generation: Option<u32>,
}

pub struct Applied {
    pub issues: Vec<String>,
    pub diff: Vec<(String, String)>,
    pub applied: bool,
    pub generation: Option<u32>,
}

/// A generation as the Profile page lists it.
#[derive(Debug, Clone)]
pub struct GenRow {
    pub entry: history::Entry,
    /// The host can name generations and show their file changes (a
    /// goattyd from before #55 only lists them).
    pub rich: bool,
}

impl HostLink {
    /// The same connection (or the same local host).
    pub fn same(&self, other: &HostLink) -> bool {
        match (self, other) {
            (HostLink::Daemon(a), HostLink::Daemon(b)) => Arc::ptr_eq(a, b),
            (HostLink::Local(a), HostLink::Local(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// The link is up (`--local` always is).
    pub fn is_connected(&self) -> bool {
        match self {
            HostLink::Daemon(c) => c.is_connected(),
            HostLink::Local(_) => true,
        }
    }

    /// The host is this computer (its files are ours to show).
    pub fn is_local(&self) -> bool {
        match self {
            HostLink::Daemon(c) => c.endpoint().is_local(),
            HostLink::Local(_) => true,
        }
    }

    /// The underlying SSH daemon connection, if this link reaches a host over SSH.
    pub fn ssh_connection(&self) -> Option<Arc<Connection>> {
        match self {
            HostLink::Daemon(c) if matches!(c.endpoint(), goatty_client::Endpoint::Ssh(_)) => Some(c.clone()),
            _ => None,
        }
    }

    /// Another host's name as goatty reaches it (the ssh destination, or
    /// the `--via` command's label): what per-host choices and the shell
    /// sync state are keyed by. `None` for this computer.
    pub fn remote_key(&self) -> Option<String> {
        match self {
            HostLink::Daemon(c) if !c.endpoint().is_local() => Some(c.endpoint().label().to_owned()),
            _ => None,
        }
    }

    /// "this computer" or `user@host`.
    pub fn label(&self) -> String {
        match self {
            HostLink::Daemon(c) if !c.endpoint().is_local() => {
                let h = c.host();
                format!("{}@{}", h.user, h.hostname)
            }
            _ => "this computer".into(),
        }
    }

    pub fn get(&self) -> Result<Snapshot> {
        Ok(match self {
            HostLink::Daemon(c) => {
                let r = c.host_settings()?;
                Snapshot { json: r.json, issues: r.issues, env_overrides: r.env_overrides, generation: r.generation }
            }
            HostLink::Local(s) => {
                let r = s.snapshot();
                Snapshot { json: r.json, issues: r.issues, env_overrides: r.env_overrides, generation: r.generation }
            }
        })
    }

    pub fn set(&self, json: String, apply: bool) -> Result<Applied> {
        Ok(match self {
            HostLink::Daemon(c) => {
                let r = c.set_host_settings(json, apply)?;
                Applied {
                    issues: r.issues,
                    diff: r.diff.into_iter().map(|d| (d.name, d.unified)).collect(),
                    applied: r.applied,
                    generation: r.generation,
                }
            }
            HostLink::Local(s) => {
                let r = s.set(&json, apply)?;
                Applied {
                    issues: r.issues,
                    diff: r.diff.into_iter().map(|d| (d.name, d.unified)).collect(),
                    applied: r.applied,
                    generation: r.generation,
                }
            }
        })
    }

    /// The generations with names and what each changed; only numbers and
    /// ages from an older goattyd.
    pub fn generations(&self) -> Result<Vec<GenRow>> {
        if let Ok(setup::Reply::Generations { list }) = self.shell(setup::Request::Generations) {
            return Ok(list.into_iter().map(|entry| GenRow { entry, rich: true }).collect());
        }
        let HostLink::Daemon(c) = self else { bail!("no generations") };
        Ok(c.generations()?
            .into_iter()
            .map(|g| GenRow {
                entry: history::Entry { number: g.number, created_unix: g.created_unix, active: g.active, name: None, changes: None },
                rich: false,
            })
            .collect())
    }

    /// Name generation `number` (`None`: remove the name).
    pub fn name_generation(&self, number: u32, name: Option<String>) -> Result<()> {
        match self.shell(setup::Request::NameGeneration { number, name })? {
            setup::Reply::Done => Ok(()),
            other => bail!("unexpected reply from the host: {other:?}"),
        }
    }

    /// The generated files generation `number` changed: (file name, unified diff).
    pub fn generation_diff(&self, number: u32) -> Result<Vec<(String, String)>> {
        match self.shell(setup::Request::GenerationDiff { number })? {
            setup::Reply::Diff { files } => Ok(files.into_iter().map(|d| (d.name, d.unified)).collect()),
            other => bail!("unexpected reply from the host: {other:?}"),
        }
    }

    pub fn rollback(&self, to: Option<u32>) -> Result<Snapshot> {
        Ok(match self {
            HostLink::Daemon(c) => {
                let r = c.rollback(to)?;
                Snapshot { json: r.json, issues: r.issues, env_overrides: r.env_overrides, generation: r.generation }
            }
            HostLink::Local(s) => {
                let r = s.rollback(to)?;
                Snapshot { json: r.json, issues: r.issues, env_overrides: r.env_overrides, generation: r.generation }
            }
        })
    }

    /// A plugin operation on the host (downloads for installs: can take a while).
    pub fn plugin(&self, req: goatty_plugin::api::Request) -> Result<goatty_plugin::api::Reply> {
        match self {
            HostLink::Daemon(c) => Ok(serde_json::from_str(&c.plugin(serde_json::to_string(&req)?)?)?),
            HostLink::Local(s) => s.plugin_request(req),
        }
    }

    /// A shell setup or diagnostics operation on the host (the status
    /// starts a shell, installs download: can take a while).
    pub fn shell(&self, req: setup::Request) -> Result<setup::Reply> {
        match self {
            HostLink::Daemon(c) => Ok(serde_json::from_str(&c.shell(serde_json::to_string(&req)?)?)?),
            HostLink::Local(s) => s.shell_request(req),
        }
    }

    /// The host's shell diagnostics (starts a shell to ask it).
    pub fn shell_status(&self) -> Result<setup::Status> {
        match self.shell(setup::Request::Status)? {
            setup::Reply::Status(s) => Ok(*s),
            other => bail!("unexpected reply from the host: {other:?}"),
        }
    }

    /// The PATH a new shell on the host gets, and where each folder came
    /// from (starts a shell to ask it).
    pub fn path_report(&self) -> Result<goatty_shell::path_report::Report> {
        match self.shell(setup::Request::Path)? {
            setup::Reply::Path(r) => Ok(*r),
            other => bail!("unexpected reply from the host: {other:?}"),
        }
    }

    /// Ask the host's running managed shells to reload. Returns how many were asked.
    /// What the host imports from the user's own shell; `refresh` runs
    /// their shell there first (slow: off the UI thread).
    pub fn import_status(&self, refresh: bool) -> Result<goatty_shell::import::Status> {
        match self.shell(setup::Request::Import { refresh })? {
            setup::Reply::Import(s) => Ok(*s),
            other => bail!("unexpected reply from the host: {other:?}"),
        }
    }

    /// goatty's shell in every terminal on the host: the line in its
    /// `.zshenv`, after `action` (`on` writes the file there).
    /// `shown`: the files the user agreed to (nothing is written when
    /// they're others now).
    pub fn everywhere(
        &self,
        action: goatty_shell::everywhere::Action,
        shown: Option<Vec<String>>,
    ) -> Result<goatty_shell::everywhere::Status> {
        match self.shell(setup::Request::Everywhere { action, shown })? {
            setup::Reply::Everywhere(s) => Ok(*s),
            other => bail!("unexpected reply from the host: {other:?}"),
        }
    }

    /// A tailnet operation on the host (its availability, pairing, paired
    /// devices). Runs `tailscale` there: off the UI thread.
    pub fn net(&self, req: &goatty_net::api::Request) -> Result<goatty_net::api::NetStatus> {
        match self {
            HostLink::Daemon(c) => serde_json::from_str::<goatty_net::api::Reply>(&c.net(serde_json::to_string(req)?)?)?.into_result(),
            HostLink::Local(_) => bail!("the tailnet needs goattyd: this window runs its shell itself (--local)"),
        }
    }

    pub fn reload_shells(&self) -> Result<u32> {
        Ok(match self {
            HostLink::Daemon(c) => c.reload_shells()?,
            HostLink::Local(s) => u32::try_from(s.reload_shells()?).unwrap_or(u32::MAX),
        })
    }
}

impl goatty_shell::sync::Remote for HostLink {
    fn plugin(&self, req: goatty_plugin::api::Request) -> Result<goatty_plugin::api::Reply> {
        HostLink::plugin(self, req)
    }

    fn shell(&self, req: setup::Request) -> Result<setup::Reply> {
        HostLink::shell(self, req)
    }

    fn settings_json(&self) -> Result<String> {
        Ok(self.get()?.json)
    }

    fn apply(&self, json: String) -> Result<bool> {
        let applied = self.set(json, true)?;
        for issue in &applied.issues {
            log::info!("host settings after the sync: {issue}");
        }
        Ok(applied.applied)
    }
}

impl goatty_shell::shared::Peer for HostLink {
    fn label(&self) -> String {
        HostLink::label(self)
    }

    fn shared(&self, req: goatty_shell::shared::api::Request) -> Result<goatty_shell::shared::api::Reply> {
        match self {
            HostLink::Daemon(c) => Ok(serde_json::from_str(&c.shared(serde_json::to_string(&req)?)?)?),
            HostLink::Local(s) => s.shared_request(req),
        }
    }
}

impl HostLink {
    /// The host takes part in the shared shell config (#313): a goattyd
    /// with protocol v18, or this process with `--local`.
    pub fn shares_config(&self) -> bool {
        match self {
            HostLink::Daemon(c) => c.shares_config(),
            HostLink::Local(_) => true,
        }
    }
}
