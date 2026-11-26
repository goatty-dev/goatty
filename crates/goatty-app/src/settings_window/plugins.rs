//! The settings window's Plugins page (Stage 5): the installed plugins as
//! a list, each with its own page (settings from its schema, grouped as
//! the manifest groups them; its tools, permissions and aliases), what the
//! registries offer, and the install review.
//!
//! Updates (#244) take one click: the page asks the host for the update's
//! plan and installs it right away when it does nothing the installed
//! version couldn't ([`goatty_plugin::changes`]); otherwise the review
//! opens, showing only what changes. "Update all" queues every update
//! ([`Batch`]) and collects the ones that need a look for the end.
//!
//! Installing and removing are host operations (`goatty_plugin::api`
//! through the daemon, or in-process with `--local`). A plugin's settings
//! are host settings (`plugins.<id>` in `host.json`): they're edited in the
//! same draft as the Shell page and applied with Preview/Apply.

use std::collections::{HashMap, VecDeque};

use goatty_config::{HostSettings, PluginConfig};
use goatty_plugin::api::{self, Catalog, Installed, PackageState, Plan};
use goatty_plugin::manifest::{Category, Setting, SettingKind};
use goatty_plugin::{Permission, fetch, permissions, schema};
use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Subscription, Window, div, prelude::*, px,
};
use serde_json::Value;

use super::kit::{self, card, opt, row, search_field, section};
use super::{HostLink, Outcome, Page, SettingsWindow};
use crate::settings::AppSettings;
use crate::toast::{self, Toast};
use crate::ui::controls::{
    ButtonKind, Tone, badge, banner, button, button_with, icon_button, progress_bar, segmented, stepper, toggle, tooltip,
};
use crate::ui::focus::{self, Ring, Scroller};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

/// The page's own state (the catalog is the host's).
#[derive(Default)]
pub struct PluginsPage {
    pub catalog: Option<Catalog>,
    in_flight: usize,
    pub error: Option<String>,
    /// The install/update being reviewed.
    pub plan: Option<Box<Plan>>,
    /// Return to this page when the review identified by `.0` closes.
    plan_return: Option<(u64, Page)>,
    review_seq: u64,
    active_review: Option<u64>,
    plan_review: Option<u64>,
    pub show_code: bool,
    pub show_advanced: bool,
    /// The installed plugin whose page is open.
    pub detail: Option<String>,
    pub tab: Tab,
    /// Plugin whose Uninstall was clicked once (the second click removes it).
    pub confirm_uninstall: Option<String>,
    /// The install this window follows (#102): started here, or found
    /// running on the host when the page loaded.
    pub follow: Option<Follow>,
    /// The loop asking the host for install progress runs.
    polling: bool,
    /// Updates and approved installs waiting their turn (#244).
    pub batch: Option<Batch>,
    /// Update reviews waiting (updates that need a look). Survives
    /// leaving the review: the Updates list offers them.
    pub review_queue: VecDeque<Box<Plan>>,
    /// The review shown came from the queue (not a Browse install).
    pub from_queue: bool,
    /// Queued reviews done (approved or skipped) in this pass, for "2 of 3".
    review_done: usize,
    /// An update's review shows everything the new version does, not
    /// only what changes.
    pub show_full_plan: bool,
    /// Text fields for string/path/list settings: `"<plugin>.<setting>"`.
    inputs: HashMap<String, Entity<TextInput>>,
    filter: Option<Entity<TextInput>>,
    _subs: Vec<Subscription>,
}

impl PluginsPage {
    pub fn is_loading(&self) -> bool {
        self.in_flight > 0
    }

    pub(super) fn begin_review(&mut self, from: Option<Page>) -> u64 {
        self.review_seq += 1;
        let id = self.review_seq;
        self.active_review = Some(id);
        self.plan = None;
        self.plan_review = None;
        self.plan_return = from.map(|page| (id, page));
        self.show_code = false;
        self.show_full_plan = false;
        id
    }

    pub(super) fn accept_review_plan(&mut self, id: u64, plan: Box<Plan>) -> bool {
        if self.active_review != Some(id) {
            return false;
        }
        self.active_review = None;
        self.plan = Some(plan);
        self.plan_review = Some(id);
        true
    }

    /// Complete a failed review if `id` is still active; `None` means stale.
    pub(super) fn fail_review(&mut self, id: u64) -> Option<Option<Page>> {
        if self.active_review != Some(id) {
            return None;
        }
        self.active_review = None;
        Some(self.take_plan_return(id))
    }

    pub(super) fn take_plan_return(&mut self, id: u64) -> Option<Page> {
        if self.plan_return.as_ref().is_some_and(|(rid, _)| *rid == id) { self.plan_return.take().map(|(_, page)| page) } else { None }
    }

    /// Close the review for good (Cancel, or Skip on a queued one): the
    /// next queued one shows, if any.
    pub(super) fn dismiss_plan(&mut self) -> Option<Option<Page>> {
        self.plan.take()?;
        let back = self.plan_review.take().and_then(|rid| self.take_plan_return(rid));
        self.plan_return = None;
        if std::mem::take(&mut self.from_queue) {
            self.review_done += 1;
            if self.show_next_review() {
                return Some(None);
            }
        }
        Some(back)
    }

    /// Show the next queued review. Returns whether there was one.
    pub(super) fn show_next_review(&mut self) -> bool {
        let Some(next) = self.review_queue.pop_front() else {
            self.review_done = 0;
            return false;
        };
        let id = self.begin_review(None);
        self.accept_review_plan(id, next);
        self.from_queue = true;
        true
    }

    /// The queued review shown is number `.0` of `.1` (`None`: a single
    /// review).
    pub fn review_position(&self) -> Option<(usize, usize)> {
        let total = self.review_done + 1 + self.review_queue.len();
        (self.from_queue && total > 1).then_some((self.review_done + 1, total))
    }

    /// Queue update reviews (each plugin once, the newest plan winning).
    #[allow(clippy::vec_box)] // Plans move between `plan` and the queue as they are.
    fn queue_reviews(&mut self, plans: Vec<Box<Plan>>) {
        for p in plans {
            if self.from_queue && self.plan.as_ref().is_some_and(|q| q.id == p.id) {
                continue;
            }
            match self.review_queue.iter_mut().find(|q| q.id == p.id) {
                Some(q) => *q = p,
                None => self.review_queue.push_back(p),
            }
        }
    }

    /// Drop waiting reviews that no longer apply: the plugin is gone, or
    /// the update on offer is another version now.
    fn prune_reviews(&mut self) {
        let Some(cat) = &self.catalog else { return };
        let current = |p: &Plan| cat.installed.iter().any(|i| i.id == p.id && i.update.as_deref() == Some(p.version.as_str()));
        self.review_queue.retain(|p| current(p));
    }

    /// What's happening with `id`'s update, if anything.
    pub fn update_state(&self, id: &str) -> Option<UpdateState> {
        if self.follow.as_ref().is_some_and(|f| f.id == id) {
            return Some(UpdateState::Installing);
        }
        if let Some(b) = &self.batch {
            if b.current.as_deref() == Some(id) {
                return Some(UpdateState::Checking);
            }
            if b.queue.iter().any(|j| j.id() == id) {
                return Some(UpdateState::Queued);
            }
        }
        if self.review_queue.iter().any(|p| p.id == id) || self.plan.as_ref().is_some_and(|p| p.id == id) {
            return Some(UpdateState::NeedsReview);
        }
        None
    }

    /// Empty the search field (so the whole list shows).
    pub(super) fn clear_filter<V: 'static>(&self, cx: &mut Context<'_, V>) {
        if let Some(f) = &self.filter {
            f.update(cx, |f, cx| f.set_text("", cx));
        }
    }

    /// Close the review and forget where it came from. A queued review
    /// goes back to waiting (Back, Esc, another page): the list offers it.
    pub(super) fn clear_review(&mut self) {
        if let Some(p) = self.plan.take()
            && std::mem::take(&mut self.from_queue)
        {
            self.review_queue.push_front(p);
        }
        self.review_done = 0;
        self.plan_return = None;
        self.plan_review = None;
        self.active_review = None;
    }
}

/// Where an update stands (the Updates list shows it instead of its button).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateState {
    /// Waiting its turn.
    Queued,
    /// Asking the host what it changes.
    Checking,
    Installing,
    /// It changes something to look at first.
    NeedsReview,
}

/// One thing a [`Batch`] does.
#[derive(Debug)]
pub enum Job {
    /// Ask for the update's plan; install it if it needs no review.
    Check { id: String, version: String },
    /// A plan the user approved.
    Install(Box<Plan>),
}

impl Job {
    fn id(&self) -> &str {
        match self {
            Job::Check { id, .. } => id,
            Job::Install(p) => &p.id,
        }
    }
}

/// Updates (and approved installs) done one after the other, reported
/// together at the end (#244).
#[derive(Debug, Default)]
pub struct Batch {
    queue: VecDeque<Job>,
    /// The plugin whose plan is being asked for, or that's installing.
    current: Option<String>,
    /// "eza 1.1.0", by whether it was an update.
    done: Vec<(bool, String)>,
    failed: Vec<String>,
    /// Updates it found that need a look (queued for review).
    reviews: usize,
    reload: bool,
    /// The host's report, for a batch of one.
    lines: Vec<String>,
    /// Stopped by a cancel: the rest isn't done.
    stopped: bool,
}

impl Batch {
    /// Updates still to do after the current one.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The summary for the result bar: (ok, lines; the first is its title).
    fn summary(&self) -> Option<(bool, Vec<String>)> {
        if self.done.is_empty() && self.failed.is_empty() && !self.stopped {
            return None;
        }
        let mut lines = Vec::new();
        let list = |update: bool| self.done.iter().filter(|(u, _)| *u == update).map(|(_, n)| n.clone()).collect::<Vec<_>>();
        let (updated, installed) = (list(true), list(false));
        let plugins = |n: usize| if n == 1 { "1 plugin".to_string() } else { format!("{n} plugins") };
        if self.stopped {
            lines.push(if self.done.is_empty() {
                "Stopped: nothing was updated".into()
            } else {
                format!("Stopped after {}", plugins(self.done.len()))
            });
        } else if !self.failed.is_empty() {
            lines.push(if self.done.is_empty() {
                format!("{} failed", plugins(self.failed.len()))
            } else {
                format!("Updated {}; {} failed", plugins(self.done.len()), self.failed.len())
            });
        }
        if self.done.len() == 1 && self.failed.is_empty() && !self.stopped && !self.lines.is_empty() {
            lines.extend(self.lines.iter().cloned());
        } else {
            if !updated.is_empty() {
                lines.push(format!("Updated {}", updated.join(", ")));
            }
            if !installed.is_empty() {
                lines.push(format!("Installed {}", installed.join(", ")));
            }
        }
        lines.extend(self.failed.iter().map(|f| format!("Failed: {f}")));
        Some((self.failed.is_empty() && !self.stopped, lines))
    }
}

