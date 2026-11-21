//! A window's layout as goattyd keeps it (#118): tabs, their split trees
//! with session ids as leaves, names, focus and zoom, and the window's
//! size and state (#128). The workspace saves
//! it on every change (`Connection::save_layout`); starting goatty again
//! claims the newest layout no window holds and reattaches its sessions
//! ([`restore`]). A window reopened from `reopen` claims its own layout on
//! each host by id ([`Claim::Id`], #252).
//!
//! The JSON is ours alone; the daemon stores it as is. Bump [`VERSION`]
//! when the format changes incompatibly (an unreadable layout is ignored,
//! its sessions stay detached).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use goatty_client::{Connection, ReplyLost};
use goatty_proto::{LayoutId, SessionId};
use goatty_term::TermSize;
use gpui::{WindowBounds, px, size};
use serde::{Deserialize, Serialize};

use super::layout::Node;
use crate::session::{AttachMode, Session};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub version: u32,
    pub tabs: Vec<SavedTab>,
    /// Index of the active tab.
    pub active: usize,
    /// Missing in layouts saved before #128.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<SavedWindow>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    /// Set by renaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Leaves are session ids.
    pub tree: Node,
    pub focused: SessionId,
    #[serde(default)]
    pub zoomed: bool,
    /// A plain SSH tab (`ssh HOST` in a session of this goattyd): restored
    /// as one, in that host's group. Missing in layouts saved before #252.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain: Option<String>,
}

/// A restored tab: its tree has session ids as leaves, all attached.
pub struct RestoredTab {
    pub name: Option<String>,
    pub tree: Node,
    pub focused: SessionId,
    pub zoomed: bool,
    pub plain: Option<String>,
}

/// A window brought back from a saved layout.
pub struct Restored {
    pub id: LayoutId,
    /// `id` is a layout the daemon kept (claimed now), not one made up for
    /// tabs that weren't in one ([`AttachMode::AllDetached`]).
    pub claimed: bool,
    pub tabs: Vec<RestoredTab>,
    pub active: usize,
    pub sessions: HashMap<SessionId, Arc<Session>>,
    pub window: Option<SavedWindow>,
}

/// How a window starts.
pub enum Startup {
    /// One tab with this session.
    Session(Arc<Session>),
    Restore(Restored),
    /// Nothing left to restore ([`AttachMode::Layout`] whose sessions all
    /// ended, [`AttachMode::AllDetached`] without any).
    Nothing,
}

/// Which saved layout to restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The newest one no window holds.
    Newest,
    /// This one (the newest free one from a goattyd older than v16).
    Id(LayoutId),
}

/// A fresh layout id: random, so windows on other machines attached to the
/// same goattyd don't collide.
pub fn new_id() -> LayoutId {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded randomly per process.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    h.finish()
}

/// Keep what can be shown: tabs whose trees still have a session in
/// `alive` (each session once), focus on a pane that exists, the active
/// index in range. `None` when no tab is left.
pub fn prune(saved: SavedWorkspace, alive: &HashSet<SessionId>) -> Option<(Vec<RestoredTab>, usize)> {
    if saved.version != VERSION {
        log::info!("saved layout has format {} (this goatty reads {VERSION}); not restoring it", saved.version);
        return None;
    }
    let mut seen = HashSet::new();
    let mut tabs = Vec::new();
    let mut active = 0;
    for (i, tab) in saved.tabs.into_iter().enumerate() {
        let Some(tree) = tab.tree.map_leaves(&mut |id| (alive.contains(&id) && seen.insert(id)).then_some(id)) else { continue };
        // The active tab, else the one before it.
        if i <= saved.active {
            active = tabs.len();
        }
        let leaves = tree.leaves();
        let focused = if leaves.contains(&tab.focused) { tab.focused } else { leaves[0] };
        let zoomed = tab.zoomed && leaves.len() > 1;
        let plain = tab.plain.filter(|h| !h.trim().is_empty());
        tabs.push(RestoredTab { name: tab.name.filter(|n| !n.trim().is_empty()), tree, focused, zoomed, plain });
    }
    if tabs.is_empty() {
        return None;
    }
    let active = active.min(tabs.len() - 1);
    Some((tabs, active))
}

