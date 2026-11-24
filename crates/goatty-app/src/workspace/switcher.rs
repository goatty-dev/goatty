//! The workspace switcher (#215): the chip at the start of the tab bar (at
//! the top of the sidebar with vertical tabs) that names the workspace the
//! active tab's host shows, and the popover it opens. Both only exist once
//! that host has two or more workspaces.
//!
//! The popover lists the host's workspaces with their tab counts, a mark
//! for what happened in the ones this window keeps in the background
//! (bell, a command finished or failed, new output: the tabs' marks rolled
//! up), and whether another window shows one. Arrows and Enter switch, F2
//! or a double-click renames in place, Delete closes, the context menu has
//! Rename / Open in new window / Close, and the last row makes a new one.

use goatty_proto::WorkspaceId;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::*, px,
};

use super::Workspace;
use super::tab_bar::{Colors, activity_look};
use crate::settings::AppSettings;
use crate::terminal::Activity;
use crate::ui::controls::tooltip;
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

/// One workspace in the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: WorkspaceId,
    pub name: String,
    pub tabs: u32,
    /// The one this window shows.
    pub current: bool,
    /// Another window shows it.
    pub elsewhere: bool,
    /// What happened in it while this window kept it in the background.
    pub mark: Option<Activity>,
}

pub enum SwitcherEvent {
    Switch(WorkspaceId),
    New,
    Rename(WorkspaceId, String),
    Close(WorkspaceId),
    Menu(WorkspaceId, Point<Pixels>),
    Dismiss,
}

/// The open switcher, for the host it was opened for.
pub(super) struct OpenSwitcher {
    pub view: Entity<Switcher>,
    pub conn: std::sync::Arc<goatty_client::Connection>,
    /// A second click this soon after the chip opened it renames (a
    /// double-click on the chip lands on the backdrop).
    opened: std::time::Instant,
    _subscription: Subscription,
}

pub struct Switcher {
    focus: FocusHandle,
    rows: Vec<Row>,
    host: SharedString,
    /// The selected row; `rows.len()` is "New workspace".
    selected: usize,
    renaming: Option<(WorkspaceId, Entity<TextInput>, [Subscription; 2])>,
    /// The rows scroll when there are many; the selected one stays in view.
    scroll: gpui::ScrollHandle,
}

impl EventEmitter<SwitcherEvent> for Switcher {}

impl Focusable for Switcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.renaming {
            Some((_, input, _)) => input.focus_handle(cx),
            None => self.focus.clone(),
        }
    }
}

impl Switcher {
    /// `rename`: start with that workspace's name being edited (a new one).
    pub fn new(rows: Vec<Row>, host: SharedString, rename: Option<WorkspaceId>, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let selected = rows.iter().position(|r| r.current).unwrap_or(0);
        let mut this = Self { focus: cx.focus_handle(), rows, host, selected, renaming: None, scroll: gpui::ScrollHandle::new() };
        match rename {
            Some(id) => this.start_rename(id, window, cx),
            None => {
                this.scroll_selected();
                this.focus.focus(window, cx);
            }
        }
        this
    }

    fn scroll_selected(&self) {
        if self.selected < self.rows.len() {
            self.scroll.scroll_to_item(self.selected);
        }
    }

    /// The host's workspaces changed while it's open.
    pub fn set_rows(&mut self, rows: Vec<Row>, cx: &mut Context<'_, Self>) {
        let before = self.selected;
        let selected = self.rows.get(self.selected).map(|r| r.id);
        let on_new = self.selected >= self.rows.len();
        self.rows = rows;
        self.selected = match selected {
            _ if on_new => self.rows.len(),
            Some(id) => self.rows.iter().position(|r| r.id == id).unwrap_or(self.selected.min(self.rows.len())),
            None => self.selected.min(self.rows.len()),
        };
        if self.renaming.as_ref().is_some_and(|(id, _, _)| !self.rows.iter().any(|r| r.id == *id)) {
            self.renaming = None;
        }
        if self.selected != before {
            self.scroll_selected();
        }
        cx.notify();
    }

