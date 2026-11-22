//! The root view of a window: tabs, each holding a split layout of
//! terminal panes, plus everything window-level: the title (the focused
//! pane's), closing when the last shell exits, the command palette overlay,
//! paste and close confirmations (`crate::paste_confirm`,
//! `crate::close_confirm`), the window's toasts (`crate::toast`) and the
//! list of recent notifications (`crate::toast::recent`).
//!
//! - `layout`: the split tree of a tab (pure, unit-tested).
//! - `tab_bar`: the horizontal tab strip (`tabs.style` modern / compact)
//!   and what every style shares: switching, closing, renaming, dragging
//!   to reorder, the mouse wheel.
//! - `sidebar`: the vertical tab style, a resizable sidebar that folds into
//!   a rail of icons, next to the panes drawn as a rounded card.
//! - `menu`: the right-click menus of tabs, panes and dividers (#186).
//! - `arrange`: rearranging panes and tabs (#124): swap, rotate, move a
//!   pane to another tab, bring a tab into another one, the minimum pane
//!   size and resizing by cells.
//! - `groups`: tabs grouped by host, with a colored label per host that
//!   folds its tabs away (once the window has a tab on another host).
//! - `gestures`: the pointer maths of the above (pure, unit-tested).
//! - `icons`: Nerd Font icons for the program running in a tab (coding
//!   agents get their logos, `crate::agent`).
//! - `notices`: what panes have to tell (an agent waiting, done; a
//!   program's notification) as toasts and desktop notifications (#205).
//! - `saved`: the layout goattyd keeps for the window, so the next
//!   `goatty` restores every tab and split (#118).
//! - `reopen`: which hosts each window has tabs on, so the next `goatty`
//!   reopens every window with its tabs on every host (#252).
//!
//! Each tab is on one host, and a window can hold tabs on several (a local
//! shell next to a few servers). A tab remembers its [`Spawner`]: new
//! panes and new tabs from it are new sessions there (another `create` on
//! the same goattyd connection, `ssh host` again for plain SSH, another
//! local PTY), started in the focused pane's directory. Closing a pane
//! ends its shell (after asking, when a program runs in it); closing the
//! window only detaches (the daemons keep the sessions).
//!
//! A tab whose session isn't there yet shows the connect screen
//! (`crate::connect`, #37): the window opens at once, and the tab gets its
//! panes when the connection is made (`open_connect_tab`).
//!
//! Each goattyd the window has tabs on keeps the layout of those tabs, and
//! `reopen` remembers which goattyds those are, so `goatty` brings back
//! every window with its tabs on every host (`goatty --ssh host`: that
//! host's). While a window is
//! active, the settings window and "reload shells" act on its active tab's
//! host (`settings_window::set_host_link`).

mod arrange;
mod chrome;
mod gestures;
pub(crate) mod groups;
pub(crate) mod icons;
mod layout;
mod menu;
mod notices;
pub mod reopen;
pub mod saved;
pub use chrome::requested as requested_decorations;
mod sidebar;
mod tab_bar;

use std::path::Path;
use std::sync::Arc;

use goatty_client::Connection;
use goatty_config::{TabActivity, TabBarShow, TabSettings, TabStyle};
use goatty_proto::LayoutId;
use goatty_term::TermSize;
use gpui::{
    AppContext, Context, DragMoveEvent, Entity, Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    ScrollHandle, Styled, Subscription, Task, Window, div, prelude::*, px, relative,
};

use std::rc::Rc;

use crate::actions::*;
use crate::app::Initial;
use crate::close_confirm::{CloseChoice, CloseConfirm, CloseConfirmEvent, Closing};
use crate::command_palette::{Ask, CommandPalette, Field, PaletteEvent, Scope};
use crate::connect::{ConnectEvent, ConnectPlan, ConnectView};
use crate::hosts::HostRef;
use crate::paste_confirm::{PasteConfirm, PasteConfirmEvent};
use crate::session::AttachMode;
use crate::session::{Session, Spawner};
use crate::settings::AppSettings;
use crate::terminal::{Activity, PasteRequest, TerminalEvent, TerminalView};
use crate::toast::recent::{RecentEvent, RecentNotifications};
use crate::toast::{self, Toast, ToastLayer};
use crate::ui::style::Ui;
use crate::ui::text_input::{TextInput, TextInputEvent};
use gestures::{Sidebar, WheelSwitch};
use layout::{Axis, Direction, Node, PaneId, Rect};
use saved::{SavedTab, SavedWindow, SavedWorkspace, Startup};

/// Width of the line between panes.
const DIVIDER: f32 = 1.0;
/// The grab area around it.
const DIVIDER_GRAB: f32 = 6.0;

/// Builds the final argv once the target host's `$HOME` (if requested) is resolved.
pub type BuildArgv = Box<dyn FnOnce(Option<&str>) -> Result<Vec<String>, String> + Send>;

/// A direct program launch into a split pane or new tab (#241, #242).
pub struct LaunchCommand {
    /// Program name / label for error and exit toasts (e.g. `lazygit`).
    pub label: String,
    /// Where to place the pane (a split direction or `Tab`).
    pub open_in: goatty_config::CommandOpenIn,
    /// For plugin commands: the host the plugin belongs to. `None` for
    /// client-side custom commands (which run on whichever host is active).
    pub origin: Option<crate::settings_window::HostLink>,
    /// Whether `build_argv` needs the target host's `$HOME` for `~` expansion.
    pub needs_home: bool,
    /// Builds the final argv once `home` (if requested) has been resolved.
    pub build_argv: BuildArgv,
    /// Shell command line offered via "Copy the command" when direct launch
    /// isn't available (plain SSH tab or host mismatch).
    pub fallback_line: String,
}

struct Pane {
    id: PaneId,
    view: Entity<TerminalView>,
    /// The newest program status report the user has seen (its tab was on
    /// screen in the focused window): `done`/`error` up to it aren't shown.
    seen: u64,
    /// Pane to focus again when this pane closes (set for launched splits).
    return_focus: Option<PaneId>,
    /// Set for panes opened directly for a command/program: `(label, started)`.
    /// If the process exits very quickly with a non-zero exit code, a toast
    /// explains why the split closed.
    launched: Option<(String, std::time::Instant)>,
    _subscriptions: [Subscription; 2],
}

struct Tab {
    id: u64,
    tree: Node,
    panes: Vec<Pane>,
    focused: PaneId,
    /// The focused pane fills the tab.
    zoomed: bool,
    /// Set by renaming; otherwise the focused pane's title.
    name: Option<String>,
    /// Where its sessions live; `None` while connecting.
    host: Option<Spawner>,
    /// The connect screen, until the tab's session is there (its tree is
    /// then a single placeholder leaf without a pane).
    connecting: Option<(Entity<ConnectView>, Subscription)>,
    /// The most important thing that happened in its panes while it was in
    /// the background (#122); cleared when it's shown.
    activity: Option<Activity>,
}

/// Do the tab settings show this kind of activity?
fn marks(tabs: &TabSettings, activity: Activity) -> bool {
    match activity {
        Activity::Bell => tabs.mark_bell,
        Activity::Finished | Activity::Failed => tabs.mark_activity != TabActivity::Off,
        Activity::Output => tabs.mark_activity == TabActivity::Output,
    }
}

impl Tab {
    fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn focused_view(&self) -> Option<&Entity<TerminalView>> {
        self.pane(self.focused).map(|p| &p.view)
    }

    /// Is pane `id` drawn when the tab is (not hidden by a zoomed pane)?
    fn shows(&self, id: PaneId) -> bool {
        !self.zoomed || self.focused == id
    }
}

/// Where a new pane goes.
#[derive(Debug, Clone, Copy)]
enum Placement {
    NewTab,
    /// Next to pane `of` in tab `tab`, on the `dir` side.
    Split {
        tab: u64,
        of: PaneId,
        dir: Direction,
    },
}

/// What a close confirmation is about.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CloseTarget {
    Pane(PaneId),
    /// By tab id.
    Tab(u64),
    /// Several tabs, by id ("Close the other tabs").
    Tabs(Vec<u64>),
}

