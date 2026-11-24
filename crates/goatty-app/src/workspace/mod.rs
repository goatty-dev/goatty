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
//! - `saved`: a workspace's layout as goattyd keeps it (#215), and
//!   merging two windows' edits of it.
//! - `sync`: the workspace each host shows in this window: saving,
//!   changes from other windows, switching, parking (#215).
//! - `switcher`: the workspace chip and its popover.
//! - `reopen`: which hosts each window has tabs on, so the next `goatty`
//!   reopens every window with its tabs on every host (#252).
//!
//! Naming: this module is the window's root view. A *workspace* in goatty's
//! model (#215) is the daemon's: a named set of tabs on one goattyd that
//! every window showing it shares (`goatty-daemon/src/workspaces.rs`,
//! `sync` here). Each host a window has tabs on shows one of its
//! workspaces.
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
//! Each goattyd the window has tabs on keeps them in the workspace the
//! window shows there, and `reopen` remembers which goattyds and workspaces
//! those are, so `goatty` brings back every window with its tabs on every
//! host (`goatty --ssh host`: that host's). While a window is
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
mod switcher;
mod sync;
mod tab_bar;

use std::path::Path;
use std::sync::Arc;

use goatty_client::Connection;
use goatty_config::{TabActivity, TabBarShow, TabSettings, TabStyle};
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
use saved::{SavedTab, SavedWindow, Startup};

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
    /// The workspace each goattyd host shows in this window (#215).
    shown: Vec<sync::Shown>,
    /// Workspaces this window switched away from, their sessions still attached.
    parked: Vec<sync::Parked>,
    /// How this window joins workspaces (its view id, where it hears about them).
    join: saved::Join,
    /// This window, for work that needs it later (`sync`).
    window_handle: gpui::AnyWindowHandle,
    /// A divider is being dragged: split sizes are saved when it ends, and
    /// changes from other windows wait for it.
    divider_dragging: bool,
    /// The workspace switcher's popover.
    switcher: Option<switcher::OpenSwitcher>,
    /// The window's size and state, recorded for reopening.
    window: SavedWindow,
    /// Recording the size waits for the window to stop resizing.
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
    /// Activate the tab of this session once its pane exists (a tab being
    /// attached off the UI thread).
    /// (connection key, session).
    show_when_attached: Option<(usize, goatty_proto::SessionId)>,
    /// What hosts said about their workspaces before this window showed
    /// one there (it was still attaching): applied by `show_joined`.
    early_events: Vec<(usize, goatty_client::WorkspaceEvent)>,
    /// Workspaces to close once the tabs moved out of them are saved elsewhere.
    move_closes: Vec<sync::MoveClose>,
    _subscriptions: Vec<Subscription>,
}

