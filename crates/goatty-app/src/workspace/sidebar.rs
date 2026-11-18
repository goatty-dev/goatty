//! The vertical tab style (`tabs.style = "vertical"`, #168), in the manner
//! of Arc and Zen: a sidebar on the left in the tab strip's color, a row
//! per tab, and the panes as a rounded card beside it.
//!
//! - **Rows**: the program's icon, the title and, below it, the directory
//!   (plus the host when remote) or, while a coding agent works or waits,
//!   what it's doing (#205); activity or agent markers, the pane count, a
//!   close button on hover. The active row is raised in the terminal's
//!   color; a background row whose agent waits for the user is tinted.
//!   "New tab" ends the list; the list scrolls and keeps the active tab in
//!   view.
//! - **Rail**: folded (`tabs.sidebar_collapsed`, Ctrl+Shift+B, the button in
//!   the footer, or dragging the edge far left) it's a column of icon tiles
//!   with their markers as dots (an agent's state as a small badge) and the
//!   title as a tooltip. A tab without an icon shows its title's first
//!   letter.
//! - **Edge**: drag it to resize (`tabs.sidebar_width`; at most 45 % of the
//!   window), past [`gestures::RAIL_WIDTH`]'s neighbourhood to fold or
//!   unfold, double-click to fold or unfold. The width is saved shortly
//!   after the drag stops, not every frame.
//! - **Host groups** (`groups`): with tabs on several hosts, a header row
//!   per host (fold chevron, the host's icon and name in its color, a +
//!   for a new tab there) over its tabs, which carry a line in that color;
//!   in the rail, a short line in the host's color over its tiles, or one
//!   tile for a folded host.
//! - **Card**: the panes get a margin and rounded corners. The terminal
//!   draws square, so a ring in the sidebar's color is drawn over its
//!   corners: a border as wide as the margin whose outer radius is the
//!   card's radius plus the margin, which leaves exactly the card's radius
//!   on its inner edge (GPUI's quads shrink the inner radius by the border
//!   width).

use std::time::Duration;

use goatty_term::StatusState;
use gpui::{
    AnyElement, Context, DragMoveEvent, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window, div, prelude::*, px,
};

use super::Workspace;
use super::gestures::{RAIL_WIDTH, Sidebar};
use super::groups::{Group, StripItem};
use super::tab_bar::{Axis, Colors, TabView, activity_look, group_mark, is_waiting, progress_line, status_color};
use crate::actions::{NewTab, OpenSettings, ToggleTabSidebar};
use crate::settings::AppSettings;
use crate::ui::controls::tooltip;
use crate::ui::icons::Icon;
use crate::ui::style::{radius, text};

/// The sidebar's padding; on its right, the gap to the card.
const PAD: f32 = 8.0;
/// The card's margin at the top, right and bottom of the window.
const MARGIN: f32 = 8.0;
/// The card's corner radius.
const CARD_RADIUS: f32 = 10.0;
/// The resize handle's width, centered on the sidebar's edge.
const HANDLE: f32 = 8.0;
/// A rail tile's size.
const TILE: f32 = 38.0;
/// How long the sidebar's size has to stay put before it's saved.
const SAVE_AFTER: Duration = Duration::from_millis(400);

/// Dragging the sidebar's edge (a GPUI drag; see `on_sidebar_drag`).
#[derive(Clone)]
pub(super) struct SidebarDrag;

impl Workspace {
    /// The sidebar's size: while its edge is dragged, the dragged one.
    fn sidebar(&self, cx: &gpui::App) -> Sidebar {
        self.sidebar_drag.map_or_else(|| saved_sidebar(cx), |(_, now)| now)
    }

    /// How wide the sidebar is drawn in this window.
    pub(super) fn sidebar_width(&self, window: &Window, cx: &gpui::App) -> f32 {
        self.sidebar(cx).drawn_width(f32::from(window.viewport_size().width) - super::chrome::frame_width(window))
    }

