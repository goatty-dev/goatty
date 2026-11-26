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
        if self.draft.is_some()
            || self.applying
            || self.plugins.follow.is_some()
            || self.plugins.batch.is_some()
            || self.plugins.is_loading()
        {
            let why = if self.draft.is_some() {
                "Apply or discard the changes first, then pick other hosts."
            } else {
                "Wait until the running change is done, then pick other hosts."
            };
            self.outcome = Some(Outcome { ok: false, lines: vec![why.into()], diff: Vec::new(), reload: false });
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
        } else if let (Some(Scope::Host(id)), Some(state)) = (&self.scope, shared_sync::state(cx)) {
            // A host that keeps its own settings is only reachable through a tab on it.
            self.host = None;
            self.host_error = Some(format!("{} keeps its own settings. Open a tab on it to change them there.", state.label(id)));
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
            // Only a host that follows has its settings in the shared config.
            Scope::Host(id) if shared_sync::state(cx)?.member(id) == Some(true) => {
                shared_sync::resolve(Some(id), &HostSettings::default(), cx)?
            }
            Scope::Host(_) => return None,
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
                    let reset: NoteAction = Rc::new(move |window, cx| {
                        let _ = this.update(cx, |v, cx| {
                            // The link goes away with the override: keep the user's place on the page.
                            crate::ui::focus::hand_over(&v.focus, crate::ui::focus::Handover::FirstOnPage, window, cx);
                            v.use_shared_value(&id, keys, cx);
                        });
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
        if self.draft.is_some() || self.applying {
            self.outcome = Some(Outcome {
                ok: false,
                lines: vec!["Apply or discard the changes first, then use the shared value.".into()],
                diff: Vec::new(),
                reload: false,
            });
            cx.notify();
            return;
        }
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
                let patch = machine_patch(&base, &draft);
                let machine = !patch.is_empty() && link_id.as_deref() == Some(id.as_str());
                (
                    shared_sync::override_edit(id, &base, &draft, cx),
                    machine.then_some(patch),
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
    fn publish_entries(&mut self, entries: Vec<model::Entry>, done: String, machine: Option<MachinePatch>, cx: &mut Context<'_, Self>) {
        self.busy = true;
        self.applying = true;
        let link = self.link.clone();
        let this = cx.entity().downgrade();
        shared_sync::publish(entries, cx, move |result, cx| {
            let machine = match (machine, link) {
                (Some(patch), Some(link)) => Some((patch, link)),
                _ => None,
            };
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                view.applying = false;
                let mut lines = Vec::new();
                let ok = match &result {
                    Ok(o) if !o.saved_anywhere() => {
                        // Not stored anywhere: keep the draft to try again.
                        lines.push("No goatty host stored the change; it's still here to apply again.".into());
                        lines.extend(o.x.errors.iter().map(|(i, e)| format!("⚠ {}: {e}", o.links[*i].label())));
                        false
                    }
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
                if let (true, Some((patch, link))) = (ok, machine) {
                    view.busy = true;
                    view.applying = true;
                    let retry = patch.clone();
                    cx.spawn(async move |this, cx| {
                        // Only the machine values changed here, onto what the host has now.
                        let r = cx
                            .background_executor()
                            .spawn(async move {
                                let current = store::parse::<HostSettings>(&link.get()?.json);
                                let mut flat = model::flatten(&model::sparse(&current.value));
                                for (p, v) in patch {
                                    match v {
                                        Some(v) => flat.insert(p, v),
                                        None => flat.remove(&p),
                                    };
                                }
                                let next = store::parse::<HostSettings>(&model::unflatten(&flat).to_string()).value;
                                link.set(store::to_json(&next, &current.unknown), true)
                            })
                            .await;
                        let _ = this.update(cx, |view, cx| {
                            view.busy = false;
                            view.applying = false;
                            if r.is_err() {
                                view.pending_machine = Some(retry);
                            }
                            view.reload_scope(cx);
                            if let Err(e) = r
                                && let Some(o) = view.outcome.as_mut()
                            {
                                o.ok = false;
                                o.lines
                                    .push(format!("⚠ This host's own settings weren't applied: {e:#}. They're still here to apply again."));
                            }
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

/// Machine-only values a draft changed: path → new value (`None`: back to its default).
type MachinePatch = Vec<(String, Option<serde_json::Value>)>;

fn machine_patch(a: &HostSettings, b: &HostSettings) -> MachinePatch {
    let only = |h: &HostSettings| -> std::collections::BTreeMap<String, serde_json::Value> {
        model::flatten(&model::sparse(h)).into_iter().filter(|(p, _)| HostSettings::scope(p) == SettingScope::Machine).collect()
    };
    let (a, b) = (only(a), only(b));
    let mut out: MachinePatch = b.iter().filter(|(p, v)| a.get(*p) != Some(v)).map(|(p, v)| (p.clone(), Some(v.clone()))).collect();
    out.extend(a.keys().filter(|p| !b.contains_key(*p)).map(|p| (p.clone(), None)));
    out
}

impl SettingsWindow {
    /// Where the bar's unapplied changes go: "All synced hosts", a host, or
    /// the active tab's host.
    pub(super) fn draft_target(&self, cx: &App) -> String {
        match (&self.scope, shared_sync::state(cx)) {
            (Some(Scope::All), _) => "all synced hosts".into(),
            (Some(Scope::Host(id)), Some(state)) if !self.scope_is_link(cx) => state.label(id),
            _ => self.host_label(),
        }
    }

    /// A preview builds on the active tab's host: only there does it show
    /// what Apply writes.
    pub(super) fn preview_unavailable(&self, cx: &App) -> Option<String> {
        (!self.scope_is_link(cx)).then(|| {
            "Preview builds the shell configuration on the active tab's host. Pick that host in the picker above to preview, or Apply \
             directly."
                .to_string()
        })
    }

    /// Plugin operations reach the active tab's host only.
    pub(super) fn plugins_unavailable(&self, cx: &App) -> Option<String> {
        match (&self.scope, shared_sync::state(cx)) {
            (Some(Scope::Host(id)), Some(state)) if !self.scope_is_link(cx) => {
                Some(format!("Plugins are installed on one host at a time: open a tab on {} to change its plugins.", state.label(id)))
            }
            _ => None,
        }
    }
}
