//! Workspaces (#215): the daemon's tabs and splits, shared by every client
//! window that shows them, tmux-session style.
//!
//! The layout itself is the client's JSON; the daemon only knows a
//! workspace's name, which sessions it shows, how many tabs it has, which
//! windows (`(connection, view)`) show it now, and a revision. A save is
//! accepted only when it was made on the newest revision; otherwise the
//! client gets the newest layout, re-applies its edit and saves again
//! ([`Workspaces::save`]). Every accepted save goes out to the other windows
//! showing it (`ServerMsg::LayoutChanged`, sent by `crate::client`).
//!
//! Lifecycle: there's always at least one workspace ("Main" on a fresh
//! daemon). Others are made explicitly ([`Workspaces::create`]) and end when
//! a save leaves them without sessions, when they're closed, or when their
//! sessions all ended while no window showed them. Workspaces carry over to
//! the next goattyd on a restart ([`Workspaces::export`]); nothing shows
//! them then.

use std::collections::{HashMap, HashSet};

use goatty_proto::{SaveOutcome, SessionId, ViewId, WorkspaceId, WorkspaceInfo, WorkspaceLayout};
use serde::{Deserialize, Serialize};

/// Larger layouts are refused (a window with hundreds of panes is still far
/// below this).
pub(crate) const MAX_JSON: usize = 256 << 10;
/// Longer names are cut.
const MAX_NAME: usize = 64;
/// The first workspace's name.
pub(crate) const MAIN: &str = "Main";

/// A window: the connection, and its view id on it.
type Viewer = (u64, ViewId);

struct Record {
    name: String,
    json: String,
    sessions: Vec<SessionId>,
    tabs: u32,
    revision: u64,
    /// Order of making: lists show workspaces in it.
    created: u64,
    /// Larger is more recently active.
    active: u64,
    views: HashSet<Viewer>,
}

#[derive(Default)]
pub(crate) struct Workspaces {
    map: HashMap<WorkspaceId, Record>,
    /// For `created` and `active`.
    seq: u64,
    /// Grows with every change to what [`Self::list`] says (`ServerMsg::Workspaces::seq`).
    version: u64,
}

/// A workspace as the next goattyd gets it (see `crate::restore`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub json: String,
    pub sessions: Vec<SessionId>,
    pub tabs: u32,
    pub revision: u64,
    pub created: u64,
    pub active: u64,
}

/// What a request changed, for `crate::client` to tell the windows.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Changes {
    /// The list changed (`WorkspacesChanged` to every watching connection).
    pub list: bool,
    /// A layout was saved: (who saved it, the connections that show it).
    pub layout: Option<(ViewId, WorkspaceLayout, Vec<u64>)>,
}

/// A fresh random id (a restarted daemon's ids never collide with an old
/// window's record of another daemon's).
fn new_id() -> WorkspaceId {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    h.finish().max(1)
}

