//! The workspace's right-click menus (#186), drawn by
//! [`crate::ui::context_menu`]:
//!
//! * a tab's: new tab, rename, duplicate, move it left/right, bring its
//!   panes into another tab, close it / the others / the ones after it,
//! * a pane's: the pane's own part ([`TerminalView::menu_entries`]: link,
//!   clipboard, command line, output), then splitting, a "Pane" submenu
//!   (split left/up, swap with a neighbour, rotate, zoom, equalize, close),
//!   "Move pane to" (a new tab, another tab on the same host), the active
//!   host's plugin commands that asked to be here, and the command palette
//!   and settings. In a program that uses the mouse, a note says how the
//!   click reaches it,
//! * a divider's: even out that split, or every split,
//! * a host group's label (`groups`): a new tab on that host, fold or
//!   unfold, show only this host, every host, the host's settings, close
//!   its tabs.
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
use crate::ui::context_menu::{self, ContextMenu, Entry, Item, MenuEvent};
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

/// What a host group's menu item does to the group.
type GroupAction = dyn Fn(&mut Workspace, &super::groups::HostKey, &mut Window, &mut Context<'_, Workspace>);

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
        let mut entries = vec![
            Item::action("New tab", "new_tab", cx).icon(Icon::Plus).into(),
            Entry::Separator,
            Item::new("Rename…", on(|this, i, w, cx| this.start_rename(i, w, cx))).icon(Icon::Pencil).into(),
            Item::new("Duplicate", on(|this, i, w, cx| this.duplicate_tab_at(i, w, cx))).icon(Icon::Copy).into(),
        ];
        if self.tabs.len() > 1 {
            let (back, on_back) = if vertical { ("Move up", "This is the first tab") } else { ("Move left", "This is the first tab") };
            let (fwd, on_fwd) = if vertical { ("Move down", "This is the last tab") } else { ("Move right", "This is the last tab") };
            // The keys move the active tab, so they're shown on its menu only.
            let keys = |name| (index == self.active).then(|| context_menu::shortcut(name, cx)).flatten();
            let mut item = Item::new(back, on(|this, i, w, cx| this.move_tab_to(this.tabs[i].id, i.saturating_sub(1), w, cx)))
                .icon(if vertical { Icon::ArrowUp } else { Icon::ArrowLeft })
                .disabled((index == 0).then_some(on_back));
            item.shortcut = keys("move_tab_left");
            entries.push(item.into());
            let mut item = Item::new(fwd, on(|this, i, w, cx| this.move_tab_to(this.tabs[i].id, i + 1, w, cx)))
                .icon(if vertical { Icon::ArrowDown } else { Icon::ArrowRight })
                .disabled(last.then_some(on_fwd));
            item.shortcut = keys("move_tab_right");
            entries.push(item.into());
            // Its panes into another tab, side by side with that tab's.
            let mut targets: Vec<Entry> = vec![Entry::Header("Put its panes beside those of".into())];
            for i in (0..self.tabs.len()).filter(|&i| i != index) {
                let into = self.tabs[i].id;
                let me = me.clone();
                targets.push(
                    Item::new(self.tab_choice_label(i, cx), move |window: &mut Window, cx: &mut gpui::App| {
                        let _ = me.update(cx, |this, cx| this.merge_tab(tab, into, window, cx));
                    })
                    .disabled(self.cannot_join(index, i))
                    .into(),
                );
            }
            entries.push(Item::submenu("Move into tab", targets).icon(Icon::SquareArrowOutUpRight).into());
        }
        entries.extend([
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
        ]);
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
        let mut pane_items: Vec<Entry> = vec![
            Item::action("Split left", "split_left", cx).icon(Icon::Columns2).into(),
            Item::action("Split up", "split_up", cx).icon(Icon::Rows2).into(),
        ];
        if split && !tab.zoomed {
            pane_items.push(Entry::Separator);
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
            // With two panes, rotating is the same as swapping.
            if tab.panes.len() > 2 {
                pane_items.push(Item::action("Rotate the panes", "rotate_panes", cx).icon(Icon::RotateCw).into());
                pane_items.push(Item::action("Rotate the panes back", "rotate_panes_back", cx).icon(Icon::RotateCcw).into());
            }
        }
        if split {
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

        // Where the pane can go: a new tab (from a split), the tabs on this host.
        let others: Vec<usize> = (0..self.tabs.len()).filter(|&i| i != index).collect();
        if split || !others.is_empty() {
            let me = cx.entity().downgrade();
            let mut moves: Vec<Entry> = vec![
                Item::action("New tab", "move_pane_to_new_tab", cx)
                    .icon(Icon::SquareArrowOutUpRight)
                    .disabled((!split).then_some("It's the tab's only pane"))
                    .into(),
            ];
            if !others.is_empty() {
                moves.push(Entry::Separator);
            }
            for i in others {
                let to = self.tabs[i].id;
                let me = me.clone();
                moves.push(
                    Item::new(self.tab_choice_label(i, cx), move |window: &mut Window, cx: &mut gpui::App| {
                        let _ = me.update(cx, |this, cx| this.move_pane(pane, Some(to), window, cx));
                    })
                    .disabled(self.cannot_join(index, i))
                    .into(),
                );
            }
            entries.push(Item::submenu("Move pane to", moves).icon(Icon::SquareArrowOutUpRight).into());
        }

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
}

impl Workspace {
    // ---- a host group's menu ------------------------------------------------------

    pub(super) fn open_group_menu(
        &mut self,
        key: super::groups::HostKey,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let groups = self.groups(cx);
        let Some(g) = groups.iter().find(|g| g.key == key) else { return };
        let me = cx.entity().downgrade();
        let on = |f: Box<GroupAction>| {
            let (me, key) = (me.clone(), key.clone());
            move |window: &mut Window, cx: &mut gpui::App| {
                let _ = me.update(cx, |this, cx| f(this, &key, window, cx));
            }
        };
        let alone = groups.len() < 2;
        let mut heading = g.label.to_string();
        if let Some(d) = &g.detail {
            heading.push_str(&format!(" · {d}"));
        }
        let n = g.tabs.len();
        let mut entries = vec![
            Entry::Header(heading.into()),
            Item::new(format!("New tab on {}", g.label), on(Box::new(|this, k, w, cx| this.new_tab_in_group(k, w, cx))))
                .icon(Icon::Plus)
                .disabled(matches!(key, super::groups::HostKey::Connecting(_)).then_some("Still connecting"))
                .into(),
            Entry::Separator,
        ];
        if g.folded {
            entries.push(
                Item::new("Show its tabs", on(Box::new(|this, k, w, cx| this.fold_group(k, false, w, cx)))).icon(Icon::ChevronDown).into(),
            );
        } else {
            entries.push(
                Item::new("Fold its tabs", on(Box::new(|this, k, w, cx| this.fold_group(k, true, w, cx))))
                    .icon(Icon::ChevronRight)
                    .disabled(alone.then_some("The only host in this window"))
                    .into(),
            );
        }
        let mut only = Item::new(format!("Show only {}", g.label), on(Box::new(|this, k, w, cx| this.show_only_group(k, w, cx))))
            .disabled(alone.then_some("The only host in this window"));
        only.shortcut = crate::ui::context_menu::shortcut("show_only_this_host", cx);
        entries.push(only.into());
        if groups.iter().any(|g| g.folded) {
            entries.push(Item::action("Show every host", "show_all_hosts", cx).into());
        }
        entries.push(Entry::Separator);
        entries.push(
            Item::new(
                "Settings for this host",
                on(Box::new(|this, k, w, cx| {
                    // The settings window follows the active tab's host.
                    if let Some(g) = this.groups(cx).into_iter().find(|g| g.key == *k)
                        && !g.active
                    {
                        this.activate(g.tabs[0], w, cx);
                    }
                    w.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
                })),
            )
            .icon(Icon::Sliders)
            .disabled(matches!(key, super::groups::HostKey::Connecting(_)).then_some("Still connecting"))
            .into(),
        );
        entries.push(Entry::Separator);
        let ids = g.ids.clone();
        let close_label = if n == 1 { "Close its tab".to_string() } else { format!("Close its {n} tabs") };
        entries.push(
            Item::new(close_label, {
                let me = me.clone();
                move |window: &mut Window, cx: &mut gpui::App| {
                    let ids = ids.clone();
                    let _ = me.update(cx, |this, cx| this.close_tabs(ids, window, cx));
                }
            })
            .icon(Icon::Close)
            .danger()
            .into(),
        );
        let owner = MenuOwner::Tab(g.ids[0]);
        self.open_menu(owner, entries, at, false, window, cx);
    }

    // ---- a divider's menu -------------------------------------------------------

    /// The menu of the divider between the parts of the split at `path` in
    /// tab `tab`.
    pub(super) fn open_divider_menu(
        &mut self,
        tab: u64,
        path: Vec<usize>,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let me = cx.entity().downgrade();
        let entries = vec![
            {
                let mut item = Item::new("Make these panes the same size", move |_: &mut Window, cx: &mut gpui::App| {
                    let _ = me.update(cx, |this, cx| this.equalize_split(tab, &path, cx));
                })
                .icon(Icon::Columns2);
                item.shortcut = Some("Double-click".into());
                item.into()
            },
            Item::action("Make all panes the same size", "equalize_panes", cx).into(),
            Entry::Separator,
            Entry::Note(Icon::MousePointerClick, "Drag the line to resize the panes.".into()),
        ];
        self.open_menu(MenuOwner::Tab(tab), entries, at, false, window, cx);
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
                start(&cmd.plugin_id, &cmd.plugin, &cmd.command, &cmd.origin, sel.as_deref(), window, cx);
            })
            .icon(c.icon())
            .into(),
        );
    }
    out
}
