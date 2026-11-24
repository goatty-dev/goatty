//! A workspace's layout as goattyd keeps it (#215): its tabs, their split
//! trees with session ids as leaves, and names; the tabs detached from it
//! (their shells keep running); and the window's size, which only this
//! computer keeps (`reopen`).
//!
//! The daemon stores the JSON as it is and shares it between every window
//! that shows the workspace. What stays per window (the active tab, the
//! focused pane, zoom, scroll position, the window's size) isn't in it.
//! Every tab has a stable id, so edits from two windows are merged tab by
//! tab ([`merge`]): a save made on an old revision comes back with the
//! newest layout, and the window re-applies its own edit on it.
//!
//! [`startup`] joins a workspace on a host and attaches its sessions.
//!
//! Bump [`VERSION`] when the format changes incompatibly (an unreadable
//! layout reads as empty; its sessions stay detached on the host).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use goatty_client::{Connection, ReplyLost, WorkspaceSink};
use goatty_proto::{SessionId, ViewId, WorkspaceInfo};
use goatty_term::TermSize;
use gpui::{WindowBounds, px, size};
use serde::{Deserialize, Serialize};

use super::layout::{Direction, Node};
use crate::session::{AttachMode, Session};

/// v1 was the window layout of goatty before workspaces; it isn't read.
pub const VERSION: u32 = 2;

/// A workspace's layout (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Layout {
    pub version: u32,
    pub tabs: Vec<SavedTab>,
    /// Tabs detached from the workspace, oldest first: their shells keep
    /// running, and "Reattach" brings them back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detached: Vec<SavedTab>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    /// Stable across windows and saves (random).
    pub id: u64,
    /// Set by renaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Leaves are session ids.
    pub tree: Node,
    /// A plain SSH tab (`ssh HOST` in a session of this goattyd), shown in
    /// that host's group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain: Option<String>,
    /// When it was detached (unix ms; tabs detached together share it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_at: Option<u64>,
}

impl SavedTab {
    /// The same tab, ignoring float noise in split sizes.
    fn same(&self, other: &SavedTab) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.plain == other.plain
            && self.detached_at == other.detached_at
            && same_tree(&self.tree, &other.tree)
    }
}

/// Two trees are the same up to tiny differences in sizes (they go through
/// renormalizing on every round trip).
pub fn same_tree(a: &Node, b: &Node) -> bool {
    match (a, b) {
        (Node::Leaf(x), Node::Leaf(y)) => x == y,
        (Node::Split { axis: ax, children: ac, sizes: asz }, Node::Split { axis: bx, children: bc, sizes: bsz }) => {
            ax == bx
                && ac.len() == bc.len()
                && asz.len() == bsz.len()
                && asz.iter().zip(bsz).all(|(x, y)| (x - y).abs() < 1e-4)
                && ac.iter().zip(bc).all(|(x, y)| same_tree(x, y))
        }
        _ => false,
    }
}

fn same_tabs(a: &[SavedTab], b: &[SavedTab]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y))
}

impl Layout {
    pub fn new(tabs: Vec<SavedTab>, detached: Vec<SavedTab>) -> Self {
        Self { version: VERSION, tabs, detached }
    }

