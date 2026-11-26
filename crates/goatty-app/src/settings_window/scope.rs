//! Which hosts the shell pages edit (#313): the scope picker in the page
//! header. **All synced hosts** edits the shared config; a host shows the
//! values it actually uses, and changing one there makes an override for
//! that host only. Hosts that don't follow the shared config are edited on
//! their own, as before. Machine-only settings (`HostSettings::MACHINE`)
//! are edited per host.

use std::collections::HashMap;
use std::rc::Rc;

use goatty_config::shared::{self as model, Scope as SettingScope};
use goatty_config::{HostSettings, Loaded, store};
use gpui::{AnyElement, App, Context, SharedString};

use super::kit::{NoteAction, RowNote, ScopeNote, opt};
use super::search::INDEX;
use super::{HostView, Outcome, Page, SettingsWindow};
use crate::shared_sync;

/// What the shell pages edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The shared config: every host that follows it.
    All,
    /// One host, by id.
    Host(String),
}

const ALL: &str = "all";

impl SettingsWindow {
    /// The active tab's host in the shared config, once an exchange met it.
    pub(super) fn link_host_id(&self, cx: &App) -> Option<String> {
        let link = self.link.as_ref()?;
        if !link.shares_config() {
            return None;
        }
        shared_sync::state(cx)?.id_of(link)
    }

    /// The pages show and edit the active tab's host's own file (no shared
    /// config there, or that host is the scope).
    pub(super) fn scope_is_link(&self, cx: &App) -> bool {
        match &self.scope {
            None => true,
            Some(Scope::All) => false,
            Some(Scope::Host(id)) => self.link_host_id(cx).as_deref() == Some(id.as_str()),
        }
    }

    /// Choose the scope the first time the shared config is known: the
    /// active tab's host when it doesn't follow it, else All synced hosts
    /// (a toast about one host opens on it: [`super::open_for_host`]).
    pub(super) fn pick_scope(&mut self, cx: &mut Context<'_, Self>) {
        if self.scope.is_some() || self.draft.is_some() {
            return;
        }
        let Some(link_id) = self.link_host_id(cx) else { return };
        let Some(state) = shared_sync::state(cx) else { return };
        let follows = state.member(&link_id) == Some(true);
        self.scope = Some(if follows { Scope::All } else { Scope::Host(link_id) });
        self.reload_scope(cx);
    }

    /// Switch the shell pages to `scope` (refused while changes are unapplied).
    pub(crate) fn set_scope(&mut self, scope: Scope, cx: &mut Context<'_, Self>) {
        if self.scope.as_ref() == Some(&scope) {
            return;
        }
        if self.draft.is_some() {
            self.outcome = Some(Outcome {
                ok: false,
                lines: vec!["Apply or discard the changes first, then pick other hosts.".into()],
                diff: Vec::new(),
                reload: false,
            });
            cx.notify();
            return;
        }
        self.scope = Some(scope);
        self.outcome = None;
        self.reload_scope(cx);
    }

    /// Show the settings of the scope.
    pub(super) fn reload_scope(&mut self, cx: &mut Context<'_, Self>) {
        if self.scope_is_link(cx) {
            self.load_host(cx);
        } else if let Some(view) = self.scoped_view(cx) {
            self.host = Some(view);
            self.host_error = None;
            self.sync_prompt_inputs(cx);
        }
        cx.notify();
    }

    /// The settings of a scope that isn't the active tab's host: resolved
    /// from the shared config (machine-only values from the active tab's
    /// host, which those pages don't edit).
    pub(super) fn scoped_view(&self, cx: &App) -> Option<HostView> {
        let machine = self.link_settings.clone().unwrap_or_default();
        let value = match self.scope.as_ref()? {
            Scope::All => shared_sync::resolve(None, &machine, cx)?,
            Scope::Host(id) => shared_sync::resolve(Some(id), &HostSettings::default(), cx)?,
        };
        Some(HostView {
            loaded: Loaded { value, issues: Vec::new(), unknown: Vec::new() },
            issues: Vec::new(),
            env_overrides: Vec::new(),
            generation: None,
        })
    }