/// A tab being renamed in place.
struct Rename {
    tab: u64,
    input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

/// Dragging a divider (a GPUI drag; see `on_drag_move`).
#[derive(Debug, Clone)]
struct DividerDrag {
    tab: u64,
    path: Vec<usize>,
    index: usize,
    axis: Axis,
    split: Rect,
}

/// Dragging a tab to reorder it.
#[derive(Clone)]
pub(super) struct DraggedTab {
    id: u64,
    label: gpui::SharedString,
    /// The program's icon (glyph, color), when the tab shows one.
    icon: Option<(char, gpui::Hsla)>,
    /// The coding agent's logo instead, when it runs there.
    agent: Option<crate::agent::Identity>,
}

/// The layout one goattyd keeps for this window's tabs on it.
struct HostLayout {
    conn: Arc<Connection>,
    id: LayoutId,
    /// What was last sent.
    json: String,
    sessions: Vec<goatty_proto::SessionId>,
}

/// This window's tabs on one goattyd, as `save_layout` collects them.
struct LayoutGroup {
    conn: Arc<Connection>,
    sessions: Vec<goatty_proto::SessionId>,
    tabs: Vec<SavedTab>,
    /// How many of them come before the active tab (the active one's index
    /// when it's among them).
    before_active: usize,
    /// Index of the active tab when it's among them, else of the one last
    /// active on this host (restored as the host's active tab).
    focus: Option<usize>,
}

pub struct Workspace {
    /// Where new tabs go when the active tab has no host yet: the first
    /// host that connected.
    home: Option<Spawner>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    rename: Option<Rename>,
    palette: Option<(Entity<CommandPalette>, Subscription)>,
    /// The snippet picker (Ctrl+Shift+S).
    snippets: Option<(Entity<crate::snippets::picker::SnippetPicker>, Subscription)>,
    /// "Notifications: Show recent".
    recent: Option<(Entity<RecentNotifications>, Subscription)>,
    paste: Option<(Entity<PasteConfirm>, PaneId, Subscription)>,
    close: Option<(Entity<CloseConfirm>, CloseTarget, Subscription)>,
    toasts: Entity<ToastLayer>,
    layouts: Vec<HostLayout>,
    /// The window's size and state, saved with the layouts.
    window: SavedWindow,
    /// A layout save waiting for the window to stop resizing.
    save_window: Option<Task<()>>,
    /// The tab strip's or sidebar's scroll position; scrolls the active tab
    /// into view.
    tab_scroll: ScrollHandle,
    /// Mouse wheel over the tabs (`tabs.scroll_to_switch`).
    wheel: WheelSwitch,
    /// The tab being dragged, drawn faded where it is.
    dragging: Option<u64>,
    /// A dragged tab moved and the tabs haven't been laid out again: their
    /// bounds are from before the move.
    moved_unpainted: bool,
    /// The left button went down on the title bar's empty space: the next
    /// move starts moving the window (`chrome`).
    title_drag: bool,
    /// The window edge under the pointer, when goatty draws the frame.
    resize_edge: Option<gpui::ResizeEdge>,
    /// The frame last told to the platform: goatty's (with the tiling) or none.
    frame_applied: Option<Option<gpui::Tiling>>,
    /// The vertical sidebar while its edge is being dragged (as it was when
    /// the drag started, and now); saved to the settings shortly after it
    /// stops and when the drag ends.
    sidebar_drag: Option<(Sidebar, Sidebar)>,
    save_sidebar: Option<Task<()>>,
    /// The open right-click menu (a tab's, a pane's or a divider's).
    menu: Option<menu::OpenMenu>,
    /// Where the panes were last painted, for sizes in cells.
    panes_bounds: Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// The host groups folded down to their label.
    folded: Vec<groups::HostKey>,
    /// The tab last active in each host's group (by id).
    group_focus: Vec<(groups::HostKey, u64)>,
    /// Incremented per host group whenever that group is detached or loses all
    /// its tabs, so an in-flight `spawn` for a departed group does not reopen it.
    host_epochs: Vec<(groups::HostKey, u64)>,
    /// What panes told (toasts, desktop notifications; `notices`).
    notices: notices::Notices,
    /// This window, for its `reopen` record.
    window_id: gpui::WindowId,
    reopen_key: u64,
    /// Tabs still connecting to restore a host's tabs (reopened windows,
    /// "Restore tabs" in the sessions picker): their host stays in the
    /// `reopen` record meanwhile, so quitting before they connect loses nothing.
    restoring: Vec<(u64, reopen::HostRecord)>,
    /// Hosts (and plain SSH hosts on them) tabs were detached from, newest
    /// last: where Ctrl+Shift+R looks first.
    recently_detached: Vec<(HostRef, Option<String>)>,
    /// The window took its size from its `reopen` record, which wins over
    /// the size a host's layout remembers.
    sized_by_record: bool,
    _subscriptions: Vec<Subscription>,
}

/// How long the window's size has to settle before it's saved (a resize
/// drag changes it every frame).
const SAVE_WINDOW_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

impl Workspace {
    pub fn new(initial: Initial, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        // The desktop's light/dark switch (themes can follow it).
        // Also how the desktop decorates the window changed (Wayland).
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            crate::settings::AppSettings::set_system_dark(cx, crate::settings::is_dark(window.appearance()));
            cx.notify();
        });
        let button_layout = cx.observe_button_layout_changed(window, |_, _, cx| cx.notify());
        let mut title_bar = cx.global::<AppSettings>().client.window.title_bar;
        // `terminal.opacity` / `terminal.blur`, live.
        let mut background = background_appearance(&cx.global::<AppSettings>().client.terminal);
        window.set_background_appearance(background);
        let settings = cx.observe_global_in::<AppSettings>(window, move |_, window, cx| {
            let wanted = background_appearance(&cx.global::<AppSettings>().client.terminal);
            if wanted != background {
                background = wanted;
                window.set_background_appearance(wanted);
            }
            let wanted = cx.global::<AppSettings>().client.window.title_bar;
            if wanted != title_bar {
                title_bar = wanted;
                window.request_decorations(chrome::requested(wanted));
            }
            cx.notify();
        });
        // Settings and "reload shells" act on the active window's host, and
        // hosts opened in a tab go to the active window.
        let activation = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
            if window.is_window_active() {
                this.window_activated(window, cx);
            }
        });
        let bounds = cx.observe_window_bounds(window, |this: &mut Self, window, cx| this.window_bounds_changed(window, cx));
        cx.set_global(crate::hosts::LastWorkspace(window.window_handle()));
        let toasts = cx.new(|cx| ToastLayer::new(window, cx));
        let mut this = Self {
            home: None,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            rename: None,
            palette: None,
            snippets: None,
            recent: None,
            paste: None,
            close: None,
            toasts,
            layouts: Vec::new(),
            window: SavedWindow::from_bounds(window.window_bounds()),
            save_window: None,
            tab_scroll: ScrollHandle::new(),
            wheel: WheelSwitch::default(),
            dragging: None,
            moved_unpainted: false,
            title_drag: false,
            resize_edge: None,
            frame_applied: None,
            sidebar_drag: None,
            save_sidebar: None,
            menu: None,
            panes_bounds: Rc::default(),
            folded: Vec::new(),
            group_focus: Vec::new(),
            host_epochs: Vec::new(),
            notices: Default::default(),
            window_id: window.window_handle().window_id(),
            reopen_key: reopen::new_key(),
            restoring: Vec::new(),
            recently_detached: Vec::new(),
            sized_by_record: false,
            _subscriptions: vec![appearance, button_layout, settings, activation, bounds],
        };
        reopen::window_opened(cx, this.window_id);
        this._subscriptions.push(cx.on_release(|this: &mut Self, cx| this.released(cx)));
        match initial {
            Initial::Ready { startup, spawner } => {
                this.home = Some(spawner.clone());
                crate::shell_sync::on_connect(spawner.host_link(), false, cx);
                match startup {
                    Startup::Session(session) => this.insert_pane(session, Placement::NewTab, spawner, None, window, cx),
                    Startup::Restore(r) => {
                        if let Some(w) = r.window {
                            w.apply(window);
                            this.window = w;
                        }
                        let active = this.insert_restored(r, &spawner, 0, window, cx);
                        this.activate(active, window, cx);
                    }
                    Startup::Nothing => this.spawn_into_empty(spawner, window, cx),
                }
            }
            Initial::Connect(plan) => this.open_connect_tab(plan, window, cx),
            Initial::Reopen(record, open_settings) => this.reopen(record, open_settings, window, cx),
        }
        this
    }

    /// The window went away with its tabs (closed while goatty runs on, or
    /// goatty quit): its shells keep running detached, and its layouts are
    /// free for the next window (the sessions picker, `reattach_detached`, or
    /// the next goatty) rather than held by a pooled connection that other
    /// windows keep open.
    fn released(&mut self, cx: &mut gpui::App) {
        for p in self.tabs.iter().flat_map(|t| t.panes.iter()) {
            p.view.read(cx).session().detach();
        }
        for layout in self.layouts.drain(..) {
            layout.conn.detach_layout(layout.id, layout.sessions, layout.json);
        }
    }

    /// Open a window from the last run (#252): a connecting tab per host
    /// that claims this window's layout there, in the order of their tabs.
    fn reopen(&mut self, record: reopen::WindowRecord, mut open_settings: Option<String>, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(w) = record.window {
            w.apply(window);
            self.window = w;
            self.sized_by_record = true;
        }
        self.reopen_key = record.key;
        let mut active = None;
        let active_host = record.active.min(record.hosts.len().saturating_sub(1));
        for (i, h) in record.hosts.into_iter().enumerate() {
            let plan = ConnectPlan {
                endpoint: h.host.host_ref().endpoint(),
                mode: AttachMode::Layout(h.layout),
                // `--settings`: once the active tab's host is there.
                open_settings: if i == active_host { open_settings.take() } else { None },
                plain: false,
                conn: None,
            };
            self.open_connect_tab(plan, window, cx);
            let tab = self.tabs[self.active].id;
            self.restoring.push((tab, h));
            if i == active_host {
                active = Some(tab);
            }
        }
        if let Some(i) = active.and_then(|id| self.tab_index(id)) {
            self.activate(i, window, cx);
        }
        self.record_window(cx);
    }

    /// The window has no tab yet and nothing to restore: a new shell on
    /// `spawner`'s host.
    fn spawn_into_empty(&mut self, spawner: Spawner, window: &mut Window, cx: &mut Context<'_, Self>) {
        let task = {
            let spawner = spawner.clone();
            cx.background_executor().spawn(async move { spawner.spawn(TermSize::new(100, 30), None) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(session) => this.insert_pane(session, Placement::NewTab, spawner, None, window, cx),
                Err(e) => {
                    let toast = Toast::error("Couldn't start a shell").body(format!("{e:#}")).key("workspace.spawn");
                    toast::show_in(window, cx, toast);
                }
            });
        })
        .detach();
    }

    /// Tell `reopen` which hosts this window has tabs on now.
    fn record_window(&self, cx: &mut Context<'_, Self>) {
        let mut hosts: Vec<reopen::HostRecord> = Vec::new();
        let mut active = 0;
        for (i, t) in self.tabs.iter().enumerate() {
            let record = match t.host.as_ref().and_then(Spawner::connection) {
                Some(conn) => {
                    let Some(layout) = self.layouts.iter().find(|l| Arc::ptr_eq(&l.conn, conn)) else { continue };
                    let Some(host) = reopen::SavedHost::of(conn.endpoint()) else { continue };
                    reopen::HostRecord { host, layout: layout.id }
                }
                None => match self.restoring.iter().find(|(id, _)| *id == t.id) {
                    Some((_, r)) => r.clone(),
                    None => continue,
                },
            };
            let at = match hosts.iter().position(|h| h.host == record.host) {
                Some(at) => at,
                None => {
                    hosts.push(record);
                    hosts.len() - 1
                }
            };
            if i == self.active {
                active = at;
            }
        }
        let record = (!hosts.is_empty()).then(|| reopen::WindowRecord {
            pid: std::process::id(),
            key: self.reopen_key,
            hosts,
            active,
            window: Some(self.window),
        });
        reopen::update(cx, self.window_id, self.reopen_key, record);
    }

    /// The window was resized, maximized or went fullscreen: save its size
    /// with the layout once it settles.
    fn window_bounds_changed(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let now = SavedWindow::capture(window, self.window);
        if now == self.window {
            return;
        }
        self.window = now;
        self.save_window = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_WINDOW_AFTER).await;
            this.update(cx, |this, cx| this.save_layout(cx)).ok();
        }));
    }

    fn window_activated(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        cx.set_global(crate::hosts::LastWorkspace(window.window_handle()));
        // A connecting tab has no host yet: clear the link rather than keep
        // the previous tab's, so "sync", "reload shells" and the settings
        // window never act on a host the user isn't looking at.
        let link = self.active_tab().and_then(|t| t.host.as_ref()).map(Spawner::host_link);
        crate::settings_window::set_host_link(cx, link);
        // The active tab is being looked at again.
        self.notices_seen(window, cx);
        cx.notify();
    }

    /// The layout entry for `conn` (a new one for a host the window had no tabs on).
    fn layout_for(&mut self, conn: &Arc<Connection>) -> &mut HostLayout {
        let i = match self.layouts.iter().position(|l| Arc::ptr_eq(&l.conn, conn)) {
            Some(i) => i,
            None => {
                self.layouts.push(HostLayout { conn: conn.clone(), id: saved::new_id(), json: String::new(), sessions: Vec::new() });
                self.layouts.len() - 1
            }
        };
        &mut self.layouts[i]
    }

    /// Rebuild the tabs of a saved layout from its attached sessions,
    /// inserted at `at`. Returns the index of its active tab.
    fn insert_restored(
        &mut self,
        r: saved::Restored,
        spawner: &Spawner,
        at: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> usize {
        if let Some(conn) = spawner.connection() {
            let has_tabs = self.tabs.iter().any(|t| t.host.as_ref().and_then(Spawner::connection).is_some_and(|c| Arc::ptr_eq(c, conn)));
            if has_tabs {
                // The window keeps its own layout there, which takes these
                // tabs in from the next save on; the claimed one is done.
                if r.claimed {
                    conn.save_layout(r.id, Vec::new(), String::new());
                }
            } else {
                self.layout_for(conn).id = r.id;
            }
        }
        let mut at = at.min(self.tabs.len());
        let first = at;
        let mut sessions = r.sessions;
        for t in r.tabs {
            let mut panes = Vec::new();
            let mut pane_of = std::collections::HashMap::new();
            let tree = t.tree.map_leaves(&mut |session_id| {
                let session = sessions.remove(&session_id)?;
                let pane = self.make_pane(session, window, cx);
                pane_of.insert(session_id, pane.id);
                let id = pane.id;
                panes.push(pane);
                Some(id)
            });
            let Some(tree) = tree else { continue };
            let focused = pane_of.get(&t.focused).copied().unwrap_or(panes[0].id);
            let id = self.next_id();
            let host = match t.plain {
                Some(host) => Spawner::Plain { host, base: Box::new(spawner.clone()) },
                None => spawner.clone(),
            };
            let tab = Tab { id, tree, panes, focused, zoomed: t.zoomed, name: t.name, host: Some(host), connecting: None, activity: None };
            self.tabs.insert(at, tab);
            at += 1;
        }
        (first + r.active).min(at.saturating_sub(1))
    }

    /// Tell each goattyd what this window's tabs on it look like now (sent
    /// only when it changed; local PTYs have nothing to save). A goattyd
    /// the window has no tabs on any more hears that the layout is gone.
    fn save_layout(&mut self, cx: &mut Context<'_, Self>) {
        let mut groups: Vec<LayoutGroup> = Vec::new();
        for (i, t) in self.tabs.iter().enumerate() {
            let Some(conn) = t.host.as_ref().and_then(Spawner::connection) else { continue };
            let Some(tab) = saved_tab(t, cx) else { continue };
            let g = match groups.iter().position(|g| Arc::ptr_eq(&g.conn, conn)) {
                Some(g) => g,
                None => {
                    groups.push(LayoutGroup { conn: conn.clone(), sessions: Vec::new(), tabs: Vec::new(), before_active: 0, focus: None });
                    groups.len() - 1
                }
            };
            let g = &mut groups[g];
            g.sessions.extend(tab.tree.leaves());
            g.tabs.push(tab);
            if i < self.active {
                g.before_active += 1;
            }
            // The active tab wins over the host's last active one.
            if i == self.active || (g.focus.is_none() && self.group_focus.iter().any(|(_, id)| *id == t.id)) {
                g.focus = Some(g.tabs.len() - 1);
            }
        }
        for g in &groups {
            self.layout_for(&g.conn);
        }
        self.layouts.retain_mut(|layout| {
            let (sessions, json) = match groups.iter_mut().find(|g| Arc::ptr_eq(&g.conn, &layout.conn)) {
                Some(g) => {
                    let active = g.focus.unwrap_or(g.before_active).min(g.tabs.len() - 1);
                    let saved =
                        SavedWorkspace { version: saved::VERSION, tabs: std::mem::take(&mut g.tabs), active, window: Some(self.window) };
                    match serde_json::to_string(&saved) {
                        Ok(j) => (std::mem::take(&mut g.sessions), j),
                        Err(e) => {
                            log::warn!("saving the layout: {e}");
                            return true;
                        }
                    }
                }
                None => (Vec::new(), String::new()),
            };
            if json != layout.json {
                layout.conn.save_layout(layout.id, sessions.clone(), json.clone());
                layout.json = json.clone();
                layout.sessions = sessions;
            }
            // Done with a host once it heard the layout is gone.
            !json.is_empty()
        });
        self.record_window(cx);
    }

    /// The window is going away with its last tab: every goattyd forgets
    /// its layout (sent before the kills, whose replies prove both arrived).
    fn forget_layouts(&mut self, cx: &mut Context<'_, Self>) {
        for layout in self.layouts.drain(..) {
            layout.conn.save_layout(layout.id, Vec::new(), String::new());
        }
        // Nothing to reopen either.
        self.restoring.clear();
        reopen::update(cx, self.window_id, self.reopen_key, None);
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // ---- lookups ------------------------------------------------------------

    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    fn focused_view(&self) -> Option<&Entity<TerminalView>> {
        self.active_tab()?.focused_view()
    }

    /// (tab index, pane) holding pane `id`.
    fn find_pane(&self, id: PaneId) -> Option<(usize, &Pane)> {
        self.tabs.iter().enumerate().find_map(|(i, t)| t.pane(id).map(|p| (i, p)))
    }

    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// What a tab is called: its name, else what its focused pane shows.
    fn tab_label(&self, tab: &Tab, cx: &gpui::App) -> String {
        if let Some(name) = &tab.name {
            return name.clone();
        }
        if let Some((view, _)) = &tab.connecting {
            return view.read(cx).host_label().to_string();
        }
        tab.focused_view().map(|v| pane_label(v.read(cx))).unwrap_or_else(|| "goatty".into())
    }

    // ---- focus and titles ---------------------------------------------------

    /// Focus the active tab's pane (or connect screen); an open palette or
    /// list of recent notifications keeps the keyboard until it closes.
    fn focus_active(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(menu) = self.menu_focus(cx) {
            menu.focus(window, cx);
        } else if let Some((view, _)) = &self.recent {
            view.focus_handle(cx).focus(window, cx);
        } else if let Some((picker, _)) = &self.snippets {
            picker.focus_handle(cx).focus(window, cx);
        } else if let Some((palette, _)) = &self.palette {
            palette.focus_handle(cx).focus(window, cx);
        } else if let Some((view, _)) = self.active_tab().and_then(|t| t.connecting.as_ref()) {
            view.focus_handle(cx).focus(window, cx);
        } else if let Some(view) = self.focused_view() {
            view.focus_handle(cx).focus(window, cx);
        }
    }

    fn update_window_title(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        if let Some((view, _)) = &tab.connecting {
            window.set_window_title(&view.read(cx).title());
            return;
        }
        let Some(view) = tab.focused_view() else { return };
        let label = self.tab_label(tab, cx);
        window.set_window_title(&view.read(cx).session().decorate_title(&label));
    }

    /// A pane got focus (click, keyboard navigation, new pane).
    fn pane_focused(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((index, _)) = self.find_pane(id) else { return };
        let tab = &mut self.tabs[index];
        if tab.focused != id {
            tab.focused = id;
            tab.zoomed = false;
        }
        self.active = index;
        self.update_window_title(window, cx);
        self.notices_seen(window, cx);
        self.save_layout(cx);
        cx.notify();
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.finish_rename(true, window, cx);
        // Each host's tabs stay together (a tab may just have joined a host).
        let id = self.tabs[index].id;
        self.active = index;
        self.regroup();
        let index = self.tab_index(id).unwrap_or(index);
        self.active = index;
        self.group_activated(index);
        self.tab_scroll.scroll_to_item(self.strip_index(index, cx));
        self.focus_active(window, cx);
        self.update_window_title(window, cx);
        let is_last_workspace = cx.try_global::<crate::hosts::LastWorkspace>().is_some_and(|w| w.0 == window.window_handle());
        if window.is_window_active() || is_last_workspace {
            // None while the tab connects (see `window_activated`).
            crate::settings_window::set_host_link(cx, self.tabs[index].host.as_ref().map(Spawner::host_link));
        }
        self.notices_seen(window, cx);
        self.save_layout(cx);
        cx.notify();
    }

    // ---- connecting -----------------------------------------------------------

    /// A new tab (after the active one) that connects for `plan` and shows
    /// the connect screen until its session is there.
    pub fn open_connect_tab(&mut self, mut plan: ConnectPlan, window: &mut Window, cx: &mut Context<'_, Self>) {
        // A host this window has tabs on already: its last layout is this
        // window's, so start a session instead of claiming another one.
        if matches!(plan.mode, AttachMode::Auto)
            && let Some(live) = crate::hosts::pool::live(&plan.endpoint)
            && self.layouts.iter().any(|l| Arc::ptr_eq(&l.conn, &live))
        {
            plan.mode = AttachMode::New;
        }
        let id = self.next_id();
        let placeholder = self.next_id();
        let view = cx.new(|cx| ConnectView::new(plan, window, cx));
        let sub = cx.subscribe_in(&view, window, move |this, _, ev: &ConnectEvent, window, cx| this.on_connect_event(id, ev, window, cx));
        let tab = Tab {
            id,
            tree: Node::Leaf(placeholder),
            panes: Vec::new(),
            focused: placeholder,
            zoomed: false,
            name: None,
            host: None,
            connecting: Some((view, sub)),
            activity: None,
        };
        let at = if self.tabs.is_empty() { 0 } else { self.active + 1 };
        self.tabs.insert(at, tab);
        self.activate(at, window, cx);
    }

    fn on_connect_event(&mut self, tab: u64, ev: &ConnectEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(index) = self.tab_index(tab) else { return };
        match ev {
            ConnectEvent::Changed => {
                if index == self.active {
                    self.update_window_title(window, cx);
                }
                cx.notify();
            }
            ConnectEvent::Cancelled => self.remove_tab(index, window, cx),
            ConnectEvent::Ready => {
                let Some((view, _)) = &self.tabs[index].connecting else { return };
                let (ready, settings, installed) = view.update(cx, |v, _| (v.take_ready(), v.take_open_settings(), v.take_installed()));
                let Some((startup, spawner)) = ready else { return };
                self.connected(index, startup, spawner, installed, window, cx);
                if let Some(page) = settings {
                    crate::settings_window::open_at(cx, crate::settings_window::Page::from_name(&page));
                }
            }
        }
    }

    /// The connecting tab at `index` got its session: show it there (a
    /// restored layout replaces the tab with its tabs).
    fn connected(
        &mut self,
        index: usize,
        startup: Startup,
        spawner: Spawner,
        installed: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.home.is_none() {
            self.home = Some(spawner.clone());
        }
        // Another host: bring the shell setup there (asks, once per change
        // made here); this computer: offer missing tools (once per run).
        crate::shell_sync::on_connect(spawner.host_link(), installed, cx);
        let was_active = index == self.active;
        let tab_id = self.tabs[index].id;
        self.restoring.retain(|(id, _)| *id != tab_id);
        let show = match startup {
            Startup::Nothing => {
                self.nothing_to_restore(index, spawner, window, cx);
                return;
            }
            Startup::Session(session) => {
                self.fill_connecting_tab(index, session, spawner, window, cx);
                return;
            }
            Startup::Restore(r) => {
                let n = r.tabs.len();
                // The window's only tab: the window is the restored one, so
                // it takes the restored size (not when restoring into a
                // window that has other tabs).
                if self.tabs.len() == 1
                    && !self.sized_by_record
                    && let Some(w) = r.window
                    && w != self.window
                {
                    w.apply(window);
                    self.window = w;
                }
                self.tabs.remove(index);
                let active = self.insert_restored(r, &spawner, index, window, cx);
                if self.active > index {
                    self.active += n.saturating_sub(1);
                }
                // In the background: its host still shows that tab first.
                if !was_active && let Some(t) = self.tabs.get(active) {
                    let (key, id) = (self.host_key(t), t.id);
                    self.group_focus.retain(|(k, _)| *k != key);
                    self.group_focus.push((key, id));
                }
                active
            }
        };
        if was_active {
            self.activate(show, window, cx);
        } else {
            self.regroup();
            self.save_layout(cx);
            cx.notify();
        }
    }

    /// The connecting tab at `index` gets `session` (on `spawner`'s host).
    fn fill_connecting_tab(
        &mut self,
        index: usize,
        session: Arc<Session>,
        spawner: Spawner,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let was_active = index == self.active;
        let pane = self.make_pane(session, window, cx);
        let tab = &mut self.tabs[index];
        (tab.tree, tab.focused) = (Node::Leaf(pane.id), pane.id);
        tab.panes = vec![pane];
        tab.connecting = None;
        tab.host = Some(spawner);
        if was_active {
            self.activate(index, window, cx);
        } else {
            self.regroup();
            self.save_layout(cx);
            cx.notify();
        }
    }

    /// The connecting tab at `index` found nothing left to restore on its
    /// host: it goes away with a note (the window's last tab gets a new
    /// shell there instead, so the window doesn't vanish).
    fn nothing_to_restore(&mut self, index: usize, spawner: Spawner, window: &mut Window, cx: &mut Context<'_, Self>) {
        let host = groups::host_label(&spawner);
        if self.tabs.len() == 1 {
            // The connect screen stays until the shell is there.
            let tab = &self.tabs[index];
            let toast = Toast::info(format!("Nothing left to restore on {host}"))
                .body("Those tabs' shells ended meanwhile; here's a new one.")
                .key("workspace.restore");
            toast::show_in(window, cx, toast);
            // Fill the placeholder tab: replaced once the shell is there.
            let placeholder = tab.id;
            let task = cx.background_executor().spawn({
                let spawner = spawner.clone();
                async move { spawner.spawn(TermSize::new(100, 30), None) }
            });
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(index) = this.tab_index(placeholder) else {
                        if let Ok(session) = result {
                            this.discard_spawned_session(session, cx);
                        }
                        return;
                    };
                    match result {
                        Ok(session) => this.fill_connecting_tab(index, session, spawner, window, cx),
                        Err(e) => {
                            let toast = Toast::error("Couldn't start a shell").body(format!("{e:#}")).key("workspace.spawn");
                            toast::show_in(window, cx, toast);
                            this.remove_tab(index, window, cx);
                        }
                    }
                });
            })
            .detach();
            cx.notify();
            return;
        }
        let toast = Toast::info(format!("Nothing left to restore on {host}"))
            .body("Those tabs' shells ended meanwhile.")
            .key(format!("workspace.restore:{host}"));
        toast::show_in(window, cx, toast);
        self.remove_tab(index, window, cx);
    }

    // ---- creating and closing panes -----------------------------------------

    fn host_epoch(&self, key: &groups::HostKey) -> u64 {
        self.host_epochs.iter().find(|(k, _)| k == key).map_or(0, |(_, e)| *e)
    }

    fn bump_host_epoch(&mut self, key: &groups::HostKey) {
        match self.host_epochs.iter_mut().find(|(k, _)| k == key) {
            Some((_, e)) => *e = e.wrapping_add(1),
            None => self.host_epochs.push((key.clone(), 1)),
        }
    }

    /// Start a session in the background and put it at `placement`: on the
    /// active tab's host (the window's first host while it connects).
    fn spawn(&mut self, placement: Placement, window: &mut Window, cx: &mut Context<'_, Self>) {
        let host = self.active_tab().and_then(|t| t.host.clone());
        let host = match placement {
            Placement::NewTab => host.or_else(|| self.home.clone()),
            Placement::Split { .. } => host,
        };
        let Some(spawner) = host else {
            let toast =
                Toast::info("Still connecting").body("New tabs and splits can open once this tab is connected.").key("workspace.spawn");
            toast::show_in(window, cx, toast);
            return;
        };
        let spawn_epoch = self.host_epoch(&groups::HostKey::of(&spawner));
        // Same size and directory as the focused pane; the new pane resizes
        // itself on its first layout.
        let (size, cwd) = match self.focused_view() {
            Some(v) => {
                let v = v.read(cx);
                let (cols, rows) = v.session().terminal.size();
                (TermSize::new(cols, rows), v.cwd().map(|p| p.to_string_lossy().into_owned()))
            }
            None => (TermSize::new(100, 30), None),
        };
        let task = {
            let spawner = spawner.clone();
            cx.background_executor().spawn(async move { spawner.spawn(size, cwd) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(session) => this.insert_pane(session, placement, spawner, Some(spawn_epoch), window, cx),
                Err(e) => {
                    let toast = Toast::error("Couldn't start a shell").body(format!("{e:#}")).key("workspace.spawn");
                    toast::show_in(window, cx, toast);
                }
            });
        })
        .detach();
    }

    /// Launch a program directly in a split pane or new tab on the active tab's
    /// host, in the focused pane's working directory (#241, #242).
    pub fn open_command(&mut self, spec: LaunchCommand, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        let Some(spawner) = tab.host.clone() else {
            let toast = Toast::info("Still connecting")
                .body("Commands can open in a split or tab once this tab is connected.")
                .key("workspace.command");
            toast::show_in(window, cx, toast);
            return;
        };
        if let Some(origin) = &spec.origin
            && !spawner.host_link().same(origin)
        {
            let copy = spec.fallback_line;
            let toast = Toast::info(format!("That command belongs to {}", origin.label()))
                .body(format!("Switch to a tab on {}, or copy the command to run it here.", origin.label()))
                .action("Copy the command", move |_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone())))
                .key("workspace.command");
            toast::show_in(window, cx, toast);
            return;
        }
        if let Spawner::Plain { host, .. } = &spawner {
            let copy = spec.fallback_line;
            let toast = Toast::info(format!("Can't launch programs directly on plain SSH ({host})"))
                .body("Install goattyd on that host to open programs in splits or tabs, or copy the command.")
                .action("Copy the command", move |_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone())))
                .key("workspace.command");
            toast::show_in(window, cx, toast);
            return;
        }
        let tab_id = tab.id;
        let focused_id = tab.focused;
        let min = self.min_pane_size(cx);
        let (placement, return_focus) = match spec.open_in {
            goatty_config::CommandOpenIn::Tab | goatty_config::CommandOpenIn::Current => (Placement::NewTab, None),
            dir_choice => {
                let dir = match dir_choice {
                    goatty_config::CommandOpenIn::SplitRight => Direction::Right,
                    goatty_config::CommandOpenIn::SplitDown => Direction::Down,
                    goatty_config::CommandOpenIn::SplitLeft => Direction::Left,
                    goatty_config::CommandOpenIn::SplitUp => Direction::Up,
                    _ => Direction::Right,
                };
                if tab.connecting.is_none() && !tab.tree.can_split(focused_id, dir, min) {
                    let toast =
                        Toast::info("Opened in a new tab").body("Not enough room for another split pane here.").key("workspace.command");
                    toast::show_in(window, cx, toast);
                    (Placement::NewTab, None)
                } else {
                    (Placement::Split { tab: tab_id, of: focused_id, dir }, Some(focused_id))
                }
            }
        };
        let spawn_epoch = self.host_epoch(&groups::HostKey::of(&spawner));
        let (size, cwd) = match self.focused_view() {
            Some(v) => {
                let v = v.read(cx);
                let (cols, rows) = v.session().terminal.size();
                (TermSize::new(cols, rows), v.cwd().map(|p| p.to_string_lossy().into_owned()))
            }
            None => (TermSize::new(100, 30), None),
        };
        let label = spec.label;
        let needs_home = spec.needs_home;
        let build_argv = spec.build_argv;
        let task = {
            let spawner = spawner.clone();
            cx.background_executor().spawn(async move {
                let home = if needs_home { spawner.resolve_home() } else { None };
                let argv = build_argv(home.as_deref()).map_err(anyhow::Error::msg)?;
                spawner.spawn_command(size, argv, cwd)
            })
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(session) => this.insert_pane_launched(
                    session,
                    placement,
                    spawner,
                    Some(spawn_epoch),
                    return_focus,
                    Some((label, std::time::Instant::now())),
                    window,
                    cx,
                ),
                Err(e) => {
                    let toast = Toast::error(format!("Couldn't start {label}")).body(format!("{e:#}")).key("workspace.command");
                    toast::show_in(window, cx, toast);
                }
            });
        })
        .detach();
    }

    fn make_pane(&mut self, session: Arc<Session>, window: &mut Window, cx: &mut Context<'_, Self>) -> Pane {
        let id = self.next_id();
        let view = cx.new(|cx| TerminalView::new(session, window, cx));
        let events =
            cx.subscribe_in(&view, window, move |this, _, ev: &TerminalEvent, window, cx| this.on_terminal_event(id, ev, window, cx));
        let focus = view.focus_handle(cx);
        // Focus inside counts too: the output viewer covers the pane.
        let focused = cx.on_focus_in(&focus, window, move |this, window, cx| this.pane_focused(id, window, cx));
        Pane { id, view, seen: 0, return_focus: None, launched: None, _subscriptions: [events, focused] }
    }

    /// Clean up a newly spawned `session` whose target host group or tab was
    /// detached or closed while the spawn was in flight.
    fn discard_spawned_session(&self, session: Arc<Session>, cx: &mut Context<'_, Self>) {
        toast::show(
            cx,
            Toast::info("New session not opened").body("The tab or host it was started for was closed or detached.").key("workspace.spawn"),
        );
        if !session.detach() {
            cx.background_executor().spawn(async move { session.kill() }).detach();
        }
    }

    /// Put `session` (from `spawner`) at `placement`.
    fn insert_pane(
        &mut self,
        session: Arc<Session>,
        placement: Placement,
        spawner: Spawner,
        spawn_epoch: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.insert_pane_launched(session, placement, spawner, spawn_epoch, None, None, window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_pane_launched(
        &mut self,
        session: Arc<Session>,
        placement: Placement,
        spawner: Spawner,
        spawn_epoch: Option<u64>,
        return_focus: Option<PaneId>,
        launched: Option<(String, std::time::Instant)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let host_key = groups::HostKey::of(&spawner);
        if spawn_epoch.is_some_and(|e| self.host_epoch(&host_key) != e) {
            self.discard_spawned_session(session, cx);
            return;
        }
        let min = self.min_pane_size(cx);

        // A split whose source tab still exists but no longer has room (or
        // whose target pane was removed from that tab's split) opens a tab; a
        // split whose source tab was closed or detached is discarded so it
        // cannot reopen a departed tab or host group.
        let target = match placement {
            Placement::Split { tab, of, dir } => {
                let Some(tab_ix) = self.tab_index(tab) else {
                    self.discard_spawned_session(session, cx);
                    return;
                };
                let found = self.tabs[tab_ix].tree.contains(of).then_some(tab_ix);
                let fits = found.filter(|&i| self.tabs[i].tree.can_split(of, dir, min));
                if found.is_some() && fits.is_none() {
                    let toast = Toast::info("Opened in a new tab").body("There was no longer room for another pane there.");
                    toast::show_in(window, cx, toast.key("pane.split"));
                }
                fits.map(|i| (i, of, dir))
            }
            Placement::NewTab => None,
        };
        let mut pane = self.make_pane(session, window, cx);
        pane.return_focus = if target.is_some() { return_focus } else { None };
        pane.launched = launched;
        let id = pane.id;
        let index = match target {
            Some((i, of, dir)) => {
                let tab = &mut self.tabs[i];
                tab.tree.split(of, id, dir);
                tab.panes.push(pane);
                tab.focused = id;
                tab.zoomed = false;
                i
            }
            None => {
                let tab_id = self.next_id();
                let tab = Tab {
                    id: tab_id,
                    tree: Node::Leaf(id),
                    panes: vec![pane],
                    focused: id,
                    zoomed: false,
                    name: None,
                    host: Some(spawner),
                    connecting: None,
                    activity: None,
                };
                // New tabs open after the current one, like browsers.
                let at = if self.tabs.is_empty() { 0 } else { self.active + 1 };
                self.tabs.insert(at, tab);
                at
            }
        };
        self.activate(index, window, cx);
    }

    /// Take pane `id` out (its shell ended or is being ended). The tab goes
    /// with its last pane, the window with its last tab.
    fn remove_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((index, _)) = self.find_pane(id) else { return };
        // The tab's menus would act on a changed tree: a pane's items on
        // whatever pane has focus next, a divider's on a shifted split.
        let tab_id = self.tabs[index].id;
        self.close_menu_of_tab(tab_id, window, cx);
        let tab = &mut self.tabs[index];
        if tab.tree == Node::Leaf(id) {
            self.remove_tab(index, window, cx);
            return;
        }
        let order = tab.tree.leaves();
        let pos = order.iter().position(|&p| p == id).unwrap_or(0);
        let return_focus = tab.pane(id).and_then(|p| p.return_focus);
        tab.tree.remove(id);
        tab.panes.retain(|p| p.id != id);
        self.forget_notices(id, cx);
        let tab = &mut self.tabs[index];
        if self.paste.as_ref().is_some_and(|(_, pane, _)| *pane == id) {
            self.paste = None;
        }
        if self.close.as_ref().is_some_and(|(_, target, _)| *target == CloseTarget::Pane(id)) {
            self.close = None;
        }
        if tab.focused == id {
            let left = tab.tree.leaves();
            tab.focused = return_focus.filter(|rf| left.contains(rf)).unwrap_or_else(|| left[pos.saturating_sub(1).min(left.len() - 1)]);
            tab.zoomed = false;
        }
        if index == self.active {
            self.focus_active(window, cx);
            self.update_window_title(window, cx);
        }
        self.save_layout(cx);
        cx.notify();
    }

    /// Remove the tab at `index` and clean up overlays/notices for it,
    /// adjusting `self.active` without activating or saving the layout yet.
    fn take_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(id) = self.tabs.get(index).map(|t| t.id) {
            self.close_menu_of_tab(id, window, cx);
        }
        let tab = self.tabs.remove(index);
        self.restoring.retain(|(id, _)| *id != tab.id);
        let key = self.host_key(&tab);
        if !self.tabs.iter().any(|t| self.host_key(t) == key) {
            self.bump_host_epoch(&key);
        }
        for pane in &tab.panes {
            self.forget_notices(pane.id, cx);
        }
        if self.rename.as_ref().is_some_and(|r| r.tab == tab.id) {
            self.rename = None;
        }
        if self.paste.as_ref().is_some_and(|(_, pane, _)| tab.pane(*pane).is_some()) {
            self.paste = None;
        }
        if self.close.as_ref().is_some_and(|(_, target, _)| match target {
            CloseTarget::Tab(t) => *t == tab.id,
            CloseTarget::Pane(p) => tab.pane(*p).is_some(),
            // The others still get closed; this one is gone already.
            CloseTarget::Tabs(_) => false,
        }) {
            self.close = None;
        }
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    fn finish_removing_tabs(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.home.as_ref().is_some_and(|h| !self.tabs.iter().any(|t| t.host.as_ref().is_some_and(|th| th.same_host(h)))) {
            self.home = self.tabs.get(self.active).or_else(|| self.tabs.first()).and_then(|t| t.host.clone());
        }
        if self.tabs.is_empty() {
            log::info!("last session ended");
            self.forget_layouts(cx);
            if cx.try_global::<crate::hosts::LastWorkspace>().is_some_and(|w| w.0 == window.window_handle()) {
                crate::settings_window::set_host_link(cx, None);
            }
            // The last window closing quits the app (see `app::run`).
            window.remove_window();
            return;
        }
        self.activate(self.active, window, cx);
    }

    fn remove_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.take_tab(index, window, cx);
        self.finish_removing_tabs(window, cx);
    }

    fn remove_tabs(&mut self, ids: &[u64], window: &mut Window, cx: &mut Context<'_, Self>) {
        let mut removed = false;
        for &id in ids {
            if let Some(index) = self.tab_index(id) {
                self.take_tab(index, window, cx);
                removed = true;
            }
        }
        if removed {
            self.finish_removing_tabs(window, cx);
        }
    }

    /// End the shells of `panes` (off the UI thread: the daemon answers).
    fn kill_sessions(&self, panes: Vec<Arc<Session>>, cx: &mut Context<'_, Self>) -> gpui::Task<()> {
        cx.background_executor().spawn(async move {
            for s in panes {
                s.kill();
            }
        })
    }

    /// Close pane `id`, asking first when a program runs in it.
    fn close_pane_by_id(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((_, pane)) = self.find_pane(id) else { return };
        let busy = busy_programs(std::iter::once(pane), cx);
        if self.should_confirm(&busy, cx) {
            self.confirm_close(CloseTarget::Pane(id), Closing::Pane, busy, window, cx);
        } else {
            self.force_close(CloseTarget::Pane(id), window, cx);
        }
    }

    /// Close the tab at `index`, asking first when a program runs in it.
    fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        let (id, panes) = (tab.id, tab.panes.len());
        let busy = busy_programs(tab.panes.iter(), cx);
        if self.should_confirm(&busy, cx) {
            self.confirm_close(CloseTarget::Tab(id), Closing::Tab { panes }, busy, window, cx);
        } else {
            self.force_close(CloseTarget::Tab(id), window, cx);
        }
    }

    /// Close the tabs `ids` (by id), asking once first when programs run
    /// in them.
    fn close_tabs(&mut self, ids: Vec<u64>, window: &mut Window, cx: &mut Context<'_, Self>) {
        match ids[..] {
            [] => {}
            [id] => {
                if let Some(index) = self.tab_index(id) {
                    self.close_tab_at(index, window, cx);
                }
            }
            _ => {
                let panes = self.tabs.iter().filter(|t| ids.contains(&t.id)).flat_map(|t| t.panes.iter());
                let busy = busy_programs(panes, cx);
                let closing = Closing::Tabs { tabs: ids.len() };
                if self.should_confirm(&busy, cx) {
                    self.confirm_close(CloseTarget::Tabs(ids), closing, busy, window, cx);
                } else {
                    self.force_close(CloseTarget::Tabs(ids), window, cx);
                }
            }
        }
    }

    fn should_confirm(&self, busy: &[String], cx: &gpui::App) -> bool {
        !busy.is_empty() && cx.global::<AppSettings>().client.panes.confirm_close
    }

    /// End the shells of `target` and take it out.
    fn force_close(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<'_, Self>) {
        let session_of = |p: &Pane| p.view.read(cx).session().clone();
        let (sessions, last): (Vec<_>, bool) = match &target {
            CloseTarget::Pane(id) => {
                let Some((index, pane)) = self.find_pane(*id) else { return };
                (vec![session_of(pane)], self.tabs.len() == 1 && self.tabs[index].panes.len() == 1)
            }
            CloseTarget::Tab(id) => {
                let Some(index) = self.tab_index(*id) else { return };
                (self.tabs[index].panes.iter().map(session_of).collect(), self.tabs.len() == 1)
            }
            CloseTarget::Tabs(ids) => {
                let closing = self.tabs.iter().filter(|t| ids.contains(&t.id));
                (closing.flat_map(|t| t.panes.iter()).map(session_of).collect(), self.tabs.iter().all(|t| ids.contains(&t.id)))
            }
        };
        let kill = self.kill_sessions(sessions, cx);
        if !last {
            kill.detach();
            self.remove_target(target, window, cx);
            return;
        }
        // Closing the window quits the app: let goattyd hear about it first
        // (the layout goes before the kills, whose replies prove both arrived).
        self.forget_layouts(cx);
        cx.spawn_in(window, async move |this, cx| {
            kill.await;
            let _ = this.update_in(cx, |this, window, cx| this.remove_target(target, window, cx));
        })
        .detach();
    }

    fn remove_target(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<'_, Self>) {
        match target {
            CloseTarget::Pane(id) => self.remove_pane(id, window, cx),
            CloseTarget::Tab(id) => {
                if let Some(index) = self.tab_index(id) {
                    self.remove_tab(index, window, cx);
                }
            }
            CloseTarget::Tabs(ids) => self.remove_tabs(&ids, window, cx),
        }
    }

    // ---- detaching panes, tabs and hosts ------------------------------------

    /// Why pane `id` cannot be detached, or `None` when it can.
    fn cannot_detach_pane(&self, id: PaneId, cx: &gpui::App) -> Option<&'static str> {
        let (_, pane) = self.find_pane(id)?;
        (!pane.view.read(cx).session().can_detach()).then_some("No goattyd (--local)")
    }

    /// Why the tab at `index` cannot be detached, or `None` when it can.
    fn cannot_detach_tab(&self, index: usize, cx: &gpui::App) -> Option<&'static str> {
        let tab = self.tabs.get(index)?;
        if tab.connecting.is_some() {
            return Some("Still connecting");
        }
        if tab.panes.iter().any(|p| p.view.read(cx).session().can_detach()) { None } else { Some("No goattyd (--local)") }
    }

    /// Why none of the tabs in `ids` can be detached, or `None` when at least
    /// one can.
    fn cannot_detach_tabs(&self, ids: &[u64], cx: &gpui::App) -> Option<&'static str> {
        let mut any_connecting = false;
        for (i, t) in self.tabs.iter().enumerate().filter(|(_, t)| ids.contains(&t.id)) {
            self.cannot_detach_tab(i, cx)?;
            any_connecting |= t.connecting.is_some();
        }
        if any_connecting { Some("Still connecting") } else { Some("No goattyd (--local)") }
    }

    fn tab_all_panes_detachable(&self, index: usize, cx: &gpui::App) -> bool {
        let Some(tab) = self.tabs.get(index) else { return false };
        tab.connecting.is_none() && !tab.panes.is_empty() && tab.panes.iter().all(|p| p.view.read(cx).session().can_detach())
    }

    fn can_detach_target(&self, target: &CloseTarget, cx: &gpui::App) -> bool {
        match target {
            CloseTarget::Pane(id) => self.cannot_detach_pane(*id, cx).is_none(),
            CloseTarget::Tab(id) => self.tab_index(*id).is_some_and(|i| self.tab_all_panes_detachable(i, cx)),
            CloseTarget::Tabs(ids) => {
                !ids.is_empty() && ids.iter().all(|id| self.tab_index(*id).is_some_and(|i| self.tab_all_panes_detachable(i, cx)))
            }
        }
    }

    /// Serialize the subset of `self.tabs` matching `include` into a
    /// [`SavedWorkspace`] `(sessions, json)`.
    fn serialize_tabs_subset(&self, include: impl Fn(&Tab) -> bool, cx: &gpui::App) -> Option<(Vec<goatty_proto::SessionId>, String)> {
        let mut sessions = Vec::new();
        let mut tabs = Vec::new();
        let mut before_active = 0;
        let mut focus = None;
        for (i, t) in self.tabs.iter().enumerate() {
            if !include(t) {
                continue;
            }
            let Some(tab) = saved_tab(t, cx) else { continue };
            sessions.extend(tab.tree.leaves());
            tabs.push(tab);
            if i < self.active {
                before_active += 1;
            }
            // As in `save_layout`: the active tab, else its host's last active one.
            if i == self.active || (focus.is_none() && self.group_focus.iter().any(|(_, id)| *id == t.id)) {
                focus = Some(tabs.len() - 1);
            }
        }
        if tabs.is_empty() {
            return None;
        }
        let active = focus.unwrap_or(before_active).min(tabs.len() - 1);
        let saved = SavedWorkspace { version: saved::VERSION, tabs, active, window: Some(self.window) };
        let json = serde_json::to_string(&saved).ok()?;
        Some((sessions, json))
    }

    /// Keep the tabs `leaving_tabs` (being detached) as a saved layout on
    /// each goattyd they're on, released so any window can restore them all
    /// at once with their splits and names (the sessions picker,
    /// `reattach_detached`, the next goatty). On a connection none of this
    /// window's other tabs use, that's the window's own layout there (its
    /// `HostLayout` goes, so `save_layout`/`forget_layouts` won't wipe it);
    /// otherwise a new one, and the window's layout loses those tabs with the
    /// next save.
    fn detach_host_layouts_for(&mut self, leaving_tabs: &[u64], cx: &mut Context<'_, Self>) {
        let mut conns: Vec<Arc<Connection>> = Vec::new();
        for t in self.tabs.iter().filter(|t| leaving_tabs.contains(&t.id)) {
            if let Some(conn) = t.host.as_ref().and_then(Spawner::connection)
                && !conns.iter().any(|c| Arc::ptr_eq(c, conn))
            {
                conns.push(conn.clone());
            }
        }
        for conn in conns {
            let on_conn = |t: &Tab| t.host.as_ref().and_then(Spawner::connection).is_some_and(|c| Arc::ptr_eq(c, &conn));
            let Some((sessions, json)) = self.serialize_tabs_subset(|t| leaving_tabs.contains(&t.id) && on_conn(t), cx) else { continue };
            let survivors = self.tabs.iter().any(|t| !leaving_tabs.contains(&t.id) && on_conn(t));
            let own = if survivors { None } else { self.layouts.iter().position(|l| Arc::ptr_eq(&l.conn, &conn)) };
            match own {
                Some(pos) => {
                    let layout = self.layouts.remove(pos);
                    conn.detach_layout(layout.id, sessions, json);
                }
                None => {
                    let id = saved::new_id();
                    conn.save_layout(id, sessions.clone(), json.clone());
                    conn.detach_layout(id, sessions, json);
                }
            }
        }
    }

    /// Unsubscribe the sessions of `leaving_panes` on their `goattyd` (unless
    /// another remaining pane in this window still shares the same session, or
    /// another window on the pooled connection took over the subscription).
    fn detach_panes_by_id(&self, leaving_panes: &[PaneId], cx: &gpui::App) {
        let mut sessions = Vec::new();
        for t in &self.tabs {
            for p in &t.panes {
                if leaving_panes.contains(&p.id) {
                    sessions.push(p.view.read(cx).session().clone());
                }
            }
        }
        for s in sessions {
            let (Some(conn), Some(sid)) = (s.connection(), s.daemon_id()) else { continue };
            let still_shown_here = self.tabs.iter().flat_map(|t| t.panes.iter()).any(|p| {
                if leaving_panes.contains(&p.id) {
                    return false;
                }
                let other = p.view.read(cx).session();
                other.daemon_id() == Some(sid) && other.connection().is_some_and(|c| Arc::ptr_eq(c, conn))
            });
            if !still_shown_here {
                s.detach();
            }
        }
    }

    /// Detach pane `id` without ending its shell.
    fn detach_pane_by_id(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((index, pane)) = self.find_pane(id) else { return };
        if !pane.view.read(cx).session().can_detach() {
            let toast = Toast::info("Can't detach a local shell")
                .body("This shell isn't running in goattyd (--local), so it can't keep running without the window.")
                .key("workspace.detach");
            toast::show_in(window, cx, toast);
            return;
        }
        let label = pane_label(pane.view.read(cx));
        let undo = self.detached_pane_snapshot(index, id, cx);
        self.note_detached(&undo);
        let only_pane = self.tabs[index].tree == Node::Leaf(id);
        if !only_pane {
            // A pane out of a split: kept as a tab of its own, restorable like
            // detached tabs (the tab it leaves keeps its own layout).
            for (conn, tabs) in &undo {
                let sessions: Vec<goatty_proto::SessionId> = tabs.iter().flat_map(|t| t.tree.leaves()).collect();
                let saved = SavedWorkspace { version: saved::VERSION, tabs: tabs.clone(), active: 0, window: Some(self.window) };
                if let Ok(json) = serde_json::to_string(&saved) {
                    conn.detach_layout(saved::new_id(), sessions, json);
                }
            }
        }
        if only_pane {
            self.save_layout(cx);
        }
        self.detach_panes_by_id(&[id], cx);
        if only_pane {
            let tab_id = self.tabs[index].id;
            self.detach_host_layouts_for(&[tab_id], cx);
        }
        self.remove_pane(id, window, cx);
        if !self.tabs.is_empty() {
            let toast =
                Toast::info(format!("Detached {label}")).body("Its session keeps running in the background.").key("workspace.detach");
            toast::show_in(window, cx, with_undo(toast, undo));
        }
    }

    /// Tabs `ids` as they are, per goattyd, to undo detaching them.
    fn detached_snapshot(&self, ids: &[u64], cx: &gpui::App) -> Detached {
        let mut out: Detached = Vec::new();
        for t in self.tabs.iter().filter(|t| ids.contains(&t.id)) {
            let Some(conn) = t.host.as_ref().and_then(Spawner::connection) else { continue };
            let Some(tab) = saved_tab(t, cx) else { continue };
            match out.iter_mut().find(|(c, _)| Arc::ptr_eq(c, conn)) {
                Some((_, tabs)) => tabs.push(tab),
                None => out.push((conn.clone(), vec![tab])),
            }
        }
        out
    }

    /// Pane `pane` of the tab at `index` as a tab of its own, to undo detaching it.
    fn detached_pane_snapshot(&self, index: usize, pane: PaneId, cx: &gpui::App) -> Detached {
        let t = &self.tabs[index];
        let (Some(conn), Some(session)) =
            (t.host.as_ref().and_then(Spawner::connection), t.pane(pane).and_then(|p| p.view.read(cx).session().daemon_id()))
        else {
            return Vec::new();
        };
        let plain = match &t.host {
            Some(Spawner::Plain { host, .. }) => Some(host.clone()),
            _ => None,
        };
        let name = (t.tree == Node::Leaf(pane)).then(|| t.name.clone()).flatten();
        vec![(conn.clone(), vec![SavedTab { name, tree: Node::Leaf(session), focused: session, zoomed: false, plain }])]
    }

    /// Undo a detach: reattach those tabs, at the end, off the UI thread.
    pub(super) fn undo_detach(&mut self, detached: Detached, window: &mut Window, cx: &mut Context<'_, Self>) {
        let size =
            self.focused_view().map(|v| v.read(cx).session().terminal.size()).map_or(TermSize::new(100, 30), |(c, r)| TermSize::new(c, r));
        for (conn, tabs) in detached {
            let task = cx.background_executor().spawn({
                let conn = conn.clone();
                async move { saved::reattach(&conn, size, tabs) }
            });
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| match result {
                    Ok(Some(r)) => {
                        let at = this.tabs.len();
                        let active = this.insert_restored(r, &Spawner::Daemon(conn), at, window, cx);
                        this.activate(active, window, cx);
                    }
                    Ok(None) => {
                        toast::show_in(
                            window,
                            cx,
                            Toast::info("Nothing to reattach").body("Those shells have ended.").key("workspace.detach"),
                        );
                    }
                    Err(e) => {
                        toast::show_in(window, cx, Toast::error("Couldn't reattach").body(format!("{e:#}")).key("workspace.detach"));
                    }
                });
            })
            .detach();
        }
    }

    /// Detach the tab at `index` without ending its shells.
    fn detach_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        if tab.connecting.is_some() {
            let toast = Toast::info("Still connecting").body("This tab doesn't have a session to detach from yet.").key("workspace.detach");
            toast::show_in(window, cx, toast);
            return;
        }
        self.detach_tabs(vec![tab.id], window, cx);
    }

    /// Detach the tabs `ids` (by id) without ending their shells.
    fn detach_tabs(&mut self, ids: Vec<u64>, window: &mut Window, cx: &mut Context<'_, Self>) {
        if ids.is_empty() {
            return;
        }
        let mut detachable_ids = Vec::new();
        let mut pane_ids = Vec::new();
        let mut single_label = None;
        let mut any_connecting = false;
        for (i, t) in self.tabs.iter().enumerate().filter(|(_, t)| ids.contains(&t.id)) {
            if t.connecting.is_some() {
                any_connecting = true;
                continue;
            }
            let tab_panes: Vec<PaneId> = t.panes.iter().filter(|p| p.view.read(cx).session().can_detach()).map(|p| p.id).collect();
            if !tab_panes.is_empty() {
                if detachable_ids.is_empty() {
                    single_label = Some(self.tab_label(t, cx));
                }
                detachable_ids.push(self.tabs[i].id);
                pane_ids.extend(tab_panes);
            }
        }
        if detachable_ids.is_empty() {
            let toast = if any_connecting {
                Toast::info("Still connecting").body("This tab doesn't have a session to detach from yet.")
            } else {
                Toast::info("Can't detach a local shell")
                    .body("This shell isn't running in goattyd (--local), so it can't keep running without the window.")
            };
            toast::show_in(window, cx, toast.key("workspace.detach"));
            return;
        }
        let n_tabs = detachable_ids.len();
        let n_sessions = pane_ids.len();
        let undo = self.detached_snapshot(&detachable_ids, cx);
        self.note_detached(&undo);
        self.save_layout(cx);
        self.detach_panes_by_id(&pane_ids, cx);
        self.detach_host_layouts_for(&detachable_ids, cx);
        self.remove_tabs(&detachable_ids, window, cx);
        if !self.tabs.is_empty() {
            let title = match (n_tabs, single_label) {
                (1, Some(label)) => format!("Detached {label}"),
                (n, _) => format!("Detached {n} tabs"),
            };
            let body = if n_sessions == 1 {
                "Its session keeps running in the background.".to_string()
            } else {
                format!("Their {n_sessions} sessions keep running in the background.")
            };
            toast::show_in(window, cx, with_undo(Toast::info(title).body(body).key("workspace.detach"), undo));
        }
    }

    /// Detach all tabs of host group `key` without ending their shells.
    fn detach_group(&mut self, key: &groups::HostKey, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(g) = self.groups(cx).into_iter().find(|g| g.key == *key) else { return };
        if let Some(why) = self.cannot_detach_tabs(&g.ids, cx) {
            let toast = if matches!(key, groups::HostKey::Connecting(_)) {
                Toast::info("Still connecting").body("This tab doesn't have a session to detach from yet.")
            } else {
                Toast::info(format!("Can't detach from {}", g.label)).body(why)
            };
            toast::show_in(window, cx, toast.key("workspace.detach"));
            return;
        }
        let label = g.label.to_string();
        let detachable_ids: Vec<u64> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| g.ids.contains(&t.id) && self.cannot_detach_tab(*i, cx).is_none())
            .map(|(_, t)| t.id)
            .collect();
        let pane_ids: Vec<PaneId> = self
            .tabs
            .iter()
            .filter(|t| detachable_ids.contains(&t.id))
            .flat_map(|t| t.panes.iter())
            .filter(|p| p.view.read(cx).session().can_detach())
            .map(|p| p.id)
            .collect();
        let n_sessions = pane_ids.len();
        let undo = self.detached_snapshot(&detachable_ids, cx);
        self.note_detached(&undo);
        self.save_layout(cx);
        self.detach_panes_by_id(&pane_ids, cx);
        self.detach_host_layouts_for(&detachable_ids, cx);
        self.remove_tabs(&detachable_ids, window, cx);
        if !self.tabs.is_empty() {
            let body = if n_sessions == 1 {
                "Its session keeps running in the background.".to_string()
            } else {
                format!("Its {n_sessions} sessions keep running in the background.")
            };
            let toast = Toast::info(format!("Detached from {label}")).body(body).key("workspace.detach");
            toast::show_in(window, cx, with_undo(toast, undo));
        }
    }

    fn detach_target(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<'_, Self>) {
        match target {
            CloseTarget::Pane(id) => self.detach_pane_by_id(id, window, cx),
            CloseTarget::Tab(id) => {
                if let Some(index) = self.tab_index(id) {
                    self.detach_tab_at(index, window, cx);
                }
            }
            CloseTarget::Tabs(ids) => self.detach_tabs(ids, window, cx),
        }
    }

    /// "Tabs: Detach from a host…": pick which host in this window to detach from.
    pub fn ask_detach_host(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let groups: Vec<groups::Group> = self.groups(cx).into_iter().filter(|g| self.cannot_detach_tabs(&g.ids, cx).is_none()).collect();
        if groups.is_empty() {
            let any_connecting = self.tabs.iter().any(|t| t.connecting.is_some());
            let toast = if any_connecting {
                Toast::info("Still connecting").body("No connected host to detach from yet.")
            } else {
                Toast::info("Can't detach a local shell")
                    .body("This window's shells aren't running in goattyd (--local), so they can't keep running without the window.")
            };
            toast::show_in(window, cx, toast.key("workspace.detach"));
            return;
        }
        // When the active tab is on this computer and another host is open,
        // pre-select the other host so Enter detaches that server right away.
        let default_ix = groups
            .iter()
            .position(|g| g.active && !g.local)
            .or_else(|| groups.iter().position(|g| !g.local))
            .or_else(|| groups.iter().position(|g| g.active))
            .unwrap_or(0);
        let default_id = default_ix.to_string();
        let keys: Vec<groups::HostKey> = groups.iter().map(|g| g.key.clone()).collect();
        let choices: Vec<(String, String, String)> = groups
            .into_iter()
            .enumerate()
            .map(|(i, g)| {
                let n = g.tabs.len();
                let tabs = if n == 1 { "1 tab".to_string() } else { format!("{n} tabs") };
                let help = match g.detail {
                    Some(d) => format!("{d} · {tabs} (sessions keep running)"),
                    None => format!("{tabs} (sessions keep running)"),
                };
                (i.to_string(), g.label.to_string(), help)
            })
            .collect();
        let mut field = Field::choice("host", "Which host?", choices);
        field.spec.default = Some(default_id);
        let me = cx.entity().downgrade();
        let ask = Ask::new("Detach from a host", vec![field], move |answers, window, cx| {
            let Some(ix) = answers.get("host").and_then(|v| v.parse::<usize>().ok()) else { return };
            let Some(key) = keys.get(ix).cloned() else { return };
            let _ = me.update(cx, |this, cx| this.detach_group(&key, window, cx));
        });
        self.open_ask(ask, window, cx);
    }

    fn on_terminal_event(&mut self, id: PaneId, ev: &TerminalEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        match ev {
            TerminalEvent::TitleChanged => {
                if self.focused_view().is_some_and(|v| self.find_pane(id).is_some_and(|(_, p)| p.view == *v)) {
                    self.update_window_title(window, cx);
                }
                cx.notify();
            }
            TerminalEvent::Exited => {
                if let Some((_, pane)) = self.find_pane(id)
                    && let Some((label, started)) = pane.launched.clone()
                    && started.elapsed() < std::time::Duration::from_secs(2)
                {
                    let session = pane.view.read(cx).session();
                    if let Some(code) = session.exit_code()
                        && code != 0
                    {
                        let body = match last_non_empty_line(&session.terminal) {
                            Some(line) => format!("Exited with status {code}: {line}"),
                            None => format!("Exited immediately with status {code}."),
                        };
                        let toast = Toast::warning(format!("{label} exited")).body(body).key("workspace.command.exit");
                        toast::show_in(window, cx, toast);
                    }
                }
                self.remove_pane(id, window, cx);
            }
            TerminalEvent::ConfirmPaste(request) => self.confirm_paste(id, request.clone(), window, cx),
            TerminalEvent::Activity(activity) => self.mark_activity(id, *activity, cx),
            TerminalEvent::ContextMenu { at, keyboard } => self.open_pane_menu(id, *at, *keyboard, window, cx),
            TerminalEvent::StatusChanged => self.pane_status_changed(id, cx),
            TerminalEvent::Notice(notice) => self.notice(id, notice.clone(), window, cx),
        }
    }

    /// Mark the pane's tab if it's in the background and the settings
    /// want this kind of activity shown. A stronger mark (a bell over a
    /// finished command over output) replaces a weaker one.
    fn mark_activity(&mut self, pane: PaneId, activity: Activity, cx: &mut Context<'_, Self>) {
        let Some((index, _)) = self.find_pane(pane) else { return };
        if index == self.active || !marks(&cx.global::<AppSettings>().client.tabs, activity) {
            return;
        }
        let tab = &mut self.tabs[index];
        if tab.activity < Some(activity) {
            tab.activity = Some(activity);
            cx.notify();
        }
    }

    // ---- moving tabs ----------------------------------------------------------

    /// Put tab `id` at position `to`.
    fn move_tab_to(&mut self, id: u64, to: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(from) = self.tab_index(id) else { return };
        let to = to.min(self.tabs.len() - 1);
        if from == to {
            return;
        }
        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self.tab_index(active_id).unwrap_or(0);
        self.activate(self.active, window, cx);
    }

    fn move_active_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        let to = (self.active as isize + delta).clamp(0, self.tabs.len() as isize - 1) as usize;
        self.move_tab_to(tab.id, to, window, cx);
    }

    // ---- renaming ---------------------------------------------------------------

    fn start_rename(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        // A double-click inside the field selects a word; it doesn't start over.
        if self.rename.as_ref().is_some_and(|r| r.tab == tab.id) {
            return;
        }
        let (tab_id, label) = (tab.id, self.tab_label(tab, cx));
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Tab name (empty: automatic)", label, cx);
            input.select_all_text(cx);
            input
        });
        let submitted = cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, window, cx| {
            if let TextInputEvent::Submitted(_) = ev {
                this.finish_rename(true, window, cx);
            }
        });
        let focus = input.focus_handle(cx);
        // Clicking elsewhere keeps what was typed.
        let blurred = cx.on_blur(&focus, window, |this, window, cx| this.finish_rename(true, window, cx));
        focus.focus(window, cx);
        self.rename = Some(Rename { tab: tab_id, input, _subscriptions: [submitted, blurred] });
        cx.notify();
    }

    fn finish_rename(&mut self, keep: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(rename) = self.rename.take() else { return };
        if keep && let Some(i) = self.tab_index(rename.tab) {
            let text = rename.input.read(cx).text().trim().to_string();
            self.tabs[i].name = (!text.is_empty()).then_some(text);
        }
        self.focus_active(window, cx);
        self.update_window_title(window, cx);
        self.save_layout(cx);
        cx.notify();
    }

    fn on_rename_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        if ev.keystroke.key == "escape" {
            self.finish_rename(false, window, cx);
            cx.stop_propagation();
        }
    }

    // ---- actions: tabs --------------------------------------------------------

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.spawn(Placement::NewTab, window, cx);
    }

    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(tab) = self.active_tab() {
            self.close_pane_by_id(tab.focused, window, cx);
        }
    }

    fn detach_pane(&mut self, _: &DetachPane, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        if tab.connecting.is_some() {
            let toast = Toast::info("Still connecting").body("This tab doesn't have a session to detach from yet.").key("workspace.detach");
            toast::show_in(window, cx, toast);
            return;
        }
        self.detach_pane_by_id(tab.focused, window, cx);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_tab_at(self.active, window, cx);
    }

    fn detach_tab(&mut self, _: &DetachTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.detach_tab_at(self.active, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.cycle_tab(true, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.cycle_tab(false, window, cx);
    }

    /// The next (or previous) tab, coming round at the ends and skipping
    /// folded host groups.
    fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let hidden: Vec<bool> = (0..self.tabs.len()).map(|i| self.hidden(i, cx)).collect();
        if let Some(to) = groups::cycle_visible(self.active, forward, &hidden) {
            self.activate(to, window, cx);
        }
    }

    fn move_tab_left(&mut self, _: &MoveTabLeft, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.move_active_tab(-1, window, cx);
    }

    fn move_tab_right(&mut self, _: &MoveTabRight, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.move_active_tab(1, window, cx);
    }

    /// Alt+digit: tab `index` if it exists; otherwise the key goes to the
    /// shell (Alt+digit is a zsh digit argument), as it does with one tab.
    fn select_tab(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<'_, Self>) {
        match index.filter(|&i| i < self.tabs.len() && self.tabs.len() > 1) {
            Some(i) => self.activate(i, window, cx),
            None => cx.propagate(),
        }
    }

    fn rename_tab(&mut self, _: &RenameTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.start_rename(self.active, window, cx);
    }

    /// A new tab next to tab `index`, on its host and in its focused pane's
    /// directory.
    fn duplicate_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if index < self.tabs.len() {
            self.activate(index, window, cx);
            self.spawn(Placement::NewTab, window, cx);
        }
    }

    fn duplicate_tab(&mut self, _: &DuplicateTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.duplicate_tab_at(self.active, window, cx);
    }

    /// The ids of every tab but the one at `index`.
    fn other_tabs(&self, index: usize) -> Vec<u64> {
        self.tabs.iter().enumerate().filter(|&(i, _)| i != index).map(|(_, t)| t.id).collect()
    }

    fn close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_tabs(self.other_tabs(self.active), window, cx);
    }

    fn detach_other_tabs(&mut self, _: &DetachOtherTabs, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.tabs.len() < 2 {
            let toast = Toast::info("No other tabs to detach").body("This is the only tab in the window.").key("workspace.detach");
            toast::show_in(window, cx, toast);
            return;
        }
        self.detach_tabs(self.other_tabs(self.active), window, cx);
    }

    fn detach_host(&mut self, _: &DetachHost, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        let key = self.host_key(tab);
        self.detach_group(&key, window, cx);
    }

    /// Ctrl+Shift+R: like a browser's "reopen closed tab", the host
    /// detached from most recently in this window first, then the active
    /// tab's host.
    fn reattach_detached(&mut self, _: &ReattachDetached, window: &mut Window, cx: &mut Context<'_, Self>) {
        let mut candidates: Vec<(HostRef, Option<String>)> = self.recently_detached.iter().rev().cloned().collect();
        let here = self.active_tab().and_then(|t| t.host.as_ref()).and_then(|h| {
            let plain = match h {
                Spawner::Plain { host, .. } => Some(host.clone()),
                _ => None,
            };
            host_ref_of(h).map(|r| (r, plain))
        });
        if let Some(here) = here
            && !candidates.contains(&here)
        {
            candidates.push(here);
        }
        if candidates.is_empty() {
            let body = if self.active_tab().is_some_and(|t| t.host.is_none()) {
                "Try again once this tab is connected."
            } else {
                "This tab's shell isn't kept by a goattyd."
            };
            toast::show_in(window, cx, Toast::info("Nothing to reattach here").body(body).key("reattach"));
            return;
        }
        self.reattach_detached_from(candidates, window, cx);
    }

    /// Remember that tabs on these hosts were just detached (for Ctrl+Shift+R).
    fn note_detached(&mut self, detached: &Detached) {
        for (conn, tabs) in detached {
            let Some(host) = host_ref_of(&Spawner::Daemon(conn.clone())) else { continue };
            let mut plains: Vec<Option<String>> = tabs.iter().map(|t| t.plain.clone()).collect();
            plains.dedup();
            for plain in plains {
                let entry = (host.clone(), plain);
                self.recently_detached.retain(|e| *e != entry);
                self.recently_detached.push(entry);
            }
        }
        const KEEP: usize = 8;
        let extra = self.recently_detached.len().saturating_sub(KEEP);
        self.recently_detached.drain(..extra);
    }

    /// Bring back what was detached last on `host` (#252): the newest saved
    /// layout no window holds (a detached host, tabs or a closed window), all
    /// its tabs at once; else the newest detached session. `plain`: only
    /// tabs of that plain SSH host (whose sessions live on `host`). Looked up
    /// off the UI thread; a note when there's nothing.
    pub(super) fn reattach_detached_on(&mut self, host: HostRef, plain: Option<String>, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.reattach_detached_from(vec![(host, plain)], window, cx);
    }

    /// [`Self::reattach_detached_on`] for the first of `candidates` that has
    /// something detached; the ones that had nothing are forgotten. The
    /// note for "nothing" names the last candidate (the active tab's host).
    fn reattach_detached_from(&mut self, candidates: Vec<(HostRef, Option<String>)>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((host, plain)) = candidates.last().cloned() else { return };
        let task = cx.background_executor().spawn(async move {
            let count = candidates.len();
            let mut misses = Vec::new();
            for (n, (host, wanted)) in candidates.into_iter().enumerate() {
                let conn = match crate::hosts::pool::open(host.endpoint()) {
                    Ok(c) => c,
                    // Unreachable now: try the next one; the last one says why.
                    Err(e) if n + 1 < count => {
                        log::info!("reattach: {}: {e:#}", host.label());
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let lookup = || -> anyhow::Result<Option<AttachMode>> {
                    let sessions = conn.list_sessions()?;
                    let layouts = saved::live_layouts(conn.list_layouts()?, &sessions);
                    Ok(match &wanted {
                        // Only that host's tabs, even out of a layout with others.
                        Some(h) => match layouts.iter().find(|l| saved::has_plain_tab(l, h)) {
                            Some(l) => saved::split_plain(&conn, l, h)?.map(AttachMode::Layout),
                            None => None,
                        },
                        None => match layouts.first() {
                            Some(l) => Some(AttachMode::Layout(l.id)),
                            None => saved::loose_detached(&sessions, &layouts).last().map(|&id| AttachMode::Id(id)),
                        },
                    })
                };
                let mode = match lookup() {
                    Ok(mode) => mode,
                    // Its link is down: try the next one; the last one says why.
                    Err(e) if n + 1 < count => {
                        log::info!("reattach: {}: {e:#}", host.label());
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                match mode {
                    Some(mode) => return anyhow::Ok((Some((conn, mode)), misses)),
                    None => misses.push((host, wanted)),
                }
            }
            anyhow::Ok((None, misses))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok((found, misses)) => {
                    this.recently_detached.retain(|e| !misses.contains(e));
                    match found {
                        Some((conn, mode)) => {
                            let plan = ConnectPlan {
                                endpoint: conn.endpoint().clone(),
                                mode,
                                open_settings: None,
                                plain: false,
                                conn: Some(conn),
                            };
                            this.open_connect_tab(plan, window, cx);
                        }
                        None => {
                            let label = plain.clone().unwrap_or_else(|| host.label().to_string());
                            let toast = Toast::info(format!("Nothing detached on {label}"))
                                .body("Every shell there is open in a window.")
                                .action("Sessions…", move |window, cx| {
                                    let host = host.clone();
                                    if let Some(Some(ws)) = window.root::<Workspace>() {
                                        ws.update(cx, |ws, cx| ws.open_palette(Scope::Sessions(host), window, cx));
                                    }
                                })
                                .key("reattach");
                            toast::show_in(window, cx, toast);
                        }
                    }
                }
                Err(e) => {
                    let toast = Toast::error("Couldn't reattach").body(format!("{e:#}")).key("reattach");
                    toast::show_in(window, cx, toast);
                }
            });
        })
        .detach();
    }

    /// A new window with a new shell on the active tab's host (this
    /// computer while it connects).
    fn new_window(&mut self, _: &NewWindow, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(spawner) = self.active_tab().and_then(|t| t.host.clone()).or_else(|| self.home.clone()) else {
            // Nothing connected yet: this computer.
            let plan = ConnectPlan {
                endpoint: goatty_client::Endpoint::Local,
                mode: AttachMode::New,
                open_settings: None,
                plain: false,
                conn: None,
            };
            cx.defer(move |cx| crate::app::open_terminal_window(cx, Initial::Connect(plan), None));
            return;
        };
        // The shell starts where the focused pane is, like a new tab.
        let (size, cwd) = match self.focused_view() {
            Some(v) => {
                let v = v.read(cx);
                let (cols, rows) = v.session().terminal.size();
                (TermSize::new(cols, rows), v.cwd().map(|p| p.to_string_lossy().into_owned()))
            }
            None => (TermSize::new(100, 30), None),
        };
        let task = cx.background_executor().spawn({
            let spawner = spawner.clone();
            async move { spawner.spawn(size, cwd) }
        });
        // Not tied to this window: it may close meanwhile.
        cx.spawn(async move |_, cx| {
            let result = task.await;
            cx.update(|cx| match result {
                Ok(session) => crate::app::open_terminal_window(cx, Initial::Ready { startup: Startup::Session(session), spawner }, None),
                Err(e) => {
                    toast::show(cx, Toast::error("Couldn't open a window").body(format!("{e:#}")).key("workspace.spawn"));
                }
            });
        })
        .detach();
    }

    /// A new tab on `host` in this window: in its group when the window has
    /// one (`plain`: its plain SSH group), else connecting there.
    pub(super) fn new_tab_on_host(&mut self, host: HostRef, plain: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let existing = self.tabs.iter().find(|t| match (&t.host, &host) {
            (Some(Spawner::Plain { host: h, .. }), HostRef::Ssh(want)) => plain && h == want,
            (Some(s @ Spawner::Daemon(_)), _) => !plain && host_ref_of(s).as_ref() == Some(&host),
            _ => false,
        });
        if let Some(t) = existing {
            let key = self.host_key(t);
            self.new_tab_in_group(&key, window, cx);
            return;
        }
        let plan = ConnectPlan { endpoint: host.endpoint(), mode: AttachMode::New, open_settings: None, plain, conn: None };
        self.open_connect_tab(plan, window, cx);
    }

    /// Ctrl+Shift+B: fold or unfold the vertical sidebar. With horizontal
    /// tabs the key goes on to the shell.
    fn toggle_tab_sidebar(&mut self, _: &ToggleTabSidebar, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if cx.global::<AppSettings>().client.tabs.style != TabStyle::Vertical {
            cx.propagate();
            return;
        }
        self.sidebar_drag = None;
        self.save_sidebar = None;
        AppSettings::update(cx, |s| s.tabs.sidebar_collapsed = !s.tabs.sidebar_collapsed);
    }

    fn toggle_vertical_tabs(&mut self, _: &ToggleVerticalTabs, _window: &mut Window, cx: &mut Context<'_, Self>) {
        AppSettings::update(cx, |s| {
            s.tabs.style = if s.tabs.style == TabStyle::Vertical { TabStyle::Modern } else { TabStyle::Vertical };
        });
    }

    /// Move `steps` tabs forward (negative: back), stopping at the ends.
    fn step_tab(&mut self, steps: isize, window: &mut Window, cx: &mut Context<'_, Self>) {
        // Folded host groups are skipped.
        let hidden: Vec<bool> = (0..self.tabs.len()).map(|i| self.hidden(i, cx)).collect();
        let to = groups::step_visible(self.active, steps, &hidden);
        if to != self.active {
            self.activate(to, window, cx);
        }
    }

    // ---- actions: panes ---------------------------------------------------------

    fn split(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<'_, Self>) {
        let min = self.min_pane_size(cx);
        let Some(tab) = self.active_tab() else { return };
        // A zoomed pane fills the tab, but splitting it goes back to the split.
        if tab.connecting.is_none() && !tab.tree.can_split(tab.focused, dir, min) {
            let way = if dir.axis() == Axis::Horizontal { "side by side" } else { "on top of each other" };
            let toast = Toast::info("Not enough room for another pane")
                .body(format!("Two panes {way} wouldn't fit here. Make the window bigger, close a pane, or split the other way."))
                .key("pane.split");
            toast::show_in(window, cx, toast);
            return;
        }
        self.spawn(Placement::Split { tab: tab.id, of: tab.focused, dir }, window, cx);
    }

    /// Alt+arrow: the neighbouring pane, or the key goes to the shell when
    /// there is none (so a single pane loses no shortcut).
    fn focus_pane(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        match (!tab.zoomed).then(|| tab.tree.neighbor(tab.focused, dir)).flatten() {
            Some(id) => {
                if let Some(p) = tab.pane(id) {
                    p.view.focus_handle(cx).focus(window, cx);
                }
            }
            None => cx.propagate(),
        }
    }

    fn resize_pane(&mut self, dir: Direction, cx: &mut Context<'_, Self>) {
        let (step, min) = (self.resize_step(dir, cx), self.min_pane_size(cx));
        let Some(tab) = self.active_tab_mut() else { return };
        let focused = tab.focused;
        if tab.zoomed || !tab.tree.resize(focused, dir, step, min) {
            cx.propagate();
            return;
        }
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_pane_zoom(&mut self, _: &TogglePaneZoom, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab_mut() else { return };
        if matches!(tab.tree, Node::Leaf(_)) {
            toast::show_in(window, cx, Toast::info("Nothing to zoom").body("Zoom lets one pane of a split fill the tab.").key("pane.zoom"));
            return;
        }
        tab.zoomed = !tab.zoomed;
        // Unzoomed, the other panes are on screen again.
        self.notices_seen(window, cx);
        self.save_layout(cx);
        cx.notify();
    }

    fn equalize_panes(&mut self, _: &EqualizePanes, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(tab) = self.active_tab_mut() {
            tab.tree.equalize();
            self.save_layout(cx);
            cx.notify();
        }
    }

    fn on_divider_drag(&mut self, ev: &DragMoveEvent<DividerDrag>, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let d = ev.drag(cx).clone();
        let Some(i) = self.tab_index(d.tab) else { return };
        let b = ev.bounds;
        let (pos, start, len) = match d.axis {
            Axis::Horizontal => (ev.event.position.x - b.origin.x, d.split.x, d.split.w),
            Axis::Vertical => (ev.event.position.y - b.origin.y, d.split.y, d.split.h),
        };
        let total = match d.axis {
            Axis::Horizontal => b.size.width,
            Axis::Vertical => b.size.height,
        };
        if total <= px(0.) || len <= 0.0 {
            return;
        }
        let fraction = (pos / total - start) / len;
        let min = self.min_pane_size(cx);
        self.tabs[i].tree.set_divider(&d.path, d.index, fraction, min);
        self.save_layout(cx);
        cx.notify();
    }

    // ---- overlays -----------------------------------------------------------------

    fn confirm_close(&mut self, target: CloseTarget, closing: Closing, busy: Vec<String>, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_palette(window, cx);
        self.close_recent(window, cx);
        let can_detach = self.can_detach_target(&target, cx);
        let dialog = cx.new(|cx| CloseConfirm::new(closing, busy, can_detach, window, cx));
        let sub = cx.subscribe_in(&dialog, window, |this, _, ev: &CloseConfirmEvent, window, cx| {
            let CloseConfirmEvent::Done(choice) = *ev;
            let Some((_, target, _)) = this.close.take() else { return };
            match choice {
                CloseChoice::Close => this.force_close(target, window, cx),
                CloseChoice::Detach => this.detach_target(target, window, cx),
                CloseChoice::Cancel => {}
            }
            this.focus_active(window, cx);
            cx.notify();
        });
        self.close = Some((dialog, target, sub));
        cx.notify();
    }

    fn confirm_paste(&mut self, pane: PaneId, request: PasteRequest, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_palette(window, cx);
        self.close_recent(window, cx);
        let dialog = cx.new(|cx| PasteConfirm::new(request, window, cx));
        let sub = cx.subscribe_in(&dialog, window, |this, _, ev: &PasteConfirmEvent, window, cx| {
            let PasteConfirmEvent::Done(text) = ev;
            let Some((_, pane, _)) = this.paste.take() else { return };
            if let Some((_, p)) = this.find_pane(pane) {
                let view = p.view.clone();
                view.focus_handle(cx).focus(window, cx);
                if let Some(text) = text {
                    view.update(cx, |pane, cx| pane.paste_text(text, cx));
                }
            } else {
                this.focus_active(window, cx);
            }
            cx.notify();
        });
        // A newer paste replaces one still waiting.
        self.paste = Some((dialog, pane, sub));
        cx.notify();
    }

    fn show_recent_notifications(&mut self, _: &ShowRecentNotifications, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.recent.is_some() {
            self.close_recent(window, cx);
            return;
        }
        self.close_palette(window, cx);
        let view = cx.new(|cx| RecentNotifications::new(window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &RecentEvent, window, cx| match ev {
            RecentEvent::Dismissed => this.close_recent(window, cx),
        });
        self.recent = Some((view, sub));
        cx.notify();
    }

    fn close_recent(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.recent.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn toggle_palette(&mut self, _: &ToggleCommandPalette, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        self.open_palette(Scope::All, window, cx);
    }

    fn connect_to_host(&mut self, _: &ConnectToHost, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.open_palette(Scope::Hosts, window, cx);
    }

    /// The active tab's host's sessions. A plain SSH tab's live in this
    /// computer's goattyd; a tab whose sessions no goattyd keeps (a local
    /// PTY), or that reaches its daemon another way, says so instead.
    fn list_sessions(&mut self, _: &ListSessions, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        let body = match tab.host.as_ref().map(host_ref_of) {
            Some(Some(host)) => return self.open_palette(Scope::Sessions(host), window, cx),
            Some(None) => "This tab's shell isn't kept by a goattyd goatty can list sessions of.",
            None => "This tab is still connecting.",
        };
        toast::show(cx, Toast::info("No sessions to list here").body(body).key("list-sessions"));
    }

    fn toggle_snippets(&mut self, _: &OpenSnippets, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.snippets.is_some() {
            self.close_snippets(window, cx);
            return;
        }
        self.open_snippets(crate::snippets::picker::Memory::default(), window, cx);
    }

    /// The snippet picker for the active tab's host, where `memory` left it.
    pub fn open_snippets(&mut self, memory: crate::snippets::picker::Memory, window: &mut Window, cx: &mut Context<'_, Self>) {
        use crate::snippets::picker::{PickerEvent, SnippetPicker};
        let Some(link) = self.active_host_link() else {
            toast::show(cx, Toast::info("No snippets yet").body("This tab is still connecting.").key("snippets.open"));
            return;
        };
        self.close_recent(window, cx);
        self.palette = None;
        let picker = cx.new(|cx| SnippetPicker::new(link.clone(), memory, window, cx));
        let sub = cx.subscribe_in(&picker, window, move |this, _, ev: &PickerEvent, window, cx| match ev {
            PickerEvent::Dismissed => this.close_snippets(window, cx),
            PickerEvent::Manage => {
                this.close_snippets(window, cx);
                crate::settings_window::open_at(cx, Some(crate::settings_window::Page::Snippets));
            }
            PickerEvent::Fill(s, memory) => {
                let (s, memory) = ((**s).clone(), memory.clone());
                this.close_snippets(window, cx);
                this.fill_snippet(&s, &link, memory, window, cx);
            }
        });
        self.snippets = Some((picker, sub));
        cx.notify();
    }

    fn close_snippets(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.snippets.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Ask for `s`'s blanks (Esc on the first goes back to the picker), then
    /// put the line on the prompt. Without blanks it goes there right away.
    fn fill_snippet(
        &mut self,
        s: &goatty_plugin::api::HostSnippet,
        link: &crate::settings_window::HostLink,
        memory: crate::snippets::picker::Memory,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if s.snippet.inputs.is_empty() {
            crate::snippets::insert(&s.snippet.render(&Default::default()), link, window, cx);
            return;
        }
        // Sources run in the pane's directory (on the host it reports).
        let cwd = self.focused_view().and_then(|v| v.read(cx).cwd());
        let back: crate::command_palette::input::WindowAction = Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
            let memory = memory.clone();
            window.defer(cx, move |window, cx| {
                if let Some(Some(ws)) = window.root::<Workspace>() {
                    ws.update(cx, |ws, cx| ws.open_snippets(memory, window, cx));
                }
            });
        });
        let ask = crate::snippets::ask_for(s, link, cwd, Some(back));
        self.open_ask(ask, window, cx);
    }

    fn open_palette(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_recent(window, cx);
        self.snippets = None;
        let active_program = self.focused_view().and_then(|v| v.read(cx).process().map(str::to_string));
        let palette = cx.new(|cx| CommandPalette::new(scope, active_program, window, cx));
        self.show_palette(palette, window, cx);
    }

    /// Ask for `ask`'s input in the palette (replacing an open one); see
    /// `command_palette::ask`.
    pub fn open_ask(&mut self, mut ask: crate::command_palette::Ask, window: &mut Window, cx: &mut Context<'_, Self>) {
        // Relative paths in the answers mean the focused pane's directory.
        if ask.local && ask.base.is_none() {
            ask.base = self.focused_view().and_then(|v| v.read(cx).cwd());
        }
        self.close_recent(window, cx);
        self.palette = None;
        self.snippets = None;
        let palette = cx.new(|cx| CommandPalette::asking(ask, window, cx));
        self.show_palette(palette, window, cx);
    }

    /// The focused pane's terminal, for commands that act on it.
    pub fn focused_terminal(&self) -> Option<Entity<TerminalView>> {
        self.focused_view().cloned()
    }

    /// The host of the active tab (`None` while it connects).
    pub fn active_host_link(&self) -> Option<crate::settings_window::HostLink> {
        self.active_tab()?.host.as_ref().map(Spawner::host_link)
    }

    fn show_palette(&mut self, palette: Entity<CommandPalette>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sub = cx.subscribe_in(&palette, window, |this, _, ev: &PaletteEvent, window, cx| match ev {
            PaletteEvent::Answered(then, answers) => {
                let (then, answers) = (then.clone(), answers.clone());
                this.close_palette(window, cx);
                // After this update: answers may update the workspace.
                window.defer(cx, move |window, cx| then(&answers, window, cx));
            }
            PaletteEvent::Dismissed => this.close_palette(window, cx),
            PaletteEvent::Back(back) => {
                let back = back.clone();
                this.close_palette(window, cx);
                window.defer(cx, move |window, cx| back(window, cx));
            }
            PaletteEvent::Run(command) => {
                let command = command.clone();
                // Focus the terminal first: actions go to the focused element.
                this.close_palette(window, cx);
                // After this update: commands may read or update the workspace.
                window.defer(cx, move |window, cx| command.run(window, cx));
            }
        });
        self.palette = Some((palette, sub));
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.palette.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    // ---- rendering ------------------------------------------------------------------

    fn render_panes(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let app = cx.global::<AppSettings>();
        let dim = app.client.panes.dim_inactive;
        let ui_font = app.ui_font.clone();
        let bg = gpui::Hsla::from(gpui::rgb(app.colors.background.0));
        let mut area = div()
            .id("panes")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .overflow_hidden()
            .bg(ui.border)
            .on_drag_move(cx.listener(Self::on_divider_drag));
        let bounds = self.panes_bounds.clone();
        area = area.child(gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full());
        let Some(tab) = self.active_tab() else { return area };
        if let Some((view, _)) = &tab.connecting {
            return area.child(view.clone());
        }
        let (leaves, dividers) = if tab.zoomed { (vec![(tab.focused, Rect::FULL)], Vec::new()) } else { tab.tree.layout(Rect::FULL) };
        let split = leaves.len() > 1;
        for (id, r) in leaves {
            let Some(pane) = tab.pane(id) else { continue };
            let faded = split && dim > 0.0 && id != tab.focused;
            // In a split, the pane whose agent waits for the user says so.
            let waiting =
                split.then(|| pane.view.read(cx).agent_status(u64::MAX)).flatten().filter(|s| s.state == goatty_term::StatusState::Blocked);
            area = area.child(
                div()
                    .id(gpui::ElementId::NamedInteger("pane".into(), id))
                    .absolute()
                    .left(relative(r.x))
                    .top(relative(r.y))
                    .w(relative(r.w))
                    .h(relative(r.h))
                    // The area's background shows through as the divider.
                    .when(r.x > 1e-4, |d| d.pl(px(DIVIDER)))
                    .when(r.y > 1e-4, |d| d.pt(px(DIVIDER)))
                    .child(
                        div()
                            .size_full()
                            .relative()
                            .child(pane.view.clone())
                            .when(faded, |d| d.child(div().absolute().inset_0().bg(bg.opacity(dim))))
                            .when_some(waiting, |d, status| d.child(waiting_badge(&status, ui, ui_font.clone()))),
                    ),
            );
        }
        for (n, d) in dividers.into_iter().enumerate() {
            let drag = DividerDrag { tab: tab.id, path: d.path.clone(), index: d.index, axis: d.axis, split: d.split };
            let handle = div().id(gpui::ElementId::NamedInteger("divider".into(), n as u64)).absolute().occlude();
            let handle = match d.axis {
                Axis::Horizontal => handle
                    .left(relative(d.at))
                    .ml(px(-DIVIDER_GRAB / 2.0))
                    .w(px(DIVIDER_GRAB))
                    .top(relative(d.split.y))
                    .h(relative(d.split.h))
                    .cursor_col_resize(),
                Axis::Vertical => handle
                    .top(relative(d.at))
                    .mt(px(-DIVIDER_GRAB / 2.0))
                    .h(px(DIVIDER_GRAB))
                    .left(relative(d.split.x))
                    .w(relative(d.split.w))
                    .cursor_row_resize(),
            };
            let (tab_id, path) = (tab.id, d.path.clone());
            let handle = handle
                .tooltip(crate::ui::controls::tooltip("Drag to resize · double-click to even out · right-click for more"))
                .on_click(cx.listener({
                    let path = path.clone();
                    move |this, ev: &gpui::ClickEvent, _, cx| {
                        if ev.click_count() == 2 {
                            this.equalize_split(tab_id, &path, cx);
                        }
                    }
                }))
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                        this.open_divider_menu(tab_id, path.clone(), ev.position, window, cx);
                        cx.stop_propagation();
                    }),
                );
            area = area.child(handle.on_drag(drag, |_, _, _, cx| cx.new(|_| gpui::EmptyView)));
        }
        area
    }
}

/// A ring around a split pane whose agent waits for the user, and a pill
/// in its corner saying what for.
/// What a detach took away, per goattyd connection: its tabs as saved.
type Detached = Vec<(Arc<Connection>, Vec<SavedTab>)>;

/// `toast` with an "Undo" button that reattaches `detached` (when there's
/// anything to reattach).
fn with_undo(toast: Toast, detached: Detached) -> Toast {
    if detached.is_empty() {
        return toast;
    }
    toast.action("Undo", move |window, cx| {
        let detached = detached.clone();
        if let Some(Some(ws)) = window.root::<Workspace>() {
            ws.update(cx, |ws, cx| ws.undo_detach(detached, window, cx));
        }
    })
}

/// The goattyd host behind `spawner` (plain SSH: the goattyd it runs on).
fn host_ref_of(spawner: &Spawner) -> Option<HostRef> {
    match spawner {
        Spawner::Daemon(conn) => match conn.endpoint() {
            goatty_client::Endpoint::Local => Some(HostRef::Local),
            goatty_client::Endpoint::Ssh(t) => Some(HostRef::Ssh(t.host.clone())),
            goatty_client::Endpoint::Tailnet(t) => Some(HostRef::Tailnet { dns: t.dns.clone(), name: t.name.clone() }),
            goatty_client::Endpoint::Command { .. } => None,
        },
        Spawner::Plain { base, .. } => host_ref_of(base),
        Spawner::Local(_) => None,
    }
}

/// Tab `t` as goattyd keeps it: session ids for leaves (`None` while it
/// has no daemon session).
fn saved_tab(t: &Tab, cx: &gpui::App) -> Option<SavedTab> {
    let session_of = |id: PaneId| t.pane(id).and_then(|p| p.view.read(cx).session().daemon_id());
    let tree = t.tree.map_leaves(&mut |id| session_of(id))?;
    let focused = session_of(t.focused).unwrap_or_else(|| tree.leaves()[0]);
    let plain = match &t.host {
        Some(Spawner::Plain { host, .. }) => Some(host.clone()),
        _ => None,
    };
    Some(SavedTab { name: t.name.clone(), tree, focused, zoomed: t.zoomed, plain })
}

fn waiting_badge(status: &crate::agent::AgentStatus, ui: Ui, font: gpui::SharedString) -> impl IntoElement {
    let icon = status.icon().unwrap_or(crate::ui::icons::Icon::Hand);
    let pill = div()
        .absolute()
        .top(px(8.))
        .right(px(10.))
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(9.))
        .py(px(3.))
        .rounded_full()
        .bg(ui.menu)
        .border_1()
        .border_color(ui.warning.opacity(0.6))
        .shadow_md()
        .font_family(font)
        .text_size(px(crate::ui::style::text::CAPTION))
        .text_color(ui.text)
        .child(icon.svg().size(px(12.)).text_color(ui.warning))
        .child(format!("{} · {}", status.who(), status.summary()));
    div().absolute().inset_0().border_2().border_color(ui.warning.opacity(0.7)).child(pill)
}