    /// The layout in `json` with only the sessions in `alive` (an empty or
    /// unreadable one is empty).
    pub fn parse(json: &str, alive: &HashSet<SessionId>) -> Self {
        if json.trim().is_empty() {
            return Self::new(Vec::new(), Vec::new());
        }
        let mut layout = match serde_json::from_str::<Layout>(json) {
            Ok(l) if l.version == VERSION => l,
            Ok(l) => {
                log::info!("workspace layout has format {} (this goatty reads {VERSION}); starting it over", l.version);
                return Self::new(Vec::new(), Vec::new());
            }
            Err(e) => {
                log::warn!("workspace layout is unreadable ({e}); starting it over");
                return Self::new(Vec::new(), Vec::new());
            }
        };
        layout.prune(alive);
        layout
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Keep what can be shown: sessions in `alive`, each once (tabs before
    /// detached ones), tabs that still have one, names that aren't blank.
    pub fn prune(&mut self, alive: &HashSet<SessionId>) {
        let mut seen = HashSet::new();
        let mut keep = |tabs: &mut Vec<SavedTab>| {
            tabs.retain_mut(|t| {
                let leaves = t.tree.leaves();
                let unique = leaves.iter().collect::<HashSet<_>>().len() == leaves.len();
                if !leaves.is_empty() && unique && leaves.iter().all(|s| alive.contains(s) && !seen.contains(s)) {
                    seen.extend(leaves);
                } else {
                    match t.tree.map_leaves(&mut |s| (alive.contains(&s) && seen.insert(s)).then_some(s)) {
                        Some(tree) => t.tree = tree,
                        None => return false,
                    }
                }
                if t.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                    t.name = None;
                }
                if t.plain.as_deref().is_some_and(|h| h.trim().is_empty()) {
                    t.plain = None;
                }
                true
            });
        };
        keep(&mut self.tabs);
        keep(&mut self.detached);
    }

    /// Every session it shows, detached tabs' too.
    pub fn sessions(&self) -> Vec<SessionId> {
        self.tabs.iter().chain(&self.detached).flat_map(|t| t.tree.leaves()).collect()
    }

    /// The same layout, ignoring float noise in split sizes.
    pub fn same(&self, other: &Layout) -> bool {
        same_tabs(&self.tabs, &other.tabs) && same_tabs(&self.detached, &other.detached)
    }

    /// The detached tabs detached last (together), oldest first.
    pub fn last_detached(&self) -> Vec<u64> {
        let Some(at) = self.detached.iter().filter_map(|t| t.detached_at).max() else {
            return self.detached.last().map(|t| t.id).into_iter().collect();
        };
        self.detached.iter().filter(|t| t.detached_at == Some(at)).map(|t| t.id).collect()
    }

    /// Bring detached tabs `ids` back, after the others.
    pub fn undetach(&mut self, ids: &[u64]) {
        let (back, stay): (Vec<SavedTab>, Vec<SavedTab>) =
            std::mem::take(&mut self.detached).into_iter().partition(|t| ids.contains(&t.id));
        self.detached = stay;
        self.tabs.extend(back.into_iter().map(|t| SavedTab { detached_at: None, ..t }));
    }
}

/// Merge two edits of `base`: `mine` (this window's) and `theirs` (what
/// the daemon has now). Per tab id: one side's change wins over the other
/// side leaving it alone; a tab removed on either side is gone; tabs added
/// on either side are kept, next to the tab before them; when both sides
/// changed a tab's splits, mine wins and the panes theirs added or removed
/// are added or removed in it too. Each session ends up in one tab.
pub fn merge(base: &Layout, mine: &Layout, theirs: &Layout) -> Layout {
    // A tab's fields merge by id across both lists: one side detaching a
    // tab the other renamed keeps the new name.
    let all = |l: &Layout| -> Vec<SavedTab> { l.tabs.iter().chain(&l.detached).cloned().collect() };
    let every = (all(base), all(mine), all(theirs));
    let tabs = merge_tabs(&base.tabs, &mine.tabs, &theirs.tabs, &every);
    let detached = merge_tabs(&base.detached, &mine.detached, &theirs.detached, &every);
    // A tab moved between the two lists by one side is in both now.
    let moved: HashSet<u64> = tabs.iter().map(|t| t.id).collect();
    let mut out = Layout::new(tabs, detached.into_iter().filter(|t| !moved.contains(&t.id)).collect());
    let alive: HashSet<SessionId> = out.sessions().into_iter().collect();
    out.prune(&alive);
    out
}

/// Which tabs one list (tabs, or detached tabs) keeps and in what order;
/// their fields come from `every` side's tabs in both lists.
fn merge_tabs(
    base: &[SavedTab],
    mine: &[SavedTab],
    theirs: &[SavedTab],
    every: &(Vec<SavedTab>, Vec<SavedTab>, Vec<SavedTab>),
) -> Vec<SavedTab> {
    let find = |list: &[SavedTab], id: u64| list.iter().position(|t| t.id == id);
    let common = |a: &[SavedTab], b: &[SavedTab]| -> Vec<u64> { a.iter().filter(|t| find(b, t.id).is_some()).map(|t| t.id).collect() };
    // Mine reordered the tabs both know: mine's order, else theirs.
    let reordered = common(mine, base) != common(base, mine);
    let (primary, secondary) = if reordered { (mine, theirs) } else { (theirs, mine) };
    let (eb, em, et) = every;
    let keep = |id: u64| {
        // Close wins across both lists: a tab in `base` removed from the
        // whole workspace by either side isn't restored via the other list.
        if find(eb, id).is_some() && (find(em, id).is_none() || find(et, id).is_none()) {
            return false;
        }
        let (b, m, t) = (find(base, id).is_some(), find(mine, id).is_some(), find(theirs, id).is_some());
        if b { m && t } else { m || t }
    };
    let mut out: Vec<u64> = primary.iter().map(|t| t.id).filter(|&id| keep(id)).collect();
    for (i, t) in secondary.iter().enumerate() {
        if out.contains(&t.id) || !keep(t.id) {
            continue;
        }
        // After the nearest tab before it that's in the result.
        let at = secondary[..i].iter().rev().find_map(|p| out.iter().position(|&o| o == p.id)).map_or(0, |p| p + 1);
        out.insert(at, t.id);
    }
    out.into_iter()
        .filter_map(|id| {
            let (b, m, t) = (find(eb, id).map(|i| &eb[i]), find(em, id).map(|i| &em[i]), find(et, id).map(|i| &et[i]));
            match (b, m, t) {
                (Some(b), Some(m), Some(t)) => merge_tab(b, m, t),
                (Some(_), None, _) | (Some(_), _, None) => None,
                (None, Some(m), _) => Some(m.clone()),
                (None, None, Some(t)) => Some(t.clone()),
                (None, None, None) => None,
            }
        })
        .collect()
}

/// `None`: every pane left the tab (each side moved some elsewhere).
fn merge_tab(base: &SavedTab, mine: &SavedTab, theirs: &SavedTab) -> Option<SavedTab> {
    let pick = |m: &Option<String>, b: &Option<String>, t: &Option<String>| if m != b { m.clone() } else { t.clone() };
    let tree = if same_tree(&mine.tree, &base.tree) {
        theirs.tree.clone()
    } else if same_tree(&theirs.tree, &base.tree) {
        mine.tree.clone()
    } else {
        // Both changed the splits: mine, with theirs' new and closed panes.
        // New ones first: removing a pane never empties the tree then.
        let (b, t) = (base.tree.leaves(), theirs.tree.leaves());
        let mut tree = mine.tree.clone();
        for new in t.iter().filter(|s| !b.contains(s)) {
            if !tree.contains(*new) {
                let last = *tree.leaves().last().expect("a tree has a leaf");
                tree.split(last, *new, Direction::Right);
            }
        }
        for gone in b.iter().filter(|s| !t.contains(s)) {
            if tree.leaves() == [*gone] {
                return None;
            }
            tree.remove(*gone);
        }
        tree
    };
    Some(SavedTab {
        id: mine.id,
        name: pick(&mine.name, &base.name, &theirs.name),
        tree,
        plain: pick(&mine.plain, &base.plain, &theirs.plain),
        detached_at: if mine.detached_at != base.detached_at { mine.detached_at } else { theirs.detached_at },
    })
}

/// The window's size (the restore size while maximized or fullscreen) in
/// logical pixels, and its state. Not its position: Wayland doesn't let
/// windows place themselves, so a restored window opens centered.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub state: WindowState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

impl SavedWindow {
    /// A new window's size.
    pub const DEFAULT: (f32, f32) = (960.0, 600.0);
    /// Smaller than this is a broken value, not a window anyone wants back.
    const MIN: (f32, f32) = (200.0, 100.0);
    /// Larger than any display; caps a corrupt value.
    const MAX: f32 = 16384.0;