    /// The scope picker for the page header (`None`: no shared config here).
    pub(super) fn scope_picker(&self, cx: &Context<'_, Self>) -> Option<AnyElement> {
        let current = match self.scope.as_ref()? {
            Scope::All => ALL.to_owned(),
            Scope::Host(id) => id.clone(),
        };
        let state = shared_sync::state(cx)?;
        let mut options = vec![opt(ALL.to_owned(), "All synced hosts").desc("The shared config: every host that follows it")];
        for id in state.listed() {
            let desc = match state.member(&id) {
                Some(true) => format!("Follows the shared config · {}", state.standing(&id).label()),
                _ => format!("Keeps its own settings · {}", state.standing(&id).label()),
            };
            options.push(opt(id.clone(), state.label(&id)).desc(desc));
        }
        let this = cx.entity().downgrade();
        Some(self.select("settings-scope", "Settings for", options, &current, cx, move |v: String, _, cx| {
            let scope = if v == ALL { Scope::All } else { Scope::Host(v) };
            let _ = this.update(cx, |view, cx| view.set_scope(scope, cx));
        }))
    }

    /// What the scope adds to each row of `page`: who overrides it, or that
    /// this host overrides it (with "Use shared value"), or that it is
    /// machine-only.
    pub(super) fn scope_notes(&self, page: Page, notes: &mut HashMap<SharedString, RowNote>, cx: &Context<'_, Self>) {
        let Some(scope) = self.scope.clone() else { return };
        let Some(state) = shared_sync::state(cx) else { return };
        let link_id = self.link_host_id(cx);
        for e in INDEX.iter().filter(|e| e.page == page && !e.keys.is_empty()) {
            let machine = e.keys.iter().all(|k| HostSettings::scope(k) == SettingScope::Machine);
            let note = match &scope {
                Scope::All if machine => Some(ScopeNote {
                    text: "This host only: pick a host above to change it there".into(),
                    accent: false,
                    action: None,
                    disable: true,
                }),
                Scope::All => {
                    let hosts = shared_sync::overridden_on(e.keys, cx);
                    (!hosts.is_empty()).then(|| {
                        let names: Vec<String> = hosts.iter().map(|h| state.label(h)).collect();
                        let first = hosts[0].clone();
                        let this = cx.entity().downgrade();
                        let go: NoteAction = Rc::new(move |_, cx| {
                            let _ = this.update(cx, |v, cx| v.set_scope(Scope::Host(first.clone()), cx));
                        });
                        ScopeNote {
                            text: format!("Overridden on {}", names.join(", ")).into(),
                            accent: false,
                            action: Some((format!("Show {}", names[0]).into(), go)),
                            disable: false,
                        }
                    })
                }
                Scope::Host(id) if machine => {
                    // Only the active tab's host's own file is reachable from here.
                    let reachable = link_id.as_deref() == Some(id.as_str());
                    Some(ScopeNote {
                        text: if reachable { "This host only".into() } else { "This host only: open a tab there to change it".into() },
                        accent: false,
                        action: None,
                        disable: !reachable,
                    })
                }
                Scope::Host(id) if state.member(id) == Some(true) && shared_sync::overrides_any(id, e.keys, cx) => {
                    let (id, keys, label) = (id.clone(), e.keys, state.label(id));
                    let this = cx.entity().downgrade();
                    let reset: NoteAction = Rc::new(move |_, cx| {
                        let _ = this.update(cx, |v, cx| v.use_shared_value(&id, keys, cx));
                    });
                    Some(ScopeNote {
                        text: format!("Only on {label}").into(),
                        accent: true,
                        action: Some(("Use shared value".into(), reset)),
                        disable: false,
                    })
                }
                Scope::Host(_) => None,
            };
            if let Some(n) = note {
                notes.entry(SharedString::from(e.label)).or_default().scope = Some(n);
            }
        }
    }

    /// "Use shared value": drop this host's overrides of `keys`.
    fn use_shared_value(&mut self, host: &str, keys: &[&str], cx: &mut Context<'_, Self>) {
        match shared_sync::reset_overrides(host, keys, cx) {
            Ok(entries) => self.publish_entries(entries, "Uses the shared value again.".into(), None, cx),
            Err(e) => {
                self.outcome = Some(Outcome { ok: false, lines: vec![format!("{e:#}")], diff: Vec::new(), reload: false });
                cx.notify();
            }
        }
    }