/// What a window (or tab) attached to `conn` starts with, by `mode`: a
/// saved layout ([`AttachMode::Auto`]: the newest free one, else a single
/// session as before; [`AttachMode::Layout`]: that one, else nothing), the
/// detached sessions ([`AttachMode::AllDetached`]), or a single session.
/// Blocks.
pub fn startup(conn: &Arc<Connection>, size: TermSize, mode: AttachMode) -> Result<Startup> {
    let restored = match mode {
        AttachMode::Auto => link_errors(conn, restore(conn, size, Claim::Newest))?,
        AttachMode::Layout(id) => link_errors(conn, restore(conn, size, Claim::Id(id)))?,
        AttachMode::AllDetached => link_errors(conn, detached(conn, size))?,
        AttachMode::New | AttachMode::Id(_) => None,
    };
    Ok(match (restored, mode) {
        (Some(r), _) => Startup::Restore(r),
        (None, AttachMode::Layout(_) | AttachMode::AllDetached) => Startup::Nothing,
        (None, _) => Startup::Session(Session::daemon(conn.clone(), size, mode, None)?),
    })
}

/// A lost link is the caller's problem; anything else means "nothing to
/// restore" (logged).
fn link_errors(conn: &Connection, r: Result<Option<Restored>>) -> Result<Option<Restored>> {
    match r {
        Ok(r) => Ok(r),
        Err(e) if e.downcast_ref::<ReplyLost>().is_some() || !conn.is_connected() => Err(e),
        Err(e) => {
            log::warn!("restoring tabs: {e:#}");
            Ok(None)
        }
    }
}

/// Claim a saved layout on `conn` (`claim`) and reattach its sessions.
/// `None` when there is none, or none of its sessions could be attached.
/// Blocks.
pub fn restore(conn: &Arc<Connection>, size: TermSize, claim: Claim) -> Result<Option<Restored>> {
    let info = match claim {
        Claim::Newest => conn.claim_layout()?,
        Claim::Id(id) => conn.claim_layout_id(id)?,
    };
    let Some(info) = info else { return Ok(None) };
    let saved: SavedWorkspace = match serde_json::from_str(&info.json) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("saved layout {:x} is unreadable ({e}); not restoring it", info.id);
            return Ok(None);
        }
    };
    let window = saved.window;
    let alive: HashSet<SessionId> = info.sessions.iter().copied().collect();
    let Some((tabs, _)) = prune(saved.clone(), &alive) else { return Ok(None) };
    let ids: Vec<SessionId> = tabs.iter().flat_map(|t| t.tree.leaves()).collect();
    let sessions = match attach_all(conn, size, &ids) {
        Ok(s) => s,
        Err(e) => {
            // Return the layout claim so a retry on a live connection can
            // reclaim it.
            conn.release_claimed_layout(info);
            return Err(e);
        }
    };
    let attached: HashSet<SessionId> = sessions.keys().copied().collect();
    let Some((tabs, active)) = prune(saved, &attached) else { return Ok(None) };
    log::info!("restored {} tabs with {} sessions", tabs.len(), sessions.len());
    Ok(Some(Restored { id: info.id, claimed: true, tabs, active, sessions, window }))
}

/// Attach `ids`, skipping sessions that ended meanwhile and ones a pane of
/// this goatty shows already on `conn` (one connection has one pane per
/// session: attaching again would leave that pane without output). When
/// the link drops or times out midway, the ones that did attach are
/// detached again and the error returned: no truncated window that
/// overwrites the saved layout.
fn attach_all(conn: &Arc<Connection>, size: TermSize, ids: &[SessionId]) -> Result<HashMap<SessionId, Arc<Session>>> {
    let ids: Vec<SessionId> = ids.iter().copied().filter(|&id| !conn.shows(id)).collect();
    let ids = &ids[..];
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
            // Ended since the claim.
            Err(e) => log::info!("not restoring session {id}: {e:#}"),
        }
    }
    if let Some(err) = link_err {
        for &id in sessions.keys() {
            conn.detach(id);
        }
        return Err(err);
    }
    Ok(sessions)
}

/// Reattach `tabs` (just detached from this window: "Undo"). A layout
/// kept for them meanwhile (a detached host's) is forgotten: they're back.
pub fn reattach(conn: &Arc<Connection>, size: TermSize, tabs: Vec<SavedTab>) -> Result<Option<Restored>> {
    let ids: Vec<SessionId> = tabs.iter().flat_map(|t| t.tree.leaves()).collect();
    let sessions = attach_all(conn, size, &ids)?;
    match conn.list_layouts() {
        Ok(layouts) => {
            for l in layouts.iter().filter(|l| !l.sessions.is_empty() && l.sessions.iter().all(|s| sessions.contains_key(s))) {
                if matches!(conn.claim_layout_id(l.id), Ok(Some(_))) {
                    conn.save_layout(l.id, Vec::new(), String::new());
                }
            }
        }
        Err(e) => log::warn!("listing the saved layouts: {e:#}"),
    }
    let attached: HashSet<SessionId> = sessions.keys().copied().collect();
    let saved = SavedWorkspace { version: VERSION, active: tabs.len().saturating_sub(1), tabs, window: None };
    let Some((tabs, active)) = prune(saved, &attached) else { return Ok(None) };
    Ok(Some(Restored { id: new_id(), claimed: false, tabs, active, sessions, window: None }))
}