    /// The size and state in `bounds`, as the platform reports them.
    pub fn from_bounds(bounds: WindowBounds) -> Self {
        let (b, state) = match bounds {
            WindowBounds::Windowed(b) => (b, WindowState::Windowed),
            WindowBounds::Maximized(b) => (b, WindowState::Maximized),
            WindowBounds::Fullscreen(b) => (b, WindowState::Fullscreen),
        };
        Self { width: f32::from(b.size.width), height: f32::from(b.size.height), state }
    }

    /// What `window` shows now, given what was captured before (`last`).
    pub fn capture(window: &gpui::Window, last: SavedWindow) -> Self {
        Self::merge(window.window_bounds(), window.is_fullscreen(), last)
    }

    /// X11 reports a maximized window's current size (not its restore
    /// size) and never reports fullscreen in its bounds. So fullscreen
    /// comes from `fullscreen`, and a window that isn't windowed keeps the
    /// last windowed size (Wayland's restore bounds are that size too).
    fn merge(bounds: WindowBounds, fullscreen: bool, last: SavedWindow) -> Self {
        let mut now = Self::from_bounds(bounds);
        if fullscreen {
            now.state = WindowState::Fullscreen;
        }
        if now.state != WindowState::Windowed {
            (now.width, now.height) = (last.width, last.height);
        }
        now
    }

    /// The size to use, clamped to something sane (a bad value falls back
    /// to the default size).
    pub fn size(&self) -> (f32, f32) {
        let default = Self::DEFAULT;
        let clamp = |v: f32, min: f32, d: f32| if v.is_finite() { v.clamp(min, Self::MAX) } else { d };
        (clamp(self.width, Self::MIN.0, default.0), clamp(self.height, Self::MIN.1, default.1))
    }