/// `name` trimmed and cut, or `None` when nothing is left.
pub(crate) fn clean_name(name: &str) -> Option<String> {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

impl Record {
    fn new(name: String, seq: u64) -> Self {
        Self { name, json: String::new(), sessions: Vec::new(), tabs: 0, revision: 0, created: seq, active: seq, views: HashSet::new() }
    }

    fn info(&self, id: WorkspaceId, alive: &impl Fn(SessionId) -> bool) -> WorkspaceInfo {
        WorkspaceInfo {
            id,
            name: self.name.clone(),
            revision: self.revision,
            tabs: self.tabs,
            sessions: self.sessions.iter().copied().filter(|&s| alive(s)).collect(),
            views: u32::try_from(self.views.len()).unwrap_or(u32::MAX),
            active: self.active,
        }
    }

    fn layout(&self, id: WorkspaceId, alive: &impl Fn(SessionId) -> bool) -> WorkspaceLayout {
        WorkspaceLayout {
            id,
            revision: self.revision,
            json: self.json.clone(),
            sessions: self.sessions.iter().copied().filter(|&s| alive(s)).collect(),
        }
    }
}

impl Workspaces {
    fn tick(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The list's version: larger is newer.
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    fn changed(&mut self) {
        self.version += 1;
    }

    /// Make "Main" when there's no workspace at all. True when it did.
    fn ensure_one(&mut self) -> bool {
        if !self.map.is_empty() {
            return false;
        }
        let seq = self.tick();
        self.map.insert(new_id(), Record::new(MAIN.into(), seq));
        self.changed();
        true
    }

    /// Drop the workspaces whose sessions all ended and that no window
    /// shows (never the last one). True when one went.
    pub(crate) fn prune(&mut self, alive: impl Fn(SessionId) -> bool) -> bool {
        self.prune_but(None, alive)
    }

    /// [`Self::prune`], keeping `keep` (a window is about to show it).
    fn prune_but(&mut self, keep: Option<WorkspaceId>, alive: impl Fn(SessionId) -> bool) -> bool {
        let before = self.map.len();
        let mut ids: Vec<(u64, WorkspaceId)> = self
            .map
            .iter()
            .filter(|(id, r)| Some(**id) != keep && r.views.is_empty() && !r.sessions.iter().any(|&s| alive(s)))
            .map(|(&id, r)| (r.created, id))
            .collect();
        // The newest dead one stays when every workspace is dead.
        ids.sort_unstable();
        if ids.len() == self.map.len() {
            ids.pop();
        }
        for (_, id) in ids {
            self.map.remove(&id);
        }
        let pruned = self.map.len() != before;
        if pruned {
            self.changed();
        }
        pruned
    }

    /// Session `id` ended: true when a workspace listed it (its list entry
    /// changed) or one ended with it.
    pub(crate) fn session_ended(&mut self, id: SessionId, alive: impl Fn(SessionId) -> bool) -> bool {
        let listed = self.map.values().any(|r| r.sessions.contains(&id));
        if listed {
            self.changed();
        }
        self.prune(alive) || listed
    }

    /// Every workspace, in the order they were made (see [`Self::prune`]).
    pub(crate) fn list(&mut self, alive: impl Fn(SessionId) -> bool) -> Vec<WorkspaceInfo> {
        self.prune(&alive);
        self.ensure_one();
        self.list_now(&alive)
    }

    fn list_now(&self, alive: &impl Fn(SessionId) -> bool) -> Vec<WorkspaceInfo> {
        let mut list: Vec<(u64, WorkspaceInfo)> = self.map.iter().map(|(&id, r)| (r.created, r.info(id, alive))).collect();
        list.sort_by_key(|(created, _)| *created);
        list.into_iter().map(|(_, i)| i).collect()
    }

    /// The most recently active workspace.
    fn newest(&self) -> Option<WorkspaceId> {
        self.map.iter().max_by_key(|(_, r)| r.active).map(|(&id, _)| id)
    }

    /// Window `(conn, view)` shows workspace `id` (else the most recently
    /// active one) from now on.
    pub(crate) fn join(
        &mut self,
        conn: u64,
        view: ViewId,
        id: Option<WorkspaceId>,
        alive: impl Fn(SessionId) -> bool,
    ) -> (WorkspaceInfo, WorkspaceLayout) {
        self.leave(conn, view);
        self.prune_but(id, &alive);
        self.ensure_one();
        let id = id.filter(|id| self.map.contains_key(id)).or_else(|| self.newest()).expect("there's always a workspace");
        self.show(conn, view, id, alive)
    }

    fn show(&mut self, conn: u64, view: ViewId, id: WorkspaceId, alive: impl Fn(SessionId) -> bool) -> (WorkspaceInfo, WorkspaceLayout) {
        let seq = self.tick();
        self.changed();
        let r = self.map.get_mut(&id).expect("shown workspaces exist");
        r.views.insert((conn, view));
        r.active = seq;
        (r.info(id, &alive), r.layout(id, &alive))
    }

    /// A new workspace (`name`, else the first free "Workspace N"), shown by
    /// `(conn, view)`.
    pub(crate) fn create(
        &mut self,
        conn: u64,
        view: ViewId,
        name: Option<&str>,
        alive: impl Fn(SessionId) -> bool,
    ) -> (WorkspaceInfo, WorkspaceLayout) {
        self.leave(conn, view);
        self.ensure_one();
        let name = name.and_then(clean_name).unwrap_or_else(|| self.next_name());
        let (id, seq) = (new_id(), self.tick());
        self.map.insert(id, Record::new(name, seq));
        self.show(conn, view, id, alive)
    }

    fn next_name(&self) -> String {
        let taken: HashSet<&str> = self.map.values().map(|r| r.name.as_str()).collect();
        (2..).map(|n| format!("Workspace {n}")).find(|n| !taken.contains(n.as_str())).expect("names run out never")
    }

    /// Window `(conn, view)` shows no workspace any more. True when it showed one.
    pub(crate) fn leave(&mut self, conn: u64, view: ViewId) -> bool {
        let left = self.map.values_mut().any(|r| r.views.remove(&(conn, view)));
        if left {
            self.changed();
        }
        left
    }

    /// Connection `conn` ended: its windows show nothing. True when one showed something.
    pub(crate) fn drop_conn(&mut self, conn: u64) -> bool {
        let mut any = false;
        for r in self.map.values_mut() {
            let before = r.views.len();
            r.views.retain(|(c, _)| *c != conn);
            any |= r.views.len() != before;
        }
        if any {
            self.changed();
        }
        any
    }

    /// Save workspace `id`'s layout, made from revision `base`, by window
    /// `(conn, view)`. See `ClientMsg::SaveWorkspace`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn save(
        &mut self,
        (conn, view): Viewer,
        id: WorkspaceId,
        base: u64,
        tabs: u32,
        sessions: Vec<SessionId>,
        json: String,
        alive: impl Fn(SessionId) -> bool,
    ) -> Result<(SaveOutcome, Changes), String> {
        if json.len() > MAX_JSON {
            return Err(format!("a workspace layout of {} bytes is too large", json.len()));
        }
        let seq = self.tick();
        let only = self.map.len() == 1;
        let Some(r) = self.map.get_mut(&id) else { return Ok((SaveOutcome::Gone, Changes::default())) };
        if r.revision != base {
            return Ok((SaveOutcome::Conflict(r.layout(id, &alive)), Changes::default()));
        }
        let ends = !only && !sessions.iter().any(|&s| alive(s));
        r.revision += 1;
        let revision = r.revision;
        if ends {
            // Its last tab closed: the windows that show it hear it went.
            self.map.remove(&id);
            self.changed();
            return Ok((SaveOutcome::Saved { revision }, Changes { list: true, layout: None }));
        }
        // Lists show tab counts and sessions (how many shells closing it ends).
        let list = r.tabs != tabs || r.sessions != sessions;
        (r.json, r.sessions, r.tabs, r.active) = (json, sessions, tabs, seq);
        let conns: Vec<u64> = {
            let mut c: Vec<u64> = r.views.iter().filter(|&&v| v != (conn, view)).map(|(c, _)| *c).collect();
            c.sort_unstable();
            c.dedup();
            c
        };
        let layout = r.layout(id, &alive);
        if list {
            self.changed();
        }
        Ok((SaveOutcome::Saved { revision }, Changes { list, layout: Some((view, layout, conns)) }))
    }

    pub(crate) fn rename(&mut self, id: WorkspaceId, name: &str) -> Result<(), String> {
        let name = clean_name(name).ok_or("a workspace needs a name")?;
        let r = self.map.get_mut(&id).ok_or("that workspace is gone")?;
        r.name = name;
        self.changed();
        Ok(())
    }

    /// Forget workspace `id` (its sessions keep running). Never the last one.
    pub(crate) fn close(&mut self, id: WorkspaceId) -> Result<(), String> {
        if !self.map.contains_key(&id) {
            return Err("that workspace is gone".into());
        }
        if self.map.len() == 1 {
            return Err("the host's only workspace can't be closed".into());
        }
        self.map.remove(&id);
        self.changed();
        Ok(())
    }

    pub(crate) fn get(&self, id: WorkspaceId, alive: impl Fn(SessionId) -> bool) -> Option<WorkspaceLayout> {
        self.map.get(&id).map(|r| r.layout(id, &alive))
    }

    /// Every connection that has a window on a workspace.
    #[cfg(test)]
    fn viewers(&self, id: WorkspaceId) -> Vec<Viewer> {
        let mut v: Vec<Viewer> = self.map.get(&id).map(|r| r.views.iter().copied().collect()).unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// Every workspace, for the next goattyd.
    pub(crate) fn export(&self) -> Vec<SavedWorkspace> {
        let mut list: Vec<SavedWorkspace> = self
            .map
            .iter()
            .map(|(&id, r)| SavedWorkspace {
                id,
                name: r.name.clone(),
                json: r.json.clone(),
                sessions: r.sessions.clone(),
                tabs: r.tabs,
                revision: r.revision,
                created: r.created,
                active: r.active,
            })
            .collect();
        list.sort_by_key(|w| w.created);
        list
    }

    /// The previous goattyd's workspaces, without windows: its connections
    /// are gone. Ones whose sessions all ended go (see [`Self::prune`]).
    pub(crate) fn import(list: Vec<SavedWorkspace>, alive: impl Fn(SessionId) -> bool) -> Self {
        let seq = list.iter().map(|w| w.created.max(w.active)).max().unwrap_or(0);
        let map = list
            .into_iter()
            .map(|w| {
                let mut r = Record::new(w.name, w.created);
                (r.json, r.sessions, r.tabs, r.revision, r.active) = (w.json, w.sessions, w.tabs, w.revision, w.active);
                (w.id, r)
            })
            .collect();
        let mut this = Self { map, seq, version: 0 };
        this.prune(alive);
        this
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(_: SessionId) -> bool {
        true
    }

    fn save(w: &mut Workspaces, who: Viewer, id: WorkspaceId, base: u64, sessions: &[SessionId], json: &str) -> SaveOutcome {
        w.save(who, id, base, sessions.len() as u32, sessions.to_vec(), json.into(), all).unwrap().0
    }

    #[test]
    fn a_fresh_daemon_has_main_and_windows_share_it() {
        let mut w = Workspaces::default();
        let list = w.list(all);
        assert_eq!(list.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), [MAIN]);
        let (a, la) = w.join(1, 10, None, all);
        let (b, _) = w.join(2, 20, None, all);
        assert_eq!((a.id, b.id, la.revision, la.json.as_str()), (list[0].id, list[0].id, 0, ""));
        assert_eq!(b.views, 2);
        assert_eq!(w.viewers(a.id), [(1, 10), (2, 20)]);
        // Two windows of one connection count twice.
        w.join(1, 11, Some(a.id), all);
        assert_eq!(w.list(all)[0].views, 3);
    }

    #[test]
    fn saves_are_revisioned_and_go_to_the_other_windows() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        w.join(2, 20, Some(main.id), all);
        w.join(1, 11, Some(main.id), all);
        let (out, changes) = w.save((1, 10), main.id, 0, 2, vec![1, 2], "a".into(), all).unwrap();
        assert_eq!(out, SaveOutcome::Saved { revision: 1 });
        let (by, layout, conns) = changes.layout.unwrap();
        // Connection 1 too: its other window shows it.
        assert_eq!((by, layout.json.as_str(), conns), (10, "a", vec![1, 2]));
        assert!(changes.list, "the tab count changed");
        // A save from revision 0 again conflicts and carries the newest layout.
        match save(&mut w, (2, 20), main.id, 0, &[1, 2, 3], "b") {
            SaveOutcome::Conflict(l) => assert_eq!((l.revision, l.json.as_str(), l.sessions), (1, "a", vec![1, 2])),
            other => panic!("{other:?}"),
        }
        assert_eq!(save(&mut w, (2, 20), main.id, 1, &[1, 2, 3], "b"), SaveOutcome::Saved { revision: 2 });
        assert_eq!(save(&mut w, (2, 20), 999, 0, &[1], "x"), SaveOutcome::Gone);
        assert!(w.save((1, 10), main.id, 2, 1, vec![1], "x".repeat(MAX_JSON + 1), all).is_err());
    }

    #[test]
    fn dead_sessions_are_pruned_and_a_workspace_ends_with_its_last_one() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        save(&mut w, (1, 10), main.id, 0, &[1, 2], "main");
        let (ops, _) = w.create(1, 10, None, all);
        assert_eq!(ops.name, "Workspace 2");
        save(&mut w, (1, 10), ops.id, 0, &[3, 4], "ops");
        // Listed with only the running sessions.
        assert_eq!(w.get(main.id, |s| s != 2).unwrap().sessions, [1]);
        // Shown: kept while its shells are gone (the window closes the tabs).
        assert_eq!(w.list(|s| s < 3).len(), 2);
        // Not shown and every session ended: gone.
        w.leave(1, 10);
        assert_eq!(w.list(|s| s < 3).iter().map(|i| i.id).collect::<Vec<_>>(), [main.id]);
        // The daemon's only workspace stays, even empty.
        assert_eq!(w.list(|_| false).iter().map(|i| i.id).collect::<Vec<_>>(), [main.id]);
        // Closing the last tab ends a workspace (not the only one).
        let (third, _) = w.create(1, 10, Some("  logs\n "), all);
        assert_eq!(third.name, "logs");
        save(&mut w, (1, 10), third.id, 0, &[5], "logs");
        let (out, changes) = w.save((1, 10), third.id, 1, 0, Vec::new(), String::new(), all).unwrap();
        assert_eq!((out, changes), (SaveOutcome::Saved { revision: 2 }, Changes { list: true, layout: None }));
        assert!(w.get(third.id, all).is_none());
        // ... but the only one keeps its (empty) layout.
        assert!(matches!(save(&mut w, (1, 10), main.id, 1, &[], ""), SaveOutcome::Saved { .. }));
        assert!(w.get(main.id, all).is_some());
    }

    #[test]
    fn an_empty_workspace_nobody_shows_goes() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        let (ops, _) = w.create(1, 10, None, all);
        save(&mut w, (1, 10), ops.id, 0, &[1], "ops");
        // Main never got a tab and nobody shows it now.
        assert_eq!(w.list(all).iter().map(|i| i.id).collect::<Vec<_>>(), [ops.id]);
        // Joining an empty workspace keeps it, though.
        let (third, _) = w.create(2, 20, None, all);
        w.leave(2, 20);
        assert_eq!(w.join(2, 20, Some(third.id), all).0.id, third.id);
        assert!(w.get(main.id, all).is_none());
    }

    #[test]
    fn joining_falls_back_to_the_most_recently_active() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        let (ops, _) = w.create(2, 20, None, all);
        // A gone id: the newest active one (ops, made last).
        assert_eq!(w.join(3, 30, Some(12345), all).0.id, ops.id);
        w.join(1, 10, Some(main.id), all);
        assert_eq!(w.join(4, 40, None, all).0.id, main.id);
        // A window shows one workspace per connection: joining another leaves it.
        w.join(1, 10, Some(ops.id), all);
        assert_eq!(w.viewers(main.id), [(4, 40)]);
        assert!(w.drop_conn(4));
        assert!(w.viewers(main.id).is_empty());
        assert!(!w.drop_conn(4));
    }

    #[test]
    fn every_change_to_the_list_moves_its_version() {
        let mut w = Workspaces::default();
        let mut last = w.version();
        let mut moved = |w: &Workspaces, what: &str| {
            assert!(w.version() > last, "{what} didn't move the version");
            last = w.version();
        };
        let (main, _) = w.join(1, 10, None, all);
        moved(&w, "join");
        save(&mut w, (1, 10), main.id, 0, &[1], "main");
        moved(&w, "a save with new tabs");
        let (ops, _) = w.create(1, 10, None, all);
        moved(&w, "create");
        w.rename(ops.id, "ops").unwrap();
        moved(&w, "rename");
        w.leave(1, 10);
        moved(&w, "leave");
        w.join(2, 20, Some(ops.id), all);
        w.close(ops.id).unwrap();
        moved(&w, "close (of the most recently active one)");
        w.join(3, 30, Some(main.id), all);
        w.drop_conn(3);
        moved(&w, "a connection ending");
    }

    #[test]
    fn a_listed_session_ending_changes_the_list() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        save(&mut w, (1, 10), main.id, 0, &[1, 2], "main");
        let before = w.version();
        assert!(w.session_ended(2, |s| s == 1), "Main listed it");
        assert!(w.version() > before);
        let before = w.version();
        assert!(!w.session_ended(9, |s| s == 1), "nobody listed it");
        assert_eq!(w.version(), before);
    }

    #[test]
    fn names_and_closing() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        save(&mut w, (1, 10), main.id, 0, &[1], "main");
        assert!(w.close(main.id).is_err(), "the only one");
        let (two, _) = w.create(1, 10, None, all);
        w.rename(two.id, "Workspace 3").unwrap();
        // Names fill the first gap.
        assert_eq!(w.create(1, 10, None, all).0.name, "Workspace 2");
        assert!(w.rename(two.id, "   ").is_err());
        assert!(w.rename(999, "x").is_err());
        w.close(two.id).unwrap();
        assert!(w.close(two.id).is_err());
        assert_eq!(w.list(all).iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), [MAIN, "Workspace 2"]);
        assert_eq!(clean_name(&"x".repeat(100)).unwrap().len(), MAX_NAME);
    }

    #[test]
    fn workspaces_carry_over_without_windows() {
        let mut w = Workspaces::default();
        let (main, _) = w.join(1, 10, None, all);
        save(&mut w, (1, 10), main.id, 0, &[1], "main");
        let (ops, _) = w.create(1, 10, Some("ops"), all);
        save(&mut w, (1, 10), ops.id, 0, &[2], "ops");
        let (gone, _) = w.create(1, 10, Some("gone"), all);
        save(&mut w, (1, 10), gone.id, 0, &[3], "gone");
        let mut next = Workspaces::import(w.export(), |s| s != 3);
        let list = next.list(all);
        assert_eq!(list.iter().map(|i| (i.name.as_str(), i.views, i.revision)).collect::<Vec<_>>(), [("Main", 0, 1), ("ops", 0, 1)]);
        // The most recently active one is still the one a window lands in.
        assert_eq!(next.join(5, 50, None, all).0.id, ops.id);
        assert_eq!(next.get(main.id, all).unwrap().json, "main");
        // Making more continues the order.
        let (later, _) = next.create(5, 50, None, all);
        assert_eq!(next.list(all).last().unwrap().id, later.id);
    }
}
