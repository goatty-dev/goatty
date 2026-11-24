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
//! * the new-tab button's (#252): a new tab here or on another host of the
//!   window, a new tab on any known host (submenu), reattaching what was
//!   detached, the sessions picker, a new window, and a "Workspace" submenu
//!   (switch, new, rename, close; #215),
//! * a workspace's (the switcher's chip and rows): switch, rename, open in
//!   a new window, close,
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
use crate::plugin_commands::{MenuCommands, run_menu_command};
use crate::settings::AppSettings;
use crate::terminal::TerminalView;
use crate::ui::context_menu::{self, ContextMenu, Entry, Item, MenuEvent};
use crate::ui::icons::Icon;

/// What the open menu is about: it closes when that goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuOwner {
    Tab(u64),
    Pane(PaneId),
    /// The new-tab button (the strip's, the sidebar's).
    NewTabButton,
    /// The workspace chip or a row of the switcher.
    Workspace,
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
            MenuOwner::NewTabButton | MenuOwner::Workspace => false,
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
        // The keys act on the active tab, so they're shown on its menu only.
        let keys = |name| (index == self.active).then(|| context_menu::shortcut(name, cx)).flatten();
        if self.tabs.len() > 1 {
            let (back, on_back) = if vertical { ("Move up", "This is the first tab") } else { ("Move left", "This is the first tab") };
            let (fwd, on_fwd) = if vertical { ("Move down", "This is the last tab") } else { ("Move right", "This is the last tab") };
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
        entries.push(Entry::Separator);
        let mut detach_tab = Item::new("Detach tab", on(|this, i, w, cx| this.detach_tab_at(i, w, cx)))
            .icon(Icon::Unlink)
            .disabled(self.cannot_detach_tab(index, cx));
        detach_tab.shortcut = keys("detach_tab");
        entries.push(detach_tab.into());
        let others = self.other_tabs(index);
        let cannot_detach_others = if self.tabs.len() < 2 { Some("This is the only tab") } else { self.cannot_detach_tabs(&others, cx) };
        let mut detach_others =
            Item::new("Detach other tabs", on(|this, i, w, cx| this.detach_tabs(this.other_tabs(i), w, cx))).disabled(cannot_detach_others);
        detach_others.shortcut = keys("detach_other_tabs");
        entries.push(detach_others.into());
        if self.groups_shown(cx) {
            let key = self.host_key(&self.tabs[index]);
            if let Some(g) = self.groups(cx).into_iter().find(|g| g.key == key) {
                let mut detach_host = Item::new(
                    format!("Detach from {}", g.label),
                    on(|this, i, w, cx| {
                        let key = this.host_key(&this.tabs[i]);
                        this.detach_group(&key, w, cx);
                    }),
                )
                .disabled(self.cannot_detach_tabs(&g.ids, cx));
                detach_host.shortcut = keys("detach_host");
                entries.push(detach_host.into());
            }
        }
        entries.push(Entry::Separator);
        let mut close_tab = Item::new("Close tab", on(|this, i, w, cx| this.close_tab_at(i, w, cx))).icon(Icon::Close);
        close_tab.shortcut = keys("close_tab");
        entries.push(close_tab.into());
        let mut close_others = Item::new("Close other tabs", on(|this, i, w, cx| this.close_tabs(this.other_tabs(i), w, cx)))
            .disabled((self.tabs.len() < 2).then_some("This is the only tab"));
        close_others.shortcut = keys("close_other_tabs");
        entries.push(close_others.into());
        entries.push(
            Item::new(
                if vertical { "Close tabs below" } else { "Close tabs to the right" },
                on(|this, i, w, cx| {
                    let after = this.tabs[i + 1..].iter().map(|t| t.id).collect();
                    this.close_tabs(after, w, cx);
                }),
            )
            .disabled(last.then_some(if vertical { "No tabs below this one" } else { "No tabs to the right" }))
            .into(),
        );
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
        {
            let me = cx.entity().downgrade();
            let mut item = Item::new("Detach the pane", move |window: &mut Window, cx: &mut gpui::App| {
                let _ = me.update(cx, |this, cx| this.detach_pane_by_id(pane, window, cx));
            })
            .icon(Icon::Unlink)
            .disabled(self.cannot_detach_pane(pane, cx));
            item.shortcut = context_menu::shortcut("detach_pane", cx);
            pane_items.push(item.into());
        }
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
        if !g.local {
            // The Hosts page lists the hosts of the list (plain ones too), not tailnet ones or typed-in ones.
            let host = self.tabs.get(g.tabs[0]).and_then(|t| t.host.as_ref());
            let ssh = (matches!(host, Some(crate::session::Spawner::Plain { .. }))
                || matches!(host.and_then(super::host_ref_of), Some(crate::hosts::HostRef::Ssh(_))))
                && crate::hosts::current(cx).iter().any(|e| e.host == *g.label);
            entries.push(Self::group_color_menu(&g.label, ssh, cx).into());
        }
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
        // Tabs detached there before (or left by a closed window), all at once.
        let spawner = self.tabs.get(g.tabs[0]).and_then(|t| t.host.as_ref());
        let plain = match spawner {
            Some(crate::session::Spawner::Plain { host, .. }) => Some(host.clone()),
            _ => None,
        };
        let reattach = match spawner.and_then(super::host_ref_of) {
            Some(host) => Item::new(format!("Reattach detached tabs on {}", g.label), {
                let me = me.clone();
                move |window: &mut Window, cx: &mut gpui::App| {
                    let (host, plain) = (host.clone(), plain.clone());
                    let _ = me.update(cx, |this, cx| this.reattach_detached_on(host, plain, window, cx));
                }
            }),
            None => Item::new("Reattach detached tabs", |_: &mut Window, _: &mut gpui::App| {})
                .disabled(Some(if matches!(key, super::groups::HostKey::Connecting(_)) { "Still connecting" } else { "Not on a goattyd" })),
        };
        entries.push(reattach.icon(Icon::History).into());
        let mut detach = Item::new(format!("Detach from {}", g.label), on(Box::new(|this, k, w, cx| this.detach_group(k, w, cx))))
            .icon(Icon::Unlink)
            .disabled(self.cannot_detach_tabs(&g.ids, cx));
        if g.active {
            detach.shortcut = crate::ui::context_menu::shortcut("detach_host", cx);
        }
        entries.push(detach.into());
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

    /// "Color" in a host group's menu (#257): the theme's colors by name,
    /// back to automatic, and the Hosts page for any other color.
    fn group_color_menu(host: &str, ssh: bool, cx: &gpui::App) -> Item {
        use goatty_config::HostColor;
        let chosen = cx.global::<AppSettings>().client.tabs.host_color(host);
        let set = |color: Option<HostColor>| {
            let host = host.to_string();
            move |_: &mut Window, cx: &mut gpui::App| {
                AppSettings::update(cx, |s| s.tabs.set_host_color(&host, color));
            }
        };
        let mut entries: Vec<Entry> = HostColor::PALETTE
            .iter()
            .map(|&(name, ansi)| {
                let mut label = name.to_string();
                label[..1].make_ascii_uppercase();
                let mut item = Item::new(label, set(Some(HostColor::Ansi(ansi))));
                if chosen == Some(HostColor::Ansi(ansi)) {
                    item = item.icon(Icon::Check);
                }
                item.into()
            })
            .collect();
        entries.push(Entry::Separator);
        let mut automatic = Item::new("Automatic", set(None));
        if chosen.is_none() {
            automatic = automatic.icon(Icon::Check);
        }
        entries.push(automatic.into());
        let mut custom = Item::new("Custom color…", {
            let host = host.to_string();
            move |_: &mut Window, cx: &mut gpui::App| crate::settings_window::open_host_color(cx, &host)
        });
        if matches!(chosen, Some(HostColor::Custom(_))) {
            custom = custom.icon(Icon::Check);
        }
        custom = custom.disabled((!ssh).then_some("Add this host in Settings → Hosts first"));
        entries.push(custom.into());
        Item::submenu("Color", entries).icon(Icon::Palette)
    }

    // ---- the new-tab button's menu -------------------------------------------------

    /// What a right click on the new-tab button offers: where a new tab
    /// can go, and getting detached tabs back.
    pub(super) fn open_new_tab_menu(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<'_, Self>) {
        use crate::hosts::HostRef;
        let me = cx.entity().downgrade();
        let on_host = |host: HostRef, plain: bool| {
            let me = me.clone();
            move |window: &mut Window, cx: &mut gpui::App| {
                let _ = me.update(cx, |this, cx| this.new_tab_on_host(host.clone(), plain, window, cx));
            }
        };
        let groups = if self.groups_shown(cx) { self.groups(cx) } else { Vec::new() };
        let active = groups.iter().find(|g| g.active);
        let mut entries: Vec<Entry> = Vec::new();
        // Bound to the host it names: the active tab may change while the menu is open.
        let here = match active {
            Some(g) => {
                let (key, me) = (g.key.clone(), me.clone());
                let mut item = Item::new(format!("New tab on {}", g.label), move |window: &mut Window, cx: &mut gpui::App| {
                    let _ = me.update(cx, |this, cx| this.new_tab_in_group(&key, window, cx));
                });
                item.shortcut = context_menu::shortcut("new_tab", cx);
                item
            }
            None => Item::action("New tab", "new_tab", cx),
        };
        entries.push(here.icon(Icon::Plus).into());
        // The window's other hosts, one click each.
        for g in groups.iter().filter(|g| !g.active) {
            let key = g.key.clone();
            let me = me.clone();
            entries.push(
                Item::new(format!("New tab on {}", g.label), move |window: &mut Window, cx: &mut gpui::App| {
                    let _ = me.update(cx, |this, cx| this.new_tab_in_group(&key, window, cx));
                })
                .icon(if g.local { Icon::Monitor } else { Icon::Server })
                .disabled(matches!(g.key, super::groups::HostKey::Connecting(_)).then_some("Still connecting"))
                .into(),
            );
        }
        // Every host goatty knows: this computer, your SSH hosts, paired
        // tailnet computers.
        let mut hosts: Vec<Entry> = vec![Item::new("This computer", on_host(HostRef::Local, false)).icon(Icon::Monitor).into()];
        let ssh = crate::hosts::current(cx);
        if !ssh.is_empty() {
            hosts.push(Entry::Header("SSH".into()));
            for e in ssh {
                let mut item = Item::new(e.label.clone(), on_host(HostRef::Ssh(e.host.clone()), e.plain)).icon(Icon::Server);
                item.shortcut = e.plain.then(|| "plain SSH".into());
                hosts.push(item.into());
            }
        }
        let tailnet: Vec<_> = crate::tailnet::current(cx).into_iter().filter(|e| e.paired).collect();
        if !tailnet.is_empty() {
            hosts.push(Entry::Header("Tailnet".into()));
            for e in tailnet {
                let host = HostRef::Tailnet { dns: e.dns.clone(), name: e.name.clone() };
                hosts.push(Item::new(e.name.clone(), on_host(host, false)).icon(Icon::Globe).into());
            }
        }
        hosts.push(Entry::Separator);
        hosts.push(Item::new("Another host…", |window: &mut Window, cx: &mut gpui::App| crate::hosts::ask_new_tab(window, cx)).into());
        entries.push(Item::submenu("New tab on…", hosts).icon(Icon::Server).into());
        entries.push(Entry::Separator);
        let not_here = match self.active_tab().map(|t| t.host.as_ref().map(super::host_ref_of)) {
            Some(Some(Some(_))) => None,
            Some(None) => Some("This tab is still connecting"),
            _ => Some("This tab isn't kept by a goattyd"),
        };
        // `~/.ssh/config` may have changed since.
        crate::hosts::refresh(cx);
        entries.push(Item::action("Reattach detached tabs", "reattach_detached", cx).icon(Icon::History).disabled(not_here).into());
        entries.push(Item::action("Sessions on this host…", "list_sessions", cx).icon(Icon::AppWindow).disabled(not_here).into());
        entries.push(Entry::Separator);
        let connecting = self.active_tab().is_none_or(|t| t.host.is_none()).then_some("This tab is still connecting");
        entries.push(Item::action("Duplicate this tab", "duplicate_tab", cx).icon(Icon::Copy).disabled(connecting).into());
        entries.push(Item::action("New window", "new_window", cx).into());
        // Workspaces (#215): found here even with one, where there's no chip.
        let no_workspace = self.active_shown().is_none().then_some("This tab isn't kept by a goattyd");
        let mut workspace: Vec<Entry> = vec![
            Item::action("Switch workspace…", "switch_workspace", cx).icon(Icon::Rows2).disabled(no_workspace).into(),
            Item::action("New workspace", "new_workspace", cx).icon(Icon::Plus).disabled(no_workspace).into(),
        ];
        if self.shows_workspaces() {
            workspace.push(Entry::Separator);
            workspace.push(Item::action("Rename the workspace…", "rename_workspace", cx).icon(Icon::Pencil).into());
            workspace.push(Item::action("Close the workspace…", "close_workspace", cx).icon(Icon::Trash).into());
        }
        entries.push(Item::submenu("Workspace", workspace).icon(Icon::Rows2).into());
        self.open_menu(MenuOwner::NewTabButton, entries, at, false, window, cx);
    }

    // ---- a workspace's menu ---------------------------------------------------

    /// The menu of workspace `id` on `conn` (the chip's, a switcher row's).
    pub(super) fn open_workspace_menu(
        &mut self,
        conn: std::sync::Arc<goatty_client::Connection>,
        id: goatty_proto::WorkspaceId,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let me = cx.entity().downgrade();
        let current = self.shown_index(&conn).is_some_and(|i| self.shown[i].id == id);
        let only = self.shown_index(&conn).is_some_and(|i| self.shown[i].list.len() < 2);
        let on = |f: fn(
            &mut Workspace,
            &std::sync::Arc<goatty_client::Connection>,
            goatty_proto::WorkspaceId,
            &mut Window,
            &mut Context<'_, Workspace>,
        )| {
            let (me, conn) = (me.clone(), conn.clone());
            move |window: &mut Window, cx: &mut gpui::App| {
                let _ = me.update(cx, |this, cx| f(this, &conn, id, window, cx));
            }
        };
        let mut entries: Vec<Entry> = Vec::new();
        if !current {
            entries.push(Item::new("Switch to it", on(|this, c, id, w, cx| this.switch_workspace(c, id, w, cx))).icon(Icon::Check).into());
        }
        entries.push(Item::new("Rename…", on(|this, _, id, w, cx| this.open_switcher(Some(id), w, cx))).icon(Icon::Pencil).into());
        entries.push(
            Item::new("Open in new window", on(|this, c, id, _, cx| this.open_workspace_in_new_window(c, id, cx)))
                .icon(Icon::SquareArrowOutUpRight)
                .into(),
        );
        entries.push(Entry::Separator);
        entries.push(
            Item::new("Close…", on(|this, c, id, w, cx| this.close_workspace(c, id, w, cx)))
                .icon(Icon::Trash)
                .disabled(only.then_some("A host always keeps one workspace"))
                .into(),
        );
        self.open_menu(MenuOwner::Workspace, entries, at, false, window, cx);
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
    let at_prompt = v.at_prompt();
    let mut out = Vec::new();
    let mut last_plugin: Option<&str> = None;
    for c in &commands.0 {
        let fits = match c.when() {
            MenuWhen::Always => true,
            MenuWhen::Selection => selection.is_some(),
            MenuWhen::Prompt => at_prompt,
        };
        if !fits || !c.fits(v.process(), at_prompt) {
            continue;
        }
        if last_plugin != Some(c.plugin.as_str()) {
            out.push(Entry::Header(c.plugin.clone().into()));
            last_plugin = Some(c.plugin.as_str());
        }
        let id = crate::plugin_commands::command_id(&c.plugin_id, &c.command.id);
        let shortcut = context_menu::shortcut(&id, cx);
        let sel = selection.clone();
        let expected_cmd = c.command.clone();
        let expected_origin = c.origin.clone();
        let mut item = Item::new(c.command.title.clone(), move |window, cx| {
            run_menu_command(&id, &expected_cmd, &expected_origin, sel.as_deref(), window, cx);
        })
        .icon(c.icon());
        item.shortcut = shortcut;
        out.push(item.into());
    }
    out
}