    /// Bring an open window to this size and state.
    pub fn apply(&self, window: &mut gpui::Window) {
        let (w, h) = self.size();
        window.resize(size(px(w), px(h)));
        match self.state {
            WindowState::Windowed => {}
            WindowState::Maximized if !window.is_maximized() => window.zoom_window(),
            WindowState::Fullscreen if !window.is_fullscreen() => window.toggle_fullscreen(),
            _ => {}
        }
    }
}

/// How a window joins workspaces: its view id and where it hears about them.
#[derive(Clone)]
pub struct Join {
    pub view: ViewId,
    pub sink: Arc<dyn WorkspaceSink>,
}

/// A workspace a window just joined, with its sessions attached.
pub struct Joined {
    pub info: WorkspaceInfo,
    /// Every workspace on the host (listed after joining), and the list's version.
    pub seq: u64,
    pub list: Vec<WorkspaceInfo>,
    pub revision: u64,
    /// The layout as the daemon has it (with running sessions only).
    pub base: Layout,
    /// What the window shows of it (`base` with detached tabs brought back
    /// for [`AttachMode::Undetach`], without sessions that ended meanwhile).
    pub show: Layout,
    pub sessions: HashMap<SessionId, Arc<Session>>,
    /// Sessions to show as new tabs (a fresh shell, `--attach`, the
    /// detached sessions no workspace shows); `Some(host)`: a plain SSH tab.
    pub extra: Vec<(Arc<Session>, Option<String>)>,
}

impl Joined {
    /// Nothing to show (a reopened window's workspace whose shells all ended).
    pub fn is_empty(&self) -> bool {
        self.show.tabs.is_empty() && self.extra.is_empty()
    }
}

/// How a window starts.
pub enum Startup {
    /// One tab with this session (`--local`; a plain SSH tab in a window
    /// that shows the host's workspace already).
    Session(Arc<Session>),
    /// A workspace on the host.
    Joined(Box<Joined>),
}

/// A tree of one pane showing session `s`.
#[cfg(test)]
pub fn leaf(s: SessionId) -> Node {
    Node::Leaf(s)
}

/// A fresh id for a tab: random, so windows on other machines showing the
/// same workspace don't collide.
pub fn new_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded randomly per process.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    h.finish().max(1)
}

/// Unix time in milliseconds (when tabs were detached).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Sessions attached for a window, let go again unless they're handed over
/// (`take`): a startup that fails halfway leaves nothing subscribed.
pub(super) struct Held(pub Vec<Arc<Session>>);

impl Held {
    fn keep(&mut self, s: &Arc<Session>) {
        self.0.push(s.clone());
    }

    /// Hand them over: nothing is let go.
    pub fn take(&mut self) {
        self.0.clear();
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for s in self.0.drain(..) {
            s.release_view();
        }
    }
}

/// Join a workspace on `conn` by `mode` as window `join` and attach its
/// sessions; `plain`: a session made for a plain SSH tab, shown in it too.
/// When something fails after joining, the window leaves the workspace
/// again and lets go of what it attached. Blocks.
pub fn startup(
    conn: &Arc<Connection>,
    size: TermSize,
    mode: AttachMode,
    join: &Join,
    plain: Option<(Arc<Session>, String)>,
) -> Result<Startup> {
    let mut held = Held(plain.iter().map(|(s, _)| s.clone()).collect());
    let target = match mode {
        AttachMode::Join(id) => id,
        AttachMode::Restore(id) | AttachMode::Undetach(id) => Some(id),
        AttachMode::Id(session) => conn.list_workspaces()?.iter().find(|w| w.sessions.contains(&session)).map(|w| w.id),
        AttachMode::New | AttachMode::AllDetached => None,
    };
    let (info, wl) = conn.join_workspace(join.view, target, join.sink.clone())?;
    match joined(conn, size, mode, info, wl, plain, &mut held) {
        Ok(j) => {
            held.take();
            Ok(Startup::Joined(Box::new(j)))
        }
        Err(e) => {
            conn.leave_workspace(join.view);
            Err(e)
        }
    }
}