/// `layouts` with only their sessions still running (`sessions`: the
/// host's), and without the ones left empty: a layout this connection
/// detached isn't pruned by the daemon.
pub fn live_layouts(layouts: Vec<goatty_proto::LayoutInfo>, sessions: &[goatty_proto::SessionInfo]) -> Vec<goatty_proto::LayoutInfo> {
    let alive: HashSet<SessionId> = sessions.iter().map(|s| s.id).collect();
    layouts
        .into_iter()
        .filter_map(|mut l| {
            l.sessions.retain(|s| alive.contains(s));
            (!l.sessions.is_empty()).then_some(l)
        })
        .collect()
}

/// Does saved layout `info` hold a plain SSH tab to `host` that still runs?
pub fn has_plain_tab(info: &goatty_proto::LayoutInfo, host: &str) -> bool {
    let Ok(saved) = serde_json::from_str::<SavedWorkspace>(&info.json) else { return false };
    let alive: HashSet<SessionId> = info.sessions.iter().copied().collect();
    prune(saved, &alive).is_some_and(|(tabs, _)| tabs.iter().any(|t| t.plain.as_deref() == Some(host)))
}

/// The detached sessions on `conn` that no saved layout shows (those come
/// back with their layout), oldest first.
pub fn loose_detached(sessions: &[goatty_proto::SessionInfo], layouts: &[goatty_proto::LayoutInfo]) -> Vec<SessionId> {
    let in_layout: HashSet<SessionId> = layouts.iter().flat_map(|l| l.sessions.iter().copied()).collect();
    let mut ids: Vec<SessionId> = sessions.iter().filter(|s| s.attached == 0 && !in_layout.contains(&s.id)).map(|s| s.id).collect();
    ids.sort_unstable();
    ids
}

/// Every loose detached session on `conn` ([`loose_detached`]), a tab each.
fn detached(conn: &Arc<Connection>, size: TermSize) -> Result<Option<Restored>> {
    let layouts = conn.list_layouts()?;
    let ids = loose_detached(&conn.list_sessions()?, &layouts);
    let sessions = attach_all(conn, size, &ids)?;
    let tabs: Vec<RestoredTab> = ids
        .iter()
        .filter(|id| sessions.contains_key(id))
        .map(|&id| RestoredTab { name: None, tree: Node::Leaf(id), focused: id, zoomed: false, plain: None })
        .collect();
    if tabs.is_empty() {
        return Ok(None);
    }
    let active = tabs.len() - 1;
    log::info!("reattached {} detached sessions", tabs.len());
    Ok(Some(Restored { id: new_id(), claimed: false, tabs, active, sessions, window: None }))
}

/// What a saved layout holds, for a list to pick it from: (tabs, panes,
/// the tabs' names: their own or their focused session's title).
pub fn summary(info: &goatty_proto::LayoutInfo, title: impl Fn(SessionId) -> Option<String>) -> Option<(usize, usize, Vec<String>)> {
    let saved: SavedWorkspace = serde_json::from_str(&info.json).ok()?;
    let alive: HashSet<SessionId> = info.sessions.iter().copied().collect();
    let (tabs, _) = prune(saved, &alive)?;
    let panes = tabs.iter().map(|t| t.tree.leaves().len()).sum();
    let names = tabs
        .iter()
        .map(|t| {
            t.name
                .clone()
                .or_else(|| title(t.focused))
                .or_else(|| t.plain.as_ref().map(|h| format!("ssh {h}")))
                .unwrap_or_else(|| "shell".into())
        })
        .collect();
    Some((tabs.len(), panes, names))
}

#[cfg(test)]
mod tests {
    use super::super::layout::Direction;
    use super::*;

    fn tab(tree: Node, focused: SessionId) -> SavedTab {
        SavedTab { name: None, tree, focused, zoomed: false, plain: None }
    }

    fn split(a: SessionId, b: SessionId) -> Node {
        let mut t = Node::Leaf(a);
        t.split(a, b, Direction::Right);
        t
    }

