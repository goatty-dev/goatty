//! What panes have to tell (#205): a coding agent waiting for the user,
//! done or failed, and programs' own notifications (`agent::notify`).
//!
//! Each notice goes out as an in-app toast and as a desktop notification
//! when `notifications.in_app` / `.system` say so for where the pane is
//! (on screen, in a background tab, in a window that isn't focused). A
//! pane has one toast at a time: a new notice replaces it in place, it
//! goes when the agent moves on (back to work, cleared) and when the user
//! looks at the pane. A program notification right after its agent's own
//! (agents that send both) is the same news and is dropped, and desktop
//! notifications from one pane are spaced out.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{Context, Window};

use super::Workspace;
use super::layout::PaneId;
use crate::agent::notify::{Notice, applies};
use crate::agent::{self, StatusKey};
use crate::settings::AppSettings;
use crate::toast::{self, ToastId};

/// A program notification this soon after the pane's agent told something
/// repeats it.
const ECHO: Duration = Duration::from_secs(5);
/// The least time between two desktop notifications from one pane.
const SYSTEM_GAP: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct Notices {
    /// The toast each pane shows, and the agent status it's about.
    toasts: HashMap<PaneId, (ToastId, Option<StatusKey>)>,
    /// When each pane's agent last told something.
    agent_at: HashMap<PaneId, Instant>,
    /// When each pane last went to the desktop.
    system_at: HashMap<PaneId, Instant>,
}

impl Workspace {
    /// Tell `notice` from `pane`, as the settings want it.
    pub(super) fn notice(&mut self, pane: PaneId, notice: Notice, window: &mut Window, cx: &mut Context<'_, Self>) {
        let settings = cx.global::<AppSettings>().client.notifications.clone();
        if !notice.wanted(&settings) {
            return;
        }
        let now = Instant::now();
        if notice.is_agent() {
            self.notices.agent_at.insert(pane, now);
        } else if self.notices.agent_at.get(&pane).is_some_and(|at| now.duration_since(*at) < ECHO) {
            return;
        }
        let Some((index, _)) = self.find_pane(pane) else { return };
        let tab = &self.tabs[index];
        let visible = index == self.active && tab.shows(pane);
        let active = window.is_window_active();
        let place = self.tab_label(tab, cx);
        if applies(settings.in_app, active, visible) {
            let workspace = cx.entity().downgrade();
            let toast = notice.toast(&place, format!("pane-notice.{pane}"), move |window, cx| {
                let _ = workspace.update(cx, |this, cx| this.show_pane(pane, window, cx));
            });
            let id = toast::show_in(window, cx, toast);
            let about = match &notice {
                Notice::Agent(a) => Some(a.key()),
                Notice::Program { .. } => None,
            };
            self.notices.toasts.insert(pane, (id, about));
        }
        let spaced = self.notices.system_at.get(&pane).is_none_or(|at| now.duration_since(*at) >= SYSTEM_GAP);
        if applies(settings.system, active, visible) && spaced {
            self.notices.system_at.insert(pane, now);
            cx.show_system_notification(notice.system(&place, window.window_handle().window_id().as_u64(), pane));
        }
    }

    /// `pane`'s program status changed: a toast about a state its agent
    /// left goes, and the tabs redraw.
    pub(super) fn pane_status_changed(&mut self, pane: PaneId, cx: &mut Context<'_, Self>) {
        if let Some((id, Some(about))) = self.notices.toasts.get(&pane) {
            let now = self.find_pane(pane).and_then(|(_, p)| {
                let view = p.view.read(cx);
                agent::notable(view.program_status(), view.process()).map(|s| s.key())
            });
            if now.as_ref() != Some(about) {
                toast::dismiss(cx, *id);
                self.notices.toasts.remove(&pane);
            }
        }
        cx.notify();
    }

    /// The user looks at the active tab: its panes' toasts have been seen.
    pub(super) fn notices_seen(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if !window.is_window_active() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        // A pane hidden behind a zoomed one isn't seen.
        let panes: Vec<PaneId> = tab.panes.iter().map(|p| p.id).filter(|&id| tab.shows(id)).collect();
        for pane in panes {
            if let Some((id, _)) = self.notices.toasts.remove(&pane) {
                toast::dismiss(cx, id);
            }
        }
    }

    /// `pane` is gone, and so is its toast (a waiting agent's stays up
    /// otherwise, its "Show" pointing nowhere).
    pub(super) fn forget_notices(&mut self, pane: PaneId, cx: &mut Context<'_, Self>) {
        if let Some((id, _)) = self.notices.toasts.remove(&pane) {
            toast::dismiss(cx, id);
        }
        self.notices.agent_at.remove(&pane);
        self.notices.system_at.remove(&pane);
    }

    /// Bring `pane` forward: its tab, focused (a notification's "Show").
    pub fn show_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((index, _)) = self.find_pane(pane) else { return };
        let tab = &mut self.tabs[index];
        if tab.focused != pane {
            tab.focused = pane;
            tab.zoomed = false;
        }
        self.activate(index, window, cx);
    }
}
