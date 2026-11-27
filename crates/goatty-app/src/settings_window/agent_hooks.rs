//! Coding agents' session hooks on the Shell page (#344, "After a
//! restart"): one row per agent set up on the host, with what installing
//! changes (the agent's config file) and a button. Only the button writes
//! the file (`goatty_shell::agent_hooks`, a copy is kept); a hook makes the
//! tab resume exactly that agent's conversation after a reboot.

use goatty_shell::agent_hooks::{Action, Agent, Hook, State, Status};
use gpui::{Context, SharedString};

use super::SettingsWindow;
use super::kit::{Row, row};
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, button};
use crate::ui::style::Ui;

#[derive(Default)]
pub struct HooksState {
    pub status: Option<Status>,
    loading: bool,
    /// The agent being changed right now.
    busy: Option<Agent>,
    /// Asking the host failed.
    pub error: Option<String>,
    /// Numbers requests: a reply for another host (or an older request) is dropped.
    request: u64,
}

impl HooksState {
    /// Forget what the host said (another host).
    pub fn reset(&mut self) {
        *self = Self { request: self.request + 1, ..Self::default() };
    }
}

impl SettingsWindow {
    /// Ask the host which agents are set up there and have goatty's hook.
    pub fn load_agent_hooks(&mut self, cx: &mut Context<'_, Self>) {
        self.agent_hooks_request(Action::Status, None, cx);
    }

    fn agent_hooks_request(&mut self, action: Action, agent: Option<Agent>, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        let h = &mut self.agent_hooks;
        if h.busy.is_some() || (action == Action::Status && h.loading) {
            return;
        }
        if action == Action::Status {
            h.loading = true;
        } else {
            h.busy = agent;
        }
        h.request += 1;
        let request = h.request;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let label = link.label();
            let result = cx.background_executor().spawn(async move { link.agent_hooks(action, agent) }).await;
            let _ = this.update(cx, |v, cx| {
                let h = &mut v.agent_hooks;
                if h.request != request {
                    return;
                }
                h.loading = false;
                h.busy = None;
                match result {
                    Ok(status) => {
                        if let Some(a) = agent {
                            let backup =
                                status.backup.as_ref().map(|b| format!(" goatty kept the file as it was in {b}.")).unwrap_or_default();
                            let file = status.hooks.iter().find(|x| x.agent == a).map(|x| x.file.clone()).unwrap_or_default();
                            let t = match action {
                                Action::Install => {
                                    Toast::success(format!("{} resumes its conversation after a reboot", a.name())).body(format!(
                                        "Added goatty's hook to {file} on {label}. It takes effect when {} starts next.{backup}",
                                        a.name()
                                    ))
                                }
                                _ => Toast::success(format!("Removed goatty's hook from {}", a.name()))
                                    .body(format!("{file} on {label}.{backup}")),
                            };
                            toast::show(cx, t.key("shell.agent-hooks"));
                        }
                        h.status = Some(status);
                        h.error = None;
                    }
                    Err(err) => {
                        let msg = format!("{err:#}");
                        // A goattyd from before #344 doesn't know the request.
                        let msg = if msg.contains("unknown variant") {
                            format!("goattyd on {label} is too old for agent hooks; update it")
                        } else {
                            msg
                        };
                        if agent.is_some() {
                            toast::show(cx, Toast::error("Couldn't change the agent's hook").body(msg.clone()).key("shell.agent-hooks"));
                        }
                        h.error = Some(msg);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The rows under "After a restart": one per agent set up on the host.
    pub(super) fn agent_hook_rows(&self, ui: Ui, on: bool, cx: &Context<'_, Self>) -> Vec<Row> {
        let h = &self.agent_hooks;
        let Some(status) = &h.status else {
            let desc = match (&h.error, h.loading) {
                (Some(e), _) => e.clone(),
                (None, true) => "Looking for coding agents on this host…".into(),
                (None, false) => "Open this page again to look for coding agents".into(),
            };
            return vec![row(ui, "Agent hooks").desc(desc).disabled(true)];
        };
        let set_up: Vec<&Hook> = status.hooks.iter().filter(|x| x.available || x.state != State::Off).collect();
        if set_up.is_empty() {
            return vec![row(ui, "Agent hooks").desc("No coding agent is set up on this host").disabled(true)];
        }
        set_up
            .into_iter()
            .map(|x| {
                let busy = h.busy == Some(x.agent);
                let desc: SharedString = if let Some(p) = &x.problem {
                    p.clone().into()
                } else {
                    let what = match x.state {
                        State::Off => format!("Without a hook, goatty guesses the conversation · adds one to {}", x.file),
                        State::On => format!("Resumes exactly its conversation · hook in {}", x.file),
                        State::Outdated => format!("The hook in {} runs another goattyd · update it", x.file),
                    };
                    match &x.note {
                        Some(n) if x.state != State::On => format!("{what} · {n}").into(),
                        _ => what.into(),
                    }
                };
                let (label, kind, action) = match x.state {
                    State::Off => ("Install hook", ButtonKind::Normal, Action::Install),
                    State::Outdated => ("Update hook", ButtonKind::Normal, Action::Install),
                    State::On => ("Remove", ButtonKind::Ghost, Action::Remove),
                };
                let agent = x.agent;
                let this = cx.entity().downgrade();
                let id = format!("agent-hook-{}", agent.id());
                let disabled = !on || x.problem.is_some() || (status.unavailable.is_some() && action == Action::Install);
                let control = button(SharedString::from(id), if busy { "Working…" } else { label }, kind, ui, move |_, cx| {
                    if !disabled {
                        let _ = this.update(cx, |v, cx| v.agent_hooks_request(action, Some(agent), cx));
                    }
                });
                row(ui, format!("{} hook", x.name)).desc(desc).disabled(disabled).control(control)
            })
            .collect()
    }
}