    pub fn start_rename(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(at) = self.rows.iter().position(|r| r.id == id) else { return };
        self.selected = at;
        self.scroll_selected();
        let name = self.rows[at].name.clone();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Workspace name", name, cx);
            input.select_all_text(cx);
            input
        });
        let submitted = cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, window, cx| {
            if let TextInputEvent::Submitted(_) = ev {
                this.finish_rename(true, window, cx);
            }
        });
        let focus = input.focus_handle(cx);
        let blurred = cx.on_blur(&focus, window, |this, window, cx| this.finish_rename(true, window, cx));
        focus.focus(window, cx);
        self.renaming = Some((id, input, [submitted, blurred]));
        cx.notify();
    }

    fn finish_rename(&mut self, keep: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((id, input, _)) = self.renaming.take() else { return };
        let name = input.read(cx).text().trim().to_string();
        if keep && !name.is_empty() && self.rows.iter().any(|r| r.id == id && r.name != name) {
            if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                r.name = name.clone();
            }
            cx.emit(SwitcherEvent::Rename(id, name));
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.renaming.is_some() {
            if ev.keystroke.key == "escape" {
                self.finish_rename(false, window, cx);
                cx.stop_propagation();
            }
            return;
        }
        let n = self.rows.len() + 1;
        match ev.keystroke.key.as_str() {
            "up" => self.selected = (self.selected + n - 1) % n,
            "down" | "tab" => self.selected = (self.selected + 1) % n,
            "home" => self.selected = 0,
            "end" => self.selected = n - 1,
            "enter" => self.pick(self.selected, cx),
            "escape" => cx.emit(SwitcherEvent::Dismiss),
            "f2" => {
                if let Some(id) = self.rows.get(self.selected).map(|r| r.id) {
                    self.start_rename(id, window, cx);
                }
            }
            "delete" => {
                if let Some(id) = self.rows.get(self.selected).map(|r| r.id) {
                    cx.emit(SwitcherEvent::Close(id));
                }
            }
            _ => return,
        }
        self.scroll_selected();
        cx.stop_propagation();
        cx.notify();
    }

    fn pick(&mut self, at: usize, cx: &mut Context<'_, Self>) {
        match self.rows.get(at) {
            Some(r) => cx.emit(SwitcherEvent::Switch(r.id)),
            None => cx.emit(SwitcherEvent::New),
        }
    }

    fn row(&self, at: usize, r: &Row, ui: Ui, cx: &mut Context<'_, Self>) -> AnyElement {
        let id = r.id;
        let selected = at == self.selected;
        let name: AnyElement = match &self.renaming {
            Some((rid, input, _)) if *rid == id => div().flex_1().min_w_0().child(input.clone()).into_any_element(),
            _ => div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .when(r.current, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                .child(r.name.clone())
                .into_any_element(),
        };
        let tabs = if r.tabs == 1 { "1 tab".to_string() } else { format!("{} tabs", r.tabs) };
        let c = Colors::get(cx);
        let mark = r.mark.map(|m| {
            let (what, color) = activity_look(m, c);
            div().id(("ws-mark", id)).size(px(7.)).rounded_full().bg(color).tooltip(tooltip(what))
        });
        div()
            .id(("ws-row", id))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(8.))
            .rounded(px(radius::CONTROL - 2.))
            .cursor_pointer()
            .when(selected, |d| d.bg(ui.accent_soft))
            .hover(|s| s.bg(ui.surface_hover))
            .child(div().w(px(14.)).flex_none().when(r.current, |d| d.child(Icon::Check.svg().size(px(13.)).text_color(ui.accent_text))))
            .child(name)
            .children(mark)
            .when(r.elsewhere, |d| {
                d.child(
                    div()
                        .id(("ws-elsewhere", id))
                        .flex_none()
                        .tooltip(tooltip("Another window shows it"))
                        .child(Icon::AppWindow.svg().size(px(13.)).text_color(ui.muted)),
                )
            })
            .child(div().flex_none().text_size(px(text::CAPTION)).text_color(ui.muted).child(tabs))
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                let current = this.rows.get(at).is_some_and(|r| r.current);
                if ev.click_count() == 2 {
                    this.start_rename(id, window, cx);
                } else if !current && this.renaming.as_ref().is_none_or(|(rid, _, _)| *rid != id) {
                    // The current one stays: a second click renames it.
                    this.selected = at;
                    this.pick(at, cx);
                } else {
                    this.selected = at;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                    this.selected = at;
                    cx.emit(SwitcherEvent::Menu(id, ev.position));
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
}

impl Render for Switcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let rows: Vec<AnyElement> = self.rows.clone().iter().enumerate().map(|(at, r)| self.row(at, r, ui, cx)).collect();
        let new_at = self.rows.len();
        let new = div()
            .id("ws-new")
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(8.))
            .rounded(px(radius::CONTROL - 2.))
            .cursor_pointer()
            .text_color(ui.muted)
            .when(self.selected == new_at, |d| d.bg(ui.accent_soft).text_color(ui.text))
            .hover(|s| s.bg(ui.surface_hover).text_color(ui.text))
            .child(div().w(px(14.)).flex_none().child(Icon::Plus.svg().size(px(13.)).text_color(ui.muted)))
            .child("New workspace")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(SwitcherEvent::New)));
        div()
            .id("workspace-switcher")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .occlude()
            .w(px(300.))
            .p(px(4.))
            .flex()
            .flex_col()
            .bg(ui.menu)
            .border_1()
            .border_color(ui.border)
            .rounded(px(radius::CONTROL))
            .shadow_lg()
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .text_size(px(text::LABEL))
            .text_color(ui.text)
            .child(
                div()
                    .px(px(8.))
                    .pt(px(6.))
                    .pb(px(4.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(ui.muted)
                    .child(format!("Workspaces on {}", self.host)),
            )
            .child(div().id("ws-rows").flex().flex_col().max_h(px(360.)).overflow_y_scroll().track_scroll(&self.scroll).children(rows))
            .child(div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(ui.divider))
            .child(new)
            .child(
                div()
                    .px(px(8.))
                    .pt(px(4.))
                    .pb(px(4.))
                    .text_size(px(text::CAPTION))
                    .text_color(ui.faint)
                    .child("Enter switches · F2 renames · right-click for more"),
            )
    }
}