/// A dialog centered over the window, which it dims.
fn modal(dialog: impl IntoElement) -> impl IntoElement {
    div().absolute().inset_0().bg(gpui::black().opacity(0.35)).flex().items_center().justify_center().px_4().child(dialog)
}

/// Last non-empty line on a terminal screen, trimmed and truncated for a toast.
fn last_non_empty_line(term: &goatty_term::Terminal) -> Option<String> {
    let tr = term.transcript();
    tr.lines.iter().rev().map(|l| l.text.trim()).find(|l| !l.is_empty()).map(|s| {
        let mut out: String = s.chars().take(160).collect();
        if s.chars().count() > 160 {
            out.push('…');
        }
        out
    })
}

/// The programs running in `panes`, other than shells (one entry per busy
/// pane). Panes whose program isn't known yet count as idle.
fn busy_programs<'a>(panes: impl Iterator<Item = &'a Pane>, cx: &gpui::App) -> Vec<String> {
    panes.filter_map(|p| p.view.read(cx).process().filter(|name| !icons::is_shell(name)).map(str::to_string)).collect()
}

/// What a pane is called when its tab has no name: the application's
/// title, else the coding agent in it, else the running program, else the
/// directory's name.
fn pane_label(view: &TerminalView) -> String {
    if let Some(t) = view.app_title() {
        return t.to_string();
    }
    if let Some(agent) = view.agent() {
        return agent.name().to_string();
    }
    if let Some(p) = view.process().filter(|p| !icons::is_shell(p)) {
        return p.to_string();
    }
    if let Some(cwd) = view.cwd() {
        return dir_label(&cwd, std::env::var_os("HOME").as_deref().map(Path::new));
    }
    view.process().unwrap_or("goatty").to_string()
}