/// An install the page shows progress for.
#[derive(Debug, Clone)]
pub struct Follow {
    pub id: String,
    /// The plugins it needs that are installed along with it.
    pub deps: Vec<String>,
    /// "Installing" or "Updating".
    pub verb: &'static str,
    /// The host's latest report (`None` until the first one, or from a
    /// goattyd that can't report progress).
    pub progress: Option<api::InstallProgress>,
    /// This window's `Install` request is still open: its reply reports
    /// the outcome. Otherwise (the request was lost with the connection,
    /// or another window started it) the progress reports carry it.
    pub awaiting_reply: bool,
    /// Where to go once it's done (the review's origin page).
    pub return_id: Option<u64>,
    /// When the host last answered a progress request.
    last_heard: std::time::Instant,
    /// The host can't report progress or cancel (a goattyd from before #102).
    pub no_progress: bool,
    /// Part of a [`Batch`]: its outcome goes into the batch's summary.
    pub batch: bool,
}

impl Follow {
    fn new(id: String, verb: &'static str, awaiting_reply: bool, return_id: Option<u64>) -> Follow {
        Follow {
            id,
            deps: Vec::new(),
            verb,
            progress: None,
            awaiting_reply,
            return_id,
            last_heard: std::time::Instant::now(),
            no_progress: false,
            batch: false,
        }
    }

    fn name(&self) -> String {
        self.progress.as_ref().map_or_else(|| self.id.clone(), |p| format!("{} {}", p.name, p.version))
    }

    fn cancelling(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.cancelling)
    }
}

/// How often the page asks for install progress.
const POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(400);
/// Give up following an install the host hasn't reported on for this long.
const LOSE_TRACK_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The request failed because the connection did (it may come back), not
/// because the host refused.
fn connection_lost(link: &HostLink, e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    !link.is_connected() || m.contains("did not answer") || m.contains("not connected to goattyd")
}

/// What an install is doing, for the progress row.
fn step_text(f: &Follow, connected: bool) -> String {
    if !connected {
        return "Connection lost: waiting for it to come back…".into();
    }
    if f.no_progress {
        return "This host's goattyd can't report progress or cancel; waiting for it to finish".into();
    }
    if f.cancelling() {
        return "Cancelling…".into();
    }
    match f.progress.as_ref().map(|p| &p.step) {
        None => "Starting…".into(),
        Some(api::Step::Waiting) => "Waiting for another plugin change on the host".into(),
        Some(api::Step::Downloading { package, index, count, bytes, total }) => {
            let of = if *count > 1 { format!(" ({index} of {count})") } else { String::new() };
            let amount = if *total > 0 { format!("{} of {}", size(*bytes), size(*total)) } else { size(*bytes) };
            format!("Downloading {package}{of} · {amount}")
        }
        Some(api::Step::Activating) => "Setting it up (can't be cancelled any more)".into(),
        Some(api::Step::Done(_)) => "Done".into(),
        Some(api::Step::Failed { .. }) => "Failed".into(),
        Some(api::Step::Cancelled) => "Cancelled".into(),
    }
}

/// How far along it is: the downloads' share, or `None` when unknown.
fn step_fraction(f: &Follow) -> Option<f32> {
    match f.progress.as_ref().map(|p| &p.step)? {
        api::Step::Downloading { index, count, bytes, total, .. } if *total > 0 && *count > 0 => {
            let done = (*index as f32 - 1.0 + (*bytes as f32 / *total as f32).min(1.0)) / *count as f32;
            Some(done * 0.9)
        }
        api::Step::Activating => Some(0.95),
        s if s.finished() => Some(1.0),
        _ => None,
    }
}

/// Which list the page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Installed,
    Browse,
}