impl Workspace {
    /// The rows for host `conn`'s workspaces.
    fn switcher_rows(&self, conn: &std::sync::Arc<goatty_client::Connection>) -> Option<(Vec<Row>, SharedString)> {
        let i = self.shown_index(conn)?;
        let sh = &self.shown[i];
        let rows = sh
            .list
            .iter()
            .map(|w| {
                let current = w.id == sh.id;
                Row {
                    id: w.id,
                    name: w.name.clone(),
                    tabs: if current { self.tabs.iter().filter(|t| super::sync::on_conn(t, &sh.conn)).count() as u32 } else { w.tabs },
                    current,
                    elsewhere: w.views > u32::from(current),
                    mark: if current { None } else { self.parked_activity(&sh.conn, w.id) },
                }
            })
            .collect();
        let host = super::groups::host_label(&crate::session::Spawner::Daemon(conn.clone()));
        Some((rows, host.into()))
    }

    /// Whether the chip shows: the active tab's host has more than one
    /// workspace.
    pub(super) fn shows_workspaces(&self) -> bool {
        self.active_shown().is_some_and(|i| self.shown[i].list.len() > 1)
    }

    /// Open the switcher (`rename`: with that workspace's name being edited).
    pub(super) fn open_switcher(&mut self, rename: Option<WorkspaceId>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let conn = self.active_shown().map(|i| self.shown[i].conn.clone());
        let Some((rows, host)) = conn.as_ref().and_then(|c| self.switcher_rows(c)) else {
            let toast = crate::toast::Toast::info("No workspaces here").body("This tab isn't kept by a goattyd (or is still connecting).");
            crate::toast::show_in(window, cx, toast.key("workspace.switch"));
            return;
        };
        self.close_palette(window, cx);
        let Some(conn) = conn else { return };
        let view = cx.new(|cx| Switcher::new(rows, host, rename, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &SwitcherEvent, window, cx| this.on_switcher_event(ev, window, cx));
        self.switcher = Some(OpenSwitcher { view, conn, opened: std::time::Instant::now(), _subscription: sub });
        cx.notify();
    }

    pub(super) fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.switcher.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// The host's workspaces changed: an open switcher follows.
    pub(super) fn refresh_switcher(&mut self, cx: &mut Context<'_, Self>) {
        let Some(open) = &self.switcher else { return };
        match self.switcher_rows(&open.conn) {
            Some((rows, _)) => open.view.clone().update(cx, |s, cx| s.set_rows(rows, cx)),
            None => self.switcher = None,
        }
    }

    fn on_switcher_event(&mut self, ev: &SwitcherEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(conn) = self.switcher.as_ref().map(|s| s.conn.clone()) else { return };
        match ev {
            SwitcherEvent::Switch(id) => {
                let id = *id;
                self.close_switcher(window, cx);
                self.switch_workspace(&conn, id, window, cx);
            }
            SwitcherEvent::New => {
                self.close_switcher(window, cx);
                self.new_workspace(window, cx);
            }
            SwitcherEvent::Rename(id, name) => self.rename_workspace(&conn, *id, name.clone(), cx),
            SwitcherEvent::Close(id) => {
                let id = *id;
                self.close_workspace(&conn, id, window, cx);
            }
            SwitcherEvent::Menu(id, at) => self.open_workspace_menu(conn, *id, *at, window, cx),
            SwitcherEvent::Dismiss => self.close_switcher(window, cx),
        }
    }

    /// Open workspace `id` on `conn` in a new window (another window showing it).
    pub(super) fn open_workspace_in_new_window(
        &mut self,
        conn: &std::sync::Arc<goatty_client::Connection>,
        id: WorkspaceId,
        cx: &mut Context<'_, Self>,
    ) {
        let plan = crate::connect::ConnectPlan {
            endpoint: conn.endpoint().clone(),
            mode: crate::session::AttachMode::Join(Some(id)),
            open_settings: None,
            plain: false,
            conn: Some(conn.clone()),
            join: None,
            active_tab: None,
        };
        cx.defer(move |cx| crate::app::open_terminal_window(cx, crate::app::Initial::Connect(plan), None));
    }

    /// The chip that names the active host's workspace and opens the
    /// switcher (`vertical`: at the top of the sidebar).
    pub(super) fn workspace_chip(&self, vertical: bool, cx: &mut Context<'_, Self>) -> Option<AnyElement> {
        if !self.shows_workspaces() {
            return None;
        }
        let i = self.active_shown()?;
        let sh = &self.shown[i];
        let c = Colors::get(cx);
        let name = sh.name();
        // Something happened in a workspace this window keeps in the background.
        let mark = sh.list.iter().filter(|w| w.id != sh.id).filter_map(|w| self.parked_activity(&sh.conn, w.id)).max();
        let open = self.switcher.is_some();
        let chip = div()
            .id("workspace-chip")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .h(px(if vertical { 28. } else { 26. }))
            .rounded(px(radius::CONTROL))
            .cursor_pointer()
            .border_1()
            .border_color(c.edge)
            .text_size(px(text::LABEL))
            .text_color(c.ui.text)
            .when(open, |d| d.bg(c.hover))
            .hover(|s| s.bg(c.hover))
            .tooltip(tooltip("Workspace: click to switch, double-click to rename"))
            .child(Icon::Rows2.svg().size(px(13.)).text_color(c.ui.muted))
            .child(div().max_w(px(160.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
            .children(mark.map(|m| div().size(px(6.)).rounded_full().bg(activity_look(m, c).1)))
            .child(Icon::ChevronsUpDown.svg().size(px(12.)).text_color(c.ui.muted))
            .on_click(cx.listener(|this, ev: &gpui::ClickEvent, window, cx| {
                if ev.click_count() == 2 {
                    let id = this.active_shown().map(|i| this.shown[i].id);
                    this.open_switcher(id, window, cx);
                } else if this.switcher.is_some() {
                    this.close_switcher(window, cx);
                } else {
                    this.open_switcher(None, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if let Some(i) = this.active_shown() {
                        let (conn, id) = (this.shown[i].conn.clone(), this.shown[i].id);
                        this.open_workspace_menu(conn, id, ev.position, window, cx);
                    }
                }),
            );
        let wrap = div().flex_none().flex().items_center();
        Some(
            if vertical {
                wrap.px(px(8.)).pt(px(8.)).pb(px(2.)).child(chip.w_full())
            } else {
                wrap.h_full().pl(px(6.)).pr(px(2.)).child(chip)
            }
            .into_any_element(),
        )
    }

    /// About how wide the chip is in the tab bar (0 without one), for
    /// sharing the strip between the tabs.
    pub(super) fn chip_width(&self) -> f32 {
        if !self.shows_workspaces() {
            return 0.0;
        }
        let name = self.active_shown().map(|i| self.shown[i].name()).unwrap_or_default();
        // Icon, gaps, padding and the chevron around the (cut) name.
        (name.chars().count().min(24) as f32 * 7.5).min(160.0) + 64.0
    }

    /// The open switcher, under the chip, over a backdrop that closes it.
    pub(super) fn render_switcher(&self, vertical: bool, cx: &mut Context<'_, Self>) -> Option<AnyElement> {
        let view = &self.switcher.as_ref()?.view;
        let top = if vertical { 48. } else { 42. };
        let close = |this: &mut Self, ev: &gpui::MouseDownEvent, window: &mut Window, cx: &mut Context<'_, Self>| {
            // The chip's double-click: its second click lands here.
            let double =
                ev.click_count >= 2 && this.switcher.as_ref().is_some_and(|s| s.opened.elapsed() < std::time::Duration::from_millis(600));
            match this.switcher.as_ref() {
                Some(s) if double => {
                    let id = this.shown_index(&s.conn).map(|i| this.shown[i].id);
                    if let Some(id) = id {
                        s.view.clone().update(cx, |v, cx| v.start_rename(id, window, cx));
                    }
                }
                _ => this.close_switcher(window, cx),
            }
        };
        let backdrop = div()
            .id("workspace-switcher-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(close))
            .on_mouse_down(MouseButton::Right, cx.listener(close));
        Some(
            div()
                .absolute()
                .inset_0()
                .child(gpui::deferred(backdrop).with_priority(1))
                .child(gpui::deferred(div().absolute().top(px(top)).left(px(8.)).child(view.clone())).with_priority(2))
                .into_any_element(),
        )
    }
}