    /// The window's root in the vertical style: a row in the sidebar's
    /// color that follows a drag of the sidebar's edge.
    pub(super) fn sidebar_frame(&self, root: gpui::Stateful<gpui::Div>, cx: &mut Context<'_, Self>) -> gpui::Stateful<gpui::Div> {
        root.flex_row().bg(Colors::get(cx).bar).on_drag_move(cx.listener(Self::on_sidebar_drag)).on_drop(cx.listener(
            |this, _: &SidebarDrag, _, cx| {
                this.save_sidebar = None;
                this.save_sidebar_size(true, cx);
            },
        ))
    }

    fn on_sidebar_drag(&mut self, ev: &DragMoveEvent<SidebarDrag>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let x = f32::from(ev.event.position.x - ev.bounds.left());
        let (start, so_far) = self.sidebar_drag.unwrap_or_else(|| (saved_sidebar(cx), saved_sidebar(cx)));
        let now = so_far.dragged_to(x, f32::from(window.viewport_size().width) - super::chrome::frame_width(window), start);
        if now == so_far && self.sidebar_drag.is_some() {
            return;
        }
        self.sidebar_drag = Some((start, now));
        cx.notify();
        self.save_sidebar = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            this.update(cx, |this, cx| this.save_sidebar_size(false, cx)).ok();
        }));
    }

    /// Save the dragged size to the settings. `done`: the drag ended.
    fn save_sidebar_size(&mut self, done: bool, cx: &mut Context<'_, Self>) {
        let Some((_, s)) = self.sidebar_drag else { return };
        let tabs = &cx.global::<AppSettings>().client.tabs;
        if tabs.sidebar_width != s.width || tabs.sidebar_collapsed != s.folded {
            AppSettings::update(cx, |c| {
                c.tabs.sidebar_width = s.width;
                c.tabs.sidebar_collapsed = s.folded;
            });
        }
        // Released outside the window: no drop, but no drag either.
        if done || !cx.has_active_drag() {
            self.sidebar_drag = None;
            cx.notify();
        }
    }

    pub(super) fn render_sidebar(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let sidebar = self.sidebar(cx);
        let width = sidebar.drawn_width(f32::from(window.viewport_size().width) - super::chrome::frame_width(window));
        let c = Colors::get(cx);
        let strip = self.strip_items(cx);
        let ui_font = cx.global::<AppSettings>().ui_font.clone();
        let frame = div()
            .id("tab-sidebar")
            .flex_none()
            .w(px(width))
            .h_full()
            .flex()
            .flex_col()
            .font_family(ui_font)
            .text_color(c.ui.text)
            .on_scroll(cx.listener(Self::on_tabs_scroll));
        if sidebar.folded {
            let tiles: Vec<_> = strip
                .into_iter()
                .map(|item| match item {
                    StripItem::Group(g) => self.rail_group(g, c, cx),
                    StripItem::Tab(t) => self.rail_tile(t, c, cx),
                })
                .collect();
            let new_tab = sidebar_button("tab-rail-new", Icon::Plus, "New tab", c).size(px(TILE)).on_click(dispatch(NewTab));
            frame
                .px(px((RAIL_WIDTH - TILE) / 2.))
                .py(px(PAD))
                .child(
                    div()
                        .id("tab-list")
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(4.))
                        .overflow_y_scroll()
                        .track_scroll(&self.tab_scroll)
                        .children(tiles)
                        .child(new_tab)
                        .child(self.tabs_rest("tab-rail-rest", true, cx).w_full().flex_1().min_h(px(TILE))),
                )
                .child(
                    div().flex_none().flex().justify_center().pt(px(6.)).child(
                        sidebar_button("tab-sidebar-unfold", Icon::PanelLeftOpen, "Unfold the tab sidebar", c)
                            .size(px(TILE))
                            .on_click(dispatch(ToggleTabSidebar)),
                    ),
                )
                .into_any_element()
        } else {
            let rows: Vec<_> = strip
                .into_iter()
                .enumerate()
                .map(|(n, item)| match item {
                    StripItem::Group(g) => self.sidebar_group(g, n == 0, c, cx),
                    StripItem::Tab(t) => self.sidebar_row(t, c, cx),
                })
                .collect();
            let new_tab = div()
                .id("tab-sidebar-new")
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(34.))
                .px(px(10.))
                .rounded(px(radius::CONTROL))
                .cursor_pointer()
                .text_size(px(text::LABEL))
                .text_color(c.ui.muted)
                .hover(|s| s.bg(c.hover).text_color(c.ui.text))
                .child(Icon::Plus.svg().size(px(16.)).text_color(c.ui.muted))
                .child("New tab")
                .on_click(dispatch(NewTab));
            let footer = div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .pt(px(6.))
                .child(
                    sidebar_button("tab-sidebar-fold", Icon::PanelLeftClose, "Fold the tab sidebar", c)
                        .size(px(30.))
                        .on_click(dispatch(ToggleTabSidebar)),
                )
                .child(sidebar_button("tab-sidebar-settings", Icon::Sliders, "Settings", c).size(px(30.)).on_click(dispatch(OpenSettings)));
            frame
                .p(px(PAD))
                .child(
                    div()
                        .id("tab-list")
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .overflow_y_scroll()
                        .track_scroll(&self.tab_scroll)
                        .children(rows)
                        .child(new_tab)
                        .child(self.tabs_rest("tab-sidebar-rest", true, cx).flex_1().min_h(px(24.))),
                )
                .child(footer)
                .into_any_element()
        }
    }

    /// A tab in the unfolded sidebar.
    fn sidebar_row(&self, t: TabView, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let icon = self.tab_icon(&t, 15.0, c, cx, || Icon::Terminal.svg().size(px(15.)).text_color(c.ui.muted).into_any_element());
        let title = self.tab_title(&t, cx);
        let renaming = self.rename.as_ref().is_some_and(|r| r.tab == t.id);
        let detail = t.detail.clone().filter(|d| *d != t.label && !renaming);
        // While an agent works or waits, what it does says more than where.
        let doing = t.status.as_ref().filter(|s| !renaming && matches!(s.state, StatusState::Working | StatusState::Blocked)).map(|s| {
            let words = s.details(60).map(|d| format!("{} · {d}", s.summary())).unwrap_or_else(|| s.summary());
            let color = if s.state == StatusState::Blocked { status_color(s.state, c) } else { c.ui.muted };
            (SharedString::from(words), color)
        });
        let marker = self.tab_marker(&t, 13.0, c);
        let waiting = !t.active && is_waiting(&t);
        let close = self.close_button(&t, 22.0, c, cx);
        let panes = (t.panes > 1).then(|| {
            div()
                .flex_none()
                .px(px(6.))
                .rounded_full()
                .bg(c.ui.text.opacity(0.08))
                .text_size(px(text::CAPTION))
                .text_color(c.ui.muted)
                .child(SharedString::from(format!("{}", t.panes)))
        });
        let text = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(div().flex().min_w_0().text_size(px(text::LABEL)).child(title))
            .when_some(doing.clone(), |d, (words, color)| {
                d.child(div().min_w_0().truncate().text_size(px(text::CAPTION)).text_color(color).child(words))
            })
            .when_some(detail.filter(|_| doing.is_none()), |d, detail| {
                d.child(div().min_w_0().truncate().text_size(px(text::CAPTION)).text_color(c.ui.faint).child(detail))
            });
        self.tab_shell(&t, Axis::Column, cx)
            .flex_none()
            .min_h(px(36.))
            .py(px(5.))
            .pl(px(10.))
            .pr(px(6.))
            .gap(px(10.))
            .rounded(px(radius::CONTROL))
            .border_1()
            .when(t.active, |d| d.bg(c.terminal).border_color(c.edge).shadow_sm().text_color(c.ui.text))
            .when(!t.active, |d| {
                d.border_color(gpui::transparent_black()).text_color(c.ui.muted).hover(|s| s.bg(c.hover).text_color(c.ui.text))
            })
            .when(waiting, |d| d.bg(c.ui.warning.opacity(0.12)).border_color(c.ui.warning.opacity(0.35)).text_color(c.ui.text))
            .child(icon)
            .child(text)
            .children(marker)
            .children(panes)
            .child(close)
            .when_some(t.group, |d, color| {
                d.child(div().absolute().left(px(3.)).top(px(9.)).bottom(px(9.)).w(px(2.)).rounded_full().bg(color))
            })
            .children(t.progress.map(|bar| progress_line(bar, ("tab-progress", t.id), 10.0, c)))
            .into_any_element()
    }

    /// A host group's header in the unfolded sidebar: a chevron, the host's
    /// icon and name in its color, a + for a new tab there; folded, how
    /// many tabs it holds and their activity. Click: fold or unfold;
    /// right-click: the group's menu.
    fn sidebar_group(&self, g: Group, first: bool, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let (key, menu_key, new_key) = (g.key.clone(), g.key.clone(), g.key.clone());
        let folded = g.folded;
        let id = g.ids[0];
        let hover_group = SharedString::from(format!("tab-group-{id}"));
        let count = folded.then(|| {
            div()
                .flex_none()
                .px(px(6.))
                .rounded_full()
                .bg(c.ui.text.opacity(0.08))
                .text_size(px(text::CAPTION))
                .text_color(c.ui.muted)
                .child(SharedString::from(g.tabs.len().to_string()))
        });
        let mark = group_mark(&g, c).filter(|_| folded).map(|color| div().flex_none().size(px(7.)).rounded_full().bg(color));
        let plus = div()
            .id(gpui::ElementId::NamedInteger("tab-group-new".into(), id))
            .flex_none()
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(radius::SMALL))
            .invisible()
            .group_hover(hover_group.clone(), |s| s.visible())
            .hover(|s| s.bg(c.ui.text.opacity(0.1)))
            .tooltip(tooltip(format!("New tab on {}", g.label)))
            .child(Icon::Plus.svg().size(px(13.)).text_color(c.ui.muted))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.new_tab_in_group(&new_key, window, cx);
            }));
        let detail = g.detail.clone().filter(|_| !folded);
        div()
            .id(gpui::ElementId::NamedInteger("tab-group".into(), id))
            .group(hover_group)
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .when(!first, |d| d.mt(px(8.)))
            .px(px(6.))
            .rounded(px(radius::CONTROL))
            .cursor_pointer()
            .hover(|s| s.bg(c.hover))
            .tooltip(tooltip(super::tab_bar::group_tooltip(&g)))
            .child((if folded { Icon::ChevronRight } else { Icon::ChevronDown }).svg().size(px(12.)).text_color(c.ui.faint))
            .child((if g.local { Icon::Monitor } else { Icon::Server }).svg().size(px(13.)).text_color(g.color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_none()
                            .max_w_full()
                            .truncate()
                            .text_size(px(text::CAPTION))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(c.ui.text)
                            .child(g.label.clone()),
                    )
                    .when_some(detail, |d, detail| {
                        d.child(div().min_w_0().truncate().text_size(px(11.)).text_color(c.ui.faint).child(detail))
                    }),
            )
            .children(mark)
            .children(count)
            // A connecting tab has no host to open another tab on yet.
            .when(!matches!(g.key, super::groups::HostKey::Connecting(_)), |d| d.child(plus))
            .on_click(cx.listener(move |this, _, window, cx| this.fold_group(&key, !folded, window, cx)))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_group_menu(menu_key.clone(), ev.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    /// A host group in the rail: a short line in its color over its tiles,
    /// or, folded, one tile with the host's icon and how many tabs it holds.
    fn rail_group(&self, g: Group, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let (key, menu_key) = (g.key.clone(), g.key.clone());
        let folded = g.folded;
        let base = div()
            .id(gpui::ElementId::NamedInteger("tab-group".into(), g.ids[0]))
            .flex_none()
            .w(px(TILE))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .rounded(px(radius::CONTROL))
            .tooltip(tooltip(super::tab_bar::group_tooltip(&g)))
            .on_click(cx.listener(move |this, _, window, cx| this.fold_group(&key, !folded, window, cx)))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_group_menu(menu_key.clone(), ev.position, window, cx);
                    cx.stop_propagation();
                }),
            );
        if !folded {
            return base
                .h(px(12.))
                .hover(|s| s.bg(c.hover))
                .child(div().w(px(22.)).h(px(3.)).rounded_full().bg(g.color))
                .into_any_element();
        }
        let dot = group_mark(&g, c)
            .map(|color| div().absolute().top(px(3.)).right(px(3.)).size(px(9.)).rounded_full().border_2().border_color(c.bar).bg(color));
        base.relative()
            .h(px(TILE))
            .bg(g.color.opacity(0.14))
            .hover(|s| s.bg(g.color.opacity(0.28)))
            .child((if g.local { Icon::Monitor } else { Icon::Server }).svg().size(px(16.)).text_color(g.color))
            .child(
                div()
                    .absolute()
                    .bottom(px(2.))
                    .right(px(4.))
                    .text_size(px(10.))
                    .text_color(c.ui.muted)
                    .child(SharedString::from(g.tabs.len().to_string())),
            )
            .children(dot)
            .into_any_element()
    }

    /// A tab in the folded sidebar: its icon (or first letter), a dot for
    /// activity, the title as a tooltip.
    fn rail_tile(&self, t: TabView, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let letter = t.label.chars().find(|ch| ch.is_alphanumeric()).unwrap_or('~').to_uppercase().to_string();
        let icon = self.tab_icon(&t, 17.0, c, cx, || {
            div()
                .text_size(px(text::BODY))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(c.ui.muted)
                .child(SharedString::from(letter))
                .into_any_element()
        });
        let tip = match (&t.detail, t.panes) {
            (Some(d), 1) => format!("{}\n{d}", t.label),
            (Some(d), n) => format!("{}\n{d} · {n} panes", t.label),
            (None, 1) => t.label.to_string(),
            (None, n) => format!("{} · {n} panes", t.label),
        };
        // An agent's state as a small badge on the corner, else the activity dot.
        let badge = t.status.as_ref().map(|s| rail_badge(s.state, s.icon(), c));
        let dot = t.activity.filter(|_| badge.is_none()).map(|a| {
            div().absolute().top(px(3.)).right(px(3.)).size(px(9.)).rounded_full().border_2().border_color(c.bar).bg(activity_look(a, c).1)
        });
        let tip = match &t.status {
            Some(s) => format!("{tip}\n{}", s.tooltip()),
            None => tip,
        };
        self.tab_shell(&t, Axis::Column, cx)
            .flex_none()
            .size(px(TILE))
            .justify_center()
            .rounded(px(radius::CONTROL))
            .border_1()
            .when(t.active, |d| d.bg(c.terminal).border_color(c.edge).shadow_sm())
            .when(!t.active, |d| d.border_color(gpui::transparent_black()).hover(|s| s.bg(c.hover)))
            .tooltip(tooltip(tip))
            .when(!t.active && is_waiting(&t), |d| d.bg(c.ui.warning.opacity(0.14)))
            .child(icon)
            .children(dot)
            .children(badge)
            .when_some(t.group, |d, color| {
                d.child(div().absolute().left(px(1.)).top(px(10.)).bottom(px(10.)).w(px(2.)).rounded_full().bg(color))
            })
            .children(t.progress.map(|bar| progress_line(bar, ("tab-progress", t.id), 7.0, c)))
            .into_any_element()
    }

    /// The panes beside the sidebar, as a rounded card.
    pub(super) fn render_card(&self, cx: &mut Context<'_, Self>) -> AnyElement {
        let c = Colors::get(cx);
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .flex()
            .flex_col()
            .pt(px(MARGIN))
            .pr(px(MARGIN))
            .pb(px(MARGIN))
            .child(self.render_panes(cx))
            // The corners: see the module docs. The ring reaches left over
            // the sidebar's padding, which is the same color.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left(px(-MARGIN))
                    .border(px(MARGIN))
                    .border_color(c.bar)
                    .rounded(px(CARD_RADIUS + MARGIN)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(MARGIN))
                    .right(px(MARGIN))
                    .bottom(px(MARGIN))
                    .left_0()
                    .rounded(px(CARD_RADIUS))
                    .border_1()
                    .border_color(c.edge),
            )
            .into_any_element()
    }

    /// The handle on the sidebar's edge: drag to resize or fold,
    /// double-click to fold or unfold.
    pub(super) fn render_sidebar_handle(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let c = Colors::get(cx);
        let x = self.sidebar_width(window, cx);
        let dragging = self.sidebar_drag.is_some();
        let line = div().w(px(2.)).h_full().rounded_full().when(dragging, |d| d.bg(c.ui.accent.opacity(0.7)));
        div()
            .id("tab-sidebar-edge")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(x - HANDLE / 2.))
            .w(px(HANDLE))
            .flex()
            .justify_center()
            .py(px(MARGIN + CARD_RADIUS))
            .occlude()
            .cursor_col_resize()
            .group("tab-sidebar-edge")
            .child(line.group_hover("tab-sidebar-edge", |s| s.bg(c.ui.accent.opacity(0.5))))
            .on_drag(SidebarDrag, |_, _, _, cx| cx.new(|_| gpui::EmptyView))
            .on_click(|ev, window, cx| {
                if ev.click_count() == 2 {
                    window.dispatch_action(Box::new(ToggleTabSidebar), cx);
                }
            })
            .into_any_element()
    }
}