    /// Apply the draft through the shared config when the scope asks for
    /// that. Returns `false` when the draft goes to the active tab's host
    /// file as before (a host that doesn't follow).
    pub(super) fn apply_scoped(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let Some(scope) = self.scope.clone() else { return false };
        let (Some(base), Some(draft)) = (self.host.as_ref().map(|h| h.loaded.value.clone()), self.draft.clone()) else { return false };
        let Some(state) = shared_sync::state(cx) else { return false };
        let link_id = self.link_host_id(cx);
        let (entries, machine_json, done) = match &scope {
            Scope::All => (
                shared_sync::shared_edit(&base, &draft, cx),
                None,
                "Shared with every host that follows the shared config. Hosts not connected now get it when they next connect.".to_string(),
            ),
            Scope::Host(id) => {
                let label = state.label(id);
                if state.member(id) != Some(true) {
                    if link_id.as_deref() == Some(id.as_str()) {
                        return false;
                    }
                    self.outcome = Some(Outcome {
                        ok: false,
                        lines: vec![format!("{label} keeps its own settings: open a tab there to change them.")],
                        diff: Vec::new(),
                        reload: false,
                    });
                    cx.notify();
                    return true;
                }
                // Machine-only values go to that host's own file (when it's this tab's).
                let machine = machine_changed(&base, &draft) && link_id.as_deref() == Some(id.as_str());
                let unknown = self.host.as_ref().map(|h| h.loaded.unknown.clone()).unwrap_or_default();
                (
                    shared_sync::override_edit(id, &base, &draft, cx),
                    machine.then(|| store::to_json(&draft, &unknown)),
                    format!("Set on {label} only. Other hosts keep the shared values."),
                )
            }
        };
        match entries {
            Ok(entries) => self.publish_entries(entries, done, machine_json, cx),
            Err(e) => self.outcome = Some(Outcome { ok: false, lines: vec![format!("{e:#}")], diff: Vec::new(), reload: false }),
        }
        cx.notify();
        true
    }

    /// Spread signed entries, then (optionally) apply machine-only values
    /// on the active tab's host, and show the scope again.
    fn publish_entries(&mut self, entries: Vec<model::Entry>, done: String, machine_json: Option<String>, cx: &mut Context<'_, Self>) {
        self.busy = true;
        let link = self.link.clone();
        let this = cx.entity().downgrade();
        shared_sync::publish(entries, cx, move |result, cx| {
            let machine = match (machine_json, link) {
                (Some(json), Some(link)) => Some((json, link)),
                _ => None,
            };
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                let mut lines = Vec::new();
                let ok = match &result {
                    Ok(o) => {
                        lines.push(done);
                        let failed: Vec<String> =
                            o.x.statuses
                                .iter()
                                .flatten()
                                .filter_map(|s| s.last_apply.as_ref())
                                .filter(|a| !a.ok)
                                .flat_map(|a| a.lines.clone())
                                .collect();
                        lines.extend(failed.into_iter().map(|l| format!("⚠ {l}")));
                        lines.extend(o.x.errors.iter().map(|(i, e)| format!("⚠ {}: {e}", o.links[*i].label())));
                        true
                    }
                    Err(e) => {
                        lines.push(format!("{e:#}"));
                        false
                    }
                };
                view.outcome = Some(Outcome { ok, lines, diff: Vec::new(), reload: false });
                if ok {
                    view.draft = None;
                }
                if let Some((json, link)) = machine {
                    view.busy = true;
                    cx.spawn(async move |this, cx| {
                        let r = cx.background_executor().spawn(async move { link.set(json, true) }).await;
                        let _ = this.update(cx, |view, cx| {
                            view.busy = false;
                            if let Err(e) = r
                                && let Some(o) = view.outcome.as_mut()
                            {
                                o.lines.push(format!("⚠ This host's own settings weren't applied: {e:#}"));
                            }
                            view.reload_scope(cx);
                        });
                    })
                    .detach();
                } else {
                    view.reload_scope(cx);
                }
                crate::plugin_commands::refresh(cx);
                crate::snippets::refresh(cx);
                cx.notify();
            });
        });
    }
}

/// A machine-only value differs.
fn machine_changed(a: &HostSettings, b: &HostSettings) -> bool {
    let only = |h: &HostSettings| {
        model::flatten(&model::sparse(h)).into_iter().filter(|(p, _)| HostSettings::scope(p) == SettingScope::Machine).collect::<Vec<_>>()
    };
    only(a) != only(b)
}
