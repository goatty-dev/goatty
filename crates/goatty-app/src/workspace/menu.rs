//! The workspace's right-click menus (#186), drawn by
//! [`crate::ui::context_menu`]:
//!
//! * a tab's: rename, duplicate, close it / the others / the ones after it,
//! * a pane's: the pane's own part ([`TerminalView::menu_entries`]: link,
//!   clipboard, command line, output), then splitting, a "Pane" submenu
//!   (swap with a neighbour, move to a new tab, zoom, equalize, close),
//!   the active host's plugin commands that asked to be here, and the
//!   command palette and settings. In a program that uses the mouse, a
//!   note says how the click reaches it.
//!
//! One menu is open at a time. Items run after it closed and focus is
//! back on the terminal, so the actions they dispatch reach that pane.

use goatty_config::{RightClick, TabStyle};
use gpui::{AppContext, Context, Entity, Pixels, Point, Window};

use super::layout::Direction;
use super::{PaneId, Workspace};
use crate::plugin_commands::{MenuCommands, start};
use crate::settings::AppSettings;
use crate::terminal::TerminalView;
use crate::ui::context_menu::{ContextMenu, Entry, Item, MenuEvent};
use crate::ui::icons::Icon;

/// What the open menu is about: it closes when that goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuOwner {
    Tab(u64),
    Pane(PaneId),
}

/// The open menu.
pub(super) struct OpenMenu {
    menu: Entity<ContextMenu>,
    pub owner: MenuOwner,
    _subscription: gpui::Subscription,
}

/// More plugin entries than this go into a submenu.
const INLINE_PLUGIN_ITEMS: usize = 3;

impl Workspace {
    /// Open `entries` as the window's menu for `owner` at `at`.
    fn open_menu(
        &mut self,
        owner: MenuOwner,
        entries: Vec<Entry>,
        at: Point<Pixels>,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.finish_rename(true, window, cx);
        let menu = cx.new(|cx| ContextMenu::new(entries, at, keyboard, window, cx));
        let sub = cx.subscribe_in(&menu, window, |this, _, ev: &MenuEvent, window, cx| match ev {
            MenuEvent::Run(handler) => {
                let handler = handler.clone();
                this.close_menu(window, cx);
                // After this update: handlers may update the workspace.
                window.defer(cx, move |window, cx| handler(window, cx));
            }
            MenuEvent::Dismissed => this.close_menu(window, cx),
        });
        self.menu = Some(OpenMenu { menu, owner, _subscription: sub });
        cx.notify();
    }

    /// Close the menu and give focus back to the terminal.
    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.menu.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    pub(super) fn render_menu(&self) -> Option<Entity<ContextMenu>> {
        self.menu.as_ref().map(|m| m.menu.clone())
    }

    pub(super) fn menu_focus(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        use gpui::Focusable;
        self.menu.as_ref().map(|m| m.menu.focus_handle(cx))
    }

    /// Close the menu if it's about tab `tab` or one of its panes (going away).
    pub(super) fn close_menu_of_tab(&mut self, tab: u64, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(owner) = self.menu.as_ref().map(|m| m.owner) else { return };
        let mine = match owner {
            MenuOwner::Tab(t) => t == tab,
            MenuOwner::Pane(p) => self.tab_index(tab).is_some_and(|i| self.tabs[i].pane(p).is_some()),
        };
        if mine {
            self.close_menu(window, cx);
        }
    }

    // ---- a tab's menu -----------------------------------------------------------

