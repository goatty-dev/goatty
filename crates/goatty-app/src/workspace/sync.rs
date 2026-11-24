//! Workspaces (#215) on the window's side: each host the window has tabs
//! on shows one of that goattyd's workspaces ([`Shown`]), shared with every
//! other window that shows it.
//!
//! * Saving: whenever the tabs change, [`Workspace::save_layout`] sends each
//!   host's tabs as a [`Layout`], made from the revision the window last
//!   heard. A conflict brings the newest layout, which is merged with the
//!   window's own edit ([`saved::merge`]) and saved again.
//! * Another window's change comes as `WorkspaceEvent::Changed`: merged the
//!   same way and applied by [`Workspace::reconcile`], which keeps panes by
//!   session id, attaches the new ones, and leaves this window's active tab,
//!   focus and zoom alone. Split sizes are saved when a divider drag ends,
//!   and a change waits while one is dragged.
//! * Switching hosts' workspaces parks the tabs of the one left
//!   ([`Parked`]): their sessions stay attached and keep their mirror
//!   terminals current (no rendering), so bells and activity show on the
//!   switcher and coming back is instant. A parked workspace never takes a
//!   session's size: its panes aren't laid out.
//!
//! Not to be confused with [`Workspace`], the window's root view.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use goatty_client::{Connection, ReplyLost, WorkspaceEvent};
use goatty_proto::{SaveOutcome, SessionId, WorkspaceId, WorkspaceInfo, WorkspaceLayout};
use goatty_term::TermSize;
use gpui::{Context, Window};

use super::layout::Node;
use super::saved::{self, Joined, Layout, SavedTab};
use super::{Pane, Tab, Workspace};
use crate::connect::ConnectPlan;
use crate::session::{AttachMode, Session, Spawner};
use crate::terminal::Activity;
use crate::toast::{self, Toast};

/// A workspace this window shows on one host.
pub(super) struct Shown {
    pub conn: Arc<Connection>,
    pub id: WorkspaceId,
    pub revision: u64,
    /// The layout as the daemon has it at `revision`: what the next save is
    /// made from, and the base a change from elsewhere is merged with.
    pub base: Layout,
    /// Its detached tabs (part of what this window saves).
    pub detached: Vec<SavedTab>,
    /// What the window shows of it now, with tabs whose sessions are still
    /// being attached (shown once they are).
    pub target: Layout,
    /// Sessions being attached for `target`: no save meanwhile (the tabs
    /// would look closed).
    pub attaching: HashSet<SessionId>,
    pub saving: bool,
    /// A change from elsewhere that came during a save or a divider drag.
    pub pending: Option<WorkspaceLayout>,
    /// The host's workspaces, as last heard.
    pub list: Vec<WorkspaceInfo>,
    /// The version of `list`: a list older than it (sent before this window
    /// joined a workspace made just now) is stale.
    pub seq: u64,
    /// This showing of the workspace (switching away and back makes a new
    /// one): a save's answer counts only for the one that sent it.
    pub epoch: u64,
    /// The window's tabs as `reconcile` last left them: what attaching the
    /// rest of `target` adds to, merged with edits made meanwhile.
    pub target_shown: Layout,
    /// A layout the daemon refused (not a lost link): not sent again as is.
    pub rejected: Option<Layout>,
    /// Detach this host once its pending edits finish saving cleanly.
    pub detach_after_save: bool,
    /// Last daemon-confirmed name per workspace while an optimistic rename is
    /// in flight, so overlapping failed renames never roll back to an
    /// unconfirmed attempt.
    pub pending_renames: HashMap<WorkspaceId, String>,
}

/// Tabs this window moved out of `workspace` into `into` on `conn`:
/// `workspace` is closed once an accepted save of `into` (started after
/// `after_seq`) holds them all, and only if `workspace` is still at
/// `source_revision` (any tabs added there meanwhile are moved too).
pub(super) struct MoveClose {
    pub conn: Arc<Connection>,
    pub workspace: WorkspaceId,
    pub into: WorkspaceId,
    pub source_revision: u64,
    pub tabs: Vec<u64>,
    pub sessions: HashSet<SessionId>,
    pub after_seq: u64,
}