/// `~` for the home directory, else the last path component.
fn dir_label(dir: &Path, home: Option<&Path>) -> String {
    if Some(dir) == home {
        return "~".into();
    }
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.to_string_lossy().into_owned())
}

/// A directory for the vertical tabs' second line: `~/…` under the home
/// directory, and only the last two components when it's deeper than three.
fn short_dir(dir: &Path, home: Option<&Path>) -> String {
    let (prefix, rest) = match home.and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => return "~".into(),
        Some(rest) => ("~/", rest),
        None => ("/", dir.strip_prefix("/").unwrap_or(dir)),
    };
    let parts: Vec<_> = rest.iter().map(|p| p.to_string_lossy()).collect();
    if parts.len() > 3 {
        return format!("…/{}", parts[parts.len() - 2..].join("/"));
    }
    format!("{prefix}{}", parts.join("/"))
}

/// Opaque unless the terminal background is see-through.
pub fn background_appearance(t: &goatty_config::TerminalSettings) -> gpui::WindowBackgroundAppearance {
    use gpui::WindowBackgroundAppearance as W;
    match (t.is_transparent(), t.blur) {
        (false, _) => W::Opaque,
        (true, false) => W::Transparent,
        (true, true) => W::Blurred,
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // However a tab became the active one, it's being looked at now
        // (its agents' results too, while the window has focus).
        let looking = window.is_window_active();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.activity = None;
            if looking {
                let (zoomed, focused) = (tab.zoomed, tab.focused);
                for pane in tab.panes.iter_mut().filter(|p| !zoomed || p.id == focused) {
                    pane.seen = pane.view.read(cx).program_status().latest;
                }
            }
        }
        // A drag that ended anywhere (even outside the window) is over.
        if !cx.has_active_drag() {
            self.dragging = None;
        }
        self.moved_unpainted = false;
        let tabs = &cx.global::<AppSettings>().client.tabs;
        let show_bar = tabs.show == TabBarShow::Always || self.tabs.len() > 1;
        let vertical = show_bar && tabs.style == TabStyle::Vertical;
        // goatty draws the title bar, and the tab bar can't be it.
        let header = chrome::csd(window).is_some() && (vertical || !show_bar);
        // Where the panes start: the palette centers over them.
        let panes_left = if vertical { self.sidebar_width(window, cx) } else { 0.0 };
        let root = div().id("workspace").size_full().relative().flex();
        let root = if vertical {
            let sidebar = self.render_sidebar(window, cx);
            let card = self.render_card(cx);
            let handle = self.render_sidebar_handle(window, cx);
            if header {
                let row = div().id("workspace-row").relative().flex().w_full().flex_1().min_h_0();
                let row = self.sidebar_frame(row, cx).child(sidebar).child(card).child(handle);
                root.flex_col().child(self.render_title_header(window, cx)).child(row)
            } else {
                self.sidebar_frame(root, cx).child(sidebar).child(card).child(handle)
            }
        } else {
            root.flex_col()
                .when(header, |d| d.child(self.render_title_header(window, cx)))
                .when(show_bar, |d| d.child(self.render_tab_bar(window, cx)))
                .child(self.render_panes(cx))
        };
        let root = root
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::show_recent_notifications))
            .on_action(|_: &DismissNotifications, _, cx| toast::dismiss_all(cx))
            .on_action(|_: &PressNotificationButton, window, cx| {
                toast::press_primary(window, cx);
            })
            .on_action(cx.listener(Self::connect_to_host))
            .on_action(cx.listener(Self::toggle_snippets))
            .on_action(cx.listener(Self::list_sessions))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::detach_pane))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::detach_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::move_tab_left))
            .on_action(cx.listener(Self::move_tab_right))
            .on_action(cx.listener(|this, _: &SelectTab1, w, cx| this.select_tab(Some(0), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab2, w, cx| this.select_tab(Some(1), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab3, w, cx| this.select_tab(Some(2), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab4, w, cx| this.select_tab(Some(3), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab5, w, cx| this.select_tab(Some(4), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab6, w, cx| this.select_tab(Some(5), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab7, w, cx| this.select_tab(Some(6), w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab8, w, cx| this.select_tab(Some(7), w, cx)))
            .on_action(cx.listener(|this, _: &SelectLastTab, w, cx| this.select_tab(this.tabs.len().checked_sub(1), w, cx)))
            .on_action(cx.listener(Self::rename_tab))
            .on_action(cx.listener(Self::duplicate_tab))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::detach_other_tabs))
            .on_action(cx.listener(Self::detach_host))
            .on_action(cx.listener(Self::reattach_detached))
            .on_action(cx.listener(Self::new_window))
            .on_action(cx.listener(Self::toggle_tab_sidebar))
            .on_action(cx.listener(Self::toggle_vertical_tabs))
            .on_action(cx.listener(|this, _: &SplitRight, w, cx| this.split(Direction::Right, w, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, w, cx| this.split(Direction::Down, w, cx)))
            .on_action(cx.listener(|this, _: &SplitLeft, w, cx| this.split(Direction::Left, w, cx)))
            .on_action(cx.listener(|this, _: &SplitUp, w, cx| this.split(Direction::Up, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, w, cx| this.focus_pane(Direction::Left, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneRight, w, cx| this.focus_pane(Direction::Right, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneUp, w, cx| this.focus_pane(Direction::Up, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneDown, w, cx| this.focus_pane(Direction::Down, w, cx)))
            .on_action(cx.listener(|this, _: &ResizePaneLeft, _, cx| this.resize_pane(Direction::Left, cx)))
            .on_action(cx.listener(|this, _: &ResizePaneRight, _, cx| this.resize_pane(Direction::Right, cx)))
            .on_action(cx.listener(|this, _: &ResizePaneUp, _, cx| this.resize_pane(Direction::Up, cx)))
            .on_action(cx.listener(|this, _: &ResizePaneDown, _, cx| this.resize_pane(Direction::Down, cx)))
            .on_action(cx.listener(Self::toggle_pane_zoom))
            .on_action(cx.listener(Self::equalize_panes))
            .on_action(cx.listener(|this, _: &SwapPaneLeft, _, cx| this.swap_pane(Direction::Left, cx)))
            .on_action(cx.listener(|this, _: &SwapPaneRight, _, cx| this.swap_pane(Direction::Right, cx)))
            .on_action(cx.listener(|this, _: &SwapPaneUp, _, cx| this.swap_pane(Direction::Up, cx)))
            .on_action(cx.listener(|this, _: &SwapPaneDown, _, cx| this.swap_pane(Direction::Down, cx)))
            .on_action(cx.listener(|this, _: &MovePaneToNewTab, w, cx| this.move_pane_to_new_tab(w, cx)))
            .on_action(cx.listener(|this, _: &MovePaneToTab, w, cx| this.ask_move_pane(w, cx)))
            .on_action(cx.listener(|this, _: &MergeTab, w, cx| this.ask_merge_tab(w, cx)))
            .on_action(cx.listener(|this, _: &RotatePanes, w, cx| this.rotate_panes(true, w, cx)))
            .on_action(cx.listener(|this, _: &RotatePanesBack, w, cx| this.rotate_panes(false, w, cx)))
            .on_action(cx.listener(|this, _: &NextHost, w, cx| this.step_group(1, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousHost, w, cx| this.step_group(-1, w, cx)))
            .on_action(cx.listener(|this, _: &ShowOnlyThisHost, w, cx| match this.active_group_key(cx) {
                Some(key) => this.show_only_group(&key, w, cx),
                None => {
                    let toast = Toast::info("All tabs are on this computer").body("Tabs on other hosts get grouped by host.");
                    toast::show_in(w, cx, toast.key("tab.fold"));
                }
            }))
            .on_action(cx.listener(|this, _: &ShowAllHosts, _, cx| this.show_all_groups(cx)))
            .on_action(cx.listener(|this, act: &crate::actions::RunPluginCommand, w, cx| {
                let id = act.id.clone();
                let sel = this.focused_view().and_then(|v| v.read(cx).session().terminal.selection_text()).filter(|t| !t.trim().is_empty());
                w.defer(cx, move |window, cx| {
                    crate::plugin_commands::run_by_id(&id, sel.as_deref(), window, cx);
                });
            }))
            .on_action(cx.listener(|this, act: &crate::actions::RunCustomCommand, w, cx| {
                let prog = this.focused_view().and_then(|v| v.read(cx).process().map(str::to_string));
                if !crate::custom_commands::matches_focused_program(&act.id, prog.as_deref(), cx) {
                    cx.propagate();
                    return;
                }
                let id = act.id.clone();
                w.defer(cx, move |window, cx| {
                    crate::custom_commands::run_by_id(&id, true, window, cx);
                });
            }))
            .children(self.render_menu())
            .child(self.toasts.clone())
            .when_some(self.palette.as_ref(), |d, (palette, _)| {
                d.child(div().absolute().top(px(48.)).left(px(panes_left)).right_0().px_4().flex().justify_center().child(palette.clone()))
            })
            .when_some(self.snippets.as_ref(), |d, (picker, _)| {
                d.child(div().absolute().top(px(48.)).left(px(panes_left)).right_0().px_4().flex().justify_center().child(picker.clone()))
            })
            .when_some(self.recent.as_ref(), |d, (view, _)| {
                d.child(div().absolute().top(px(48.)).left(px(panes_left)).right_0().px_4().flex().justify_center().child(view.clone()))
            })
            .when_some(self.paste.as_ref(), |d, (dialog, _, _)| d.child(modal(dialog.clone())))
            .when_some(self.close.as_ref(), |d, (dialog, _, _)| d.child(modal(dialog.clone())))
            .into_any_element();
        self.window_frame(root, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_labels() {
        let home = Path::new("/home/me");
        assert_eq!(dir_label(Path::new("/home/me"), Some(home)), "~");
        assert_eq!(dir_label(Path::new("/home/me/coding/goatty"), Some(home)), "goatty");
        assert_eq!(dir_label(Path::new("/"), Some(home)), "/");
        assert_eq!(dir_label(Path::new("/etc"), None), "etc");
    }

    #[test]
    fn short_directories() {
        let home = Some(Path::new("/home/me"));
        assert_eq!(short_dir(Path::new("/home/me"), home), "~");
        assert_eq!(short_dir(Path::new("/home/me/coding/goatty"), home), "~/coding/goatty");
        assert_eq!(short_dir(Path::new("/home/me/a/b/c/d"), home), "…/c/d");
        assert_eq!(short_dir(Path::new("/"), home), "/");
        assert_eq!(short_dir(Path::new("/etc/nginx"), home), "/etc/nginx");
        assert_eq!(short_dir(Path::new("/var/lib/docker/volumes"), None), "…/docker/volumes");
        assert_eq!(short_dir(Path::new("/home/meow"), home), "/home/meow");
    }
}
