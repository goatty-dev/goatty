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
    /// The newest activity mark heard from the host: a list older than it
    /// (sent before this window joined a workspace made just now) is stale.
    pub newest: u64,
}

impl Shown {
    fn new(conn: Arc<Connection>, info: &WorkspaceInfo, revision: u64, base: Layout, list: Vec<WorkspaceInfo>) -> Self {
        Self {
            conn,
            id: info.id,
            revision,
            detached: base.detached.clone(),
            target: base.clone(),
            base,
            attaching: HashSet::new(),
            saving: false,
            pending: None,
            newest: list.iter().map(|w| w.active).max().unwrap_or(0).max(info.active),
            list,
        }
    }

    /// Take the host's list `list`, unless it's older than one heard
    /// already. False when it was stale.
    fn take_list(&mut self, list: Vec<WorkspaceInfo>) -> bool {
        let newest = list.iter().map(|w| w.active).max().unwrap_or(0);
        if newest < self.newest {
            return false;
        }
        self.newest = newest;
        self.list = list;
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

    fn save_shown(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        let sh = &self.shown[i];
        if sh.saving || !sh.attaching.is_empty() || self.divider_dragging {
            return;
        }
        let mine = self.mine(i, cx);
        let sh = &mut self.shown[i];
        if mine.same(&sh.base) {
            return;
        }
        sh.saving = true;
        let (conn, id, base, view) = (sh.conn.clone(), sh.id, sh.revision, self.join.view);
        let (json, sessions, tabs) = (mine.to_json(), mine.sessions(), u32::try_from(mine.tabs.len()).unwrap_or(u32::MAX));
        let key = conn.key();
        let task = cx.background_executor().spawn(async move { conn.save_workspace(view, id, base, tabs, sessions, json) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| this.saved(key, id, mine, result, cx));
        })
        .detach();
    }