/// How long the window's size has to settle before it's recorded (a resize
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
        // What the hosts tell this window about its workspaces, handled in order.
        let (events_tx, events_rx) = flume::unbounded::<(usize, goatty_client::WorkspaceEvent)>();
        cx.spawn_in(window, async move |this, cx| {
            while let Ok((conn, event)) = events_rx.recv_async().await {
                if this.update_in(cx, |this, window, cx| this.on_workspace_event(conn, event, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let reopen_key = reopen::new_key();
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
            shown: Vec::new(),
            parked: Vec::new(),
            join: saved::Join { view: reopen_key, sink: Arc::new(sync::WindowSink(events_tx)) },
            window_handle: window.window_handle(),
            divider_dragging: false,
            switcher: None,
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
            reopen_key,
            restoring: Vec::new(),
            recently_detached: Vec::new(),
            show_when_attached: None,
            early_events: Vec::new(),
            move_closes: Vec::new(),
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
                    Startup::Joined(j) => this.show_joined(0, j, spawner, None, true, window, cx),
                }
            }
            Initial::Connect(plan) => this.open_connect_tab(plan, window, cx),
            Initial::Reopen(record, open_settings) => this.reopen(record, open_settings, window, cx),
        }
        this
    }

    /// The window went away with its tabs (closed while goatty runs on, or
    /// goatty quit): its shells keep running, its workspaces stay on their
    /// hosts for the next window (or the next goatty) to show.
    fn released(&mut self, cx: &mut gpui::App) {
        for p in self.tabs.iter().flat_map(|t| t.panes.iter()) {
            p.view.read(cx).session().release_view();
        }
        self.leave_all(cx);
    }

    /// Open a window from the last run (#252): a connecting tab per host
    /// that joins the workspace this window showed there, in the order of
    /// their tabs.
    fn reopen(&mut self, record: reopen::WindowRecord, mut open_settings: Option<String>, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(w) = record.window {
            w.apply(window);
            self.window = w;
        }
        self.reopen_key = record.key;
        self.join.view = record.key;
        let mut active = None;
        let active_host = record.active.min(record.hosts.len().saturating_sub(1));
        for (i, h) in record.hosts.into_iter().enumerate() {
            let plan = ConnectPlan {
                endpoint: h.host.host_ref().endpoint(),
                mode: AttachMode::Restore(h.workspace),
                // `--settings`: once the active tab's host is there.
                open_settings: if i == active_host { open_settings.take() } else { None },
                plain: false,
                conn: None,
                join: None,
                active_tab: h.active_tab,
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

    /// Tell `reopen` which hosts this window has tabs on now.
    fn record_window(&self, cx: &mut Context<'_, Self>) {
        let mut hosts: Vec<reopen::HostRecord> = Vec::new();
        let mut active = 0;
        for (i, t) in self.tabs.iter().enumerate() {
            let record = match t.host.as_ref().and_then(Spawner::connection) {
                Some(conn) => {
                    let Some(shown) = self.shown.iter().find(|s| Arc::ptr_eq(&s.conn, conn)) else { continue };
                    let Some(host) = reopen::SavedHost::of(conn.endpoint()) else { continue };
                    // The tab to show there: the active one, else the host's last active one.
                    let key = self.host_key(t);
                    let last = self.group_focus.iter().find(|(k, _)| *k == key).map(|(_, id)| *id);
                    let active_tab = self.active_tab().filter(|a| sync::on_conn(a, conn)).map(|a| a.id).or(last);
                    reopen::HostRecord { host, workspace: shown.id, active_tab }
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

    /// The window was resized, maximized or went fullscreen: record its
    /// size once it settles.
    fn window_bounds_changed(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let now = SavedWindow::capture(window, self.window);
        if now == self.window {
            return;
        }
        self.window = now;
        self.save_window = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_WINDOW_AFTER).await;
            this.update(cx, |this, cx| this.record_window(cx)).ok();
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
        } else if let Some(s) = &self.switcher {
            s.view.focus_handle(cx).focus(window, cx);
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
    /// the connect screen until its session is there. A host this window
    /// shows a workspace on already gets what `plan` asks for there instead.
    pub fn open_connect_tab(&mut self, plan: ConnectPlan, window: &mut Window, cx: &mut Context<'_, Self>) {
        let at = if self.tabs.is_empty() { 0 } else { self.active + 1 };
        self.open_connect_tab_at(plan, at, window, cx);
    }

    /// [`Self::open_connect_tab`] at position `at`.
    fn open_connect_tab_at(&mut self, mut plan: ConnectPlan, at: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !plan.plain
            && let Some(live) = crate::hosts::pool::live(&plan.endpoint)
            && let Some(i) = self.shown_index(&live)
        {
            self.open_on_shown(i, plan.mode, window, cx);
            return;
        }
        // Plain SSH runs in this computer's goattyd: it joins a workspace
        // there only when the window shows none yet.
        if plan.plain && crate::hosts::pool::live(&goatty_client::Endpoint::Local).is_some_and(|l| self.shown_index(&l).is_some()) {
            plan.join = None;
        } else if plan.join.is_none() {
            plan.join = Some(self.join.clone());
        }
        let id = saved::new_id();
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
        let at = at.min(self.tabs.len());
        self.tabs.insert(at, tab);
        self.activate(at, window, cx);
    }

    /// What `mode` asks for, on host `i`, whose workspace this window shows.
    fn open_on_shown(&mut self, i: usize, mode: AttachMode, window: &mut Window, cx: &mut Context<'_, Self>) {
        let conn = self.shown[i].conn.clone();
        let current = self.shown[i].id;
        let show_host = |this: &mut Self, window: &mut Window, cx: &mut Context<'_, Self>| {
            if let Some(at) = this.tabs.iter().position(|t| sync::on_conn(t, &conn)) {
                this.activate(at, window, cx);
            }
        };
        match mode {
            AttachMode::Join(Some(id)) | AttachMode::Restore(id) if id != current => self.switch_workspace(&conn, id, window, cx),
            // Another workspace's detached tabs: show it, with them back.
            AttachMode::Undetach(id) if id != current => self.switch_workspace_with(&conn, AttachMode::Undetach(id), window, cx),
            AttachMode::Join(_) | AttachMode::Restore(_) => show_host(self, window, cx),
            AttachMode::Undetach(_) => {
                let all: Vec<u64> = self.shown[i].detached.iter().map(|t| t.id).collect();
                self.undetach_tabs(i, Some(all), window, cx);
            }
            AttachMode::New => {
                if let Some(at) = self.tabs.iter().position(|t| sync::on_conn(t, &conn)) {
                    let key = self.host_key(&self.tabs[at]);
                    self.new_tab_in_group(&key, window, cx);
                }
            }
            AttachMode::Id(session) => {
                // Shown already: that tab; one of this workspace's detached
                // tabs: back; in another workspace: that one; else a tab of
                // its own.
                let at = self.tabs.iter().position(|t| {
                    sync::on_conn(t, &conn) && t.panes.iter().any(|p| p.view.read(cx).session().daemon_id() == Some(session))
                });
                let detached = self.shown[i].detached.iter().find(|t| t.tree.contains(session)).map(|t| t.id);
                let elsewhere = self.shown[i].list.iter().find(|w| w.id != current && w.sessions.contains(&session)).map(|w| w.id);
                match (at, detached, elsewhere) {
                    (Some(at), _, _) => self.activate(at, window, cx),
                    (None, Some(tab), _) => self.undetach_tabs(i, Some(vec![tab]), window, cx),
                    (None, None, Some(_)) => self.switch_workspace_with(&conn, AttachMode::Id(session), window, cx),
                    (None, None, None) => self.add_session_tabs(i, vec![session], window, cx),
                }
            }
            AttachMode::AllDetached => {
                let (epoch, workspace_id) = (self.shown[i].epoch, self.shown[i].id);
                let task = cx.background_executor().spawn({
                    let conn = conn.clone();
                    async move {
                        let list = conn.list_workspaces()?;
                        let shown: std::collections::HashSet<goatty_proto::SessionId> =
                            list.iter().flat_map(|w| w.sessions.iter().copied()).collect();
                        anyhow::Ok(saved::loose_detached(&conn.list_sessions()?, &shown))
                    }
                });
                cx.spawn_in(window, async move |this, cx| {
                    let result = task.await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(ids) if ids.is_empty() => {
                            let toast = Toast::info("No detached sessions").body("Every shell there is in a workspace.");
                            toast::show_in(window, cx, toast.key("reattach"));
                        }
                        Ok(ids) => {
                            if let Some(i) =
                                this.shown_index(&conn).filter(|&i| this.shown[i].epoch == epoch && this.shown[i].id == workspace_id)
                            {
                                this.add_session_tabs(i, ids, window, cx);
                            } else {
                                let toast = Toast::info("Nothing was added").body("This window shows another workspace there now.");
                                toast::show_in(window, cx, toast.key("reattach"));
                            }
                        }
                        Err(e) => {
                            toast::show_in(window, cx, Toast::error("Couldn't list the sessions").body(format!("{e:#}")).key("reattach"));
                        }
                    });
                })
                .detach();
            }
        }
    }

    /// Show sessions `ids` on host `i` as a new tab each (attached off the
    /// UI thread), after its other tabs.
    fn add_session_tabs(&mut self, i: usize, ids: Vec<goatty_proto::SessionId>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let mut target = self.local_target(i, cx);
        let first = ids.first().copied();
        target.tabs.extend(ids.into_iter().map(|s| SavedTab {
            id: saved::new_id(),
            name: None,
            tree: Node::Leaf(s),
            plain: None,
            detached_at: None,
        }));
        // The first one is shown once it's there (maybe right away).
        if let Some(s) = first {
            self.focus_session_when_shown(i, s);
        }
        self.reconcile(i, target, std::collections::HashMap::new(), None, window, cx);
    }

    /// Bring host `i`'s detached tabs back (`ids`; `None`: the ones detached last).
    fn undetach_tabs(&mut self, i: usize, ids: Option<Vec<u64>>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let mut target = self.local_target(i, cx);
        let ids = ids.unwrap_or_else(|| target.last_detached());
        if ids.is_empty() {
            toast::show_in(window, cx, Toast::info("Nothing detached in this workspace").key("reattach"));
            return;
        }
        let first = target.detached.iter().find(|t| ids.contains(&t.id)).and_then(|t| t.tree.leaves().first().copied());
        target.undetach(&ids);
        if let Some(s) = first {
            self.focus_session_when_shown(i, s);
        }
        self.reconcile(i, target, std::collections::HashMap::new(), None, window, cx);
        self.save_layout(cx);
    }

    /// Show the tab with session `s` of host `i` once its pane exists
    /// (attaching is off the UI thread).
    fn focus_session_when_shown(&mut self, i: usize, s: goatty_proto::SessionId) {
        self.show_when_attached = Some((self.shown[i].conn.key(), s));
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
                let (ready, settings, installed, active_tab) =
                    view.update(cx, |v, _| (v.take_ready(), v.take_open_settings(), v.take_installed(), v.active_tab()));
                let Some((startup, spawner)) = ready else { return };
                self.connected(index, startup, spawner, installed, active_tab, window, cx);
                if let Some(page) = settings {
                    crate::settings_window::open_at(cx, crate::settings_window::Page::from_name(&page));
                }
            }
        }
    }

    /// The connecting tab at `index` got there: show what it brought (a
    /// joined workspace replaces the tab with its tabs).
    #[allow(clippy::too_many_arguments)]
    fn connected(
        &mut self,
        index: usize,
        startup: Startup,
        spawner: Spawner,
        installed: bool,
        active_tab: Option<u64>,
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
        match startup {
            Startup::Session(session) => self.fill_connecting_tab(index, session, spawner, window, cx),
            Startup::Joined(j) if j.is_empty() => self.nothing_to_restore(index, j, spawner, window, cx),
            Startup::Joined(j) => {
                self.tabs.remove(index);
                if self.active > index {
                    self.active -= 1;
                }
                let base = match &spawner {
                    Spawner::Plain { base, .. } => (**base).clone(),
                    other => other.clone(),
                };
                self.show_joined(index, j, base, active_tab, was_active, window, cx);
            }
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

    /// The connecting tab at `index` found nothing left to restore in its
    /// host's workspace (`j`): it goes away with a note, and the window stops
    /// showing that workspace. The window's last tab gets a new shell there
    /// instead, so the window doesn't vanish.
    fn nothing_to_restore(
        &mut self,
        index: usize,
        j: Box<saved::Joined>,
        spawner: Spawner,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let host = groups::host_label(&spawner);
        if self.tabs.len() == 1 {
            let toast = Toast::info(format!("Nothing left to restore on {host}"))
                .body("Those tabs' shells ended meanwhile; here's a new one.")
                .key("workspace.restore");
            toast::show_in(window, cx, toast);
            // The connect screen stays until the shell is there.
            let placeholder = self.tabs[index].id;
            let view_id = self.join.view;
            let task = cx.background_executor().spawn({
                let spawner = spawner.clone();
                async move { spawner.spawn(TermSize::new(100, 30), None) }
            });
            cx.spawn_in(window, async move |this, cx| {
                let mut result = Some(task.await);
                let mut j = Some(j);
                let spawner_cleanup = spawner.clone();
                let delivered = this.update_in(cx, |this, window, cx| {
                    let result = result.take().expect("spawn result");
                    let j = j.take().expect("joined workspace");
                    // Nothing shows the joined workspace if this goes wrong.
                    let leave = |this: &Self| {
                        if let Some(conn) = spawner.connection()
                            && this.shown_index(conn).is_none()
                        {
                            conn.leave_workspace(this.join.view);
                        }
                    };
                    let Some(index) = this.tab_index(placeholder) else {
                        if let Ok(session) = result {
                            this.discard_spawned_session(session, cx);
                        }
                        leave(this);
                        return;
                    };
                    match result {
                        Ok(session) => {
                            this.tabs.remove(index);
                            let j = Box::new(saved::Joined { extra: vec![(session, None)], ..*j });
                            this.show_joined(index, j, spawner, None, true, window, cx);
                        }
                        Err(e) => {
                            let toast = Toast::error("Couldn't start a shell").body(format!("{e:#}")).key("workspace.spawn");
                            toast::show_in(window, cx, toast);
                            leave(this);
                            this.remove_tab(index, window, cx);
                        }
                    }
                });
                // The window went away meanwhile: release the shell and leave
                // the pending workspace join.
                if delivered.is_err() {
                    if let Some(Ok(session)) = result
                        && !session.release_view()
                    {
                        cx.background_executor().spawn(async move { session.kill() }).detach();
                    }
                    if let Some(conn) = spawner_cleanup.connection() {
                        conn.leave_workspace(view_id);
                    }
                }
            })
            .detach();
            cx.notify();
            return;
        }
        if let Some(conn) = spawner.connection() {
            conn.leave_workspace(self.join.view);
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
        let return_focus = Some(focused_id);
        let placement = match spec.open_in {
            goatty_config::CommandOpenIn::Tab | goatty_config::CommandOpenIn::Current => Placement::NewTab,
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
                    Placement::NewTab
                } else {
                    Placement::Split { tab: tab_id, of: focused_id, dir }
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
        // The pane takes over the session's view claim (another window of
        // this goatty may show it too: the last one to let go detaches it).
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
        if !session.release_view() {
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
        pane.return_focus = return_focus;
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
                let tab_id = saved::new_id();
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
        // A host whose workspace lost its last tab here: the workspace ends
        // (unless it's the host's only one) and the window shows the host's
        // most recently active other workspace, else it's done with the host.
        // (One still attaching its tabs isn't empty.)
        let emptied: Vec<usize> = (0..self.shown.len())
            .filter(|&i| self.shown[i].attaching.is_empty() && !self.tabs.iter().any(|t| sync::on_conn(t, &self.shown[i].conn)))
            .collect();
        let mut next = Vec::new();
        for &i in emptied.iter().rev() {
            self.save_shown_now(i, cx);
            let sh = self.shown.remove(i);
            let other = sh.list.iter().filter(|w| w.id != sh.id).max_by_key(|w| w.active).map(|w| w.id);
            match other {
                Some(id) => {
                    // Its parked tabs stay for the next one to take back.
                    next.push((sh.conn.clone(), id));
                }
                None => {
                    sh.conn.leave_workspace(self.join.view);
                    self.release_parked_of(&sh.conn, cx);
                }
            }
        }
        for (conn, id) in next {
            let plan = ConnectPlan {
                endpoint: conn.endpoint().clone(),
                mode: AttachMode::Join(Some(id)),
                open_settings: None,
                plain: false,
                conn: Some(conn),
                join: None,
                active_tab: None,
            };
            let at = (self.active + 1).min(self.tabs.len());
            self.open_connect_tab_at(plan, at, window, cx);
        }
        if self.tabs.is_empty() {
            log::info!("last session ended");
            self.forget_window(cx);
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
        let return_focus = if index == self.active {
            let tab = &self.tabs[index];
            tab.pane(tab.focused).and_then(|p| p.return_focus)
        } else {
            None
        };
        self.take_tab(index, window, cx);
        if let Some(origin) = return_focus
            && let Some((origin_tab, _)) = self.find_pane(origin)
        {
            self.active = origin_tab;
            self.tabs[origin_tab].focused = origin;
        }
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
        // Closing the window's last tab may quit the app: the kills' replies
        // come first, so goattyd heard everything.
        let last = last && !self.shown.iter().any(|s| s.list.iter().any(|w| w.id != s.id));
        if !last {
            kill.detach();
            self.remove_target(target, window, cx);
            return;
        }
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

    /// Let go of the sessions of `leaving_panes` (detached from this window;
    /// another pane of this goatty may still show one).
    fn detach_panes_by_id(&self, leaving_panes: &[PaneId], cx: &gpui::App) {
        for p in self.tabs.iter().flat_map(|t| t.panes.iter()).filter(|p| leaving_panes.contains(&p.id)) {
            p.view.read(cx).session().release_view();
        }
    }

    /// Detach pane `id` without ending its shell: out of a split, it
    /// becomes a detached tab of its workspace.
    fn detach_pane_by_id(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((index, pane)) = self.find_pane(id) else { return };
        if !pane.view.read(cx).session().can_detach() {
            let toast = Toast::info("Can't detach a local shell")
                .body("This shell isn't running in goattyd (--local), so it can't keep running without the window.")
                .key("workspace.detach");
            toast::show_in(window, cx, toast);
            return;
        }
        if self.tabs[index].tree == Node::Leaf(id) {
            let tab = self.tabs[index].id;
            self.detach_tabs(vec![tab], window, cx);
            return;
        }
        let label = pane_label(pane.view.read(cx));
        let session = pane.view.read(cx).session().daemon_id();
        let conn = self.tabs[index].host.as_ref().and_then(Spawner::connection).cloned();
        let (Some(session), Some(conn)) = (session, conn) else { return };
        let Some(i) = self.shown_index(&conn) else { return };
        let plain = match &self.tabs[index].host {
            Some(Spawner::Plain { host, .. }) => Some(host.clone()),
            _ => None,
        };
        let tab = SavedTab { id: saved::new_id(), name: None, tree: Node::Leaf(session), plain, detached_at: Some(saved::now_ms()) };
        let undo = vec![Undo::Tabs { conn: conn.clone(), workspace: self.shown[i].id, ids: vec![tab.id] }];
        self.shown[i].detached.push(tab);
        self.note_detached(&undo);
        self.detach_panes_by_id(&[id], cx);
        self.remove_pane(id, window, cx);
        let toast = Toast::info(format!("Detached {label}")).body("Its session keeps running in the background.").key("workspace.detach");
        toast::show_in(window, cx, with_undo(toast, undo));
    }

    /// Undo a detach: the tabs come back from their workspace's detached
    /// tabs, or the window shows the host's workspace again.
    pub(super) fn undo_detach(&mut self, detached: Detached, window: &mut Window, cx: &mut Context<'_, Self>) {
        for undo in detached {
            let (conn, workspace, ids) = match undo {
                Undo::Tabs { conn, workspace, ids } => (conn, workspace, Some(ids)),
                Undo::Host { conn, workspace } => (conn, workspace, None),
            };
            match (self.shown_index(&conn), ids) {
                (Some(i), Some(ids)) if self.shown[i].id == workspace => self.undetach_tabs(i, Some(ids), window, cx),
                (_, ids) => {
                    let mode = if ids.is_some() { AttachMode::Undetach(workspace) } else { AttachMode::Join(Some(workspace)) };
                    let plan = ConnectPlan {
                        endpoint: conn.endpoint().clone(),
                        mode,
                        open_settings: None,
                        plain: false,
                        conn: Some(conn),
                        join: None,
                        active_tab: None,
                    };
                    self.open_connect_tab(plan, window, cx);
                }
            }
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

    /// Detach the tabs `ids` (by id) without ending their shells. Some of a
    /// host's tabs become detached tabs of its workspace (every window that
    /// shows it loses them); all of them: the window stops showing that
    /// workspace, which stays as it is on the host.
    fn detach_tabs(&mut self, ids: Vec<u64>, window: &mut Window, cx: &mut Context<'_, Self>) {
        if ids.is_empty() {
            return;
        }
        let mut detachable_ids = Vec::new();
        let mut pane_ids = Vec::new();
        let mut single_label = None;
        let mut any_connecting = false;
        for t in self.tabs.iter().filter(|t| ids.contains(&t.id)) {
            if t.connecting.is_some() {
                any_connecting = true;
                continue;
            }
            let tab_panes: Vec<PaneId> = t.panes.iter().filter(|p| p.view.read(cx).session().can_detach()).map(|p| p.id).collect();
            if !tab_panes.is_empty() && t.host.as_ref().and_then(Spawner::connection).is_some() {
                if detachable_ids.is_empty() {
                    single_label = Some(self.tab_label(t, cx));
                }
                detachable_ids.push(t.id);
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
        let mut undo: Detached = Vec::new();
        let mut deferred_tabs: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let at = saved::now_ms();
        let conns: Vec<Arc<goatty_client::Connection>> = self.shown.iter().map(|s| s.conn.clone()).collect();
        for conn in conns.into_iter().rev() {
            let Some(i) = self.shown_index(&conn) else { continue };
            let leaving: Vec<u64> =
                self.tabs.iter().filter(|t| detachable_ids.contains(&t.id) && sync::on_conn(t, &conn)).map(|t| t.id).collect();
            if leaving.is_empty() {
                continue;
            }
            let all = self.tabs.iter().filter(|t| sync::on_conn(t, &conn)).all(|t| detachable_ids.contains(&t.id));
            if all {
                // Keep the workspace until any unsaved edit or pending rebase
                // finishes saving; on conflict it merges and retries, and on
                // failure the edits remain in this window.
                if self.shown_needs_save(i, cx) {
                    deferred_tabs.extend(&leaving);
                    self.save_then_detach(i, window, cx);
                    continue;
                }
                let sh = self.shown.remove(i);
                sh.conn.leave_workspace(self.join.view);
                undo.push(Undo::Host { conn: conn.clone(), workspace: sh.id });
                self.release_parked_of(&conn, cx);
            } else {
                let tabs: Vec<SavedTab> = self
                    .tabs
                    .iter()
                    .filter(|t| leaving.contains(&t.id))
                    .filter_map(|t| saved_tab(t, cx))
                    .map(|t| SavedTab { detached_at: Some(at), ..t })
                    .collect();
                undo.push(Undo::Tabs { conn: conn.clone(), workspace: self.shown[i].id, ids: tabs.iter().map(|t| t.id).collect() });
                self.shown[i].detached.extend(tabs);
            }
        }
        if !deferred_tabs.is_empty() {
            detachable_ids.retain(|id| !deferred_tabs.contains(id));
            pane_ids.clear();
            single_label = None;
            for t in self.tabs.iter().filter(|t| detachable_ids.contains(&t.id)) {
                if single_label.is_none() {
                    single_label = Some(self.tab_label(t, cx));
                }
                pane_ids.extend(t.panes.iter().filter(|p| p.view.read(cx).session().can_detach()).map(|p| p.id));
            }
        }
        if detachable_ids.is_empty() {
            return;
        }
        let n_tabs = detachable_ids.len();
        let n_sessions = pane_ids.len();
        self.note_detached(&undo);
        self.detach_panes_by_id(&pane_ids, cx);
        self.remove_tabs(&detachable_ids, window, cx);
        self.save_layout(cx);
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

    /// Complete a full-host detach that waited for its unsaved workspace edits
    /// to save cleanly.
    pub(super) fn finish_deferred_detach(&mut self, epoch: u64, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.shown.iter().position(|s| s.epoch == epoch) else { return };
        let conn = self.shown[i].conn.clone();
        let ids: Vec<u64> = self.tabs.iter().filter(|t| sync::on_conn(t, &conn)).map(|t| t.id).collect();
        if !ids.is_empty() {
            self.detach_tabs(ids, window, cx);
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
        self.detach_tabs(g.ids.clone(), window, cx);
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
        // A pane of a workspace this window keeps in the background.
        if self.find_pane(id).is_none() && self.find_parked_pane(id).is_some() {
            match ev {
                TerminalEvent::Exited => self.remove_parked_pane(id, cx),
                TerminalEvent::Activity(activity) if marks(&cx.global::<AppSettings>().client.tabs, *activity) => {
                    self.mark_parked_activity(id, *activity, cx)
                }
                _ => {}
            }
            return;
        }
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
        for undo in detached {
            let conn = match undo {
                Undo::Tabs { conn, .. } | Undo::Host { conn, .. } => conn,
            };
            let Some(host) = host_ref_of(&Spawner::Daemon(conn.clone())) else { continue };
            let entry = (host, None);
            self.recently_detached.retain(|e| *e != entry);
            self.recently_detached.push(entry);
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
    ///
    /// On a host whose workspace the window shows: its tabs detached last
    /// (`plain`: that plain SSH host's), else the newest detached session
    /// no workspace shows. On another host: its most recently active
    /// workspace that no window shows, else the newest such session.
    fn reattach_detached_from(&mut self, candidates: Vec<(HostRef, Option<String>)>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((host, plain)) = candidates.last().cloned() else { return };
        // A workspace shown here with detached tabs: right away.
        for (h, wanted) in &candidates {
            let Some(conn) = crate::hosts::pool::live(&h.endpoint()) else { continue };
            let Some(i) = self.shown_index(&conn) else { continue };
            let ids: Vec<u64> = match wanted {
                Some(p) => self.shown[i].detached.iter().filter(|t| t.plain.as_deref() == Some(p.as_str())).map(|t| t.id).collect(),
                None => saved::Layout::new(Vec::new(), self.shown[i].detached.clone()).last_detached(),
            };
            if !ids.is_empty() {
                self.undetach_tabs(i, Some(ids), window, cx);
                return;
            }
        }
        let shown: Vec<Arc<Connection>> = self.shown.iter().map(|s| s.conn.clone()).collect();
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
                let here = shown.iter().any(|c| Arc::ptr_eq(c, &conn));
                let lookup = || -> anyhow::Result<Option<AttachMode>> {
                    let list = conn.list_workspaces()?;
                    if !here
                        && wanted.is_none()
                        && let Some(w) = list.iter().filter(|w| w.views == 0 && !w.sessions.is_empty()).max_by_key(|w| w.active)
                    {
                        return Ok(Some(AttachMode::Join(Some(w.id))));
                    }
                    // A plain SSH host's tabs only: a loose local shell isn't one.
                    if wanted.is_some() {
                        return Ok(None);
                    }
                    let in_workspace: std::collections::HashSet<goatty_proto::SessionId> =
                        list.iter().flat_map(|w| w.sessions.iter().copied()).collect();
                    Ok(saved::loose_detached(&conn.list_sessions()?, &in_workspace).last().map(|&id| AttachMode::Id(id)))
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
                                join: None,
                                active_tab: None,
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

    /// A new window on the same workspace as this one's active tab's host
    /// (another view of it, #215): it opens at the active tab. Tabs that
    /// aren't on a goattyd: a new shell there, as before.
    fn new_window(&mut self, _: &NewWindow, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(i) = self.active_shown() {
            let sh = &self.shown[i];
            let plan = ConnectPlan {
                endpoint: sh.conn.endpoint().clone(),
                mode: AttachMode::Join(Some(sh.id)),
                open_settings: None,
                plain: false,
                conn: Some(sh.conn.clone()),
                join: None,
                active_tab: self.active_tab().map(|t| t.id),
            };
            cx.defer(move |cx| crate::app::open_terminal_window(cx, Initial::Connect(plan), None));
            return;
        }
        self.new_window_with_shell(window, cx);
    }

    /// A new window with a new shell on the active tab's host (this
    /// computer while it connects).
    fn new_window_with_shell(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(spawner) = self.active_tab().and_then(|t| t.host.clone()).or_else(|| self.home.clone()) else {
            // Nothing connected yet: this computer.
            let plan = ConnectPlan {
                endpoint: goatty_client::Endpoint::Local,
                mode: AttachMode::New,
                open_settings: None,
                plain: false,
                conn: None,
                join: None,
                active_tab: None,
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
        let plan = ConnectPlan {
            endpoint: host.endpoint(),
            mode: AttachMode::New,
            open_settings: None,
            plain,
            conn: None,
            join: None,
            active_tab: None,
        };
        self.open_connect_tab(plan, window, cx);
    }

    // ---- actions: workspaces ----------------------------------------------------

    fn on_switch_workspace(&mut self, _: &SwitchWorkspace, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
        } else {
            self.open_switcher(None, window, cx);
        }
    }

    fn on_new_workspace(&mut self, _: &NewWorkspace, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.new_workspace(window, cx);
    }

    fn on_rename_workspace(&mut self, _: &RenameWorkspace, window: &mut Window, cx: &mut Context<'_, Self>) {
        let id = self.active_shown().map(|i| self.shown[i].id);
        self.open_switcher(id, window, cx);
    }

    fn on_close_workspace(&mut self, _: &CloseWorkspace, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.active_shown() else {
            let toast = Toast::info("No workspace here").body("This tab isn't kept by a goattyd (or is still connecting).");
            toast::show_in(window, cx, toast.key("workspace.close"));
            return;
        };
        let (conn, id) = (self.shown[i].conn.clone(), self.shown[i].id);
        self.close_workspace(&conn, id, window, cx);
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
        // Saved when the drag ends (see `render`), not on every move.
        self.divider_dragging = true;
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
        let pane = self.focused_view().map(|v| {
            let v = v.read(cx);
            (v.process().map(str::to_string), v.at_prompt())
        });
        let palette = cx.new(|cx| {
            let mut p = CommandPalette::new(scope, window, cx);
            if let Some((process, at_prompt)) = pane {
                p.set_pane(process, at_prompt, cx);
            }
            p
        });
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
/// What a detach did, to undo it.
#[derive(Clone)]
pub(super) enum Undo {
    /// These tabs became detached tabs of the workspace.
    Tabs { conn: Arc<Connection>, workspace: goatty_proto::WorkspaceId, ids: Vec<u64> },
    /// The window stopped showing the host's workspace.
    Host { conn: Arc<Connection>, workspace: goatty_proto::WorkspaceId },
}

/// What a detach did, per host.
pub(super) type Detached = Vec<Undo>;

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
    let plain = match &t.host {
        Some(Spawner::Plain { host, .. }) => Some(host.clone()),
        _ => None,
    };
    Some(SavedTab { id: t.id, name: t.name.clone(), tree, plain, detached_at: None })
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
            if self.divider_dragging {
                // The split's sizes are saved now; changes from other
                // windows that waited for it apply.
                self.divider_dragging = false;
                cx.defer_in(window, |this, window, cx| {
                    let pending: Vec<(usize, goatty_proto::WorkspaceLayout)> =
                        this.shown.iter_mut().filter_map(|s| s.pending.take().map(|p| (s.conn.key(), p))).collect();
                    for (key, layout) in pending {
                        this.on_workspace_event(key, goatty_client::WorkspaceEvent::Changed(layout), window, cx);
                    }
                    this.save_layout(cx);
                });
            }
        }
        self.moved_unpainted = false;
        let tabs = &cx.global::<AppSettings>().client.tabs;
        // The workspace chip lives in the tab bar: it shows with two workspaces.
        let show_bar = tabs.show == TabBarShow::Always || self.tabs.len() > 1 || self.shows_workspaces();
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
            .on_action(cx.listener(Self::on_switch_workspace))
            .on_action(cx.listener(|this, _: &NextWorkspace, w, cx| this.cycle_workspace(true, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousWorkspace, w, cx| this.cycle_workspace(false, w, cx)))
            .on_action(cx.listener(Self::on_new_workspace))
            .on_action(cx.listener(Self::on_rename_workspace))
            .on_action(cx.listener(Self::on_close_workspace))
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
                // Bound to a command for another program (vim's, outside vim):
                // the key goes to the terminal as if nothing were bound.
                let fits_not = this.focused_view().is_some_and(|v| {
                    let v = v.read(cx);
                    crate::plugin_commands::passes_through(&act.id, v.process(), v.at_prompt(), cx)
                });
                if fits_not {
                    cx.propagate();
                    return;
                }
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
            .children(self.render_switcher(vertical, cx))
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