/// A new [`Shown::epoch`].
fn next_epoch() -> u64 {
    static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Monotonic sequence for ordering workspace saves against pending moves.
fn next_save_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Shown {
    fn new(conn: Arc<Connection>, info: &WorkspaceInfo, revision: u64, base: Layout, (seq, list): (u64, Vec<WorkspaceInfo>)) -> Self {
        Self {
            conn,
            id: info.id,
            revision,
            detached: base.detached.clone(),
            target: base.clone(),
            target_shown: base.clone(),
            epoch: next_epoch(),
            rejected: None,
            detach_after_save: false,
            pending_renames: HashMap::new(),
            base,
            attaching: HashSet::new(),
            saving: false,
            pending: None,
            seq,
            list,
        }
    }

    /// Take the host's list `list` (version `seq`), unless it's older than
    /// one heard already. False when it was stale.
    fn take_list(&mut self, seq: u64, list: Vec<WorkspaceInfo>) -> bool {
        if seq < self.seq {
            return false;
        }
        self.seq = seq;
        self.list = list;
        self.pending_renames.clear();
        true
    }

    pub fn name(&self) -> String {
        self.list.iter().find(|w| w.id == self.id).map_or_else(|| "Workspace".into(), |w| w.name.clone())
    }
}

/// A workspace this window showed on a host and switched away from: its
/// tabs, panes and sessions, kept attached in the background.
pub(super) struct Parked {
    pub conn: Arc<Connection>,
    pub id: WorkspaceId,
    pub base: Layout,
    pub detached: Vec<SavedTab>,
    pub tabs: Vec<Tab>,
    /// Tabs of a closed workspace on their way into the next one this
    /// window shows on that host (whichever it turns out to be).
    pub moving: bool,
    /// When this parked workspace had tabs whose sessions were still attaching
    /// at park/move time: `(instantiated_at_park, full_target)` so returning
    /// preserves those pending tabs while honoring panes that exit while parked.
    pub pending_layout: Option<(Layout, Layout)>,
    /// The [`Shown::epoch`] it had (a late save answer still updates `base`).
    pub epoch: u64,
    /// The tab that was active there (shown again on coming back).
    pub active: Option<u64>,
}

impl Parked {
    /// The most important thing that happened in its tabs meanwhile.
    pub fn activity(&self) -> Option<Activity> {
        self.tabs.iter().filter_map(|t| t.activity).max()
    }
}

/// Where a connection tells this window about its workspaces: queued for
/// the window's task (`Workspace::new`).
pub(super) struct WindowSink(pub flume::Sender<(usize, WorkspaceEvent)>);

impl goatty_client::WorkspaceSink for WindowSink {
    fn workspace(&self, conn: usize, event: WorkspaceEvent) {
        let _ = self.0.send((conn, event));
    }
}

/// Tab `t` lives on `conn`.
pub(super) fn on_conn(t: &Tab, conn: &Arc<Connection>) -> bool {
    t.host.as_ref().and_then(Spawner::connection).is_some_and(|c| Arc::ptr_eq(c, conn))
}

/// The panes of `tabs`, by session id.
fn panes_by_session(tabs: Vec<Tab>, cx: &gpui::App) -> (HashMap<SessionId, Pane>, Vec<Tab>) {
    let mut pool = HashMap::new();
    let mut shells = Vec::new();
    for mut t in tabs {
        for p in std::mem::take(&mut t.panes) {
            match p.view.read(cx).session().daemon_id() {
                Some(id) => {
                    pool.insert(id, p);
                }
                None => drop(p),
            }
        }
        shells.push(t);
    }
    (pool, shells)
}

impl Workspace {
    // ---- lookups ------------------------------------------------------------

    pub(super) fn shown_index(&self, conn: &Arc<Connection>) -> Option<usize> {
        self.shown.iter().position(|s| Arc::ptr_eq(&s.conn, conn))
    }

    fn shown_by_key(&self, key: usize) -> Option<usize> {
        self.shown.iter().position(|s| s.conn.key() == key)
    }

    /// The workspace shown on the active tab's host.
    pub(super) fn active_shown(&self) -> Option<usize> {
        let conn = self.active_tab()?.host.as_ref()?.connection()?;
        self.shown_index(conn)
    }

    /// This window's view of host `i`'s workspace: its tabs and detached tabs.
    pub(super) fn mine(&self, i: usize, cx: &gpui::App) -> Layout {
        let sh = &self.shown[i];
        let tabs = self.tabs.iter().filter(|t| on_conn(t, &sh.conn)).filter_map(|t| super::saved_tab(t, cx)).collect();
        Layout::new(tabs, sh.detached.clone())
    }

    /// This window's full local target for host `i`'s workspace: local edits
    /// made since `target_shown` rebased onto `target`, so tabs whose sessions
    /// are still attaching are preserved while locally closed tabs stay gone.
    pub(super) fn local_target(&self, i: usize, cx: &gpui::App) -> Layout {
        let mine = self.mine(i, cx);
        let sh = &self.shown[i];
        if sh.target.same(&sh.target_shown) { mine } else { saved::merge(&sh.target_shown, &mine, &sh.target) }
    }

    /// A size for attaching sessions: the focused pane's.
    pub(super) fn pane_size(&self, cx: &gpui::App) -> TermSize {
        self.focused_view().map(|v| v.read(cx).session().terminal.size()).map_or(TermSize::new(100, 30), |(c, r)| TermSize::new(c, r))
    }

    // ---- saving -------------------------------------------------------------

    /// Tell each goattyd what this window's tabs on it look like now (only
    /// when they changed), and `reopen` which workspaces the window shows.
    pub(super) fn save_layout(&mut self, cx: &mut Context<'_, Self>) {
        for i in 0..self.shown.len() {
            self.save_shown(i, cx);
        }
        self.record_window(cx);
    }

    /// Whether host `i`'s workspace still has unsaved edits, an in-flight
    /// save/attach, or a queued remote layout that must finish before detaching.
    pub(super) fn shown_needs_save(&self, i: usize, cx: &gpui::App) -> bool {
        let sh = &self.shown[i];
        sh.saving || sh.pending.is_some() || !sh.attaching.is_empty() || !self.mine(i, cx).same(&sh.base)
    }

    /// Save host `i`'s workspace (merging any pending layout and rebasing on
    /// conflict), then detach the host once it is cleanly saved.
    pub(super) fn save_then_detach(&mut self, i: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let conn = self.shown[i].conn.clone();
        let key = conn.key();
        if !self.shown[i].saving
            && let Some(layout) = self.shown[i].pending.take()
        {
            self.apply_remote(key, layout, window, cx);
        }
        let Some(i) = self.shown_index(&conn) else { return };
        self.shown[i].detach_after_save = true;
        self.shown[i].rejected = None;
        self.save_shown(i, cx);
    }

    fn save_shown(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        let sh = &self.shown[i];
        if sh.saving || !sh.attaching.is_empty() || self.divider_dragging {
            return;
        }
        let mine = self.mine(i, cx);
        let sh = &mut self.shown[i];
        if mine.same(&sh.base) {
            if sh.detach_after_save && sh.pending.is_none() {
                sh.detach_after_save = false;
                let epoch = sh.epoch;
                self.later(cx, move |this, window, cx| this.finish_deferred_detach(epoch, window, cx));
            }
            return;
        }
        if sh.rejected.as_ref().is_some_and(|r| mine.same(r)) {
            return;
        }
        sh.saving = true;
        let save_seq = next_save_seq();
        let (conn, id, base, view, epoch) = (sh.conn.clone(), sh.id, sh.revision, self.join.view, sh.epoch);
        let (json, sessions, tabs) = (mine.to_json(), mine.sessions(), u32::try_from(mine.tabs.len()).unwrap_or(u32::MAX));
        let task = cx.background_executor().spawn(async move { conn.save_workspace(view, id, base, tabs, sessions, json) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| this.saved(epoch, save_seq, mine, result, cx));
        })
        .detach();
    }

    /// Save host `i`'s departing tabs now, rebasing on conflict (the window
    /// stops showing that workspace right after because its last tab closed).
    pub(super) fn save_shown_now(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        let mut mine = self.mine(i, cx);
        let sh = &self.shown[i];
        if mine.same(&sh.base) {
            return;
        }
        let (conn, id, mut rev, mut base, view) = (sh.conn.clone(), sh.id, sh.revision, sh.base.clone(), self.join.view);
        cx.background_executor()
            .spawn(async move {
                for _ in 0..5 {
                    let (json, sessions, tabs) = (mine.to_json(), mine.sessions(), u32::try_from(mine.tabs.len()).unwrap_or(u32::MAX));
                    match conn.save_workspace(view, id, rev, tabs, sessions, json) {
                        Ok(SaveOutcome::Saved { .. } | SaveOutcome::Gone) => break,
                        Ok(SaveOutcome::Conflict(wl)) => {
                            let alive: HashSet<SessionId> = wl.sessions.iter().copied().collect();
                            let theirs = Layout::parse(&wl.json, &alive);
                            let merged = saved::merge(&base, &mine, &theirs);
                            if merged.same(&theirs) {
                                break;
                            }
                            base = theirs;
                            rev = wl.revision;
                            mine = merged;
                        }
                        Err(e) => {
                            log::info!("saving a workspace the window left: {e:#}");
                            break;
                        }
                    }
                }
            })
            .detach();
    }

    /// The window is closing with its last tab: it shows no workspace any more.
    pub(super) fn forget_window(&mut self, cx: &mut Context<'_, Self>) {
        let view = self.join.view;
        for sh in self.shown.drain(..) {
            sh.conn.leave_workspace(view);
        }
        let parked = std::mem::take(&mut self.parked);
        for p in parked {
            self.release_tabs(p.tabs, cx);
        }
        self.restoring.clear();
        super::reopen::update(cx, self.window_id, self.reopen_key, None);
    }

    /// A save made by the showing `epoch` was answered (`sent`: what it sent).
    fn saved(&mut self, epoch: u64, save_seq: u64, sent: Layout, result: anyhow::Result<SaveOutcome>, cx: &mut Context<'_, Self>) {
        let Some(i) = self.shown.iter().position(|s| s.epoch == epoch) else {
            // Parked meanwhile: that's what the daemon has now (so coming
            // back doesn't count this edit as new again). A conflict is
            // merged when it's shown again.
            if let Ok(SaveOutcome::Saved { .. }) = result
                && let Some(p) = self.parked.iter_mut().find(|p| p.epoch == epoch)
            {
                let (key, into) = (p.conn.key(), p.id);
                p.base = sent.clone();
                self.close_moved_from(key, into, save_seq, &sent, cx);
            }
            return;
        };
        self.shown[i].saving = false;
        match result {
            Ok(SaveOutcome::Saved { revision }) => {
                let sh = &mut self.shown[i];
                sh.revision = revision;
                sh.rejected = None;
                let ended = sent.tabs.is_empty() && sent.detached.is_empty();
                let (key, into) = (sh.conn.key(), sh.id);
                sh.base = sent.clone();
                self.close_moved_from(key, into, save_seq, &sent, cx);
                if ended {
                    self.ended_here(i, cx);
                    return;
                }
            }
            // Someone saved first: re-apply this window's edit on theirs
            // (unless a newer layout is waiting already).
            Ok(SaveOutcome::Conflict(layout)) => {
                let sh = &mut self.shown[i];
                if sh.pending.as_ref().is_none_or(|p| p.revision < layout.revision) {
                    sh.pending = Some(layout);
                }
            }
            Ok(SaveOutcome::Gone) => {
                self.workspace_gone(i, cx);
                return;
            }
            // The link dropped: the window saves again when it joins again
            // (not now: that would fail at once, over and over).
            Err(e) if !self.shown[i].conn.is_connected() => {
                if std::mem::take(&mut self.shown[i].detach_after_save) {
                    toast::show(cx, Toast::error("Couldn't save the workspace").body(format!("{e:#}")).key("workspace.save"));
                }
                return;
            }
            // Timed out while still connected: apply any queued remote layout
            // and schedule a retry so local edits don't stay unsaved.
            Err(e) if e.downcast_ref::<ReplyLost>().is_some() => {
                log::info!("saving the workspace timed out: {e:#}");
                if std::mem::take(&mut self.shown[i].detach_after_save) {
                    toast::show(cx, Toast::error("Couldn't save the workspace").body(format!("{e:#}")).key("workspace.save"));
                }
                if let Some(layout) = self.shown[i].pending.take() {
                    self.defer_remote(i, layout, cx);
                    return;
                }
                let conn = self.shown[i].conn.clone();
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                    let _ = this.update(cx, |this, cx| {
                        if let Some(i) = this.shown.iter().position(|s| s.epoch == epoch)
                            && conn.is_connected()
                        {
                            this.save_shown(i, cx);
                        }
                    });
                })
                .detach();
                return;
            }
            Err(e) => {
                log::warn!("saving the workspace: {e:#}");
                toast::show(cx, Toast::error("Couldn't save the workspace").body(format!("{e:#}")).key("workspace.save"));
                // Not this layout again; the next edit tries again.
                self.shown[i].detach_after_save = false;
                self.shown[i].rejected = Some(sent);
            }
        }
        if let Some(layout) = self.shown[i].pending.take() {
            // Apply the newer/conflict layout before starting another save,
            // so we don't immediately re-send against the stale base revision.
            self.defer_remote(i, layout, cx);
            return;
        }
        self.save_layout(cx);
    }

    /// Workspaces whose tabs this window moved into `into` on host `key` are
    /// closed once an accepted save of `into` (started after the move) holds
    /// every surviving moved tab, and only if the source hasn't gained tabs
    /// since it was snapshotted.
    fn close_moved_from(&mut self, key: usize, into: WorkspaceId, save_seq: u64, accepted: &Layout, cx: &mut Context<'_, Self>) {
        let saved: HashSet<u64> = accepted.tabs.iter().chain(&accepted.detached).map(|t| t.id).collect();
        let present_in_into: HashSet<u64> = {
            let mut ids = HashSet::new();
            if let Some(i) = self.shown.iter().position(|s| s.conn.key() == key && s.id == into) {
                let local = self.local_target(i, cx);
                ids.extend(local.tabs.iter().chain(&local.detached).map(|t| t.id));
            }
            for p in self.parked.iter().filter(|p| p.conn.key() == key && p.id == into) {
                ids.extend(p.tabs.iter().map(|t| t.id));
                ids.extend(p.detached.iter().map(|t| t.id));
                if let Some((_, full)) = &p.pending_layout {
                    ids.extend(full.tabs.iter().chain(&full.detached).map(|t| t.id));
                }
            }
            ids
        };
        let moving_into = self.parked.iter().any(|p| p.moving && p.conn.key() == key && p.id == into);
        let (done, waiting): (Vec<MoveClose>, Vec<MoveClose>) = std::mem::take(&mut self.move_closes).into_iter().partition(|m| {
            m.conn.key() == key
                && m.into == into
                && save_seq > m.after_seq
                && !moving_into
                && m.tabs.iter().filter(|t| present_in_into.contains(t)).all(|t| saved.contains(t))
        });
        self.move_closes = waiting;
        for m in done {
            self.close_source_if_unchanged(m.conn, m.workspace, m.into, m.source_revision, m.sessions, cx);
        }
    }

    /// Close source workspace `workspace` only if its revision still matches
    /// `source_revision` (via a revision-checked empty save). If another window
    /// saved new sessions into it meanwhile, move those into `into` too before
    /// closing so no unmoved tab can be lost.
    fn close_source_if_unchanged(
        &mut self,
        conn: Arc<Connection>,
        workspace: WorkspaceId,
        into: WorkspaceId,
        source_revision: u64,
        moved_sessions: HashSet<SessionId>,
        cx: &mut Context<'_, Self>,
    ) {
        enum CloseSource {
            Done,
            Added { revision: u64, layout: Layout, sessions: HashMap<SessionId, Arc<Session>> },
        }
        let view = self.join.view;
        let size = self.pane_size(cx);
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            let moved_sessions = moved_sessions.clone();
            async move {
                let mut rev = source_revision;
                for _ in 0..5 {
                    match conn.save_workspace(view, workspace, rev, 0, Vec::new(), String::new())? {
                        SaveOutcome::Saved { .. } | SaveOutcome::Gone => return anyhow::Ok(CloseSource::Done),
                        SaveOutcome::Conflict(wl) => {
                            let alive: HashSet<SessionId> = wl.sessions.iter().copied().collect();
                            let mut latest = Layout::parse(&wl.json, &alive);
                            let unmoved: HashSet<SessionId> =
                                latest.sessions().into_iter().filter(|s| !moved_sessions.contains(s)).collect();
                            latest.prune(&unmoved);
                            if latest.tabs.is_empty() && latest.detached.is_empty() {
                                rev = wl.revision;
                                continue;
                            }
                            let ids: Vec<SessionId> = latest.tabs.iter().flat_map(|t| t.tree.leaves()).collect();
                            let sessions = saved::attach_all(&conn, size, &ids)?;
                            return Ok(CloseSource::Added { revision: wl.revision, layout: latest, sessions });
                        }
                    }
                }
                anyhow::bail!("the workspace kept changing while closing");
            }
        });
        cx.spawn(async move |this, cx| {
            let mut result = Some(task.await);
            let delivered = this.update(cx, |this, cx| match result.take().expect("close source result") {
                Ok(CloseSource::Done) => {}
                Ok(CloseSource::Added { revision, layout, sessions }) => {
                    let mut pending = Some((layout, sessions, moved_sessions));
                    let conn = conn.clone();
                    let (handle, me) = (this.window_handle, cx.weak_entity());
                    cx.defer(move |cx| {
                        let ok = handle
                            .update(cx, |_, window, cx| {
                                me.update(cx, |this, cx| {
                                    let (mut added, sessions, mut moved_sessions) = pending.take().expect("added");
                                    let Some(i) = this.shown_index(&conn).filter(|&i| this.shown[i].id == into) else {
                                        for s in sessions.values() {
                                            s.release_view();
                                        }
                                        // Destination is no longer shown: keep the source open
                                        // with only the unmoved tabs so nothing is lost or duplicated.
                                        let (json, sess, tabs) =
                                            (added.to_json(), added.sessions(), u32::try_from(added.tabs.len()).unwrap_or(u32::MAX));
                                        let c = conn.clone();
                                        cx.background_executor()
                                            .spawn(async move {
                                                let _ = c.save_workspace(view, workspace, revision, tabs, sess, json);
                                            })
                                            .detach();
                                        toast::show_in(
                                            window,
                                            cx,
                                            Toast::info("Workspace kept open")
                                                .body("Another window added tabs to it while it was closing.")
                                                .key("workspace.close"),
                                        );
                                        return;
                                    };
                                    let alive: HashSet<SessionId> =
                                        added.tabs.iter().flat_map(|t| t.tree.leaves()).filter(|s| sessions.contains_key(s)).collect();
                                    let detached: HashSet<SessionId> = added.detached.iter().flat_map(|t| t.tree.leaves()).collect();
                                    added.prune(&alive.union(&detached).copied().collect());
                                    if added.tabs.is_empty() && added.detached.is_empty() {
                                        this.close_source_if_unchanged(conn, workspace, into, revision, moved_sessions, cx);
                                        return;
                                    }
                                    let mut target = this.local_target(i, cx);
                                    let existing: HashSet<u64> = target.tabs.iter().chain(&target.detached).map(|t| t.id).collect();
                                    for t in added.tabs.iter_mut().chain(added.detached.iter_mut()) {
                                        if existing.contains(&t.id) {
                                            t.id = saved::new_id();
                                        }
                                    }
                                    let ids: Vec<u64> = added.tabs.iter().chain(&added.detached).map(|t| t.id).collect();
                                    moved_sessions.extend(added.sessions());
                                    target.tabs.extend(added.tabs);
                                    target.detached.extend(added.detached);
                                    this.move_closes.push(MoveClose {
                                        conn: conn.clone(),
                                        workspace,
                                        into,
                                        source_revision: revision,
                                        tabs: ids,
                                        sessions: moved_sessions,
                                        after_seq: next_save_seq(),
                                    });
                                    this.reconcile(i, target, sessions, None, window, cx);
                                    this.save_layout(cx);
                                })
                            })
                            .and_then(|r| r);
                        if ok.is_err()
                            && let Some((_, sessions, _)) = pending
                        {
                            for s in sessions.values() {
                                s.release_view();
                            }
                        }
                    });
                }
                Err(e) => {
                    toast::show(cx, Toast::error("Couldn't close the workspace").body(format!("{e:#}")).key("workspace.close"));
                }
            });
            if delivered.is_err()
                && let Some(Ok(CloseSource::Added { sessions, .. })) = result
            {
                for s in sessions.values() {
                    s.release_view();
                }
            }
        })
        .detach();
    }

    /// Apply a layout from elsewhere on the next update (it needs the window).
    fn defer_remote(&mut self, i: usize, layout: WorkspaceLayout, cx: &mut Context<'_, Self>) {
        let key = self.shown[i].conn.key();
        self.later(cx, move |this, window, cx| this.apply_remote(key, layout, window, cx));
    }

    /// Run `f` with the window, once the current update is done.
    pub(super) fn later(&self, cx: &mut Context<'_, Self>, f: impl FnOnce(&mut Self, &mut Window, &mut Context<'_, Self>) + 'static) {
        let (handle, me) = (self.window_handle, cx.weak_entity());
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| me.update(cx, |this, cx| f(this, window, cx)));
        });
    }

    // ---- changes from elsewhere ---------------------------------------------

    pub(super) fn on_workspace_event(&mut self, key: usize, event: WorkspaceEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        // Before this window shows the host's workspace (it's still
        // attaching its sessions): kept for when it does (`show_joined`).
        if self.shown_by_key(key).is_none() {
            let newer = |a: &WorkspaceEvent, b: &WorkspaceEvent| match (a, b) {
                (WorkspaceEvent::Changed(x), WorkspaceEvent::Changed(y)) => x.id == y.id,
                (WorkspaceEvent::List { .. }, WorkspaceEvent::List { .. }) => true,
                _ => false,
            };
            if !matches!(event, WorkspaceEvent::Rejoined { .. }) {
                self.early_events.retain(|(k, e)| !(*k == key && newer(e, &event)));
                self.early_events.push((key, event));
            }
            return;
        }
        match event {
            WorkspaceEvent::Changed(layout) => self.apply_remote(key, layout, window, cx),
            WorkspaceEvent::List { seq, list } => {
                self.apply_list(key, seq, list, cx);
            }
            WorkspaceEvent::Rejoined { info, layout } => {
                let Some(i) = self.shown_by_key(key) else { return };
                self.shown[i].saving = false;
                // A restarted daemon counts its lists from the start again.
                self.shown[i].seq = 0;
                if self.shown[i].id != info.id {
                    // Its workspace ended while the link was down: what the
                    // daemon joined it to instead replaces it (its revisions
                    // count from that one's).
                    let mine = self.mine(i, cx);
                    let sh = &mut self.shown[i];
                    sh.id = info.id;
                    sh.base = mine.clone();
                    sh.target = mine.clone();
                    sh.target_shown = mine;
                    sh.revision = 0;
                    sh.pending = None;
                }
                // Attaches from the old link start over with the merge.
                self.shown[i].attaching.clear();
                self.apply_remote(key, layout, window, cx);
            }
        }
    }

    /// Apply a host workspace list update; returns `false` if the shown
    /// workspace was removed (`workspace_gone`).
    fn apply_list(&mut self, key: usize, seq: u64, list: Vec<WorkspaceInfo>, cx: &mut Context<'_, Self>) -> bool {
        let Some(i) = self.shown_by_key(key) else { return false };
        if seq < self.shown[i].seq {
            return true;
        }
        let gone = !list.iter().any(|w| w.id == self.shown[i].id);
        // Parked workspaces that ended (their tabs were closed or moved elsewhere).
        let conn = self.shown[i].conn.clone();
        let ended: Vec<usize> = (0..self.parked.len())
            .rev()
            .filter(|&p| Arc::ptr_eq(&self.parked[p].conn, &conn) && !list.iter().any(|w| w.id == self.parked[p].id))
            .collect();
        for p in ended {
            let parked = self.parked.remove(p);
            self.release_tabs(parked.tabs, cx);
        }
        self.shown[i].take_list(seq, list);
        if gone {
            self.workspace_gone(i, cx);
        }
        self.refresh_switcher(cx);
        cx.notify();
        !gone
    }

    /// Workspace `layout` changed elsewhere (or this window's save
    /// conflicted with it): merge it with this window's edit and show that.
    fn apply_remote(&mut self, key: usize, layout: WorkspaceLayout, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.shown_by_key(key).filter(|&i| self.shown[i].id == layout.id) else { return };
        let sh = &mut self.shown[i];
        if layout.revision < sh.revision {
            return;
        }
        if sh.saving || self.divider_dragging {
            if sh.pending.as_ref().is_none_or(|p| p.revision < layout.revision) {
                sh.pending = Some(layout);
            }
            return;
        }
        let alive: HashSet<SessionId> = layout.sessions.iter().copied().collect();
        let theirs = Layout::parse(&layout.json, &alive);
        let local = self.local_target(i, cx);
        let merged = saved::merge(&self.shown[i].base, &local, &theirs);
        let sh = &mut self.shown[i];
        sh.base = theirs;
        sh.revision = layout.revision;
        if merged.tabs.is_empty() && merged.detached.is_empty() {
            // Emptied elsewhere: it ended (the list says so next).
            return;
        }
        self.reconcile(i, merged, HashMap::new(), None, window, cx);
        self.save_layout(cx);
        // Another window closed or detached every tab this one showed there.
        let conn = self.shown[i].conn.clone();
        if self.shown[i].attaching.is_empty() && !self.tabs.iter().any(|t| on_conn(t, &conn)) {
            self.finish_removing_tabs(window, cx);
        }
    }

    /// Make this window's tabs on host `i` show `target` (`at`: where they
    /// go when the window has none there now). Panes are kept by session;
    /// `fresh` are attached sessions for new ones; the others are attached
    /// in the background and their tabs show up then. The active tab, focus,
    /// zoom and activity marks stay this window's.
    pub(super) fn reconcile(
        &mut self,
        i: usize,
        target: Layout,
        mut fresh: HashMap<SessionId, Arc<Session>>,
        at: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let conn = self.shown[i].conn.clone();
        self.shown[i].detached = target.detached.clone();
        self.shown[i].target = target.clone();
        let active_id = self.active_tab().map(|t| t.id);
        let pos = self.tabs.iter().position(|t| on_conn(t, &conn)).or(at).unwrap_or(self.tabs.len()).min(self.tabs.len());
        let (mine, rest): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| on_conn(t, &conn));
        self.tabs = rest;
        let pos = pos.min(self.tabs.len());
        // What stays this window's, by tab id: (focused session, zoomed, activity).
        let mut kept: HashMap<u64, (Option<SessionId>, bool, Option<Activity>)> = HashMap::new();
        for t in &mine {
            let focused = t.pane(t.focused).and_then(|p| p.view.read(cx).session().daemon_id());
            kept.insert(t.id, (focused, t.zoomed, t.activity));
        }
        let (mut pool, _) = panes_by_session(mine, cx);
        // Panes this window keeps elsewhere (a parked workspace's tabs that
        // moved here) are used too.
        let wanted: HashSet<SessionId> = target.tabs.iter().flat_map(|t| t.tree.leaves()).collect();
        for p in self.parked.iter_mut().filter(|p| Arc::ptr_eq(&p.conn, &conn)) {
            for t in &mut p.tabs {
                for pane in std::mem::take(&mut t.panes) {
                    match pane.view.read(cx).session().daemon_id().filter(|id| wanted.contains(id) && !pool.contains_key(id)) {
                        Some(id) => {
                            if t.tree.leaves().len() > 1 {
                                t.tree.remove(pane.id);
                            }
                            pool.insert(id, pane);
                        }
                        None => t.panes.push(pane),
                    }
                }
                if !t.panes.is_empty() && !t.tree.contains(t.focused) {
                    t.focused = t.panes[0].id;
                }
            }
            // A tab whose panes all moved here is gone from it.
            p.tabs.retain(|t| !t.panes.is_empty());
        }
        let mut missing = Vec::new();
        let mut tabs = Vec::new();
        for st in &target.tabs {
            let mut panes = Vec::new();
            let mut pane_of = HashMap::new();
            let tree = st.tree.map_leaves(&mut |sid| {
                let pane = match pool.remove(&sid) {
                    Some(p) => p,
                    None => match fresh.remove(&sid) {
                        Some(s) => self.make_pane(s, window, cx),
                        None => {
                            missing.push(sid);
                            return None;
                        }
                    },
                };
                pane_of.insert(sid, pane.id);
                let id = pane.id;
                panes.push(pane);
                Some(id)
            });
            let Some(tree) = tree else { continue };
            let (focused, zoomed, activity) = kept.get(&st.id).copied().unwrap_or((None, false, None));
            let focused = focused.and_then(|s| pane_of.get(&s).copied()).unwrap_or(panes[0].id);
            let host = match &st.plain {
                Some(h) => Spawner::Plain { host: h.clone(), base: Box::new(Spawner::Daemon(conn.clone())) },
                None => Spawner::Daemon(conn.clone()),
            };
            let zoomed = zoomed && !matches!(tree, Node::Leaf(_));
            tabs.push(Tab { id: st.id, tree, panes, focused, zoomed, name: st.name.clone(), host: Some(host), connecting: None, activity });
        }
        let gone: Vec<Pane> = pool.into_values().collect();
        let gone_ids: Vec<super::PaneId> = gone.iter().map(|p| p.id).collect();
        for p in gone {
            p.view.read(cx).session().release_view();
        }
        for (_, s) in fresh {
            // Attached for a tab that's gone meanwhile.
            s.release_view();
        }
        for id in gone_ids {
            self.forget_notices(id, cx);
        }
        let n = tabs.len();
        for (k, t) in tabs.into_iter().enumerate() {
            self.tabs.insert(pos + k, t);
        }
        // A tab the user asked for (reattached, attached by id) is shown once it's there.
        let mut active_id = active_id;
        if let Some((k, s)) = self.show_when_attached
            && k == conn.key()
            && let Some(t) =
                self.tabs.iter().find(|t| on_conn(t, &conn) && t.panes.iter().any(|p| p.view.read(cx).session().daemon_id() == Some(s)))
        {
            active_id = Some(t.id);
            self.show_when_attached = None;
        }
        self.after_tabs_replaced(active_id, pos, n, window, cx);
        self.shown[i].target_shown = self.mine(i, cx);
        let attaching = &mut self.shown[i].attaching;
        missing.retain(|s| attaching.insert(*s));
        if !missing.is_empty() {
            self.attach_missing(conn, missing, window, cx);
        }
    }

    /// Clean up after tabs were replaced: overlays about tabs or panes that
    /// are gone, and the active tab (kept by id).
    fn after_tabs_replaced(&mut self, active_id: Option<u64>, pos: usize, n: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.rename.as_ref().is_some_and(|r| self.tab_index(r.tab).is_none()) {
            self.rename = None;
        }
        if self.paste.as_ref().is_some_and(|(_, pane, _)| self.find_pane(*pane).is_none()) {
            self.paste = None;
        }
        if let Some(owner) = self.menu.as_ref().map(|m| m.owner) {
            let gone = match owner {
                super::menu::MenuOwner::Tab(t) => self.tab_index(t).is_none(),
                super::menu::MenuOwner::Pane(p) => self.find_pane(p).is_none(),
                super::menu::MenuOwner::NewTabButton | super::menu::MenuOwner::Workspace => false,
            };
            if gone {
                self.close_menu(window, cx);
            }
        }
        if self.tabs.is_empty() {
            cx.notify();
            return;
        }
        let active = active_id.and_then(|id| self.tab_index(id)).unwrap_or_else(|| pos.min(self.tabs.len() - 1));
        if n > 0 || active_id.is_some_and(|id| self.tab_index(id).is_none()) {
            self.active = active;
            self.regroup();
            self.active = active_id.and_then(|id| self.tab_index(id)).unwrap_or(self.active.min(self.tabs.len() - 1));
            self.focus_active(window, cx);
            self.update_window_title(window, cx);
        } else {
            self.active = active;
        }
        cx.notify();
    }

    /// Attach sessions another window put into this host's workspace, then
    /// show their tabs.
    fn attach_missing(&mut self, conn: Arc<Connection>, ids: Vec<SessionId>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let size = self.pane_size(cx);
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            let ids = ids.clone();
            async move { saved::attach_all(&conn, size, &ids) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            // Let go of them if the window is gone before it takes them.
            let mut held = saved::Held(result.as_ref().map(|s| s.values().cloned().collect()).unwrap_or_default());
            let _ = this.update_in(cx, |this, window, cx| {
                held.take();
                let Some(i) = this.shown_index(&conn) else {
                    if let Ok(sessions) = result {
                        for s in sessions.values() {
                            s.release_view();
                        }
                    }
                    return;
                };
                let fresh = match result {
                    Ok(s) => s,
                    // The link dropped or timed out: those tabs aren't gone.
                    // No save meanwhile (they'd look closed); joining again
                    // after a reconnect attaches them, else try again.
                    Err(e) => {
                        log::info!("attaching another window's tabs: {e:#}");
                        if this.shown[i].conn.is_connected() {
                            let (conn, ids) = (conn.clone(), ids.clone());
                            cx.spawn_in(window, async move |this, cx| {
                                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                                let _ = this.update_in(cx, |this, window, cx| {
                                    let ids: Vec<SessionId> = match this.shown_index(&conn) {
                                        Some(i) => ids.into_iter().filter(|s| this.shown[i].attaching.contains(s)).collect(),
                                        None => return,
                                    };
                                    if !ids.is_empty() {
                                        this.attach_missing(conn, ids, window, cx);
                                    }
                                });
                            })
                            .detach();
                        } else if std::mem::take(&mut this.shown[i].detach_after_save) {
                            toast::show_in(
                                window,
                                cx,
                                Toast::error("Couldn't save the workspace").body(format!("{e:#}")).key("workspace.save"),
                            );
                        }
                        return;
                    }
                };
                for id in &ids {
                    this.shown[i].attaching.remove(id);
                }
                // Sessions that couldn't be attached ended: their panes go.
                // What's still to come, with what this window did meanwhile.
                let mine = this.mine(i, cx);
                let mut target = saved::merge(&this.shown[i].target_shown, &mine, &this.shown[i].target);
                let alive: HashSet<SessionId> =
                    target.sessions().into_iter().filter(|s| !ids.contains(s) || fresh.contains_key(s)).collect();
                target.prune(&alive);
                this.reconcile(i, target, fresh, None, window, cx);
                this.save_layout(cx);
            });
        })
        .detach();
    }

    // ---- joining, switching -------------------------------------------------

    /// Show workspace `j` (just joined on `spawner`'s host) at `index`.
    /// `active`: the tab to show (a reopened window's); `focus`: show the
    /// joined workspace (else it opens in the background).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn show_joined(
        &mut self,
        index: usize,
        j: Box<Joined>,
        spawner: Spawner,
        active: Option<u64>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(conn) = spawner.connection().cloned() else { return };
        let Joined { info, seq, list, revision, base, show, sessions, extra } = *j;
        let mut target = show;
        // A second connecting tab to a host this window shows already (or a
        // window that switched while connecting): the same workspace takes
        // this one's tabs in, merged with what the window did meanwhile.
        let i = match self.shown_index(&conn) {
            Some(i) if self.shown[i].id == info.id => {
                let mine = self.mine(i, cx);
                if revision >= self.shown[i].revision {
                    target = saved::merge(&self.shown[i].base, &mine, &target);
                    (self.shown[i].base, self.shown[i].revision) = (base, revision);
                } else {
                    // Older than what the window shows: it removes nothing.
                    target = mine;
                }
                i
            }
            other => {
                if let Some(i) = other {
                    self.park(i, cx);
                }
                self.shown.push(Shown::new(conn.clone(), &info, revision, base, (seq, list.clone())));
                self.shown.len() - 1
            }
        };
        if !self.shown[i].take_list(seq, list) && !self.shown[i].list.iter().any(|w| w.id == info.id) {
            // An older list than one heard: it still has to show this one.
            self.shown[i].list.push(info.clone());
        }
        // Its tabs from when this window showed it before, and tabs this
        // window moved into it (closing another workspace).
        let mut active = active;
        while let Some(p) = self.parked.iter().position(|p| Arc::ptr_eq(&p.conn, &conn) && (p.id == info.id || p.moving)) {
            let parked = self.parked.remove(p);
            if parked.moving {
                let after = next_save_seq();
                for m in self.move_closes.iter_mut().filter(|m| Arc::ptr_eq(&m.conn, &conn) && m.into == parked.id) {
                    m.into = info.id;
                    m.after_seq = after;
                }
            } else {
                active = active.or(parked.active);
            }
            let current = Layout::new(parked.tabs.iter().filter_map(|t| super::saved_tab(t, cx)).collect(), parked.detached.clone());
            let mine = match parked.pending_layout {
                Some((at_park, full)) => saved::merge(&at_park, &current, &full),
                None => current,
            };
            target = saved::merge(&parked.base, &mine, &target);
            let at = index.min(self.tabs.len());
            for (k, t) in parked.tabs.into_iter().enumerate() {
                self.tabs.insert(at + k, t);
            }
        }
        self.reconcile(i, target, sessions, Some(index), window, cx);
        // Parked workspaces this one took every pane from, or that are gone.
        self.release_parked(&conn, cx);
        // What the host said while the sessions were being attached.
        let key = conn.key();
        let (early, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.early_events).into_iter().partition(|(k, _)| *k == key);
        self.early_events = rest;
        for (_, event) in early {
            match event {
                WorkspaceEvent::List { seq, list } => {
                    if !self.apply_list(key, seq, list, cx) {
                        for (s, _) in extra {
                            s.release_view();
                        }
                        return;
                    }
                }
                WorkspaceEvent::Changed(layout)
                    if self
                        .shown_by_key(key)
                        .is_some_and(|i| self.shown[i].id == layout.id && layout.revision > self.shown[i].revision) =>
                {
                    self.later(cx, move |this, window, cx| this.apply_remote(key, layout, window, cx));
                }
                _ => {}
            }
        }
        // New tabs it was asked for, after its others.
        let mut show_tab = active.filter(|id| self.tab_index(*id).is_some());
        for (session, plain) in extra {
            let host = match plain {
                Some(host) => Spawner::Plain { host, base: Box::new(Spawner::Daemon(conn.clone())) },
                None => Spawner::Daemon(conn.clone()),
            };
            let pane = self.make_pane(session, window, cx);
            let id = saved::new_id();
            let at = self.tabs.iter().rposition(|t| on_conn(t, &conn)).map_or(index.min(self.tabs.len()), |p| p + 1);
            let tab = Tab {
                id,
                tree: Node::Leaf(pane.id),
                focused: pane.id,
                panes: vec![pane],
                zoomed: false,
                name: None,
                host: Some(host),
                connecting: None,
                activity: None,
            };
            self.tabs.insert(at, tab);
            show_tab = Some(id);
        }
        if self.home.is_none() {
            self.home = Some(Spawner::Daemon(conn.clone()));
        }
        let show = show_tab
            .and_then(|id| self.tab_index(id))
            .or_else(|| self.tabs.iter().position(|t| on_conn(t, &conn)))
            .unwrap_or(0)
            .min(self.tabs.len().saturating_sub(1));
        if self.tabs.is_empty() {
            return;
        }
        if focus {
            self.activate(show, window, cx);
        } else {
            self.regroup();
            self.save_layout(cx);
            cx.notify();
        }
    }

    /// Put away the tabs of host `i`'s workspace (switching to another one):
    /// their sessions stay attached. Returns where its tabs were.
    pub(super) fn park(&mut self, i: usize, cx: &mut Context<'_, Self>) -> usize {
        self.park_shown(i, true, cx)
    }

    fn park_shown(&mut self, i: usize, save: bool, cx: &mut Context<'_, Self>) -> usize {
        if save {
            // Its last edit first (a parked workspace saves nothing more).
            self.save_shown(i, cx);
        }
        // Preserve tabs whose sessions are still attaching so returning to
        // this workspace doesn't treat them as locally closed.
        let pending_layout =
            (!self.shown[i].target.same(&self.shown[i].target_shown)).then(|| (self.mine(i, cx), self.local_target(i, cx)));
        let sh = self.shown.remove(i);
        let pos = self.tabs.iter().position(|t| on_conn(t, &sh.conn)).unwrap_or(self.tabs.len());
        let active_id = self.active_tab().map(|t| t.id);
        let (tabs, rest): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| on_conn(t, &sh.conn));
        // The tab to show when coming back: the active one, else the host's last.
        let last = tabs.iter().find(|t| self.group_focus.iter().any(|(_, id)| *id == t.id)).map(|t| t.id);
        let shown_active = active_id.filter(|id| tabs.iter().any(|t| t.id == *id)).or(last);
        self.tabs = rest;
        if let Some(id) = active_id {
            self.active = self.tab_index(id).unwrap_or(pos.min(self.tabs.len().saturating_sub(1)));
        }
        // Out of sight: another window showing these sessions sizes them now.
        for p in tabs.iter().flat_map(|t| t.panes.iter()) {
            p.view.read(cx).disown_size();
        }
        if let Some(old) = self.parked.iter().position(|p| Arc::ptr_eq(&p.conn, &sh.conn) && p.id == sh.id) {
            let old = self.parked.remove(old);
            self.release_tabs(old.tabs, cx);
        }
        self.parked.push(Parked {
            conn: sh.conn,
            id: sh.id,
            base: sh.base,
            detached: sh.detached,
            tabs,
            moving: false,
            pending_layout,
            epoch: sh.epoch,
            active: shown_active,
        });
        pos
    }

    /// Show workspace `id` on host `conn` in this window instead of the
    /// one it shows there now.
    pub(super) fn switch_workspace(&mut self, conn: &Arc<Connection>, id: WorkspaceId, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.shown_index(conn).is_some_and(|i| self.shown[i].id == id) {
            return;
        }
        self.switch_workspace_with(conn, AttachMode::Join(Some(id)), window, cx);
    }

    /// Show what `mode` asks for on host `conn` (another workspace there)
    /// instead of the workspace this window shows there now.
    pub(super) fn switch_workspace_with(
        &mut self,
        conn: &Arc<Connection>,
        mode: AttachMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(i) = self.shown_index(conn) else { return };
        self.close_switcher(window, cx);
        let pos = self.park(i, cx);
        let plan = ConnectPlan {
            endpoint: conn.endpoint().clone(),
            mode,
            open_settings: None,
            plain: false,
            conn: Some(conn.clone()),
            join: None,
            active_tab: None,
        };
        self.open_connect_tab_at(plan, pos, window, cx);
    }

    /// The next (or previous) workspace on the active tab's host.
    pub(super) fn cycle_workspace(&mut self, forward: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.active_shown() else {
            cx.propagate();
            return;
        };
        let sh = &self.shown[i];
        if sh.list.len() < 2 {
            cx.propagate();
            return;
        }
        let at = sh.list.iter().position(|w| w.id == sh.id).unwrap_or(0);
        let n = sh.list.len();
        let next = if forward { (at + 1) % n } else { (at + n - 1) % n };
        let (conn, id) = (sh.conn.clone(), sh.list[next].id);
        self.switch_workspace(&conn, id, window, cx);
    }

    /// The workspace this window showed on host `i` doesn't exist any more
    /// (closed elsewhere): the most recently active other one takes its
    /// place, else its tabs go (the window closes with its last tab).
    pub(super) fn workspace_gone(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        let sh = self.shown.remove(i);
        let others: Vec<&WorkspaceInfo> = sh.list.iter().filter(|w| w.id != sh.id).collect();
        let pos = self.tabs.iter().position(|t| on_conn(t, &sh.conn)).unwrap_or(self.tabs.len());
        let (tabs, rest): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| on_conn(t, &sh.conn));
        self.tabs = rest;
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        // Its panes are kept a moment: the workspace that takes its place may show them.
        self.parked.push(Parked {
            conn: sh.conn.clone(),
            id: sh.id,
            base: sh.base,
            detached: Vec::new(),
            tabs,
            moving: false,
            pending_layout: None,
            epoch: sh.epoch,
            active: None,
        });
        let conn = sh.conn.clone();
        let rejoin = !others.is_empty();
        self.later(cx, move |this, window, cx| {
            if rejoin {
                let plan = ConnectPlan {
                    endpoint: conn.endpoint().clone(),
                    mode: AttachMode::Join(None),
                    open_settings: None,
                    plain: false,
                    conn: Some(conn.clone()),
                    join: None,
                    active_tab: None,
                };
                this.open_connect_tab_at(plan, pos, window, cx);
            } else {
                conn.leave_workspace(this.join.view);
                this.release_parked_of(&conn, cx);
                this.finish_removing_tabs(window, cx);
            }
            // The gone workspace's parked panes, unless the new one took them.
            this.release_parked(&conn, cx);
        });
    }

    /// This window closed the last tab of host `i`'s workspace (saved
    /// empty, so it ended unless it's the host's only one): the most
    /// recently active other workspace there takes its place, else the
    /// host is done in this window.
    fn ended_here(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        if self.tabs.iter().any(|t| on_conn(t, &self.shown[i].conn)) {
            // Tabs came back meanwhile (a new tab): they save as usual.
            self.save_layout(cx);
            return;
        }
        let sh = &self.shown[i];
        let others = sh.list.iter().any(|w| w.id != sh.id);
        if others {
            self.workspace_gone(i, cx);
        } else {
            let sh = self.shown.remove(i);
            sh.conn.leave_workspace(self.join.view);
            if self.tabs.is_empty() {
                // Nothing left in the window: it closes, as with its last tab.
                self.later(cx, |this, window, cx| this.finish_removing_tabs(window, cx));
            }
            self.record_window(cx);
        }
    }

    /// Let go of `tabs`' sessions (detach them unless another window shows them).
    pub(super) fn release_tabs(&mut self, tabs: Vec<Tab>, cx: &mut Context<'_, Self>) {
        for t in tabs {
            for p in &t.panes {
                p.view.read(cx).session().release_view();
                self.forget_notices(p.id, cx);
            }
        }
    }

    /// Parked workspaces on `conn` that have nothing left (their panes were
    /// taken back) or no longer exist on the host let go of their sessions.
    fn release_parked(&mut self, conn: &Arc<Connection>, cx: &mut Context<'_, Self>) {
        let known: Option<HashSet<WorkspaceId>> =
            self.shown.iter().find(|s| Arc::ptr_eq(&s.conn, conn)).map(|s| s.list.iter().map(|w| w.id).collect());
        let (gone, stay): (Vec<Parked>, Vec<Parked>) = std::mem::take(&mut self.parked).into_iter().partition(|p| {
            Arc::ptr_eq(&p.conn, conn) && (p.tabs.iter().all(|t| t.panes.is_empty()) || known.as_ref().is_some_and(|k| !k.contains(&p.id)))
        });
        self.parked = stay;
        for p in gone {
            self.release_tabs(p.tabs, cx);
        }
    }

    /// Every parked workspace on `conn` lets go of its sessions (the window
    /// is done with that host).
    pub(super) fn release_parked_of(&mut self, conn: &Arc<Connection>, cx: &mut Context<'_, Self>) {
        let (gone, stay): (Vec<Parked>, Vec<Parked>) =
            std::mem::take(&mut self.parked).into_iter().partition(|p| Arc::ptr_eq(&p.conn, conn));
        self.parked = stay;
        for p in gone {
            self.release_tabs(p.tabs, cx);
        }
    }

    // ---- making, renaming, closing ------------------------------------------

    /// "New workspace": an empty workspace on the active tab's host with a
    /// fresh shell in the focused pane's directory, shown in this window
    /// (the others stay where they are), its name ready to edit.
    pub(super) fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(conn) = self.active_tab().and_then(|t| t.host.as_ref()).and_then(Spawner::connection).cloned() else {
            let toast = Toast::info("Workspaces live on a goattyd").body("This tab isn't kept by one (or is still connecting).");
            toast::show_in(window, cx, toast.key("workspace.new"));
            return;
        };
        let (size, cwd) = match self.focused_view() {
            Some(v) => {
                let v = v.read(cx);
                let (cols, rows) = v.session().terminal.size();
                (TermSize::new(cols, rows), v.cwd().map(|p| p.to_string_lossy().into_owned()))
            }
            None => (TermSize::new(100, 30), None),
        };
        let join = self.join.clone();
        let current = self.shown_index(&conn).map(|i| self.shown[i].id);
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            async move {
                let (info, layout) = conn.create_workspace(join.view, None, join.sink.clone())?;
                let session = match Session::daemon(conn.clone(), size, None, cwd) {
                    Ok(s) => s,
                    Err(e) => {
                        // No shell, no workspace: back to the one the window shows.
                        let _ = conn.close_workspace(info.id);
                        match current {
                            Some(id) => drop(conn.join_workspace(join.view, Some(id), join.sink.clone())),
                            None => conn.leave_workspace(join.view),
                        }
                        return Err(e);
                    }
                };
                let (seq, list) = conn.list_workspaces_seq().unwrap_or_else(|_| (0, vec![info.clone()]));
                anyhow::Ok(Box::new(Joined {
                    info,
                    seq,
                    list,
                    revision: layout.revision,
                    base: Layout::new(Vec::new(), Vec::new()),
                    show: Layout::new(Vec::new(), Vec::new()),
                    sessions: HashMap::new(),
                    extra: vec![(session, None)],
                }))
            }
        });
        let view_id = self.join.view;
        cx.spawn_in(window, async move |this, cx| {
            let mut result = Some(task.await);
            let delivered = this.update_in(cx, |this, window, cx| match result.take().expect("new workspace result") {
                Ok(j) => {
                    let id = j.info.id;
                    let pos = match this.shown_index(&conn) {
                        Some(i) => this.park(i, cx),
                        None => this.tabs.len(),
                    };
                    this.show_joined(pos, j, Spawner::Daemon(conn.clone()), None, true, window, cx);
                    this.open_switcher(Some(id), window, cx);
                }
                Err(e) => {
                    toast::show_in(window, cx, Toast::error("Couldn't make a workspace").body(format!("{e:#}")).key("workspace.new"));
                }
            });
            // The window closed before taking ownership: release the new
            // shell's view claim, leave the join, and drop the empty workspace.
            if delivered.is_err()
                && let Some(Ok(j)) = result
            {
                let id = j.info.id;
                for (s, _) in &j.extra {
                    s.release_view();
                }
                conn.leave_workspace(view_id);
                cx.background_executor()
                    .spawn(async move {
                        let _ = conn.close_workspace(id);
                    })
                    .detach();
            }
        })
        .detach();
    }

    pub(super) fn rename_workspace(&mut self, conn: &Arc<Connection>, id: WorkspaceId, name: String, cx: &mut Context<'_, Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        // Shown right away; keep the last daemon-confirmed name so overlapping
        // failed renames never roll back to an unconfirmed attempt.
        for sh in self.shown.iter_mut().filter(|s| Arc::ptr_eq(&s.conn, conn)) {
            if let Some(w) = sh.list.iter_mut().find(|w| w.id == id) {
                sh.pending_renames.entry(id).or_insert_with(|| w.name.clone());
                w.name = name.clone();
            }
        }
        cx.notify();
        let attempted = name.clone();
        let conn = conn.clone();
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            async move {
                // Failed: what the daemon calls it, to show again.
                conn.rename_workspace(id, name).map_err(|e| (e, conn.list_workspaces_seq().ok()))
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    for sh in this.shown.iter_mut().filter(|s| Arc::ptr_eq(&s.conn, &conn)) {
                        if sh.pending_renames.contains_key(&id)
                            && let Some(w) = sh.list.iter().find(|w| w.id == id)
                        {
                            if w.name == attempted {
                                sh.pending_renames.remove(&id);
                            } else {
                                // A newer optimistic rename is in flight: this one is now confirmed.
                                sh.pending_renames.insert(id, attempted.clone());
                            }
                        }
                    }
                }
                Err((e, list)) => {
                    toast::show(cx, Toast::error("Couldn't rename the workspace").body(format!("{e:#}")).key("workspace.rename"));
                    let mut changed = false;
                    for sh in this.shown.iter_mut().filter(|s| Arc::ptr_eq(&s.conn, &conn)) {
                        let applied = list.as_ref().is_some_and(|(seq, list)| sh.take_list(*seq, list.clone()));
                        if applied {
                            changed = true;
                        } else if let Some(w) = sh.list.iter_mut().find(|w| w.id == id && w.name == attempted)
                            && let Some(confirmed) = sh.pending_renames.remove(&id)
                        {
                            w.name = confirmed;
                            changed = true;
                        }
                    }
                    if changed {
                        this.refresh_switcher(cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Close workspace `id` on `conn`: an empty one right away; one with
    /// running shells (including detached tabs) asks first whether to move its
    /// tabs to another workspace or end them.
    pub(super) fn close_workspace(&mut self, conn: &Arc<Connection>, id: WorkspaceId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.shown_index(conn) else { return };
        let list = self.shown[i].list.clone();
        if list.len() < 2 {
            let toast = Toast::info("This is the host's only workspace").body("Close its tabs instead; a host always keeps one workspace.");
            toast::show_in(window, cx, toast.key("workspace.close"));
            return;
        }
        let Some(info) = list.iter().find(|w| w.id == id).cloned() else { return };
        let here = self.shown[i].id == id;
        // Where its tabs would go: the workspace shown here, or (closing
        // that one) the most recently active other one.
        let Some(into) = (if here {
            list.iter().filter(|w| w.id != id).max_by_key(|w| w.active)
        } else {
            list.iter().find(|w| w.id == self.shown[i].id)
        })
        .cloned() else {
            return;
        };
        let (tabs, shells) = if here {
            let local = self.local_target(i, cx);
            (local.tabs.len(), local.sessions())
        } else {
            (info.tabs as usize, info.sessions.clone())
        };
        self.close_switcher(window, cx);
        if shells.is_empty() {
            self.finish_close(conn, id, Some(into.id), false, Vec::new(), window, cx);
            return;
        }
        let me = cx.entity().downgrade();
        let shells_text = if shells.len() == 1 { "1 shell".to_string() } else { format!("{} shells", shells.len()) };
        let (move_label, move_help, question) = if tabs == 0 {
            (
                format!("Move detached tabs to {}", into.name),
                "Nothing ends; they stay detached there".to_string(),
                format!("Close “{}”: what about its detached tabs?", info.name),
            )
        } else {
            let tabs_text = if tabs == 1 { "1 tab".to_string() } else { format!("{tabs} tabs") };
            (
                format!("Move {tabs_text} to {}", into.name),
                "Nothing ends; they show up there".to_string(),
                format!("Close “{}”: what about its tabs?", info.name),
            )
        };
        let choices = vec![
            ("move".to_string(), move_label, move_help),
            ("close".to_string(), format!("Close them (ends {shells_text})"), "Whatever runs in them stops".to_string()),
        ];
        let mut field = crate::command_palette::Field::choice("what", &question, choices);
        field.spec.default = Some("move".into());
        let conn = conn.clone();
        let ask = crate::command_palette::Ask::new("Close workspace", vec![field], move |answers, window, cx| {
            let kill = answers.get("what").is_some_and(|a| a == "close");
            let shells = shells.clone();
            let _ = me.update(cx, |this, cx| this.finish_close(&conn, id, Some(into.id), kill, shells, window, cx));
        });
        self.open_ask(ask, window, cx);
    }

    /// The shells host `i`'s workspace has in this window: its panes',
    /// still-attaching tabs', and detached tabs'.
    fn shells_here(&self, i: usize, cx: &gpui::App) -> Vec<SessionId> {
        self.local_target(i, cx).sessions()
    }

    /// The workspace changed while its close was asked about: ask again.
    fn ask_close_again(&mut self, conn: &Arc<Connection>, id: WorkspaceId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let toast = Toast::info("The workspace changed meanwhile").body("Nothing ended; here's the question again.");
        toast::show_in(window, cx, toast.key("workspace.close"));
        self.close_workspace(conn, id, window, cx);
    }

    /// Close workspace `id`: its tabs move into workspace `into` (closing
    /// it once they're saved there), or with `kill` the shells the user
    /// was asked about (`confirmed`) end. When there are others by now,
    /// the question comes again.
    #[allow(clippy::too_many_arguments)]
    fn finish_close(
        &mut self,
        conn: &Arc<Connection>,
        id: WorkspaceId,
        into: Option<WorkspaceId>,
        kill: bool,
        confirmed: Vec<SessionId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(i) = self.shown_index(conn) else { return };
        let here = self.shown[i].id == id;
        let conn = conn.clone();
        if here {
            if kill && self.shells_here(i, cx).iter().any(|s| !confirmed.contains(s)) {
                self.ask_close_again(&conn, id, window, cx);
                return;
            }
            // Show the other one; this one's tabs go with it (or end).
            let next = into.or_else(|| self.shown[i].list.iter().filter(|w| w.id != id).max_by_key(|w| w.active).map(|w| w.id));
            let Some(next) = next else { return };
            let source_revision = self.shown[i].revision;
            // Capture the full layout (including tabs whose sessions are still
            // attaching) before parking so no attaching session is dropped.
            let full = self.local_target(i, cx);
            let pos = self.park_shown(i, false, cx);
            let Some(p) = self.parked.iter().position(|p| Arc::ptr_eq(&p.conn, &conn) && p.id == id) else { return };
            let leaving = self.parked.remove(p);
            let moved: Vec<u64> = full.tabs.iter().chain(&full.detached).map(|t| t.id).collect();
            let moved_sessions: HashSet<SessionId> = full.sessions().into_iter().collect();
            if kill || moved.is_empty() {
                let instantiated: HashSet<SessionId> =
                    leaving.tabs.iter().flat_map(|t| t.panes.iter()).filter_map(|p| p.view.read(cx).session().daemon_id()).collect();
                let extra_kills: Vec<SessionId> = moved_sessions.iter().copied().filter(|s| !instantiated.contains(s)).collect();
                let sessions: Vec<Arc<Session>> =
                    leaving.tabs.iter().flat_map(|t| t.panes.iter()).map(|p| p.view.read(cx).session().clone()).collect();
                self.release_tabs(leaving.tabs, cx);
                if kill {
                    self.kill_sessions(sessions, cx).detach();
                    let c = conn.clone();
                    cx.background_executor()
                        .spawn(async move {
                            for s in extra_kills {
                                if let Err(e) = c.kill(s) {
                                    log::info!("ending session {s}: {e:#}");
                                }
                            }
                        })
                        .detach();
                    self.forget_workspace_on_daemon(conn.clone(), id, cx);
                } else {
                    self.close_source_if_unchanged(conn.clone(), id, next, source_revision, HashSet::new(), cx);
                }
            } else {
                // They (including detached and still-attaching tabs) join the
                // next workspace this window shows there as tabs it added
                // (`show_joined`); this one is closed once they're saved there.
                let at_park = Layout::new(leaving.tabs.iter().filter_map(|t| super::saved_tab(t, cx)).collect(), full.detached.clone());
                self.parked.push(Parked {
                    conn: conn.clone(),
                    id: next,
                    base: Layout::new(Vec::new(), Vec::new()),
                    detached: full.detached.clone(),
                    tabs: leaving.tabs,
                    moving: true,
                    pending_layout: Some((at_park, full)),
                    epoch: 0,
                    active: None,
                });
                self.move_closes.push(MoveClose {
                    conn: conn.clone(),
                    workspace: id,
                    into: next,
                    source_revision,
                    tabs: moved,
                    sessions: moved_sessions,
                    after_seq: next_save_seq(),
                });
            }
            let plan = ConnectPlan {
                endpoint: conn.endpoint().clone(),
                mode: AttachMode::Join(Some(next)),
                open_settings: None,
                plain: false,
                conn: Some(conn.clone()),
                join: None,
                active_tab: None,
            };
            self.open_connect_tab_at(plan, pos, window, cx);
            return;
        }
        // Not shown here: its tabs come from its layout.
        enum Done {
            Changed,
            Ended,
            Move { revision: u64, layout: Layout, sessions: HashMap<SessionId, Arc<Session>> },
        }
        let size = self.pane_size(cx);
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            async move {
                let Some(wl) = conn.get_workspace(id)? else {
                    return anyhow::Ok(Done::Ended);
                };
                let revision = wl.revision;
                let layout = Layout::parse(&wl.json, &wl.sessions.iter().copied().collect());
                if kill {
                    let all = layout.sessions();
                    if all.iter().any(|s| !confirmed.contains(s)) {
                        return anyhow::Ok(Done::Changed);
                    }
                    for s in all {
                        if let Err(e) = conn.kill(s) {
                            log::warn!("ending session {s}: {e:#}");
                        }
                    }
                    conn.close_workspace(id)?;
                    return Ok(Done::Ended);
                }
                let ids: Vec<SessionId> = layout.tabs.iter().flat_map(|t| t.tree.leaves()).collect();
                let sessions = saved::attach_all(&conn, size, &ids)?;
                Ok(Done::Move { revision, layout, sessions })
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let mut result = Some(task.await);
            let delivered = this.update_in(cx, |this, window, cx| {
                let result = result.take().expect("close result");
                if let Some(p) = this.parked.iter().position(|p| Arc::ptr_eq(&p.conn, &conn) && p.id == id) {
                    let parked = this.parked.remove(p);
                    if !matches!(result, Ok(Done::Changed)) {
                        this.release_tabs(parked.tabs, cx);
                    } else {
                        this.parked.push(parked);
                    }
                }
                match result {
                    Ok(Done::Changed) => this.ask_close_again(&conn, id, window, cx),
                    Ok(Done::Ended) => {}
                    Ok(Done::Move { revision, layout, sessions }) => {
                        // Into the workspace the question named, or not at all.
                        let Some(i) = this.shown_index(&conn).filter(|&i| into.is_none_or(|w| this.shown[i].id == w)) else {
                            for s in sessions.values() {
                                s.release_view();
                            }
                            let toast = Toast::info("Nothing was moved").body("This window shows another workspace there now.");
                            toast::show_in(window, cx, toast.key("workspace.close"));
                            return;
                        };
                        let dest_id = this.shown[i].id;
                        // Panes whose shell ended meanwhile drop out of their tab.
                        let mut moved = Layout::new(layout.tabs, layout.detached);
                        let alive: HashSet<SessionId> =
                            moved.tabs.iter().flat_map(|t| t.tree.leaves()).filter(|s| sessions.contains_key(s)).collect();
                        let detached: HashSet<SessionId> = moved.detached.iter().flat_map(|t| t.tree.leaves()).collect();
                        moved.prune(&alive.union(&detached).copied().collect());
                        let ids: Vec<u64> = moved.tabs.iter().chain(&moved.detached).map(|t| t.id).collect();
                        let moved_sessions: HashSet<SessionId> = moved.sessions().into_iter().collect();
                        if ids.is_empty() {
                            this.close_source_if_unchanged(conn.clone(), id, dest_id, revision, HashSet::new(), cx);
                            return;
                        }
                        let mut target = this.local_target(i, cx);
                        target.tabs.extend(moved.tabs);
                        target.detached.extend(moved.detached);
                        // Closed once they're saved here (and only if the source
                        // revision still matches our snapshot).
                        this.move_closes.push(MoveClose {
                            conn: conn.clone(),
                            workspace: id,
                            into: dest_id,
                            source_revision: revision,
                            tabs: ids,
                            sessions: moved_sessions,
                            after_seq: next_save_seq(),
                        });
                        this.reconcile(i, target, sessions, None, window, cx);
                        this.save_layout(cx);
                    }
                    Err(e) => {
                        toast::show_in(
                            window,
                            cx,
                            Toast::error("Couldn't close the workspace").body(format!("{e:#}")).key("workspace.close"),
                        );
                    }
                }
            });
            if delivered.is_err()
                && let Some(Ok(Done::Move { sessions, .. })) = result
            {
                for s in sessions.values() {
                    s.release_view();
                }
            }
        })
        .detach();
    }

    /// Tell the daemon workspace `id` is gone (its sessions keep running).
    fn forget_workspace_on_daemon(&mut self, conn: Arc<Connection>, id: WorkspaceId, cx: &mut Context<'_, Self>) {
        let task = cx.background_executor().spawn(async move { conn.close_workspace(id) });
        cx.spawn(async move |_, cx| {
            if let Err(e) = task.await {
                cx.update(|cx| toast::show(cx, Toast::error("Couldn't close the workspace").body(format!("{e:#}")).key("workspace.close")));
            }
        })
        .detach();
    }

    /// The window is going away: it stops showing its workspaces and lets
    /// go of every session (they keep running).
    pub(super) fn leave_all(&mut self, cx: &mut gpui::App) {
        for sh in self.shown.drain(..) {
            sh.conn.leave_workspace(self.join.view);
        }
        for p in self.parked.drain(..) {
            for t in &p.tabs {
                for pane in &t.panes {
                    pane.view.read(cx).session().release_view();
                }
            }
        }
    }

    /// Pane `id` of a parked workspace, if it's one: `(parked, tab index)`.
    pub(super) fn find_parked_pane(&self, id: super::PaneId) -> Option<(usize, usize)> {
        self.parked.iter().enumerate().find_map(|(p, parked)| parked.tabs.iter().position(|t| t.pane(id).is_some()).map(|t| (p, t)))
    }

    /// A parked workspace's pane ended: it leaves its tab (the tab goes with
    /// its last pane).
    pub(super) fn remove_parked_pane(&mut self, id: super::PaneId, cx: &mut Context<'_, Self>) {
        let Some((p, t)) = self.find_parked_pane(id) else { return };
        let tab = &mut self.parked[p].tabs[t];
        if tab.tree == Node::Leaf(id) {
            self.parked[p].tabs.remove(t);
        } else {
            tab.tree.remove(id);
            tab.panes.retain(|pane| pane.id != id);
            if tab.focused == id {
                tab.focused = tab.tree.leaves()[0];
            }
        }
        self.forget_notices(id, cx);
        cx.notify();
    }

    /// Mark a parked workspace's tab for activity in one of its panes.
    pub(super) fn mark_parked_activity(&mut self, pane: super::PaneId, activity: Activity, cx: &mut Context<'_, Self>) {
        let Some((p, t)) = self.find_parked_pane(pane) else { return };
        let tab = &mut self.parked[p].tabs[t];
        if tab.activity < Some(activity) {
            tab.activity = Some(activity);
            cx.notify();
        }
    }

    /// What happened in the parked workspace `id` on `conn` meanwhile.
    pub(super) fn parked_activity(&self, conn: &Arc<Connection>, id: WorkspaceId) -> Option<Activity> {
        self.parked.iter().find(|p| Arc::ptr_eq(&p.conn, conn) && p.id == id).and_then(Parked::activity)
    }
}