/// The sidebar's size in the settings.
/// An agent's state on a rail tile's corner: its icon on a disc in the
/// state's color, ringed in the sidebar's color.
fn rail_badge(state: StatusState, icon: Option<Icon>, c: Colors) -> AnyElement {
    let color = status_color(state, c);
    let disc = div()
        .absolute()
        .top(px(1.))
        .right(px(1.))
        .size(px(14.))
        .rounded_full()
        .border_2()
        .border_color(c.bar)
        .bg(color)
        .flex()
        .items_center()
        .justify_center();
    match (state, icon) {
        // A spinner reads badly at this size: a plain dot.
        (StatusState::Working, _) | (_, None) => disc.size(px(10.)).top(px(3.)).right(px(3.)).into_any_element(),
        (_, Some(icon)) => disc.child(icon.svg().size(px(8.)).text_color(c.bar)).into_any_element(),
    }
}

fn saved_sidebar(cx: &gpui::App) -> Sidebar {
    let tabs = &cx.global::<AppSettings>().client.tabs;
    Sidebar { width: tabs.sidebar_width, folded: tabs.sidebar_collapsed }
}

/// A square icon button in the sidebar's colors (size is the caller's).
fn sidebar_button(id: &'static str, icon: Icon, tip: &'static str, c: Colors) -> gpui::Stateful<gpui::Div> {
    let group = SharedString::from(id);
    div()
        .id(id)
        .group(group.clone())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .cursor_pointer()
        .hover(|s| s.bg(c.hover))
        .tooltip(tooltip(tip))
        .child(icon.svg().size(px(16.)).text_color(c.ui.muted).group_hover(group, |s| s.text_color(c.ui.text)))
}

/// A click handler that dispatches `action` from the clicked element.
fn dispatch<A: gpui::Action + Clone>(action: A) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
    move |_, window, cx| window.dispatch_action(Box::new(action.clone()), cx)
}