    pub(super) fn open_tab_menu(&mut self, tab: u64, at: Point<Pixels>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(index) = self.tab_index(tab) else { return };
        let vertical = cx.global::<AppSettings>().client.tabs.style == TabStyle::Vertical;
        let me = cx.entity().downgrade();
        // Each item finds its tab again by id: tabs may move meanwhile.
        let on = |f: fn(&mut Workspace, usize, &mut Window, &mut Context<'_, Workspace>)| {
            let me = me.clone();
            move |window: &mut Window, cx: &mut gpui::App| {
                let _ = me.update(cx, |this, cx| {
                    if let Some(i) = this.tab_index(tab) {
                        f(this, i, window, cx);
                    }
                });
            }
        };
        let last = index + 1 == self.tabs.len();
        let entries = vec![
            Item::new("Rename…", on(|this, i, w, cx| this.start_rename(i, w, cx))).icon(Icon::Pencil).into(),
            Item::new("Duplicate", on(|this, i, w, cx| this.duplicate_tab_at(i, w, cx))).icon(Icon::Copy).into(),
            Entry::Separator,
            Item::new("Close tab", on(|this, i, w, cx| this.close_tab_at(i, w, cx))).icon(Icon::Close).into(),
            Item::new("Close other tabs", on(|this, i, w, cx| this.close_tabs(this.other_tabs(i), w, cx)))
                .disabled((self.tabs.len() < 2).then_some("This is the only tab"))
                .into(),
            Item::new(
                if vertical { "Close tabs below" } else { "Close tabs to the right" },
                on(|this, i, w, cx| {
                    let after = this.tabs[i + 1..].iter().map(|t| t.id).collect();
                    this.close_tabs(after, w, cx);
                }),
            )
            .disabled(last.then_some(if vertical { "No tabs below this one" } else { "No tabs to the right" }))
            .into(),
        ];
        self.open_menu(MenuOwner::Tab(tab), entries, at, false, window, cx);
    }

    // ---- a pane's menu ----------------------------------------------------------

    pub(super) fn open_pane_menu(
        &mut self,
        pane: PaneId,
        at: Point<Pixels>,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((index, p)) = self.find_pane(pane) else { return };
        let view = p.view.clone();
        let mut entries = view.update(cx, |v, cx| v.menu_entries(cx));

        let tab = &self.tabs[index];
        let split = tab.panes.len() > 1;
        entries.push(Entry::Separator);
        entries.push(Item::action("Split right", "split_right", cx).icon(Icon::Columns2).into());
        entries.push(Item::action("Split down", "split_down", cx).icon(Icon::Rows2).into());
        let mut pane_items: Vec<Entry> = Vec::new();
        if split && !tab.zoomed {
            for (dir, label, action) in [
                (Direction::Left, "Swap with the pane on the left", "swap_pane_left"),
                (Direction::Right, "Swap with the pane on the right", "swap_pane_right"),
                (Direction::Up, "Swap with the pane above", "swap_pane_up"),
                (Direction::Down, "Swap with the pane below", "swap_pane_down"),
            ] {
                if tab.tree.neighbor(pane, dir).is_some() {
                    pane_items.push(Item::action(label, action, cx).icon(Icon::ArrowLeftRight).into());
                }
            }
        }
        if split {
            pane_items.push(Item::action("Move to a new tab", "move_pane_to_new_tab", cx).icon(Icon::SquareArrowOutUpRight).into());
            pane_items.push(Entry::Separator);
            let zoom = if tab.zoomed { "Back to the split" } else { "Zoom (fill the tab)" };
            pane_items.push(Item::action(zoom, "toggle_pane_zoom", cx).icon(Icon::Maximize2).into());
            pane_items.push(Item::action("Make all panes the same size", "equalize_panes", cx).into());
        }
        pane_items.push(Entry::Separator);
        pane_items.push(Item::action("New tab", "new_tab", cx).icon(Icon::Plus).into());
        pane_items.push(Entry::Separator);
        pane_items.push(Item::action("Close the pane", "close_pane", cx).icon(Icon::Close).danger().into());
        entries.push(Item::submenu("Pane", pane_items).icon(Icon::AppWindow).into());

        let plugins = plugin_entries(&view, cx);
        if !plugins.is_empty() {
            entries.push(Entry::Separator);
            let count = plugins.iter().filter(|e| matches!(e, Entry::Item(_))).count();
            if count > INLINE_PLUGIN_ITEMS {
                entries.push(Item::submenu("Plugins", plugins).icon(Icon::Puzzle).into());
            } else {
                entries.extend(plugins);
            }
        }

        entries.push(Entry::Separator);
        entries.push(Item::action("Command palette…", "command_palette", cx).icon(Icon::CommandKey).into());
        entries.push(Item::action("Settings", "open_settings", cx).icon(Icon::Sliders).into());

        let v = view.read(cx);
        if v.program_uses_mouse() {
            let program = v.process().filter(|p| !super::icons::is_shell(p)).unwrap_or("This program").to_string();
            let note = match cx.global::<AppSettings>().client.mouse.right_click {
                RightClick::Always => format!("{program} uses the mouse: Shift+right-click sends the click to it."),
                _ => format!("{program} uses the mouse and gets right clicks; Shift+right-click opens this menu."),
            };
            entries.push(Entry::Separator);
            entries.push(Entry::Note(Icon::MousePointerClick, note.into()));
        }
        self.open_menu(MenuOwner::Pane(pane), entries, at, keyboard, window, cx);
    }