/// A host request's result, back on the UI thread.
type Done<T> = Box<dyn FnOnce(&mut SettingsWindow, anyhow::Result<T>, &mut Context<'_, SettingsWindow>)>;

pub(super) fn size(bytes: u64) -> String {
    if bytes < 1_000_000 { format!("{} kB", bytes.div_ceil(1000)) } else { format!("{:.1} MB", bytes as f64 / 1e6) }
}

impl SettingsWindow {
    // ---- Host calls ---------------------------------------------------------

    fn plugin_call<T: Send + 'static>(
        &mut self,
        req: api::Request,
        map: fn(api::Reply) -> Option<T>,
        done: Done<T>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(link) = self.link.clone() else {
            self.plugins.error = Some("No host to manage plugins on.".into());
            cx.notify();
            return;
        };
        if let Some(why) = self.plugins_unavailable(cx) {
            self.plugins.error = Some(why.clone());
            // The caller's state (a followed install, a review) ends through its own path.
            done(self, Err(anyhow::anyhow!(why)), cx);
            cx.notify();
            return;
        }
        self.plugins.in_flight += 1;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let reply = link.plugin(req)?;
                    map(reply).ok_or_else(|| anyhow::anyhow!("unexpected reply from the host"))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.plugins.in_flight = view.plugins.in_flight.saturating_sub(1);
                done(view, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Load what's installed and available (`refresh`: fetch the registries).
    pub fn load_plugins(&mut self, refresh: bool, cx: &mut Context<'_, Self>) {
        self.plugin_call(
            api::Request::Catalog { refresh },
            |r| match r {
                api::Reply::Catalog(c) => Some(c),
                _ => None,
            },
            Box::new(|view, result, cx| match result {
                Ok(c) => {
                    view.plugins.catalog = Some(c);
                    view.plugins.prune_reviews();
                    view.plugins.error = None;
                    view.sync_plugin_inputs(cx);
                }
                Err(e) => view.plugins.error = Some(format!("{e:#}")),
            }),
            cx,
        );
        self.find_running_install(cx);
    }

    /// Pick up an install running on the host that this window doesn't
    /// follow yet (started before a reconnect, or from another window).
    /// A goattyd from before #102 can't tell: nothing to show then.
    fn find_running_install(&mut self, cx: &mut Context<'_, Self>) {
        if self.plugins.follow.is_some() {
            return;
        }
        let Some(link) = self.link.clone() else { return };
        let asked = link.clone();
        cx.spawn(async move |this, cx| {
            let reply = cx.background_executor().spawn(async move { link.plugin(api::Request::Installs) }).await;
            let Ok(api::Reply::Installs { installs: list }) = reply else { return };
            let Some(running) = list.into_iter().find(|p| !p.step.finished()) else { return };
            let _ = this.update(cx, |view, cx| {
                // The window may have moved to another host meanwhile.
                if view.plugins.follow.is_none() && view.link.as_ref().is_some_and(|l| l.same(&asked)) {
                    let mut f = Follow::new(running.id.clone(), "Installing", false, None);
                    f.progress = Some(running);
                    view.plugins.follow = Some(f);
                    view.poll_installs(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Ask the host for install progress until the followed install is
    /// over. Survives a dropped connection: once it's back, the host's
    /// report says how the install ended.
    fn poll_installs(&mut self, cx: &mut Context<'_, Self>) {
        if self.plugins.polling {
            return;
        }
        self.plugins.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_EVERY).await;
                let Ok(Some(link)) = this.update(cx, |v, _| v.link.clone().filter(|_| v.plugins.follow.is_some())) else { break };
                let probe = link.clone();
                let reply = cx.background_executor().spawn(async move { probe.plugin(api::Request::Installs) }).await;
                let Ok(keep) = this.update(cx, |v, cx| v.on_installs(&link, reply, cx)) else { break };
                if !keep {
                    break;
                }
            }
            let _ = this.update(cx, |v, _| v.plugins.polling = false);
        })
        .detach();
    }

    /// A progress report arrived. Returns whether to keep asking.
    fn on_installs(&mut self, link: &HostLink, reply: anyhow::Result<api::Reply>, cx: &mut Context<'_, Self>) -> bool {
        let Some(f) = self.plugins.follow.as_mut() else { return false };
        cx.notify();
        let list = match reply {
            Ok(api::Reply::Installs { installs: list }) => list,
            Err(e) if connection_lost(link, &e) => {
                if f.last_heard.elapsed() < LOSE_TRACK_AFTER {
                    return true;
                }
                let name = f.name();
                self.plugins.follow = None;
                self.plugins.batch = None;
                toast::show(cx, Toast::warning(format!("Lost track of {name}")).body("The host didn't come back; see the Plugins page."));
                return false;
            }
            // A goattyd from before #102 can't report progress: only the
            // install's reply will tell. Lost that too: show what's there now.
            _ => {
                if f.awaiting_reply {
                    f.no_progress = true;
                } else {
                    self.plugins.follow = None;
                    self.load_host(cx);
                    self.load_plugins(false, cx);
                    self.batch_lost_track(cx);
                }
                return false;
            }
        };
        f.last_heard = std::time::Instant::now();
        let mine = list.into_iter().find(|p| p.id == f.id);
        if f.awaiting_reply {
            if mine.is_some() {
                f.progress = mine;
            }
            return true;
        }
        // The reply was lost: the report carries the outcome.
        let Some(p) = mine else {
            // The host forgot it (goattyd restarted): show what's there now.
            self.plugins.follow = None;
            self.load_host(cx);
            self.load_plugins(false, cx);
            self.batch_lost_track(cx);
            return false;
        };
        if !p.step.finished() {
            f.progress = Some(p);
            return true;
        }
        let f = self.plugins.follow.take().expect("followed");
        let result = match p.step {
            api::Step::Done(c) => Ok(c),
            api::Step::Failed { error } => Err(anyhow::anyhow!(error)),
            _ => Err(fetch::Cancelled.into()),
        };
        let name = f.name();
        let ids = [f.id].into_iter().chain(f.deps).collect();
        self.install_finished(result, ids, name, f.verb == "Updating", f.return_id, f.batch, cx);
        false
    }

    /// Stop the followed install (refused by the host once it's activating).
    pub fn cancel_install(&mut self, cx: &mut Context<'_, Self>) {
        let Some(id) = self.plugins.follow.as_ref().map(|f| f.id.clone()) else { return };
        self.plugin_call(
            api::Request::CancelInstall { id },
            |r| match r {
                api::Reply::Installs { installs } => Some(installs),
                _ => None,
            },
            Box::new(|view, result: anyhow::Result<Vec<api::InstallProgress>>, cx| match result {
                Ok(list) => {
                    if let Some(f) = view.plugins.follow.as_mut()
                        && let Some(p) = list.into_iter().find(|p| p.id == f.id)
                    {
                        f.progress = Some(p);
                    }
                }
                Err(e) => {
                    toast::show(cx, Toast::error("Couldn't cancel").body(format!("{e:#}")).key("plugins.cancel"));
                }
            }),
            cx,
        );
    }

    /// Open the install review for `id` and return to `from` once closed.
    pub fn review_plugin_from(&mut self, id: String, from: Page, cx: &mut Context<'_, Self>) {
        self.go(Page::Plugins, cx);
        self.start_review(id, None, Some(from), cx);
    }

    /// Ask the host what installing (or updating to) `id` would do.
    pub fn review_plugin(&mut self, id: String, version: Option<String>, cx: &mut Context<'_, Self>) {
        self.start_review(id, version, None, cx);
    }

    fn start_review(&mut self, id: String, version: Option<String>, from: Option<Page>, cx: &mut Context<'_, Self>) {
        let req_id = self.plugins.begin_review(from);
        self.plugin_call(
            api::Request::Plan { id, version },
            |r| match r {
                api::Reply::Plan(p) => Some(p),
                _ => None,
            },
            Box::new(move |view, result, cx| match result {
                Ok(p) => {
                    if view.plugins.accept_review_plan(req_id, p) {
                        view.scroll.set_offset(gpui::point(px(0.), px(0.)));
                    }
                }
                Err(e) => {
                    let Some(back) = view.plugins.fail_review(req_id) else { return };
                    view.outcome = Some(Outcome { ok: false, lines: vec![format!("{e:#}")], diff: Vec::new(), reload: false });
                    if let Some(page) = back {
                        view.go(page, cx);
                    }
                }
            }),
            cx,
        );
        if self.link.is_none() {
            self.plugins.fail_review(req_id);
        }
    }

    /// Install the reviewed plan (the host refuses if it changed since).
    /// Its progress shows on the page, with a Cancel button until it's
    /// activated. While other updates run (or more reviews wait), it's
    /// queued behind them and the next review shows.
    pub fn install_reviewed(&mut self, cx: &mut Context<'_, Self>) {
        let Some(plan) = self.plugins.plan.take() else { return };
        let return_id = self.plugins.plan_review.take();
        let from_queue = std::mem::take(&mut self.plugins.from_queue);
        if from_queue || self.plugins.batch.is_some() || self.plugins.follow.is_some() {
            self.plugins.batch.get_or_insert_default().queue.push_back(Job::Install(plan));
            if from_queue {
                self.plugins.review_done += 1;
            }
            if !(from_queue && self.plugins.show_next_review()) {
                if let Some(rid) = return_id
                    && let Some(back) = self.plugins.take_plan_return(rid)
                {
                    self.go(back, cx);
                }
            } else {
                self.scroll.set_offset(gpui::point(px(0.), px(0.)));
            }
            self.next_in_batch(cx);
            cx.notify();
            return;
        }
        self.install_plan(plan, return_id, false, cx);
    }

    /// Start installing `plan` and follow it.
    fn install_plan(&mut self, plan: Box<Plan>, return_id: Option<u64>, batch: bool, cx: &mut Context<'_, Self>) {
        let id = plan.id.clone();
        let update = plan.replaces.is_some();
        let verb = if update { "Updating" } else { "Installing" };
        let mut follow = Follow::new(id.clone(), verb, true, return_id);
        follow.batch = batch;
        let deps: Vec<String> = plan.dependencies.iter().map(|d| d.id.clone()).collect();
        follow.deps = deps.clone();
        let ids: Vec<String> = [id.clone()].into_iter().chain(deps).collect();
        let name = format!("{} {}", plan.name, plan.version);
        follow.progress = Some(api::InstallProgress {
            id: id.clone(),
            name: plan.name.clone(),
            version: plan.version.clone(),
            step: api::Step::Waiting,
            cancelling: false,
        });
        self.plugins.follow = Some(follow);
        let link = self.link.clone();
        let req = api::Request::Install { id: plan.id.clone(), version: Some(plan.version.clone()), approved: plan.digest.clone() };
        self.plugin_call(
            req,
            changed,
            Box::new(move |view, result, cx| {
                let Some(f) = view.plugins.follow.as_mut().filter(|f| f.id == id) else {
                    // Not followed any more (another host): just report it.
                    view.after_change(result, ids, true, return_id, true, cx);
                    return;
                };
                if let (Err(e), Some(link)) = (&result, &link)
                    && connection_lost(link, e)
                {
                    // The install goes on on the host: its progress reports
                    // will say how it ended, once the connection is back.
                    f.awaiting_reply = false;
                    view.poll_installs(cx);
                    return;
                }
                let cancelled = f.cancelling();
                view.plugins.follow = None;
                let result = match result {
                    Err(_) if cancelled => Err(fetch::Cancelled.into()),
                    r => r,
                };
                view.install_finished(result, ids, name, update, return_id, batch, cx);
            }),
            cx,
        );
        self.poll_installs(cx);
    }

    /// An install ended: into the batch's summary (and on with the batch),
    /// or reported on its own.
    #[allow(clippy::too_many_arguments)]
    fn install_finished(
        &mut self,
        result: anyhow::Result<api::Changed>,
        ids: Vec<String>,
        name: String,
        update: bool,
        return_id: Option<u64>,
        batch: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let in_batch = batch && self.plugins.batch.is_some();
        if let Some(b) = self.plugins.batch.as_mut().filter(|_| in_batch) {
            b.current = None;
            match &result {
                Ok(c) => {
                    b.done.push((update, name));
                    b.reload |= c.reload;
                    b.lines.extend(c.lines.iter().cloned());
                }
                Err(e) if fetch::Cancelled::is(e) => {
                    // A cancel stops the whole run.
                    b.queue.clear();
                    b.stopped = true;
                }
                Err(e) => b.failed.push(format!("{name}: {e:#}")),
            }
        }
        self.after_change(result, ids, true, return_id, !in_batch, cx);
        self.next_in_batch(cx);
    }

    /// The followed install's outcome is unknown (the host forgot it):
    /// go on with the batch.
    fn batch_lost_track(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(b) = self.plugins.batch.as_mut() {
            b.current = None;
        }
        self.next_in_batch(cx);
    }

    /// Update each of `items` (id, version), one after the other: an
    /// update that does nothing the installed version couldn't installs
    /// right away; the others wait for the user's review, which opens when
    /// the rest is done. Already queued or running ones are skipped.
    pub fn update_plugins(&mut self, items: Vec<(String, String)>, cx: &mut Context<'_, Self>) {
        let mut added = false;
        for (id, version) in items {
            if self.plugins.update_state(&id).is_some() {
                continue;
            }
            self.plugins.batch.get_or_insert_default().queue.push_back(Job::Check { id, version });
            added = true;
        }
        if added {
            self.next_in_batch(cx);
        }
        cx.notify();
    }

    /// Do the batch's next job, once nothing else is installing.
    fn next_in_batch(&mut self, cx: &mut Context<'_, Self>) {
        if self.plugins.follow.is_some() {
            return; // Resumes when that install ends.
        }
        let Some(b) = self.plugins.batch.as_mut() else { return };
        if b.current.is_some() {
            return;
        }
        let Some(job) = b.queue.pop_front() else {
            self.finish_batch(cx);
            return;
        };
        b.current = Some(job.id().to_owned());
        match job {
            Job::Install(plan) => self.install_plan(plan, None, true, cx),
            Job::Check { id, version } => {
                let asked = id.clone();
                self.plugin_call(
                    api::Request::Plan { id, version: Some(version) },
                    |r| match r {
                        api::Reply::Plan(p) => Some(p),
                        _ => None,
                    },
                    Box::new(move |view, result: anyhow::Result<Box<Plan>>, cx| {
                        // Gone with the host, or stopped meanwhile.
                        let Some(b) = view.plugins.batch.as_mut().filter(|b| b.current.as_deref() == Some(asked.as_str())) else {
                            return;
                        };
                        b.current = None;
                        match result {
                            // A goattyd that doesn't send the installed
                            // version: no way to tell, so review it.
                            Ok(plan) if plan.changes().is_none_or(|c| c.needs_review()) => {
                                b.reviews += 1;
                                view.plugins.queue_reviews(vec![plan]);
                            }
                            Ok(plan) => b.queue.push_front(Job::Install(plan)),
                            Err(e) => b.failed.push(format!("{asked}: {e:#}")),
                        }
                        view.next_in_batch(cx);
                    }),
                    cx,
                );
            }
        }
    }

    /// The batch is done: report it, and offer the updates that need a look.
    fn finish_batch(&mut self, cx: &mut Context<'_, Self>) {
        let Some(b) = self.plugins.batch.take() else { return };
        if let Some((ok, lines)) = b.summary() {
            self.outcome = Some(Outcome { ok, lines, diff: Vec::new(), reload: b.reload });
        }
        if b.reviews == 0 || b.stopped || self.plugins.review_queue.is_empty() {
            return;
        }
        // Open the first review only where the user is looking at the list.
        let on_list = self.page == Page::Plugins
            && self.plugins.tab == Tab::Installed
            && self.plugins.detail.is_none()
            && self.plugins.plan.is_none()
            && self.plugins.active_review.is_none();
        if on_list {
            self.plugins.show_next_review();
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
            return;
        }
        if self.plugins.plan.is_some() && self.plugins.from_queue {
            return; // Already reviewing: the rest follow.
        }
        let n = self.plugins.review_queue.len();
        let this = cx.entity().downgrade();
        let t = Toast::info(if n == 1 { "An update needs your review".to_string() } else { format!("{n} updates need your review") })
            .body("Check what changes before they install.")
            .key("plugins.review")
            .action("Review", move |_, cx| {
                let _ = this.update(cx, |v, cx| v.open_queued_review(cx));
            });
        toast::show(cx, t);
    }

    /// Stop the batch: nothing more starts; a running download is cancelled.
    pub fn stop_updates(&mut self, cx: &mut Context<'_, Self>) {
        let Some(b) = self.plugins.batch.as_mut() else { return };
        b.queue.clear();
        b.stopped = true;
        if self.plugins.follow.as_ref().is_some_and(|f| f.batch) {
            self.cancel_install(cx);
        } else if b.current.is_none() {
            self.finish_batch(cx);
        }
        cx.notify();
    }

    /// Skip the queued review shown (it stays not updated): the next one shows.
    pub fn skip_review(&mut self, cx: &mut Context<'_, Self>) {
        if self.plugins.dismiss_plan().is_some() {
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        cx.notify();
    }

    /// Show the first waiting review (the list's "Review" on waiting updates).
    pub fn open_queued_review(&mut self, cx: &mut Context<'_, Self>) {
        if self.page != Page::Plugins {
            self.go(Page::Plugins, cx);
        }
        if self.plugins.plan.is_none() && self.plugins.show_next_review() {
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        cx.notify();
    }

    /// Drop the waiting reviews (and close the one shown).
    pub fn skip_all_reviews(&mut self, cx: &mut Context<'_, Self>) {
        self.plugins.review_queue.clear();
        self.skip_review(cx);
    }

    pub fn uninstall_plugin(&mut self, id: String, cx: &mut Context<'_, Self>) {
        let host = self.link.clone();
        self.uninstall_plugins(vec![id], host, cx);
    }

    /// Uninstall `ids` one after the other (each before the ones it
    /// needs), stopping at the first that fails. When the last one leaves
    /// plugins that were only installed for it, offers to remove those too.
    /// Only on `host`: nothing is removed once the window shows another one.
    fn uninstall_plugins(&mut self, mut ids: Vec<String>, host: Option<HostLink>, cx: &mut Context<'_, Self>) {
        self.plugins.confirm_uninstall = None;
        if ids.is_empty() {
            return;
        }
        let same = match (&self.link, &host) {
            (Some(a), Some(b)) => a.same(b),
            _ => false,
        };
        if !same {
            toast::show(cx, Toast::info("Not uninstalled").body("The settings window shows another host now."));
            return;
        }
        let id = ids.remove(0);
        let req = api::Request::Uninstall { id: id.clone() };
        self.plugin_call(
            req,
            changed,
            Box::new(move |view, result, cx| {
                let next = match &result {
                    Ok(_) if !ids.is_empty() => Some(ids),
                    _ => None,
                };
                let unneeded = result.as_ref().map(|c| c.unneeded.clone()).unwrap_or_default();
                view.after_change(result, vec![id.clone()], false, None, true, cx);
                if let Some(rest) = next {
                    view.uninstall_plugins(rest, host, cx);
                } else if !unneeded.is_empty() {
                    view.offer_unneeded(&id, unneeded, host, cx);
                }
            }),
            cx,
        );
    }

    /// A toast offering to uninstall the plugins `id` needed that nothing
    /// needs now.
    fn offer_unneeded(&self, id: &str, unneeded: Vec<String>, host: Option<HostLink>, cx: &mut Context<'_, Self>) {
        let name = |i: &str| {
            self.plugins
                .catalog
                .as_ref()
                .and_then(|c| c.installed.iter().find(|x| x.id == i))
                .map_or_else(|| i.to_owned(), |x| x.manifest.name.clone())
        };
        let names: Vec<String> = unneeded.iter().map(|i| name(i)).collect();
        let this = cx.entity().downgrade();
        let one = unneeded.len() == 1;
        let t = Toast::info(format!("{} {} no longer needed", names.join(", "), if one { "is" } else { "are" }))
            .body(format!("goatty installed {} for {} (or another removed plugin).", if one { "it" } else { "them" }, name(id)))
            .key("plugins.unneeded")
            .duration(std::time::Duration::from_secs(20))
            .action("Uninstall", move |_, cx| {
                let _ = this.update(cx, |v, cx| v.uninstall_plugins(unneeded.clone(), host.clone(), cx));
            });
        toast::show(cx, t);
    }

    /// An install or uninstall finished: report it, keep an unapplied
    /// draft in step with the host (which changed `plugins.<id>` for each
    /// of `ids`), reload.
    fn after_change(
        &mut self,
        result: anyhow::Result<api::Changed>,
        ids: Vec<String>,
        installed: bool,
        return_id: Option<u64>,
        report: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let first = ids.first().cloned();
        // Installed or removed for every host that follows the shared config (#313).
        if result.is_ok()
            && self.scope == Some(super::scope::Scope::All)
            && let Some(link) = self.link.clone()
        {
            crate::shared_sync::share_plugins(link, ids.clone(), installed, cx);
        }
        match result {
            Ok(c) => {
                for id in ids {
                    let Some(draft) = self.draft.as_mut() else { break };
                    if installed {
                        if goatty_shell::builtins::get(&id).is_some() {
                            draft.edit_plugin(&id, |p| p.enabled = true);
                        } else {
                            // Keep non-builtin plugin entries even with default
                            // config so applying the draft doesn't remove them.
                            draft.plugins.entry(id).or_default().enabled = true;
                        }
                    } else {
                        draft.plugins.remove(&id);
                    }
                }
                if report {
                    self.outcome = Some(Outcome { ok: true, lines: c.lines, diff: Vec::new(), reload: c.reload });
                }
            }
            Err(e) if fetch::Cancelled::is(&e) => {
                if report {
                    let what = first.as_deref().unwrap_or("the plugin");
                    toast::show(cx, Toast::info(format!("Cancelled {what}")).body("Nothing on the host changed."));
                }
            }
            Err(e) if report => {
                self.outcome = Some(Outcome { ok: false, lines: vec![format!("{e:#}")], diff: Vec::new(), reload: false });
            }
            Err(_) => {}
        }
        self.load_host(cx);
        self.load_plugins(false, cx);
        // Its palette commands come or go.
        crate::plugin_commands::refresh(cx);
        crate::snippets::refresh(cx);
        if let Some(rid) = return_id
            && let Some(back) = self.plugins.take_plan_return(rid)
        {
            self.go(back, cx);
        }
    }

    // ---- Settings -----------------------------------------------------------

    /// Stored values of a plugin (draft first), resolved against its schema.
    fn plugin_values(&self, inst: &Installed) -> schema::Values {
        let stored = self.host_value().and_then(|h| h.plugins.get(&inst.id)).map(|p| p.settings.clone()).unwrap_or_default();
        schema::resolve(&inst.manifest, &stored).0
    }

    fn edit_plugin(&mut self, id: &str, cx: &mut Context<'_, Self>, f: impl FnOnce(&mut PluginConfig)) {
        let id = id.to_owned();
        self.edit_host(cx, move |h: &mut HostSettings| h.edit_plugin(&id, f));
    }

    /// Set (or with `None` reset) one plugin setting in the draft.
    fn set_plugin_value(&mut self, id: &str, key: &str, value: Option<Value>, cx: &mut Context<'_, Self>) {
        let key = key.to_owned();
        self.edit_plugin(id, cx, move |p| match value {
            Some(v) => {
                p.settings.insert(key, v);
            }
            None => {
                p.settings.remove(&key);
            }
        });
    }

    /// Text fields for the installed plugins' string, path and list
    /// settings, showing the current values.
    pub fn sync_plugin_inputs(&mut self, cx: &mut Context<'_, Self>) {
        if self.plugins.filter.is_none() {
            let filter = cx.new(|cx| TextInput::new("Search plugins", "", cx).bare());
            self.plugins._subs.push(cx.subscribe(&filter, |_, _, _: &TextInputEvent, cx| cx.notify()));
            self.plugins.filter = Some(filter);
        }
        let Some(cat) = self.plugins.catalog.clone() else { return };
        for inst in &cat.installed {
            let values = self.plugin_values(inst);
            for s in inst.manifest.settings.iter().filter(|s| matches!(s.kind, SettingKind::String | SettingKind::Path | SettingKind::List))
            {
                let key = format!("{}.{}", inst.id, s.id);
                let text = match values.get(&s.id) {
                    Some(Value::Array(items)) => items.iter().map(schema::plain_string).collect::<Vec<_>>().join(", "),
                    Some(v) => schema::plain_string(v),
                    None => String::new(),
                };
                match self.plugins.inputs.get(&key) {
                    Some(input) => {
                        if input.read(cx).text() != text {
                            input.update(cx, |i, cx| i.set_text(text, cx));
                        }
                    }
                    None => {
                        let placeholder = s.placeholder.clone().unwrap_or_default();
                        let input = cx.new(|cx| TextInput::new(placeholder, text, cx).label(s.label.clone()));
                        let (id, setting) = (inst.id.clone(), s.clone());
                        self.plugins._subs.push(cx.subscribe(&input, move |view, _, ev: &TextInputEvent, cx| {
                            let (TextInputEvent::Changed(text) | TextInputEvent::Submitted(text)) = ev;
                            let value = if text.trim().is_empty() && setting.kind != SettingKind::List {
                                None // back to the default
                            } else {
                                match schema::parse_input(&setting, text) {
                                    Ok(v) => Some(v),
                                    Err(_) => return, // keep typing; invalid values never reach the draft
                                }
                            };
                            view.set_plugin_value(&id, &setting.id, value, cx);
                        }));
                        self.plugins.inputs.insert(key, input);
                    }
                }
            }
        }
    }

    // ---- Rendering ----------------------------------------------------------

    pub(super) fn plugins_page(&self, ui: Ui, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        if let Some(plan) = &self.plugins.plan {
            return self.plan_view(ui, plan, cx);
        }
        if let Some(inst) =
            self.plugins.detail.as_ref().and_then(|id| self.plugins.catalog.as_ref()?.installed.iter().find(|i| &i.id == id))
        {
            let detail = self.plugin_detail(ui, inst, cx);
            return div()
                .flex()
                .flex_col()
                .gap(px(20.))
                .children(self.plugins.follow.as_ref().map(|f| self.install_progress(ui, f, cx)))
                .children(self.batch_progress(ui, cx))
                .child(detail)
                .into_any_element();
        }
        let mut page = div().flex().flex_col().gap(px(20.));
        if let Some(e) = &self.plugins.error {
            page = page.child(banner(ui, Tone::Danger, "Couldn't talk to the host", vec![e.clone()]));
        }
        if let Some(f) = &self.plugins.follow {
            page = page.child(self.install_progress(ui, f, cx));
        }
        page = page.children(self.batch_progress(ui, cx));
        let Some(cat) = &self.plugins.catalog else {
            return page
                .child(card(ui).child(kit::empty(
                    ui,
                    Icon::Loader,
                    if self.plugins.is_loading() { "Loading plugins…" } else { "No plugins to show" },
                    format!("From {}", self.host_label()),
                )))
                .into_any_element();
        };
        for r in cat.registries.iter().filter(|r| r.error.is_some()) {
            page = page.child(banner(
                ui,
                Tone::Warning,
                format!("Couldn't read the {} registry", r.name),
                vec![r.error.clone().unwrap_or_default()],
            ));
        }
        if !cat.issues.is_empty() {
            page = page.child(banner(ui, Tone::Warning, "Some plugins have problems", cat.issues.clone()));
        }

        let query = self.plugins.filter.as_ref().map(|f| f.read(cx).text().to_lowercase()).unwrap_or_default();
        let matches = |id: &str, name: &str, summary: &str, keywords: &[String]| {
            query.is_empty()
                || id.contains(&query)
                || name.to_lowercase().contains(&query)
                || summary.to_lowercase().contains(&query)
                || keywords.iter().any(|k| k.contains(&query))
        };
        let available: Vec<&api::Available> = cat.available.iter().filter(|a| a.installed.is_none()).collect();
        let tab = self.plugins.tab;
        let installed_label = format!("Installed  {}", cat.installed.len());
        let browse_label = format!("Browse  {}", available.len());
        page = page.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(segmented(
                    "plugins-tab",
                    &[(Tab::Installed, installed_label.as_str()), (Tab::Browse, browse_label.as_str())],
                    &tab,
                    ui,
                    {
                        let this = cx.entity().downgrade();
                        move |t, _, cx| {
                            let _ = this.update(cx, |v, cx| {
                                v.plugins.tab = t;
                                cx.notify();
                            });
                        }
                    },
                ))
                .children(self.plugins.filter.as_ref().map(|f| div().flex_1().child(search_field(ui, "plugin-search", f, window, cx))))
                .child(
                    icon_button(
                        "plugins-refresh",
                        if self.plugins.is_loading() { Icon::Loader } else { Icon::Refresh },
                        "Check the registries for new plugins and updates",
                        ui,
                        self.act(cx, |v, cx| v.load_plugins(true, cx)),
                    )
                    .size(px(38.)),
                ),
        );

        match tab {
            Tab::Installed => {
                let shown = |i: &&Installed| matches(&i.id, &i.manifest.name, &i.manifest.summary, &i.manifest.keywords);
                let updates: Vec<&Installed> = cat.installed.iter().filter(|i| i.update.is_some() && !i.builtin).filter(shown).collect();
                if !updates.is_empty() {
                    page = page.child(self.updates_section(ui, cat, &updates, cx));
                }
                let builtin: Vec<&Installed> = cat.installed.iter().filter(|i| i.builtin).filter(shown).collect();
                let installed: Vec<&Installed> = cat.installed.iter().filter(|i| !i.builtin).filter(shown).collect();
                if !installed.is_empty() || query.is_empty() {
                    let mut s = section(ui, "Installed");
                    if installed.is_empty() {
                        s = s.child(
                            row(ui, "Nothing installed yet")
                                .icon(Icon::Package)
                                .desc("Add tools like eza, zoxide or bat, in goatty's own directory: never system-wide, no sudo")
                                .on_click(
                                    "plugins-browse",
                                    cx.listener(|v, _, _, cx| {
                                        v.plugins.tab = Tab::Browse;
                                        cx.notify();
                                    }),
                                ),
                        );
                    }
                    page = page.child(s.children(installed.into_iter().map(|i| self.plugin_row(ui, i, cx))));
                }
                if !builtin.is_empty() {
                    page = page.child(
                        section(ui, "Built in")
                            .desc("Come with goatty: switch them off instead of uninstalling")
                            .children(builtin.into_iter().map(|i| self.plugin_row(ui, i, cx))),
                    );
                }
                if query.is_empty() {
                    let on = cx.global::<AppSettings>().file.value.plugins.notify_updates;
                    page = page.child(
                        section(ui, "Notifications").child(
                            row(ui, "Tell me about plugin updates")
                                .desc("Checks the active tab's host every few hours; nothing installs without a click")
                                .control(toggle("plugins-notify-updates", on, ui, |_, cx| {
                                    AppSettings::update(cx, |s| s.plugins.notify_updates = !s.plugins.notify_updates);
                                })),
                        ),
                    );
                }
            }
            Tab::Browse => {
                let list: Vec<&&api::Available> = available.iter().filter(|a| matches(&a.id, &a.name, &a.summary, &a.keywords)).collect();
                if list.is_empty() {
                    let (title, why) = if cat.registries.iter().all(|r| r.sequence.is_none()) {
                        ("No registry could be read", "Check the connection and press refresh.")
                    } else if !query.is_empty() {
                        ("No plugin matches", "Search looks at names, summaries and keywords.")
                    } else {
                        ("You have them all", "Every plugin the registries offer is installed.")
                    };
                    page = page.child(card(ui).child(kit::empty(ui, Icon::Puzzle, title, why)));
                } else {
                    page = page
                        .child(div().grid().grid_cols(2).gap(px(12.)).children(list.into_iter().map(|a| self.available_card(ui, a, cx))));
                }
            }
        }
        let regs: Vec<String> = cat
            .registries
            .iter()
            .map(|r| match r.sequence {
                Some(_) => format!("{} ({} plugins)", r.name, r.plugins),
                None => format!("{} (not fetched)", r.name),
            })
            .collect();
        page.child(kit::hint(ui, format!("Registries: {}. Add one with goattyd plugin registry add.", regs.join(", "))).px(px(4.)))
            .into_any_element()
    }

    /// The installed plugins with a newer version: each with its own
    /// Update (one click), and Update all. Updates that change permissions
    /// or shell code open their review instead of installing.
    fn updates_section(&self, ui: Ui, cat: &Catalog, updates: &[&Installed], cx: &Context<'_, Self>) -> AnyElement {
        let idle: Vec<(String, String)> = updates
            .iter()
            .filter(|i| self.plugins.update_state(&i.id).is_none())
            .filter_map(|i| Some((i.id.clone(), i.update.clone()?)))
            .collect();
        let waiting = updates.iter().filter(|i| self.plugins.update_state(&i.id) == Some(UpdateState::NeedsReview)).count();
        let running = self.plugins.batch.is_some();
        let desc = match (running, waiting) {
            (true, _) => "Updating one after the other…".to_string(),
            (false, 0) => format!("{} available", updates.len()),
            (false, 1) => "One waits for your review".to_string(),
            (false, n) => format!("{n} wait for your review"),
        };
        let mut s = section(ui, "Updates").desc(desc);
        // One action, by what makes sense now.
        if running {
            s = s.action(button_with(
                "plugins-update-all",
                Some(Icon::Close),
                "Stop updates",
                ButtonKind::Normal,
                ui,
                self.act_and_focus(cx, None, |v, cx| v.stop_updates(cx)),
            ));
        } else if idle.len() > 1 {
            let n = idle.len();
            s = s.action(button_with(
                "plugins-update-all",
                Some(Icon::Download),
                format!("Update all {n}"),
                ButtonKind::Primary,
                ui,
                self.act_and_focus(cx, None, move |v, cx| v.update_plugins(idle.clone(), cx)),
            ));
        } else if waiting > 0 && idle.is_empty() {
            s = s.action(button_with(
                "plugins-update-all",
                Some(Icon::Shield),
                if waiting == 1 { "Review".to_string() } else { format!("Review {waiting}") },
                ButtonKind::Primary,
                ui,
                self.act_and_focus(cx, None, |v, cx| v.open_queued_review(cx)),
            ));
        }
        // After a row's button turns into a state badge, focus moves to the
        // next row's button (or the page's first control).
        let next_idle = |id: &str| -> Option<SharedString> {
            let ids: Vec<&String> = updates.iter().filter(|i| self.plugins.update_state(&i.id).is_none()).map(|i| &i.id).collect();
            let at = ids.iter().position(|i| *i == id)?;
            ids.get(at + 1).map(|i| SharedString::from(format!("update-{i}")))
        };
        s.children(updates.iter().map(|inst| self.update_row(ui, cat, inst, next_idle(&inst.id), cx)))
            .footer("New permissions, programs or shell code wait for your review; other updates install with one click.")
            .into_any_element()
    }

    /// Like [`act`](Self::act), then moves keyboard focus to the control
    /// `to` (or the page's first one): the clicked button goes away.
    fn act_and_focus(
        &self,
        cx: &Context<'_, Self>,
        to: Option<SharedString>,
        f: impl Fn(&mut Self, &mut Context<'_, Self>) + 'static,
    ) -> impl Fn(&mut Window, &mut gpui::App) + 'static {
        let this = cx.entity().downgrade();
        move |window, cx| {
            let _ = this.update(cx, |v, cx| {
                f(v, cx);
                let target = to.clone().map_or(focus::Handover::FirstOnPage, |id| focus::Handover::Control(id.into()));
                focus::hand_over(&v.focus, target, window, cx);
            });
        }
    }

    /// One update: old → new, what to expect, and its button or state.
    fn update_row(&self, ui: Ui, cat: &Catalog, inst: &Installed, next: Option<SharedString>, cx: &Context<'_, Self>) -> AnyElement {
        let newer = inst.update.clone().unwrap_or_default();
        let new_perms = new_permissions(cat, inst);
        let name = inst.manifest.name.clone();
        let mut desc = format!("{} → {newer}", inst.version);
        if !new_perms.is_empty() {
            let short: Vec<&str> = new_perms.iter().map(|p| short_permission(*p)).collect();
            desc.push_str(&format!(" · new: {}", short.join(", ")));
        }
        let id = inst.id.clone();
        let control = match self.plugins.update_state(&id) {
            Some(UpdateState::Queued) => badge("Queued", Tone::Neutral, ui).into_any_element(),
            Some(UpdateState::Checking) => badge("Checking…", Tone::Neutral, ui).into_any_element(),
            Some(UpdateState::Installing) => badge("Updating…", Tone::Accent, ui).into_any_element(),
            Some(UpdateState::NeedsReview) => button_with(
                SharedString::from(format!("update-review-{id}")),
                Some(Icon::Shield),
                "Review",
                ButtonKind::Primary,
                ui,
                self.act_and_focus(cx, None, move |v, cx| v.open_review_for(&id, cx)),
            )
            .aria_label(SharedString::from(format!("Review the {name} update")))
            .into_any_element(),
            None => {
                let item = vec![(id.clone(), newer.clone())];
                let review = !new_perms.is_empty();
                button_with(
                    SharedString::from(format!("update-{id}")),
                    Some(if review { Icon::Shield } else { Icon::Download }),
                    if review { "Review" } else { "Update" },
                    ButtonKind::Normal,
                    ui,
                    self.act_and_focus(cx, next, move |v, cx| v.update_plugins(item.clone(), cx)),
                )
                .aria_label(SharedString::from(if review {
                    format!("Review the {name} update")
                } else {
                    format!("Update {name} to {newer}")
                }))
                .into_any_element()
            }
        };
        row(ui, name.clone())
            .icon(if new_perms.is_empty() { Icon::Package } else { Icon::Shield })
            .desc(desc)
            .control(control)
            .into_any_element()
    }

    /// Show the waiting review of `id`'s update (from the list, or the
    /// plugin's page, which it returns to).
    pub fn open_review_for(&mut self, id: &str, cx: &mut Context<'_, Self>) {
        if self.plugins.plan.as_ref().is_some_and(|p| p.id == id) {
            return;
        }
        if let Some(i) = self.plugins.review_queue.iter().position(|p| p.id == id)
            && let Some(p) = self.plugins.review_queue.remove(i)
        {
            self.plugins.clear_review();
            self.plugins.review_queue.push_front(p);
        }
        self.open_queued_review(cx);
    }

    /// The followed install: what it's doing, how far, and Cancel (Stop
    /// all, in a batch: a cancel stops the rest too).
    pub(super) fn install_progress(&self, ui: Ui, f: &Follow, cx: &Context<'_, Self>) -> AnyElement {
        let connected = self.link.as_ref().is_some_and(HostLink::is_connected);
        let cancellable = connected && !f.no_progress && !f.cancelling() && f.progress.as_ref().is_none_or(|p| p.step.cancellable());
        let title = format!("{} {}", f.verb, f.name());
        let queued = self.plugins.batch.as_ref().map_or(0, Batch::queued);
        let label = if f.batch && queued > 0 { "Stop all" } else { "Cancel" };
        // Unavailable, not hidden (and focus stays on it): the
        // description (step_text) and the tooltip say why it's off.
        let cancel = if cancellable {
            button(
                "plugin-install-cancel",
                label,
                ButtonKind::Ghost,
                ui,
                self.act(cx, |v, cx| if v.plugins.batch.is_some() { v.stop_updates(cx) } else { v.cancel_install(cx) }),
            )
        } else {
            let why = if !connected {
                "Not connected to the host"
            } else if f.no_progress {
                "This host's goattyd can't cancel installs"
            } else if f.cancelling() {
                "Already cancelling"
            } else {
                "It's being set up: too late to cancel"
            };
            button("plugin-install-cancel", label, ButtonKind::Ghost, ui, |_, _| {}).opacity(0.45).cursor_default().tooltip(tooltip(why))
        };
        let mut desc = step_text(f, connected);
        if queued > 0 {
            desc.push_str(&format!(" · {queued} more after this"));
        }
        let r = row(ui, title.clone())
            .icon(Icon::Download)
            .desc(desc)
            .below(progress_bar("plugin-install-progress", step_fraction(f), title, ui).mt(px(4.)))
            .control(cancel);
        kit::group(ui).child(r).into_any_element()
    }

    /// The batch's state while no install runs (asking the host what an
    /// update changes): the same card, so the page doesn't jump.
    pub(super) fn batch_progress(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<AnyElement> {
        if self.plugins.follow.is_some() {
            return None;
        }
        let b = self.plugins.batch.as_ref()?;
        let id = b.current.as_ref()?;
        let name = self
            .plugins
            .catalog
            .as_ref()
            .and_then(|c| c.installed.iter().find(|i| &i.id == id))
            .map_or_else(|| id.clone(), |i| i.manifest.name.clone());
        let mut desc = "Asking the host what the update changes".to_string();
        if b.queued() > 0 {
            desc.push_str(&format!(" · {} more after this", b.queued()));
        }
        let title = format!("Checking {name}");
        let r = row(ui, title.clone())
            .icon(Icon::Download)
            .desc(desc)
            .below(progress_bar("plugin-install-progress", None, title, ui).mt(px(4.)))
            .control(button("plugin-install-cancel", "Stop all", ButtonKind::Ghost, ui, self.act(cx, |v, cx| v.stop_updates(cx))));
        Some(kit::group(ui).child(r).into_any_element())
    }

    /// The round icon standing for a plugin.
    pub(super) fn plugin_avatar(ui: Ui, category: Category, builtin: bool, size: f32) -> impl IntoElement {
        let icon = match category {
            Category::Application => Icon::AppWindow,
            Category::Environment if builtin => Icon::Zap,
            Category::Environment => Icon::Package,
        };
        div()
            .flex_shrink_0()
            .size(px(size))
            .rounded(px(size * 0.3))
            .bg(ui.accent_soft)
            .flex()
            .items_center()
            .justify_center()
            .child(icon.svg().size(px(size * 0.5)).text_color(ui.accent_text))
    }

    fn enabled(&self, id: &str) -> bool {
        self.host_value().and_then(|h| h.plugins.get(id)).is_none_or(|p| p.enabled)
    }

    /// An installed plugin in the list: opens its page.
    fn plugin_row(&self, ui: Ui, inst: &Installed, cx: &Context<'_, Self>) -> AnyElement {
        let id = inst.id.clone();
        let enabled = self.enabled(&id);
        let missing = inst.packages.iter().any(|p| matches!(p.state, PackageState::Missing));
        let fetch = inst.builtin && inst.packages.iter().any(|p| p.available && p.needs_install());
        let open_id = id.clone();
        let row_id: gpui::ElementId = SharedString::from(format!("plugin-{id}")).into();
        let row = div()
            .id(row_id.clone())
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(inst.manifest.name.clone()))
            .flex()
            .items_center()
            .gap(px(14.))
            .min_h(px(64.))
            .px(px(16.))
            .py(px(10.))
            .cursor_pointer()
            .rounded(px(radius::CARD))
            .hover(|s| s.bg(ui.surface_hover))
            .child(Self::plugin_avatar(ui, inst.manifest.category, inst.builtin, 38.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(text::BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if enabled { ui.text } else { ui.muted })
                                    .child(SharedString::from(inst.manifest.name.clone())),
                            )
                            .when_some(inst.update.clone(), |d, v| {
                                d.child(match self.plugins.update_state(&inst.id) {
                                    Some(UpdateState::Installing) => badge("Updating…", Tone::Accent, ui),
                                    Some(UpdateState::NeedsReview) => badge("Update needs review", Tone::Warning, ui),
                                    Some(UpdateState::Queued | UpdateState::Checking) => badge("Update queued", Tone::Neutral, ui),
                                    None => badge(format!("Update {v}"), Tone::Accent, ui),
                                })
                            })
                            .when(fetch && enabled, |d| d.child(badge(if missing { "Needs install" } else { "Update" }, Tone::Warning, ui)))
                            .when(inst.incompatible.is_some(), |d| d.child(badge("Not for this host", Tone::Danger, ui)))
                            .when(!inst.verified && !inst.builtin, |d| d.child(badge("Unverified", Tone::Warning, ui))),
                    )
                    .child(
                        div()
                            .text_size(px(text::SMALL))
                            .text_color(ui.muted)
                            .truncate()
                            .child(SharedString::from(inst.manifest.summary.clone())),
                    ),
            )
            .child({
                let id = id.clone();
                div().on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
                    toggle(
                        SharedString::from(format!("enabled-{id}")),
                        enabled,
                        ui,
                        self.act(cx, move |v, cx| v.edit_plugin(&id, cx, move |p| p.enabled = !enabled)),
                    )
                    .aria_label(SharedString::from(format!("{} plugin", inst.manifest.name))),
                )
            })
            .child(Icon::ChevronRight.svg().size(px(16.)).text_color(ui.faint))
            .on_click(cx.listener(move |v, _, _, cx| {
                v.plugins.detail = Some(open_id.clone());
                v.plugins.confirm_uninstall = None;
                v.plugins.show_advanced = false;
                v.scroll.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            }));
        focus::tab(row, &row_id, ui, Ring::Inner, Scroller::Page).into_any_element()
    }

    fn perm_icons(ui: Ui, perms: &[Permission]) -> impl IntoElement {
        let item = |p: Permission, icon: Icon, tone: gpui::Hsla| {
            perms.contains(&p).then(|| {
                div()
                    .id(SharedString::from(format!("perm-{}", p.key())))
                    .size(px(24.))
                    .rounded_full()
                    .bg(tone.opacity(0.12))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon.svg().size(px(13.)).text_color(tone))
                    .tooltip(tooltip(p.title()))
            })
        };
        div()
            .flex()
            .gap(px(4.))
            .children(item(Permission::Binaries, Icon::Package, ui.muted))
            .children(item(Permission::ShellCode, Icon::Code, ui.warning))
            .children(item(Permission::Network, Icon::Globe, ui.muted))
    }

    fn available_card(&self, ui: Ui, a: &api::Available, cx: &Context<'_, Self>) -> AnyElement {
        let (id, version) = (a.id.clone(), a.version.clone());
        card(ui)
            .p(px(16.))
            .gap(px(10.))
            .child(
                div().flex().items_center().gap(px(12.)).child(Self::plugin_avatar(ui, a.category, false, 38.)).child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(text::BODY))
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(SharedString::from(a.name.clone())),
                        )
                        .child(
                            div()
                                .text_size(px(text::CAPTION))
                                .text_color(ui.muted)
                                .truncate()
                                .child(SharedString::from(format!("{} · {}", a.publisher, a.version))),
                        ),
                ),
            )
            .child(
                div().h(px(40.)).text_size(px(text::SMALL)).text_color(ui.muted).line_clamp(2).child(SharedString::from(a.summary.clone())),
            )
            .child(
                div().flex().items_center().justify_between().child(Self::perm_icons(ui, &a.permissions)).child(match &a.incompatible {
                    Some(why) => div()
                        .id(SharedString::from(format!("incompat-{id}")))
                        .child(badge("Not for this host", Tone::Neutral, ui))
                        .tooltip(tooltip(why.clone()))
                        .into_any_element(),
                    None => button_with(
                        SharedString::from(format!("review-{id}")),
                        Some(Icon::Download),
                        "Install",
                        ButtonKind::Normal,
                        ui,
                        self.act(cx, move |v, cx| v.review_plugin(id.clone(), Some(version.clone()), cx)),
                    )
                    .into_any_element(),
                }),
            )
            .into_any_element()
    }

    // ---- A plugin's page ----------------------------------------------------

    fn plugin_detail(&self, ui: Ui, inst: &Installed, cx: &Context<'_, Self>) -> AnyElement {
        let id = inst.id.clone();
        let enabled = self.enabled(&id);
        let confirming = self.plugins.confirm_uninstall.as_deref() == Some(id.as_str());
        let fetch = inst.builtin && inst.packages.iter().any(|p| p.available && p.needs_install());
        let outdated = inst.packages.iter().any(|p| matches!(&p.state, PackageState::Managed { version } if *version != p.version));
        let source = if inst.builtin {
            "Built into goatty".to_string()
        } else if inst.verified {
            format!("From the {} registry", inst.source)
        } else {
            "Installed from a local file, unverified".to_string()
        };
        let mut by = format!("Version {} · {}", inst.version, inst.manifest.publisher);
        if let Some(l) = &inst.manifest.license {
            by.push_str(&format!(" · {l}"));
        }

        let mut actions = div().flex().flex_wrap().items_center().gap(px(8.));
        if let Some(newer) = inst.update.clone() {
            let (id, v) = (id.clone(), newer.clone());
            actions = actions.child(match self.plugins.update_state(&id) {
                Some(UpdateState::NeedsReview) => button_with(
                    SharedString::from(format!("update-{id}")),
                    Some(Icon::Shield),
                    format!("Review the update to {newer}"),
                    ButtonKind::Primary,
                    ui,
                    self.act_and_focus(cx, None, move |view, cx| view.open_review_for(&id, cx)),
                )
                .into_any_element(),
                Some(state) => badge(
                    match state {
                        UpdateState::Installing => format!("Updating to {newer}…"),
                        UpdateState::Checking => "Checking the update…".into(),
                        _ => "Update queued".into(),
                    },
                    Tone::Accent,
                    ui,
                )
                .into_any_element(),
                None => {
                    let review = self.plugins.catalog.as_ref().is_some_and(|c| !new_permissions(c, inst).is_empty());
                    button_with(
                        SharedString::from(format!("update-{id}")),
                        Some(if review { Icon::Shield } else { Icon::Download }),
                        if review { format!("Review the update to {newer}") } else { format!("Update to {newer}") },
                        ButtonKind::Primary,
                        ui,
                        self.act_and_focus(cx, None, move |view, cx| view.update_plugins(vec![(id.clone(), v.clone())], cx)),
                    )
                    .into_any_element()
                }
            });
        }
        if fetch {
            let id = id.clone();
            actions = actions.child(button_with(
                SharedString::from(format!("fetch-{id}")),
                Some(Icon::Download),
                if outdated { "Update the tool" } else { "Install the tool" },
                ButtonKind::Primary,
                ui,
                self.act(cx, move |v, cx| v.review_plugin(id.clone(), None, cx)),
            ));
        }
        if let Some(home) = inst.manifest.homepage.clone() {
            actions =
                actions.child(button_with("plugin-home", Some(Icon::ExternalLink), "Website", ButtonKind::Ghost, ui, move |_, cx| {
                    cx.open_url(&home)
                }));
        }
        actions = actions.child(div().flex_1());
        if !inst.builtin {
            let id = id.clone();
            actions = actions.child(button_with(
                SharedString::from(format!("uninstall-{id}")),
                Some(Icon::Trash),
                if confirming { "Really uninstall?" } else { "Uninstall" },
                ButtonKind::Danger,
                ui,
                self.act(cx, move |v, cx| {
                    if v.plugins.confirm_uninstall.as_deref() == Some(id.as_str()) {
                        v.uninstall_plugin(id.clone(), cx);
                        v.plugins.detail = None;
                    } else {
                        v.plugins.confirm_uninstall = Some(id.clone());
                        cx.notify();
                    }
                }),
            ));
        }
        let hero = card(ui)
            .p(px(20.))
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(Self::plugin_avatar(ui, inst.manifest.category, inst.builtin, 52.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div().text_size(px(text::BODY)).font_weight(FontWeight::MEDIUM).child(SharedString::from(source)),
                                    )
                                    .when(inst.verified && !inst.builtin, |d| {
                                        d.child(
                                            div()
                                                .id("verified")
                                                .child(Icon::ShieldCheck.svg().size(px(16.)).text_color(ui.success))
                                                .tooltip(tooltip("Signed by its registry")),
                                        )
                                    }),
                            )
                            .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(by))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(4.))
                            .child(
                                toggle(SharedString::from(format!("enabled-{id}")), enabled, ui, {
                                    let id = id.clone();
                                    self.act(cx, move |v, cx| v.edit_plugin(&id, cx, move |p| p.enabled = !enabled))
                                })
                                .aria_label(SharedString::from(format!("{} plugin", inst.manifest.name))),
                            )
                            .child(div().text_size(px(text::CAPTION)).text_color(ui.muted).child(if enabled { "On" } else { "Off" })),
                    ),
            )
            .when(!inst.manifest.description.trim().is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(text::BODY))
                        .text_color(ui.text)
                        .child(SharedString::from(inst.manifest.description.trim().to_string())),
                )
            })
            .when_some(inst.incompatible.clone(), |d, why| d.child(banner(ui, Tone::Warning, "Off on this host", vec![why])))
            .child(actions);

        let mut page = div().flex().flex_col().gap(px(28.)).child(hero);

        // Settings, in the manifest's groups; advanced ones behind an expander.
        let settings: Vec<&Setting> = inst.manifest.settings.iter().collect();
        if !settings.is_empty() {
            let values = self.plugin_values(inst);
            let mut groups: Vec<(Option<String>, Vec<&Setting>)> = Vec::new();
            for s in settings.iter().filter(|s| !s.advanced) {
                match groups.iter_mut().find(|(g, _)| *g == s.group) {
                    Some((_, v)) => v.push(s),
                    None => groups.push((s.group.clone(), vec![s])),
                }
            }
            for (g, items) in groups {
                let title = g.unwrap_or_else(|| "Settings".into());
                page = page
                    .child(section(ui, title).children(items.into_iter().map(|s| self.setting_row(ui, inst, s, &values, !enabled, cx))));
            }
            let advanced: Vec<&&Setting> = settings.iter().filter(|s| s.advanced).collect();
            if !advanced.is_empty() {
                let open = self.plugins.show_advanced;
                let mut s = section(ui, "Advanced").child(
                    row(ui, if open { "Hide advanced settings" } else { "Show advanced settings" })
                        .desc(format!("{} more, for special setups", advanced.len()))
                        .on_click(
                            "plugin-advanced",
                            cx.listener(|v, _, _, cx| {
                                v.plugins.show_advanced = !v.plugins.show_advanced;
                                cx.notify();
                            }),
                        ),
                );
                if open {
                    s = s.children(advanced.into_iter().map(|st| self.setting_row(ui, inst, st, &values, !enabled, cx)));
                }
                page = page.child(s);
            }
            if !enabled {
                page = page.child(kit::hint(ui, "Turn the plugin on to use these settings.").px(px(4.)));
            }
        }

        // The programs it needs.
        if !inst.packages.is_empty() {
            page = page.child(section(ui, "Tools").children(inst.packages.iter().map(|p| {
                let (desc, tone, label) = match &p.state {
                    PackageState::System { path } => (format!("Installed on the system: {path}"), Tone::Success, "Installed"),
                    PackageState::Managed { version } if *version == p.version => {
                        (format!("goatty's copy, version {version}"), Tone::Success, "Installed")
                    }
                    PackageState::Managed { version } => {
                        (format!("goatty's copy, version {version}; {} is available", p.version), Tone::Accent, "Update")
                    }
                    PackageState::Missing if p.available => {
                        ("Not installed: goatty can download it into its own directory".into(), Tone::Warning, "Missing")
                    }
                    PackageState::Missing => {
                        ("Not installed, and goatty has no build for this host: use your package manager".into(), Tone::Danger, "Missing")
                    }
                };
                row(ui, p.id.clone()).icon(Icon::Package).desc(desc).control(badge(label, tone, ui))
            })));
        }

        // What it may do.
        let granted = permissions::describe(&inst.manifest);
        if !granted.is_empty() {
            page = page.child(section(ui, "Permissions").children(granted.into_iter().map(|g| {
                let icon = perm_icon(g.permission);
                row(ui, g.title.clone()).icon(icon).desc_opt((!g.details.is_empty()).then(|| g.details.join(", ")))
            })));
        }

        // Its aliases live on the Aliases page.
        if !inst.manifest.aliases.is_empty() {
            let name = inst.manifest.name.clone();
            page = page.child(
                section(ui, "Aliases").child(
                    row(ui, format!("{} aliases", inst.manifest.aliases.len()))
                        .icon(Icon::Link2)
                        .desc(inst.manifest.aliases.iter().take(6).map(|a| a.name.clone()).collect::<Vec<_>>().join("  ·  "))
                        .on_click(
                            "plugin-aliases",
                            cx.listener(move |v, _, _, cx| {
                                v.go(Page::Aliases, cx);
                                v.show_plugin_aliases(&name, cx);
                            }),
                        ),
                ),
            );
        }
        page.into_any_element()
    }

    /// One schema-driven control.
    fn setting_row(&self, ui: Ui, inst: &Installed, s: &Setting, values: &schema::Values, off: bool, cx: &Context<'_, Self>) -> AnyElement {
        let id = inst.id.clone();
        let key = s.id.clone();
        let value = values.get(&s.id).cloned().unwrap_or(Value::Null);
        let prefix = format!("ps-{id}-{key}");
        let elem_id = |suffix: &str| SharedString::from(format!("{prefix}{suffix}"));
        let this = cx.entity().downgrade();
        let set = move |v: Option<Value>| {
            let (this, id, key) = (this.clone(), id.clone(), key.clone());
            move |_: &mut gpui::Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |view, cx| view.set_plugin_value(&id, &key, v.clone(), cx));
            }
        };
        let r = row(ui, s.label.clone()).desc_opt((!s.help.is_empty()).then(|| s.help.clone())).disabled(off);
        let r = match s.kind {
            SettingKind::Bool => {
                let on = value.as_bool().unwrap_or(false);
                r.control(toggle(elem_id(""), on, ui, set(Some(Value::Bool(!on)))).aria_label(s.label.clone()))
            }
            SettingKind::Choice => {
                let selected = schema::plain_string(&value);
                let short = s.choices.len() <= 3
                    && s.choices.iter().map(|c| c.label.len()).sum::<usize>() <= 24
                    && s.choices.iter().all(|c| c.help.is_empty());
                let this = cx.entity().downgrade();
                let (pid, key) = (inst.id.clone(), s.id.clone());
                let pick = move |v: String, _: &mut gpui::Window, cx: &mut gpui::App| {
                    let _ = this.update(cx, |view, cx| view.set_plugin_value(&pid, &key, Some(Value::String(v)), cx));
                };
                if short {
                    let options: Vec<(String, &str)> = s.choices.iter().map(|c| (c.value.clone(), c.label.as_str())).collect();
                    r.control(segmented(&elem_id(""), &options, &selected, ui, pick))
                } else {
                    let options = s
                        .choices
                        .iter()
                        .map(|c| {
                            if c.help.is_empty() {
                                opt(c.value.clone(), c.label.clone())
                            } else {
                                opt(c.value.clone(), c.label.clone()).desc(c.help.clone())
                            }
                        })
                        .collect();
                    r.control(self.select(&elem_id(""), &s.label, options, &selected, cx, pick))
                }
            }
            SettingKind::Integer => {
                let n = value.as_i64().unwrap_or(0);
                let step = s.step.unwrap_or(1);
                let clamp = |x: i64| x.clamp(s.min.unwrap_or(i64::MIN), s.max.unwrap_or(i64::MAX));
                let shown = match &s.unit {
                    Some(u) => format!("{n} {u}"),
                    None => n.to_string(),
                };
                r.control(stepper(
                    &elem_id(""),
                    shown,
                    ui,
                    set(Some(Value::from(clamp(n - step)))),
                    set(Some(Value::from(clamp(n + step)))),
                ))
            }
            SettingKind::String | SettingKind::Path | SettingKind::List => {
                match self.plugins.inputs.get(&format!("{}.{}", inst.id, s.id)) {
                    Some(input) => r.below(input.clone()),
                    None => r.control(div().text_size(px(text::SMALL)).child(SharedString::from(schema::plain_string(&value)))),
                }
            }
        };
        r.into_any_element()
    }
}