    #[test]
    fn json_format() {
        let saved = SavedWorkspace {
            version: VERSION,
            tabs: vec![
                SavedTab { name: Some("build".into()), tree: split(3, 7), focused: 7, zoomed: true, plain: None },
                SavedTab { plain: Some("pi".into()), ..tab(Node::Leaf(9), 9) },
            ],
            active: 1,
            window: Some(SavedWindow { width: 1200.0, height: 800.5, state: WindowState::Maximized }),
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains(r#""window":{"width":1200.0,"height":800.5,"state":"maximized"}"#), "{json}");
        assert!(json.contains(r#""plain":"pi""#) && json.matches("plain").count() == 1, "{json}");
        assert_eq!(serde_json::from_str::<SavedWorkspace>(&json).unwrap(), saved);
        // Unset fields are optional.
        let short: SavedWorkspace = serde_json::from_str(r#"{"version":1,"tabs":[{"tree":{"leaf":4},"focused":4}],"active":0}"#).unwrap();
        assert_eq!(short.tabs[0], tab(Node::Leaf(4), 4));
        assert_eq!(short.window, None);
        let windowed: SavedWorkspace =
            serde_json::from_str(r#"{"version":1,"tabs":[],"active":0,"window":{"width":800,"height":500}}"#).unwrap();
        assert_eq!(windowed.window, Some(SavedWindow { width: 800.0, height: 500.0, state: WindowState::Windowed }));
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

    #[test]
    fn pruning_drops_ended_sessions_and_fixes_focus() {
        let saved = SavedWorkspace {
            version: VERSION,
            tabs: vec![
                SavedTab { name: Some(" ".into()), tree: split(1, 2), focused: 2, zoomed: true, plain: None },
                tab(Node::Leaf(3), 3),
                tab(Node::Leaf(4), 4),
            ],
            active: 1,
            window: None,
        };
        let alive: HashSet<SessionId> = [1, 4].into();
        let (tabs, active) = prune(saved.clone(), &alive).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].tree, Node::Leaf(1));
        assert_eq!((tabs[0].focused, tabs[0].zoomed, tabs[0].name.as_deref()), (1, false, None));
        // The active tab (3) ended: the one before it is active.
        assert_eq!(active, 0);
        assert!(prune(saved.clone(), &HashSet::new()).is_none());
        assert!(prune(SavedWorkspace { version: VERSION + 1, ..saved }, &alive).is_none());
    }

    #[test]
    fn plain_tabs_and_summaries() {
        let saved = SavedWorkspace {
            version: VERSION,
            tabs: vec![
                SavedTab { name: Some("logs".into()), ..tab(split(1, 2), 1) },
                SavedTab { plain: Some(" ".into()), ..tab(Node::Leaf(3), 3) },
                SavedTab { plain: Some("pi".into()), ..tab(Node::Leaf(4), 4) },
            ],
            active: 0,
            window: None,
        };
        let (tabs, _) = prune(saved.clone(), &[1, 2, 3, 4].into()).unwrap();
        assert_eq!(tabs.iter().map(|t| t.plain.as_deref()).collect::<Vec<_>>(), [None, None, Some("pi")]);
        let info = goatty_proto::LayoutInfo { id: 1, json: serde_json::to_string(&saved).unwrap(), sessions: vec![1, 2, 4], saved_unix: 0 };
        let (tabs, panes, names) = summary(&info, |id| (id == 4).then(|| "htop".to_string())).unwrap();
        assert_eq!((tabs, panes, names), (2, 3, vec!["logs".to_string(), "htop".to_string()]));
        assert!(summary(&goatty_proto::LayoutInfo { json: "nope".into(), ..info }, |_| None).is_none());
    }

    #[test]
    fn loose_detached_sessions_skip_open_ones_and_saved_layouts() {
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
        let layouts = [goatty_proto::LayoutInfo { id: 1, json: String::new(), sessions: vec![2], saved_unix: 0 }];
        assert_eq!(loose_detached(&[s(5, 0), s(2, 0), s(3, 1), s(1, 0)], &layouts), [1, 5]);
    }

    #[test]
    fn a_session_shows_once() {
        let saved = SavedWorkspace { version: VERSION, tabs: vec![tab(split(1, 2), 1), tab(split(2, 3), 3)], active: 1, window: None };
        let (tabs, active) = prune(saved, &[1, 2, 3].into()).unwrap();
        assert_eq!(tabs[0].tree.leaves(), [1, 2]);
        assert_eq!(tabs[1].tree, Node::Leaf(3));
        assert_eq!(active, 1);
    }
}