    // ---- moving panes ---------------------------------------------------------

    /// Let the focused pane trade places with its neighbour on the `dir` side.
    pub(super) fn swap_pane(&mut self, dir: Direction, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab_mut() else { return };
        let focused = tab.focused;
        let Some(other) = (!tab.zoomed).then(|| tab.tree.neighbor(focused, dir)).flatten() else {
            cx.propagate();
            return;
        };
        tab.tree.swap(focused, other);
        self.save_layout(cx);
        cx.notify();
    }

    /// Take the focused pane out of its split into a tab of its own, after
    /// the current one.
    pub(super) fn move_pane_to_new_tab(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        if tab.panes.len() < 2 {
            let toast =
                crate::toast::Toast::info("This pane is the tab's only one").body("Moving it to a new tab works for a pane of a split.");
            crate::toast::show_in(window, cx, toast.key("pane.move"));
            return;
        }
        let id = tab.focused;
        let new_tab = self.next_id();
        let tab = &mut self.tabs[self.active];
        let pos = tab.tree.leaves().iter().position(|&p| p == id).unwrap_or(0);
        tab.tree.remove(id);
        let Some(at) = tab.panes.iter().position(|p| p.id == id) else { return };
        let pane = tab.panes.remove(at);
        let left = tab.tree.leaves();
        tab.focused = left[pos.saturating_sub(1).min(left.len() - 1)];
        tab.zoomed = false;
        let host = tab.host.clone();
        self.tabs.insert(
            self.active + 1,
            super::Tab {
                id: new_tab,
                tree: super::Node::Leaf(id),
                panes: vec![pane],
                focused: id,
                zoomed: false,
                name: None,
                host,
                connecting: None,
                activity: None,
            },
        );
        self.activate(self.active + 1, window, cx);
    }
}

/// The active host's plugin commands for `view`'s menu: those that fit what's
/// in the pane (a selection, a prompt), grouped under each plugin's name.
fn plugin_entries(view: &Entity<TerminalView>, cx: &gpui::App) -> Vec<Entry> {
    use goatty_plugin::commands::MenuWhen;
    let Some(commands) = cx.try_global::<MenuCommands>() else { return Vec::new() };
    let v = view.read(cx);
    let selection = v.session().terminal.selection_text().filter(|t| !t.trim().is_empty());
    let at_prompt = !v.session().terminal.command_running() && v.process().is_none_or(super::icons::is_shell);
    let mut out = Vec::new();
    let mut last_plugin: Option<&str> = None;
    for c in &commands.0 {
        let fits = match c.when() {
            MenuWhen::Always => true,
            MenuWhen::Selection => selection.is_some(),
            MenuWhen::Prompt => at_prompt,
        };
        if !fits {
            continue;
        }
        if last_plugin != Some(c.plugin.as_str()) {
            out.push(Entry::Header(c.plugin.clone().into()));
            last_plugin = Some(c.plugin.as_str());
        }
        let (cmd, sel) = (c.clone(), selection.clone());
        out.push(
            Item::new(c.command.title.clone(), move |window, cx| {
                start(&cmd.plugin, &cmd.command, &cmd.origin, sel.as_deref(), window, cx);
            })
            .icon(c.icon())
            .into(),
        );
    }
    out
}