/// Permissions `inst`'s update asks for that the installed version
/// doesn't have, from the registry's listing (before any plan is asked for).
fn new_permissions(cat: &Catalog, inst: &Installed) -> Vec<Permission> {
    let Some(a) = cat.available.iter().find(|a| a.id == inst.id && inst.update.as_ref() == Some(&a.version)) else { return Vec::new() };
    let old = permissions::all(&inst.manifest);
    a.permissions.iter().filter(|p| !old.contains(p)).copied().collect()
}

/// A permission in a word or two, for the Updates list.
fn short_permission(p: Permission) -> &'static str {
    match p {
        Permission::Binaries => "programs",
        Permission::ShellCode => "shell code",
        Permission::Network => "network",
        Permission::Aliases => "aliases",
        Permission::Env => "variables",
        Permission::ConfigFiles => "config files",
        Permission::Commands => "commands",
        Permission::Snippets => "snippets",
        Permission::SnippetSources => "snippet sources",
    }
}

pub(super) fn perm_icon(p: Permission) -> Icon {
    match p {
        Permission::Binaries => Icon::Package,
        Permission::ShellCode => Icon::Code,
        Permission::Network => Icon::Globe,
        Permission::Aliases => Icon::Link2,
        Permission::Env => Icon::Sliders,
        Permission::ConfigFiles => Icon::FileText,
        Permission::Commands => Icon::SquareTerminal,
        Permission::Snippets => Icon::TextCursorInput,
        Permission::SnippetSources => Icon::Zap,
    }
}