/// [`startup`] once joined: attach what the window shows.
fn joined(
    conn: &Arc<Connection>,
    size: TermSize,
    mode: AttachMode,
    info: WorkspaceInfo,
    wl: goatty_proto::WorkspaceLayout,
    plain: Option<(Arc<Session>, String)>,
    held: &mut Held,
) -> Result<Joined> {
    // Listed after joining: it counts this window.
    let (seq, list) = conn.list_workspaces_seq()?;
    let alive: HashSet<SessionId> = wl.sessions.iter().copied().collect();
    let base = Layout::parse(&wl.json, &alive);
    let mut show = base.clone();
    if let AttachMode::Undetach(_) = mode {
        let all: Vec<u64> = show.detached.iter().map(|t| t.id).collect();
        show.undetach(&all);
    }
    if let AttachMode::Id(session) = mode
        && let Some(t) = show.detached.iter().find(|t| t.tree.contains(session))
    {
        let id = t.id;
        show.undetach(&[id]);
    }
    let ids: Vec<SessionId> = show.tabs.iter().flat_map(|t| t.tree.leaves()).collect();
    let sessions = attach_all(conn, size, &ids)?;
    sessions.values().for_each(|s| held.keep(s));
    let attached: HashSet<SessionId> = sessions.keys().copied().collect();
    let detached: HashSet<SessionId> = show.detached.iter().flat_map(|t| t.tree.leaves()).collect();
    let mut kept = show.clone();
    kept.prune(&attached.union(&detached).copied().collect());
    show.tabs = kept.tabs;
    let mut extra = Vec::new();
    match mode {
        AttachMode::Id(session) if !sessions.contains_key(&session) => {
            let s = Session::daemon(conn.clone(), size, Some(session), None)?;
            held.keep(&s);
            extra.push((s, None));
        }
        AttachMode::AllDetached => {
            let shown: HashSet<SessionId> = list.iter().flat_map(|w| w.sessions.iter().copied()).chain(base.sessions()).collect();
            let loose = loose_detached(&conn.list_sessions()?, &shown);
            for (_, s) in attach_all(conn, size, &loose)?.into_iter().collect::<std::collections::BTreeMap<_, _>>() {
                held.keep(&s);
                extra.push((s, None));
            }
        }
        _ => {}
    }
    if let Some((session, host)) = plain {
        extra.push((session, Some(host)));
    }
    let fresh = matches!(mode, AttachMode::New) || (matches!(mode, AttachMode::Join(_)) && show.tabs.is_empty() && extra.is_empty());
    if fresh {
        let s = Session::daemon(conn.clone(), size, None, None)?;
        held.keep(&s);
        extra.push((s, None));
    }
    log::info!("joined workspace {:?} on {} ({} tabs)", info.name, conn.endpoint().label(), show.tabs.len());
    Ok(Joined { revision: wl.revision, info, seq, list, base, show, sessions, extra })
}

/// Attach `ids` (sessions another window of this goatty shows are shared),
/// skipping sessions that ended meanwhile. When the link drops or times out
/// midway, the ones that did attach are let go again and the error returned:
/// no truncated window that overwrites the workspace.
pub fn attach_all(conn: &Arc<Connection>, size: TermSize, ids: &[SessionId]) -> Result<HashMap<SessionId, Arc<Session>>> {
    let mut sessions = HashMap::with_capacity(ids.len());
    let mut link_err = None;
    for (id, res) in Session::daemon_attach_many(conn, size, ids) {
        match res {
            Ok(s) => {
                sessions.insert(id, s);
            }
            Err(e) if e.downcast_ref::<ReplyLost>().is_some() || !conn.is_connected() => {
                if link_err.is_none() {
                    link_err = Some(e);
                }
            }
            // Ended since the layout was read.
            Err(e) => log::info!("not showing session {id}: {e:#}"),
        }
    }
    if let Some(err) = link_err {
        for s in sessions.values() {
            s.release_view();
        }
        return Err(err);
    }
    Ok(sessions)
}

/// The detached sessions on a host that no workspace shows (`shown`),
/// oldest first.
pub fn loose_detached(sessions: &[goatty_proto::SessionInfo], shown: &HashSet<SessionId>) -> Vec<SessionId> {
    let mut ids: Vec<SessionId> = sessions.iter().filter(|s| s.attached == 0 && !shown.contains(&s.id)).map(|s| s.id).collect();
    ids.sort_unstable();
    ids
}