    /// Save host `i`'s tabs now, without waiting for the answer (the window
    /// stops showing that workspace right after).
    pub(super) fn save_shown_now(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        let mine = self.mine(i, cx);
        let sh = &self.shown[i];
        if mine.same(&sh.base) {
            return;
        }
        let (conn, id, base, view) = (sh.conn.clone(), sh.id, sh.revision, self.join.view);
        let (json, sessions, tabs) = (mine.to_json(), mine.sessions(), u32::try_from(mine.tabs.len()).unwrap_or(u32::MAX));
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = conn.save_workspace(view, id, base, tabs, sessions, json) {
                    log::info!("saving a workspace the window left: {e:#}");
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

    /// A save of workspace `id` on connection `key` was answered.
    fn saved(&mut self, key: usize, id: WorkspaceId, sent: Layout, result: anyhow::Result<SaveOutcome>, cx: &mut Context<'_, Self>) {
        let Some(i) = self.shown_by_key(key).filter(|&i| self.shown[i].id == id) else { return };
        self.shown[i].saving = false;
        match result {
            Ok(SaveOutcome::Saved { revision }) => {
                let sh = &mut self.shown[i];
                sh.revision = revision;
                let ended = sent.tabs.is_empty() && sent.detached.is_empty();
                sh.base = sent;
                if ended {
                    self.ended_here(i, cx);
                    return;
                }
            }
            // Someone saved first: re-apply this window's edit on theirs.
            Ok(SaveOutcome::Conflict(layout)) => self.shown[i].pending = Some(layout),
            Ok(SaveOutcome::Gone) => {
                self.workspace_gone(i, cx);
                return;
            }
            // The link dropped: the window saves again when it joins again.
            Err(e) if e.downcast_ref::<ReplyLost>().is_some() || !self.shown[i].conn.is_connected() => {}
            Err(e) => {
                log::warn!("saving the workspace: {e:#}");
                toast::show(cx, Toast::error("Couldn't save the workspace").body(format!("{e:#}")).key("workspace.save"));
                // Don't try the same again and again.
                self.shown[i].base = sent;
            }
        }
        if let Some(layout) = self.shown[i].pending.take() {
            self.defer_remote(i, layout, cx);
        }
        self.save_layout(cx);
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
        match event {
            WorkspaceEvent::Changed(layout) => self.apply_remote(key, layout, window, cx),
            WorkspaceEvent::List(list) => {
                let Some(i) = self.shown_by_key(key) else { return };
                if list.iter().map(|w| w.active).max().unwrap_or(0) < self.shown[i].newest {
                    return;
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
                self.shown[i].take_list(list);
                if gone {
                    self.workspace_gone(i, cx);
                }
                self.refresh_switcher(cx);
                cx.notify();
            }
            WorkspaceEvent::Rejoined { info, layout } => {
                let Some(i) = self.shown_by_key(key) else { return };
                self.shown[i].saving = false;
                self.shown[i].newest = self.shown[i].newest.max(info.active);
                if self.shown[i].id != info.id {
                    // Its workspace ended while the link was down: what the
                    // daemon joined it to instead replaces it.
                    let mine = self.mine(i, cx);
                    let sh = &mut self.shown[i];
                    sh.id = info.id;
                    sh.base = mine;
                }
                self.apply_remote(key, layout, window, cx);
            }
        }
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
            sh.pending = Some(layout);
            return;
        }
        let alive: HashSet<SessionId> = layout.sessions.iter().copied().collect();
        let theirs = Layout::parse(&layout.json, &alive);
        let mine = self.mine(i, cx);
        let merged = saved::merge(&self.shown[i].base, &mine, &theirs);
        let sh = &mut self.shown[i];
        sh.base = theirs;
        sh.revision = layout.revision;
        if merged.tabs.is_empty() && merged.detached.is_empty() {
            // Emptied elsewhere: it ended (the list says so next).
            return;
        }
        self.reconcile(i, merged, HashMap::new(), None, window, cx);
        self.save_layout(cx);
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
                let mut taken = Vec::new();
                t.panes.retain(|pane| {
                    let id = pane.view.read(cx).session().daemon_id();
                    let take = id.is_some_and(|id| wanted.contains(&id) && !pool.contains_key(&id));
                    if take {
                        taken.push(id.unwrap_or_default());
                    }
                    !take
                });
                let _ = taken;
            }
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
            s.add_view();
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
        if let Some(s) = self.show_when_attached
            && let Some(t) = self.tabs.iter().find(|t| t.panes.iter().any(|p| p.view.read(cx).session().daemon_id() == Some(s)))
        {
            active_id = Some(t.id);
            self.show_when_attached = None;
        }
        self.after_tabs_replaced(active_id, pos, n, window, cx);
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
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(i) = this.shown_index(&conn) else {
                    if let Ok(sessions) = result {
                        for s in sessions.values() {
                            s.add_view();
                            s.release_view();
                        }
                    }
                    return;
                };
                for id in &ids {
                    this.shown[i].attaching.remove(id);
                }
                let fresh = match result {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("attaching another window's tabs: {e:#}");
                        HashMap::new()
                    }
                };
                // Sessions that couldn't be attached ended: their panes go.
                let mut target = this.shown[i].target.clone();
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
        let Joined { info, list, revision, base, show, sessions, extra } = *j;
        // A second connecting tab to a host this window shows already (or a
        // window that switched while connecting): the same workspace takes
        // this one's tabs in.
        let i = match self.shown_index(&conn) {
            Some(i) if self.shown[i].id == info.id => i,
            Some(i) => {
                let at = self.park(i, cx);
                let _ = at;
                self.shown.push(Shown::new(conn.clone(), &info, revision, base.clone(), list.clone()));
                self.shown.len() - 1
            }
            None => {
                self.shown.push(Shown::new(conn.clone(), &info, revision, base.clone(), list.clone()));
                self.shown.len() - 1
            }
        };
        if !self.shown[i].take_list(list) {
            // An older list than one heard: it still has to show this one.
            if !self.shown[i].list.iter().any(|w| w.id == info.id) {
                self.shown[i].list.push(info.clone());
            }
        }
        self.shown[i].newest = self.shown[i].newest.max(info.active);
        // Its tabs from when this window showed it before.
        let mut target = show;
        if let Some(p) = self.parked.iter().position(|p| Arc::ptr_eq(&p.conn, &conn) && p.id == info.id) {
            let parked = self.parked.remove(p);
            let mine = Layout::new(parked.tabs.iter().filter_map(|t| super::saved_tab(t, cx)).collect(), parked.detached.clone());
            target = saved::merge(&parked.base, &mine, &target);
            let at = index.min(self.tabs.len());
            for (k, t) in parked.tabs.into_iter().enumerate() {
                self.tabs.insert(at + k, t);
            }
        }
        self.shown[i].base = base;
        self.shown[i].revision = revision;
        self.reconcile(i, target, sessions, Some(index), window, cx);
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
        // Its last edit first (a parked workspace saves nothing more).
        self.save_shown(i, cx);
        let sh = self.shown.remove(i);
        let pos = self.tabs.iter().position(|t| on_conn(t, &sh.conn)).unwrap_or(self.tabs.len());
        let active_id = self.active_tab().map(|t| t.id);
        let (tabs, rest): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| on_conn(t, &sh.conn));
        self.tabs = rest;
        if let Some(id) = active_id {
            self.active = self.tab_index(id).unwrap_or(pos.min(self.tabs.len().saturating_sub(1)));
        }
        self.parked.retain(|p| !(Arc::ptr_eq(&p.conn, &sh.conn) && p.id == sh.id));
        self.parked.push(Parked { conn: sh.conn, id: sh.id, base: sh.base, detached: sh.detached, tabs });
        pos
    }

    /// Show workspace `id` on host `conn` in this window instead of the
    /// one it shows there now.
    pub(super) fn switch_workspace(&mut self, conn: &Arc<Connection>, id: WorkspaceId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.shown_index(conn) else { return };
        if self.shown[i].id == id {
            return;
        }
        self.close_switcher(window, cx);
        let pos = self.park(i, cx);
        let plan = ConnectPlan {
            endpoint: conn.endpoint().clone(),
            mode: AttachMode::Join(Some(id)),
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
        self.parked.push(Parked { conn: sh.conn.clone(), id: sh.id, base: sh.base, detached: Vec::new(), tabs });
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
            this.release_parked(&conn, None, cx);
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

    /// Parked workspaces on `conn` (all but `keep`) let go of their sessions.
    fn release_parked(&mut self, conn: &Arc<Connection>, keep: Option<WorkspaceId>, cx: &mut Context<'_, Self>) {
        let (gone, stay): (Vec<Parked>, Vec<Parked>) = std::mem::take(&mut self.parked)
            .into_iter()
            .partition(|p| Arc::ptr_eq(&p.conn, conn) && Some(p.id) != keep && !p.tabs.iter().any(|t| !t.panes.is_empty()) || false);
        self.parked = stay;
        for p in gone {
            self.release_tabs(p.tabs, cx);
        }
        // Parked workspaces that no longer exist on the host.
        let known: HashSet<WorkspaceId> =
            self.shown.iter().filter(|s| Arc::ptr_eq(&s.conn, conn)).flat_map(|s| s.list.iter().map(|w| w.id)).collect();
        if known.is_empty() {
            return;
        }
        let (gone, stay): (Vec<Parked>, Vec<Parked>) =
            std::mem::take(&mut self.parked).into_iter().partition(|p| Arc::ptr_eq(&p.conn, conn) && !known.contains(&p.id));
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
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            async move {
                let (info, layout) = conn.create_workspace(join.view, None, join.sink.clone())?;
                let list = conn.list_workspaces().unwrap_or_else(|_| vec![info.clone()]);
                let session = Session::daemon(conn.clone(), size, None, cwd)?;
                anyhow::Ok(Box::new(Joined {
                    info,
                    list,
                    revision: layout.revision,
                    base: Layout::new(Vec::new(), Vec::new()),
                    show: Layout::new(Vec::new(), Vec::new()),
                    sessions: HashMap::new(),
                    extra: vec![(session, None)],
                }))
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
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
        })
        .detach();
    }

    pub(super) fn rename_workspace(&mut self, conn: &Arc<Connection>, id: WorkspaceId, name: String, cx: &mut Context<'_, Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        // Shown right away; the daemon's list confirms it.
        for sh in self.shown.iter_mut().filter(|s| Arc::ptr_eq(&s.conn, conn)) {
            if let Some(w) = sh.list.iter_mut().find(|w| w.id == id) {
                w.name = name.clone();
            }
        }
        cx.notify();
        let conn = conn.clone();
        let task = cx.background_executor().spawn(async move { conn.rename_workspace(id, name) });
        cx.spawn(async move |_, cx| {
            if let Err(e) = task.await {
                cx.update(|cx| {
                    toast::show(cx, Toast::error("Couldn't rename the workspace").body(format!("{e:#}")).key("workspace.rename"))
                });
            }
        })
        .detach();
    }

    /// Close workspace `id` on `conn`: an empty or detached one right away;
    /// one with running shells asks first whether to move its tabs to
    /// another workspace or end them.
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
        let tabs = if here { self.tabs.iter().filter(|t| on_conn(t, conn)).count() } else { info.tabs as usize };
        self.close_switcher(window, cx);
        if tabs == 0 || info.sessions.is_empty() {
            self.finish_close(conn, id, None, false, window, cx);
            return;
        }
        let shells = info.sessions.len();
        let me = cx.entity().downgrade();
        let tabs_text = if tabs == 1 { "1 tab".to_string() } else { format!("{tabs} tabs") };
        let shells_text = if shells == 1 { "1 shell".to_string() } else { format!("{shells} shells") };
        let choices = vec![
            ("move".to_string(), format!("Move {tabs_text} to {}", into.name), "Nothing ends; they show up there".to_string()),
            ("close".to_string(), format!("Close them (ends {shells_text})"), "Whatever runs in them stops".to_string()),
        ];
        let mut field = crate::command_palette::Field::choice("what", &format!("Close “{}”: what about its tabs?", info.name), choices);
        field.spec.default = Some("move".into());
        let conn = conn.clone();
        let ask = crate::command_palette::Ask::new("Close workspace", vec![field], move |answers, window, cx| {
            let kill = answers.get("what").is_some_and(|a| a == "close");
            let _ = me.update(cx, |this, cx| this.finish_close(&conn, id, Some(into.id), kill, window, cx));
        });
        self.open_ask(ask, window, cx);
    }

    /// Close workspace `id`: its tabs move into workspace `into` (or end,
    /// with `kill`), then the daemon forgets it.
    fn finish_close(
        &mut self,
        conn: &Arc<Connection>,
        id: WorkspaceId,
        into: Option<WorkspaceId>,
        kill: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(i) = self.shown_index(conn) else { return };
        let here = self.shown[i].id == id;
        let conn = conn.clone();
        if here {
            // Show the other one; this one's tabs go with it (or end).
            let next = into.or_else(|| self.shown[i].list.iter().filter(|w| w.id != id).max_by_key(|w| w.active).map(|w| w.id));
            let Some(next) = next else { return };
            let pos = self.park(i, cx);
            let Some(p) = self.parked.iter().position(|p| Arc::ptr_eq(&p.conn, &conn) && p.id == id) else { return };
            let leaving = self.parked.remove(p);
            if kill {
                let sessions: Vec<Arc<Session>> =
                    leaving.tabs.iter().flat_map(|t| t.panes.iter()).map(|p| p.view.read(cx).session().clone()).collect();
                self.release_tabs(leaving.tabs, cx);
                self.kill_sessions(sessions, cx).detach();
            } else {
                // They join `next` as tabs this window added to it.
                match self.parked.iter_mut().find(|p| Arc::ptr_eq(&p.conn, &conn) && p.id == next) {
                    Some(p) => p.tabs.extend(leaving.tabs),
                    None => self.parked.push(Parked {
                        conn: conn.clone(),
                        id: next,
                        base: Layout::new(Vec::new(), Vec::new()),
                        detached: Vec::new(),
                        tabs: leaving.tabs,
                    }),
                }
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
            self.forget_workspace_on_daemon(conn, id, cx);
            return;
        }
        // Not shown here: its tabs come from its layout.
        let size = self.pane_size(cx);
        let task = cx.background_executor().spawn({
            let conn = conn.clone();
            async move {
                let layout = conn.get_workspace(id)?;
                let layout = layout.map(|l| Layout::parse(&l.json, &l.sessions.iter().copied().collect())).unwrap_or_default();
                let ids: Vec<SessionId> = layout.tabs.iter().flat_map(|t| t.tree.leaves()).collect();
                let sessions = if kill {
                    for &s in &ids {
                        if let Err(e) = conn.kill(s) {
                            log::warn!("ending session {s}: {e:#}");
                        }
                    }
                    HashMap::new()
                } else {
                    saved::attach_all(&conn, size, &ids)?
                };
                conn.close_workspace(id)?;
                anyhow::Ok((layout, sessions))
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok((layout, sessions)) => {
                    this.parked.retain(|p| !(Arc::ptr_eq(&p.conn, &conn) && p.id == id));
                    let Some(i) = this.shown_index(&conn) else { return };
                    if !kill && !layout.tabs.is_empty() {
                        let mut target = this.mine(i, cx);
                        target.tabs.extend(layout.tabs.into_iter().filter(|t| t.tree.leaves().iter().all(|s| sessions.contains_key(s))));
                        this.reconcile(i, target, sessions, None, window, cx);
                        this.save_layout(cx);
                    }
                }
                Err(e) => {
                    toast::show_in(window, cx, Toast::error("Couldn't close the workspace").body(format!("{e:#}")).key("workspace.close"));
                }
            });
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