fn changed(r: api::Reply) -> Option<api::Changed> {
    match r {
        api::Reply::Changed(c) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_plan(id: &str) -> Box<Plan> {
        Box::new(Plan {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            summary: String::new(),
            description: String::new(),
            publisher: "test".into(),
            homepage: None,
            license: None,
            source: "test".into(),
            verified: true,
            builtin: false,
            replaces: None,
            downloads: Vec::new(),
            permissions: Vec::new(),
            aliases: Vec::new(),
            env: Vec::new(),
            files: Vec::new(),
            shell_code: None,
            requires: Vec::new(),
            warnings: Vec::new(),
            dependencies: Vec::new(),
            digest: "sha256:test".into(),
            previous: None,
        })
    }

    #[test]
    fn stale_review_completion_does_not_overwrite_newer_review_or_navigation() {
        let mut page = PluginsPage::default();
        let req_a = page.begin_review(Some(Page::Profile));
        page.clear_review();
        let req_b = page.begin_review(Some(Page::Profile));

        // Stale failure or completion for A must be ignored and keep B's plan_return.
        assert_eq!(page.fail_review(req_a), None);
        assert!(!page.accept_review_plan(req_a, test_plan("a")));
        assert!(page.plan.is_none());

        // Active review B succeeds and keeps its return page until consumed.
        assert!(page.accept_review_plan(req_b, test_plan("b")));
        assert_eq!(page.plan.as_ref().map(|p| p.id.as_str()), Some("b"));
        assert_eq!(page.take_plan_return(req_a), None);
        assert_eq!(page.take_plan_return(req_b), Some(Page::Profile));
    }

    #[test]
    fn dismiss_plan_clears_plan_and_returns_origin_page() {
        let mut page = PluginsPage::default();
        let req = page.begin_review(Some(Page::Profile));
        assert!(page.accept_review_plan(req, test_plan("greet")));
        assert_eq!(page.dismiss_plan(), Some(Some(Page::Profile)));
        assert!(page.plan.is_none());
        assert_eq!(page.take_plan_return(req), None);
    }

    #[test]
    fn queued_reviews_count_and_back_keeps_them_waiting() {
        let mut page = PluginsPage::default();
        page.queue_reviews(vec![test_plan("a"), test_plan("b"), test_plan("a")]);
        assert_eq!(page.review_queue.len(), 2, "one review per plugin");
        assert!(page.show_next_review());
        assert_eq!(page.review_position(), Some((1, 2)));
        assert_eq!(page.update_state("b"), Some(UpdateState::NeedsReview));

        // Back (or another page) keeps the shown review waiting, in front.
        page.clear_review();
        assert!(page.plan.is_none());
        assert_eq!(page.review_queue.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);

        // Skip drops it and goes on to the next, then back to the list.
        assert!(page.show_next_review());
        assert_eq!(page.dismiss_plan(), Some(None));
        assert_eq!(page.plan.as_ref().map(|p| p.id.as_str()), Some("b"));
        assert_eq!(page.review_position(), Some((2, 2)));
        assert_eq!(page.dismiss_plan(), Some(None));
        assert!(page.plan.is_none() && page.review_queue.is_empty());
        assert_eq!(page.review_position(), None);
    }

    #[test]
    fn a_browse_install_review_leaves_the_update_queue_alone() {
        let mut page = PluginsPage::default();
        page.queue_reviews(vec![test_plan("upd")]);
        let req = page.begin_review(Some(Page::Profile));
        assert!(page.accept_review_plan(req, test_plan("new")));
        assert_eq!(page.review_position(), None);
        // Cancel goes back where it came from, not into the queued update.
        assert_eq!(page.dismiss_plan(), Some(Some(Page::Profile)));
        assert!(page.plan.is_none());
        assert_eq!(page.review_queue.len(), 1);
        // Leaving a Browse review doesn't queue it either.
        let req = page.begin_review(None);
        assert!(page.accept_review_plan(req, test_plan("new")));
        page.clear_review();
        assert_eq!(page.review_queue.len(), 1);
    }

    #[test]
    fn a_batch_reports_once() {
        let mut b = Batch::default();
        assert_eq!(b.summary(), None, "nothing done: nothing to report");
        b.done.push((true, "eza 1.1.0".into()));
        b.lines.push("Updated eza to 1.1.0; reload running shells to use it".into());
        assert_eq!(b.summary(), Some((true, b.lines.clone())), "one update keeps the host's own report");
        b.done.push((true, "zoxide 0.9.8".into()));
        b.failed.push("bat 0.25: no network".into());
        let (ok, lines) = b.summary().unwrap();
        assert!(!ok);
        assert_eq!(lines, ["Updated 2 plugins; 1 failed", "Updated eza 1.1.0, zoxide 0.9.8", "Failed: bat 0.25: no network"]);

        let stopped = Batch { stopped: true, ..Default::default() };
        assert_eq!(stopped.summary(), Some((false, vec!["Stopped: nothing was updated".to_string()])));

        let page = PluginsPage {
            batch: Some(Batch { queue: [Job::Check { id: "eza".into(), version: "1.1.0".into() }].into(), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(page.update_state("eza"), Some(UpdateState::Queued));
        assert_eq!(page.update_state("bat"), None);
    }
}