/// What a list of tabs holds, for lists: (tabs, panes, the tabs' names:
/// their own or their focused session's title).
pub fn summary(tabs: &[SavedTab], title: impl Fn(SessionId) -> Option<String>) -> (usize, usize, Vec<String>) {
    let panes = tabs.iter().map(|t| t.tree.leaves().len()).sum();
    let names = tabs
        .iter()
        .map(|t| {
            t.name
                .clone()
                .or_else(|| t.tree.leaves().first().and_then(|&s| title(s)))
                .or_else(|| t.plain.as_ref().map(|h| format!("ssh {h}")))
                .unwrap_or_else(|| "shell".into())
        })
        .collect();
    (tabs.len(), panes, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u64, tree: Node) -> SavedTab {
        SavedTab { id, name: None, tree, plain: None, detached_at: None }
    }

    fn split(a: SessionId, b: SessionId) -> Node {
        let mut t = Node::Leaf(a);
        t.split(a, b, Direction::Right);
        t
    }

    fn ids(l: &Layout) -> Vec<u64> {
        l.tabs.iter().map(|t| t.id).collect()
    }

    #[test]
    fn json_format() {
        let layout = Layout::new(
            vec![
                SavedTab { name: Some("build".into()), ..tab(1, split(3, 7)) },
                SavedTab { plain: Some("pi".into()), ..tab(2, Node::Leaf(9)) },
            ],
            vec![SavedTab { detached_at: Some(5), ..tab(3, Node::Leaf(4)) }],
        );
        let json = layout.to_json();
        assert!(json.contains(r#""version":2"#) && json.contains(r#""plain":"pi""#) && json.matches("plain").count() == 1, "{json}");
        assert!(json.contains(r#""detached":[{"id":3,"tree":{"leaf":4},"detached_at":5}]"#), "{json}");
        assert_eq!(Layout::parse(&json, &[3, 7, 9, 4].into()), layout);
        // Unset fields are optional; old, empty and broken layouts read as empty.
        let short = Layout::parse(r#"{"version":2,"tabs":[{"id":5,"tree":{"leaf":4}}]}"#, &[4].into());
        assert_eq!(short.tabs, [tab(5, Node::Leaf(4))]);
        let v1 = r#"{"version":1,"tabs":[{"tree":{"leaf":4},"focused":4}],"active":0}"#;
        for json in ["", "nope", v1] {
            assert_eq!(Layout::parse(json, &[4].into()), Layout::new(Vec::new(), Vec::new()));
        }
    }

    #[test]
    fn pruning_drops_ended_sessions_and_shows_each_once() {
        let mut l = Layout::new(
            vec![SavedTab { name: Some(" ".into()), ..tab(1, split(1, 2)) }, tab(2, Node::Leaf(3)), tab(3, split(2, 4))],
            vec![tab(4, Node::Leaf(4)), tab(5, Node::Leaf(5))],
        );
        l.prune(&[1, 2, 4, 5].into());
        assert_eq!(ids(&l), [1, 3]);
        assert_eq!((l.tabs[0].tree.clone(), l.tabs[0].name.clone()), (split(1, 2), None));
        assert_eq!(l.tabs[1].tree, Node::Leaf(4), "2 shows in the first tab already");
        assert_eq!(l.detached.iter().map(|t| t.id).collect::<Vec<_>>(), [5]);
        assert_eq!(l.sessions(), [1, 2, 4, 5]);
    }

    #[test]
    fn detached_tabs_come_back_together() {
        let mut l = Layout::new(
            vec![tab(1, Node::Leaf(1))],
            vec![
                SavedTab { detached_at: Some(10), ..tab(2, Node::Leaf(2)) },
                SavedTab { detached_at: Some(20), ..tab(3, Node::Leaf(3)) },
                SavedTab { detached_at: Some(20), ..tab(4, Node::Leaf(4)) },
            ],
        );
        assert_eq!(l.last_detached(), [3, 4]);
        l.undetach(&[3, 4]);
        assert_eq!(ids(&l), [1, 3, 4]);
        assert_eq!(l.tabs[1].detached_at, None);
        assert_eq!(l.last_detached(), [2]);
    }

    #[test]
    fn edits_from_two_windows_are_merged_tab_by_tab() {
        let base = Layout::new(vec![tab(1, Node::Leaf(1)), tab(2, Node::Leaf(2)), tab(3, Node::Leaf(3))], Vec::new());
        // Mine: renamed tab 1, split tab 2, opened tab 4 after tab 1.
        let mut mine = base.clone();
        mine.tabs[0].name = Some("mine".into());
        mine.tabs[1].tree = split(2, 20);
        mine.tabs.insert(1, tab(4, Node::Leaf(4)));
        // Theirs: closed tab 3, renamed tab 2, opened tab 5 at the end.
        let mut theirs = base.clone();
        theirs.tabs.remove(2);
        theirs.tabs[1].name = Some("theirs".into());
        theirs.tabs.push(tab(5, Node::Leaf(5)));
        let merged = merge(&base, &mine, &theirs);
        assert_eq!(ids(&merged), [1, 4, 2, 5]);
        assert_eq!(merged.tabs[0].name.as_deref(), Some("mine"));
        assert_eq!((merged.tabs[2].name.as_deref(), merged.tabs[2].tree.clone()), (Some("theirs"), split(2, 20)));
        // Merging is the same from the other side, but for order of additions.
        let other = merge(&base, &theirs, &mine);
        assert_eq!(other.tabs.iter().map(|t| t.id).collect::<HashSet<_>>(), merged.tabs.iter().map(|t| t.id).collect());
        // Nothing changed on one side: the other side's layout.
        assert!(merge(&base, &base, &theirs).same(&theirs));
        assert!(merge(&base, &mine, &base).same(&mine));
    }

    #[test]
    fn closing_wins_and_both_sides_changing_splits_keeps_both_panes() {
        let base = Layout::new(vec![tab(1, split(1, 2)), tab(2, Node::Leaf(3))], Vec::new());
        // Mine splits tab 1 again; theirs closes pane 2 in it and adds pane 7 there.
        let mut mine = base.clone();
        mine.tabs[0].tree.split(1, 5, Direction::Down);
        let mut theirs = base.clone();
        theirs.tabs[0].tree = split(1, 7);
        // Theirs also renamed tab 2, which mine closed.
        theirs.tabs[1].name = Some("x".into());
        mine.tabs.remove(1);
        let merged = merge(&base, &mine, &theirs);
        assert_eq!(ids(&merged), [1]);
        let mut leaves = merged.tabs[0].tree.leaves();
        leaves.sort_unstable();
        assert_eq!(leaves, [1, 5, 7]);
    }

    #[test]
    fn a_pane_one_side_closed_and_the_other_moved_away_goes() {
        // Mine closed pane 2; theirs detached pane 1 (a tab of its own,
        // detached) and split in pane 3.
        let base = Layout::new(vec![tab(1, split(1, 2))], Vec::new());
        let mut mine = base.clone();
        mine.tabs[0].tree = Node::Leaf(1);
        let mut theirs = base.clone();
        theirs.tabs[0].tree = split(2, 3);
        theirs.detached.push(SavedTab { detached_at: Some(1), ..tab(9, Node::Leaf(1)) });
        let merged = merge(&base, &mine, &theirs);
        assert_eq!(merged.tabs[0].tree, Node::Leaf(3));
        assert_eq!(merged.detached.iter().map(|t| t.tree.clone()).collect::<Vec<_>>(), [Node::Leaf(1)], "pane 1 stays detached");
    }

    #[test]
    fn a_tab_detached_on_one_side_keeps_the_other_sides_rename() {
        let base = Layout::new(vec![tab(1, Node::Leaf(1)), tab(2, Node::Leaf(2))], Vec::new());
        let mut renamed = base.clone();
        renamed.tabs[0].name = Some("renamed".into());
        let mut detached = base.clone();
        let t = detached.tabs.remove(0);
        detached.detached.push(SavedTab { detached_at: Some(5), ..t });
        // Both orders: whoever saved first.
        for merged in [merge(&base, &renamed, &detached), merge(&base, &detached, &renamed)] {
            assert_eq!(ids(&merged), [2]);
            assert_eq!(merged.detached.len(), 1);
            assert_eq!((merged.detached[0].name.as_deref(), merged.detached[0].detached_at), (Some("renamed"), Some(5)));
        }
    }

    #[test]
    fn both_panes_moving_out_of_a_tab_keeps_both_moves() {
        // Tab 1 holds panes 1 and 2; mine moves 2 to a new tab 2, theirs
        // moves 1 to a new tab 3.
        let base = Layout::new(vec![tab(1, split(1, 2))], Vec::new());
        let mut mine = base.clone();
        mine.tabs[0].tree = Node::Leaf(1);
        mine.tabs.push(tab(2, Node::Leaf(2)));
        let mut theirs = base.clone();
        theirs.tabs[0].tree = Node::Leaf(2);
        theirs.tabs.push(tab(3, Node::Leaf(1)));
        let merged = merge(&base, &mine, &theirs);
        let mut got: Vec<(u64, Vec<SessionId>)> = merged.tabs.iter().map(|t| (t.id, t.tree.leaves())).collect();
        got.sort();
        assert_eq!(got, [(2, vec![2]), (3, vec![1])]);
    }

    #[test]
    fn a_session_twice_in_one_split_shows_once() {
        let mut l = Layout::new(vec![tab(1, split(4, 4))], Vec::new());
        l.prune(&[4].into());
        assert_eq!(l.tabs[0].tree, Node::Leaf(4));
    }

    #[test]
    fn pruning_drops_empty_split_trees() {
        let empty = Node::Split { axis: super::super::layout::Axis::Horizontal, children: Vec::new(), sizes: Vec::new() };
        let mut l = Layout::new(vec![tab(1, empty.clone()), tab(2, Node::Leaf(1))], vec![tab(3, empty)]);
        l.prune(&[1].into());
        assert_eq!(ids(&l), [2]);
        assert!(l.detached.is_empty());
    }

    #[test]
    fn closing_a_tab_wins_over_concurrent_detach() {
        let base = Layout::new(vec![tab(1, Node::Leaf(1)), tab(2, Node::Leaf(2))], Vec::new());
        let mut closed = base.clone();
        closed.tabs.remove(0);
        let mut detached = base.clone();
        let t = detached.tabs.remove(0);
        detached.detached.push(SavedTab { detached_at: Some(5), ..t });
        for merged in [merge(&base, &closed, &detached), merge(&base, &detached, &closed)] {
            assert_eq!(ids(&merged), [2]);
            assert!(merged.detached.is_empty(), "a closed tab must not return as detached");
        }
    }

    #[test]
    fn reordering_and_detaching_merge_too() {
        let base = Layout::new(vec![tab(1, Node::Leaf(1)), tab(2, Node::Leaf(2)), tab(3, Node::Leaf(3))], Vec::new());
        // Mine moved tab 3 first; theirs detached tab 2 and opened tab 4 after 1.
        let mut mine = base.clone();
        let t = mine.tabs.remove(2);
        mine.tabs.insert(0, t);
        let mut theirs = base.clone();
        let t = theirs.tabs.remove(1);
        theirs.detached.push(SavedTab { detached_at: Some(9), ..t });
        theirs.tabs.insert(1, tab(4, Node::Leaf(4)));
        let merged = merge(&base, &mine, &theirs);
        assert_eq!(ids(&merged), [3, 1, 4]);
        assert_eq!(merged.detached.iter().map(|t| t.id).collect::<Vec<_>>(), [2]);
        // A session never shows twice: mine added pane 4 to tab 1 as well.
        let mut mine = base.clone();
        mine.tabs[0].tree = split(1, 4);
        let merged = merge(&base, &mine, &theirs);
        assert_eq!(merged.sessions().iter().filter(|&&s| s == 4).count(), 1);
    }

    #[test]
    fn float_noise_is_not_a_change() {
        let mut a = split(1, 2);
        let b = a.clone();
        if let Node::Split { sizes, .. } = &mut a {
            sizes[0] += 1e-6;
            sizes[1] -= 1e-6;
        }
        assert!(same_tree(&a, &b));
        let l = |t| Layout::new(vec![tab(1, t)], Vec::new());
        assert!(l(a).same(&l(b)));
    }

    #[test]
    fn summaries_and_loose_sessions() {
        let tabs = [
            SavedTab { name: Some("logs".into()), ..tab(1, split(1, 2)) },
            tab(2, Node::Leaf(4)),
            SavedTab { plain: Some("pi".into()), ..tab(3, Node::Leaf(5)) },
        ];
        let (n, panes, names) = summary(&tabs, |id| (id == 4).then(|| "htop".to_string()));
        assert_eq!((n, panes, names), (3, 4, vec!["logs".to_string(), "htop".to_string(), "ssh pi".to_string()]));
        let s = |id, attached| goatty_proto::SessionInfo {
            id,
            pid: None,
            title: String::new(),
            cwd: None,
            cols: 80,
            rows: 24,
            attached,
            created_unix: 0,
            process: None,
        };
        assert_eq!(loose_detached(&[s(5, 0), s(2, 0), s(3, 1), s(1, 0)], &[2].into()), [1, 5]);
    }

    #[test]
    fn window_size_is_clamped() {
        let d = SavedWindow::DEFAULT;
        let w = |width, height| SavedWindow { width, height, state: WindowState::Windowed };
        assert_eq!(w(1200.0, 700.0).size(), (1200.0, 700.0));
        assert_eq!(w(10.0, 1e9).size(), (200.0, 16384.0));
        assert_eq!(w(f32::NAN, f32::INFINITY).size(), d);
    }

    #[test]
    fn maximized_and_fullscreen_keep_the_windowed_size() {
        let b = |w: f32, h: f32| gpui::Bounds::new(gpui::point(px(0.), px(0.)), size(px(w), px(h)));
        let windowed = SavedWindow { width: 1000.0, height: 700.0, state: WindowState::Windowed };
        assert_eq!(SavedWindow::merge(WindowBounds::Windowed(b(1100.0, 750.0)), false, windowed).width, 1100.0);
        // X11: the maximized size, not the restore size.
        let max = SavedWindow::merge(WindowBounds::Maximized(b(2560.0, 1400.0)), false, windowed);
        assert_eq!(max, SavedWindow { state: WindowState::Maximized, ..windowed });
        // X11: fullscreen only from `is_fullscreen`.
        let full = SavedWindow::merge(WindowBounds::Windowed(b(2560.0, 1440.0)), true, max);
        assert_eq!(full, SavedWindow { state: WindowState::Fullscreen, ..windowed });
    }
}
