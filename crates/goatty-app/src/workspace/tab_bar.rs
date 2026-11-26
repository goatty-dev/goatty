//! The tabs, drawn by goatty (not the window manager). This module draws
//! the horizontal strip in two styles (`tabs.style`) and holds what every
//! style shares; the vertical style is in `sidebar`.
//!
//! - **modern**: browser-like. Background tabs are quiet text that floats
//!   above the bar's bottom edge, a rounded pill on hover, with faint
//!   dividers between them. The active tab has the terminal's background,
//!   rounded top corners and flared bottom ones, so it merges into the
//!   panes. Icon, title, close button (always on the active tab, on hover
//!   for the others), a + button.
//! - **compact**: one line of small monospace text, kitty-style: number,
//!   icon and title. The active tab has the terminal's background (it
//!   merges into the panes) and an accent-colored number.
//!
//! Every style: click to switch, middle-click to close, double-click to
//! rename, right-click for the tab's menu (`menu`), double-click the
//! empty space for a new tab (or, when goatty draws the title bar, to
//! maximize: the bar is the title bar then, see `chrome`). A settings
//! button sits at the end. Dragging a tab moves it live: the others make
//! room as it passes their middle (`gestures::reorder_target`). With
//! `tabs.scroll_to_switch` the mouse wheel over the tabs switches tabs;
//! otherwise it scrolls tabs that don't fit. The active tab is always
//! scrolled into view. A background tab shows what happened in it since it
//! was last shown (#122, `tabs.mark_bell` / `tabs.mark_activity`): a bell,
//! or a dot for a finished command (red when it failed) or for new output.
//!
//! Coding agents (#205, `crate::agent`) show their logo as the tab's icon
//! and their state in place of the activity mark: a spinner while working
//! (and a progress line along the tab's bottom when they say how far
//! along; OSC 9;4 progress bars show there too), a pulsing badge in the
//! warning color while they wait for the user (the tab is tinted too, when
//! in the background), a check or a cross when done or failed, until the
//! tab is looked at. Hovering the mark tells what the agent said.
//!
//! With tabs on several hosts (`groups`), each host's tabs follow a chip
//! with the host's name, like a browser's tab groups (#253): solid in the
//! host's color while the group is open, with a line in that color along
//! the bar's bottom edge joining the chip to the group's background tabs;
//! folded, a tinted chip with the number of tabs and their activity.
//! Click the chip to fold, right-click for the host's menu.

use std::time::{Duration, Instant};

use goatty_config::TabStyle;
use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, DragMoveEvent, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Render, ScrollDelta, ScrollEvent, SharedString, StatefulInteractiveElement, Styled, Transformation, Window, div, percentage,
    prelude::*, px, rgb,
};

use goatty_term::{ProgressState, StatusState};

use super::chrome;
use super::gestures::{Wheel, reorder_target};
use super::groups::{Group, StripItem};
use super::icons::{IconColor, icon_for};
use super::{DraggedTab, Tab, Workspace, marks};
use crate::actions::{NewTab, OpenSettings};
use crate::agent::{self, AgentStatus, Identity};
use crate::settings::AppSettings;
use crate::terminal::Activity;
use crate::ui::controls::tooltip;
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};

const MODERN_HEIGHT: f32 = 40.0;
/// The active modern tab, which reaches down into the panes.
const MODERN_TAB_HEIGHT: f32 = 34.0;
/// Background modern tabs, which float above the bar's bottom edge.
const MODERN_BACK_HEIGHT: f32 = 28.0;
/// How far the active modern tab's bottom corners curve out.
const FLARE: f32 = 8.0;
/// How far background modern tabs float above the bar's bottom edge.
const MODERN_LIFT: f32 = MODERN_TAB_HEIGHT - MODERN_BACK_HEIGHT - 2.0;
const COMPACT_HEIGHT: f32 = 26.0;
/// Modern tabs share the strip down to this width; then it scrolls.
const MIN_TAB: f32 = 96.0;
const MAX_TAB: f32 = 220.0;
/// Narrower modern tabs only show a close button when active.
const NARROW_TAB: f32 = 120.0;

/// Colors of the tabs, from the theme.
#[derive(Clone, Copy)]
pub(crate) struct Colors {
    pub ui: Ui,
    /// Behind the tabs: the strip, the sidebar, the frame around the card.
    pub bar: Hsla,
    /// The terminal's background (the active tab).
    pub terminal: Hsla,
    pub hover: Hsla,
    /// A faint line around raised things (the active tab in the sidebar,
    /// the panes card): lighter in dark themes, darker in light ones.
    pub edge: Hsla,
}

impl Colors {
    pub fn get(cx: &App) -> Self {
        let ui = Ui::get(cx);
        let colors = &cx.global::<AppSettings>().colors;
        let terminal: Hsla = rgb(colors.background.0).into();
        // Darker than the terminal in dark themes (lighter when the terminal
        // is near black and can't get darker), a bit greyer in light ones.
        let bar = if colors.is_dark() {
            let l = if terminal.l < 0.06 { terminal.l + 0.045 } else { terminal.l * 0.6 };
            Hsla { l, ..terminal }
        } else {
            Hsla { l: (terminal.l - 0.06).max(0.0), ..terminal }
        };
        // Between the bar and the terminal: a hovered tab never outshines the active one.
        let hover = if colors.is_dark() { Hsla { l: (bar.l + 0.03).min(1.0), ..bar } } else { Hsla { l: (bar.l - 0.05).max(0.0), ..bar } };
        let edge = ui.text.opacity(if colors.is_dark() { 0.07 } else { 0.1 });
        Self { ui, bar, terminal, hover, edge }
    }

    pub fn icon(&self, color: IconColor, cx: &App) -> Hsla {
        match color {
            IconColor::Text => self.ui.text,
            IconColor::Ansi(i) => rgb(cx.global::<AppSettings>().colors.ansi[i as usize % 16].0).into(),
        }
    }
}

/// The tab under the pointer while dragging: its icon and title on a
/// raised card.
impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = Colors::get(cx);
        let icon = match (&self.agent, self.icon) {
            (Some(agent), _) => agent.logo().element(14., c.ui.text),
            (None, Some((glyph, color))) => div()
                .font(crate::fonts::icons())
                .text_size(px(14.))
                .text_color(color)
                .child(SharedString::from(glyph.to_string()))
                .into_any_element(),
            (None, None) => Icon::Terminal.svg().size(px(14.)).text_color(c.ui.muted).into_any_element(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .max_w(px(260.))
            .h(px(32.))
            .px(px(12.))
            .rounded(px(radius::CONTROL))
            .bg(c.terminal)
            .border_1()
            .border_color(c.ui.accent.opacity(0.6))
            .shadow_md()
            .text_size(px(text::LABEL))
            .text_color(c.ui.text)
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .child(icon)
            .child(div().min_w_0().truncate().child(self.label.clone()))
    }
}

/// What one tab shows.
pub(super) struct TabView {
    pub index: usize,
    pub id: u64,
    pub label: SharedString,
    pub active: bool,
    pub process: Option<String>,
    pub panes: usize,
    /// What happened in it while in the background.
    pub activity: Option<Activity>,
    /// The coding agent in its focused pane (its logo is the tab's icon).
    pub agent: Option<Identity>,
    /// The most important agent status among its panes (not idle).
    pub status: Option<AgentStatus>,
    /// How far along the focused pane's work is (an agent's progress, else
    /// an OSC 9;4 progress bar).
    pub progress: Option<Bar>,
    /// The sidebar's second line: the directory, and the host when remote.
    pub detail: Option<SharedString>,
    /// Still connecting (no session yet).
    pub connecting: bool,
    /// Being dragged: drawn faded where it is.
    pub dragged: bool,
    /// Its host group's color, when tabs are grouped by host.
    pub group: Option<Hsla>,
    /// The last tab of its host group (the group's line ends here).
    pub group_end: bool,
    /// Drawn after another background tab: a faint divider between them.
    pub divider: bool,
    /// Right after / right before the active tab in the strip (a group's
    /// line makes room for its flared corners).
    pub prev_active: bool,
    pub next_active: bool,
}

/// A progress line: how full, or moving when it can't tell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Bar {
    /// `0.0..=1.0`, `None` while indeterminate.
    pub fill: Option<f32>,
    pub look: BarLook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarLook {
    Normal,
    Error,
    Paused,
}

/// Which way tabs are laid out (for dragging).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Axis {
    Row,
    Column,
}

impl Workspace {
    pub(super) fn render_tab_bar(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let strip = self.strip_items(cx);
        let c = Colors::get(cx);
        let style = cx.global::<AppSettings>().client.tabs.style;
        let modern = style != TabStyle::Compact;
        let (labels, tabs) = strip.iter().fold((0.0, 0), |(l, t), i| match i {
            StripItem::Group(g) => (l + group_label_width(g), t),
            StripItem::Tab(_) => (l, t + 1),
        });
        let active_at = strip.iter().position(|i| matches!(i, StripItem::Tab(t) if t.active));
        let win_bar = self.windows_custom_title_bar;
        let csd = self.has_custom_title_bar(window);
        let control = if modern { 24.0 } else { 18.0 };
        let left = chrome::window_buttons(chrome::Side::Left, control, win_bar, c, window, cx);
        let right = chrome::window_buttons(chrome::Side::Right, control, win_bar, c, window, cx);
        // The settings button, the new-tab menu's chevron, and the window
        // buttons when goatty draws them.
        let chrome_room = 32.0 + 12.0 + chrome::buttons_width(control, win_bar, window, cx) + self.chip_width();
        let room = f32::from(window.viewport_size().width) - chrome::frame_width(window) - labels - chrome_room;
        let width = modern_tab_width(room, tabs);
        let height = if modern { MODERN_HEIGHT } else { COMPACT_HEIGHT };
        // A divider goes between two background tabs, never next to the
        // active one or a group's label.
        let mut after_tab = false;
        let items: Vec<AnyElement> = strip
            .into_iter()
            .enumerate()
            .map(|(at, item)| {
                let next_active = active_at == Some(at + 1);
                match item {
                    StripItem::Group(g) => {
                        after_tab = false;
                        self.group_label(g, modern, at == 0, next_active, c, cx)
                    }
                    StripItem::Tab(mut t) => {
                        t.divider = after_tab && !t.active;
                        t.prev_active = at > 0 && active_at == Some(at - 1);
                        t.next_active = next_active;
                        after_tab = !t.active;
                        if modern { self.modern_tab(t, width, c, cx) } else { self.compact_tab(t, c, cx) }
                    }
                }
            })
            .collect();
        let new_tab = div()
            .id("tab-new")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(c.ui.muted)
            .hover(|s| s.bg(c.hover).text_color(c.ui.text))
            .tooltip(tooltip("New tab (right-click for more)"))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewTab), cx))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_new_tab_menu(ev.position, window, cx);
                }),
            );
        let new_tab = if modern {
            new_tab.w(px(26.)).h(px(28.)).rounded_l_md().child(Icon::Plus.svg().size(px(15.)).text_color(c.ui.muted))
        } else {
            new_tab.h_full().pl(px(10.)).pr(px(6.)).child("+")
        };
        // Where else a new tab can go: the same menu as a right click on +.
        let more = div()
            .id("tab-new-more")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(c.hover))
            .tooltip(tooltip("New tab on another host, detached tabs, new window"))
            .child(Icon::ChevronDown.svg().size(px(if modern { 12. } else { 11. })).text_color(c.ui.muted))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_new_tab_menu(ev.position, window, cx);
                }),
            );
        let more = if modern { more.w(px(16.)).h(px(28.)).rounded_r_md() } else { more.h_full().pl(px(2.)).pr(px(6.)) };
        // + and its menu's chevron as one split button.
        let new_tab = div()
            .flex_none()
            .flex()
            .when(modern, |d| d.items_center().mb(px(MODERN_LIFT)))
            .when(!modern, |d| d.h_full())
            .child(new_tab)
            .child(more);
        let settings = div()
            .id("tab-settings")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(c.ui.muted)
            .hover(|s| s.bg(c.hover))
            .tooltip(tooltip("Settings"))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx));
        let settings = if modern {
            settings
                .size(px(28.))
                .mb(px(MODERN_LIFT))
                .when(win_bar, |d| d.mr(px(4.)))
                .rounded_md()
                .child(Icon::Sliders.svg().size(px(15.)).text_color(c.ui.muted))
        } else {
            settings.h_full().px(px(8.)).child(Icon::Sliders.svg().size(px(13.)).text_color(c.ui.muted))
        };
        // With goatty's title bar, the empty space is the title bar's.
        let rest = self.tabs_rest("tab-bar-rest", !csd, cx).flex_1().h_full();
        // Some room to grab even when the tabs overflow and scroll.
        let rest = if csd { self.title_bar_area(rest.min_w(px(48.)), window, cx) } else { rest };
        let bar = div()
            .id("tab-bar")
            .flex_none()
            .h(px(height))
            .w_full()
            .relative()
            .flex()
            .bg(c.bar)
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .on_scroll(cx.listener(Self::on_tabs_scroll))
            .when(modern, |d| d.items_end().pl(px(6.)).when(!win_bar, |d| d.pr(px(6.))).gap(px(2.)))
            .when(!modern, |d| d.items_stretch().text_size(px(12.)))
            // The seam to the panes, which the active tab opens.
            .child(div().absolute().bottom_0().left_0().right_0().h(px(1.)).bg(seam(c)))
            .children(left)
            .children(
                self.workspace_chip(false, cx)
                    .map(|chip| div().flex_none().h_full().flex().items_center().when(modern, |d| d.mb(px(MODERN_LIFT))).child(chip)),
            )
            .child(
                // The tabs scroll sideways when they don't fit.
                div()
                    .id("tab-strip")
                    .flex()
                    .min_w_0()
                    .flex_shrink(1.)
                    .h_full()
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    // Room for the active tab's flared corners.
                    .when(modern, |d| d.items_end().gap(px(2.)).px(px(FLARE)))
                    .children(items),
            )
            .child(new_tab)
            .child(rest)
            .child(settings)
            .children(right);
        chrome::round_top(bar, window).into_any_element()
    }

    /// The empty space after the tabs: double-click for a new tab (with
    /// `new_tab`), drag a tab here to move it to the end.
    pub(super) fn tabs_rest(&self, id: &'static str, new_tab: bool, cx: &mut Context<'_, Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .when(new_tab, |d| {
                d.on_click(|ev, window, cx| {
                    if ev.click_count() == 2 {
                        window.dispatch_action(Box::new(NewTab), cx);
                    }
                })
            })
            .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<DraggedTab>, window, cx| {
                if !this.moved_unpainted && ev.bounds.contains(&ev.event.position) {
                    let id = ev.drag(cx).id;
                    this.dragging = Some(id);
                    this.move_tab_to(id, usize::MAX, window, cx);
                    this.moved_unpainted = true;
                }
            }))
            .on_drop(cx.listener(|this, d: &DraggedTab, window, cx| this.move_tab_to(d.id, usize::MAX, window, cx)))
    }

    pub(super) fn tab_views(&self, cx: &App) -> Vec<TabView> {
        self.tabs.iter().enumerate().map(|(i, t)| self.tab_view(i, t, cx)).collect()
    }

    fn tab_view(&self, index: usize, tab: &Tab, cx: &App) -> TabView {
        let view = tab.focused_view().map(|v| v.read(cx));
        let process = view.and_then(|v| v.process().map(str::to_string));
        let statuses: Vec<AgentStatus> = tab.panes.iter().filter_map(|p| p.view.read(cx).agent_status(p.seen)).collect();
        let status = agent::most_important(statuses.iter()).cloned();
        let agent = view.and_then(|v| v.agent());
        let progress = view.and_then(|v| {
            let own = v.agent_status(u64::MAX).filter(|s| s.state == StatusState::Working);
            match own.and_then(|s| s.progress) {
                Some(p) => Some(Bar { fill: Some(f32::from(p) / 100.0), look: BarLook::Normal }),
                None => v.program_status().progress.map(|p| Bar {
                    fill: p.percent.filter(|_| p.state != ProgressState::Indeterminate).map(|n| f32::from(n) / 100.0),
                    look: match p.state {
                        ProgressState::Error => BarLook::Error,
                        ProgressState::Paused => BarLook::Paused,
                        _ => BarLook::Normal,
                    },
                }),
            }
        });
        let active = index == self.active;
        // Turning a mark off in the settings hides the ones already there.
        let activity = tab.activity.filter(|&a| !active && marks(&cx.global::<AppSettings>().client.tabs, a));
        let connecting = tab.connecting.is_some();
        let detail = if connecting {
            Some("Connecting…".into())
        } else {
            let host = view.and_then(|v| v.session().host_label.clone()).or_else(|| match &tab.host {
                Some(crate::session::Spawner::Plain { host, .. }) => Some(host.clone()),
                _ => None,
            });
            let home = std::env::var_os("HOME");
            let dir = view.and_then(|v| v.cwd()).map(|d| super::short_dir(&d, home.as_deref().map(std::path::Path::new)));
            match (dir, host) {
                (Some(d), Some(h)) => Some(format!("{d} · {h}").into()),
                (Some(d), None) => Some(d.into()),
                (None, Some(h)) => Some(h.into()),
                (None, None) => None,
            }
        };
        TabView {
            index,
            id: tab.id,
            label: self.tab_label(tab, cx).into(),
            active,
            process,
            panes: tab.panes.len(),
            activity,
            agent,
            status,
            progress,
            detail,
            connecting,
            dragged: self.dragging == Some(tab.id),
            group: None,
            group_end: false,
            divider: false,
            prev_active: false,
            next_active: false,
        }
    }

    /// The mouse wheel over the tabs: switch tabs (`tabs.scroll_to_switch`),
    /// else let the tabs scroll.
    pub(super) fn on_tabs_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !cx.global::<AppSettings>().client.tabs.scroll_to_switch {
            return;
        }
        // Not the strip's own scrolling (the active tab stays in view).
        cx.stop_propagation();
        let dominant = |x: f32, y: f32| if y.abs() >= x.abs() { y } else { x };
        let wheel = match ev.delta {
            ScrollDelta::Lines(p) => Wheel::Lines(dominant(p.x, p.y)),
            ScrollDelta::Pixels(p) => Wheel::Pixels(dominant(f32::from(p.x), f32::from(p.y))),
        };
        let steps = self.wheel.feed(wheel, Instant::now());
        if steps != 0 {
            self.step_tab(steps, window, cx);
        }
    }

    /// The tab's mark, `size` px across: its agent's state when it has one
    /// to show, else what happened in the background. Hovering it says
    /// what it means.
    pub(super) fn tab_marker(&self, t: &TabView, size: f32, c: Colors) -> Option<AnyElement> {
        match &t.status {
            Some(status) => Some(status_marker(status, ("tab-status", t.id), size, c)),
            None => self.activity_marker(t, size, c),
        }
    }

    /// The activity marker, `size` px across: a bell, or a dot colored by
    /// what happened. Hovering it says what it means.
    pub(super) fn activity_marker(&self, t: &TabView, size: f32, c: Colors) -> Option<AnyElement> {
        let activity = t.activity?;
        let (tip, color) = activity_look(activity, c);
        let mark = match activity {
            Activity::Bell => Icon::Bell.svg().size(px(size)).text_color(color).into_any_element(),
            _ => div().size(px(size * 0.55)).rounded_full().bg(color).into_any_element(),
        };
        Some(
            div()
                .id(gpui::ElementId::NamedInteger("tab-activity".into(), t.id))
                .flex_none()
                .size(px(size))
                .flex()
                .items_center()
                .justify_center()
                .tooltip(tooltip(tip))
                .child(mark)
                .into_any_element(),
        )
    }

    /// The program icon in `size` px, or `None` (unknown program, icons
    /// off). `color` overrides the icon's own color.
    pub(super) fn process_icon(&self, process: Option<&str>, size: f32, color: Option<Hsla>, c: Colors, cx: &App) -> Option<AnyElement> {
        let (glyph, own) = self.process_glyph(process, c, cx)?;
        Some(
            div()
                .flex_none()
                .w(px(size + 2.))
                .flex()
                .justify_center()
                .font(crate::fonts::icons())
                .text_size(px(size))
                .line_height(px(size + 2.))
                .text_color(color.unwrap_or(own))
                .child(SharedString::from(glyph.to_string()))
                .into_any_element(),
        )
    }

    /// The program's glyph and color, unless icons are off or it has none.
    fn process_glyph(&self, process: Option<&str>, c: Colors, cx: &App) -> Option<(char, Hsla)> {
        if !cx.global::<AppSettings>().client.tabs.icons {
            return None;
        }
        let icon = icon_for(process?)?;
        Some((icon.glyph, c.icon(icon.color, cx)))
    }

    /// The tab's icon: a spinner while it connects, the agent's logo, the
    /// program's icon, or `fallback`.
    pub(super) fn tab_icon(&self, t: &TabView, size: f32, c: Colors, cx: &App, fallback: impl FnOnce() -> AnyElement) -> AnyElement {
        if t.connecting {
            return Icon::Loader
                .svg()
                .size(px(size))
                .text_color(c.ui.muted)
                .with_animation(("tab-connecting", t.id), Animation::new(Duration::from_millis(900)).repeat(), |s, t| {
                    s.with_transformation(Transformation::rotate(percentage(t)))
                })
                .into_any_element();
        }
        if let Some(logo) = self.agent_logo(t, size, c.ui.text, cx) {
            return logo;
        }
        self.process_icon(t.process.as_deref(), size, None, c, cx).unwrap_or_else(fallback)
    }

    /// The tab's agent's logo (unless icons are off), its marks tinted `tint`.
    pub(super) fn agent_logo(&self, t: &TabView, size: f32, tint: Hsla, cx: &App) -> Option<AnyElement> {
        if !cx.global::<AppSettings>().client.tabs.icons {
            return None;
        }
        Some(t.agent.as_ref()?.logo().element(size, tint))
    }

    /// What every style's tab does: switch, close, rename, open its menu,
    /// move while dragged (along `axis`), switch with the wheel.
    pub(super) fn tab_shell(&self, t: &TabView, axis: Axis, cx: &mut Context<'_, Self>) -> gpui::Stateful<gpui::Div> {
        let (index, id) = (t.index, t.id);
        let c = Colors::get(cx);
        let icons = cx.global::<AppSettings>().client.tabs.icons;
        let drag = DraggedTab {
            id,
            label: t.label.clone(),
            icon: self.process_glyph(t.process.as_deref(), c, cx),
            agent: t.agent.clone().filter(|_| icons),
        };
        let renaming = self.rename.as_ref().is_some_and(|r| r.tab == id);
        div()
            .id(gpui::ElementId::NamedInteger("tab".into(), id))
            .group(SharedString::from(format!("tab-{id}")))
            .relative()
            .flex()
            .items_center()
            .min_w_0()
            .cursor_pointer()
            .when(t.dragged, |d| d.opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    // The active tab: leave focus alone (it may be in the rename field).
                    if this.active != index {
                        this.activate(index, window, cx);
                    }
                }),
            )
            .on_mouse_up(MouseButton::Middle, cx.listener(move |this, _, window, cx| this.close_tab_at(index, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_tab_menu(id, ev.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                if ev.click_count() == 2 {
                    this.start_rename(index, window, cx);
                }
            }))
            .on_scroll(cx.listener(Self::on_tabs_scroll))
            // Dragging in the rename field selects text instead.
            .when(!renaming, |d| d.on_drag(drag, |d, _, _, cx| cx.new(|_| d.clone())))
            .on_drag_move(cx.listener(move |this, ev: &DragMoveEvent<DraggedTab>, window, cx| {
                // Every tab hears every move: only the one under the pointer
                // acts, and only on bounds from after the last move.
                if this.moved_unpainted || !ev.bounds.contains(&ev.event.position) {
                    return;
                }
                let dragged = ev.drag(cx).id;
                if this.dragging != Some(dragged) {
                    this.dragging = Some(dragged);
                    cx.notify();
                }
                let (Some(from), Some(over)) = (this.tab_index(dragged), this.tab_index(id)) else { return };
                let (pos, start, end) = match axis {
                    Axis::Row => (ev.event.position.x, ev.bounds.left(), ev.bounds.right()),
                    Axis::Column => (ev.event.position.y, ev.bounds.top(), ev.bounds.bottom()),
                };
                if let Some(to) = reorder_target(from, over, f32::from(pos), f32::from(start), f32::from(end)) {
                    this.move_tab_to(dragged, to, window, cx);
                    this.moved_unpainted = true;
                }
            }))
    }

    /// The tab's title, or the rename field while renaming it.
    pub(super) fn tab_title(&self, t: &TabView, cx: &mut Context<'_, Self>) -> AnyElement {
        match self.rename.as_ref().filter(|r| r.tab == t.id) {
            Some(r) => {
                div().flex_1().min_w(px(80.)).on_key_down(cx.listener(Self::on_rename_key)).child(r.input.clone()).into_any_element()
            }
            None => div().flex_1().min_w_0().truncate().child(t.label.clone()).into_any_element(),
        }
    }

    /// The close button, `size` px: always on the active tab, on hover on
    /// the others. Shift+click detaches the tab instead of closing it.
    pub(super) fn close_button(&self, t: &TabView, size: f32, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let index = t.index;
        let group = SharedString::from(format!("tab-{}", t.id));
        let tip = if self.cannot_detach_tab(index, cx).is_none() { "Close tab (Shift+click: detach)" } else { "Close tab" };
        div()
            .id(gpui::ElementId::NamedInteger("tab-close".into(), t.id))
            .flex_none()
            .size(px(size))
            .rounded(px(radius::SMALL))
            .flex()
            .items_center()
            .justify_center()
            .hover(|s| s.bg(c.ui.text.opacity(0.1)))
            .tooltip(tooltip(tip))
            .child(Icon::Close.svg().size(px(size * 0.66)).text_color(c.ui.muted))
            .when(!t.active, |d| d.invisible().group_hover(group, |s| s.visible()))
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                if ev.modifiers().shift {
                    this.detach_tab_at(index, window, cx);
                } else {
                    this.close_tab_at(index, window, cx);
                }
            }))
            .into_any_element()
    }

    fn modern_tab(&self, t: TabView, width: f32, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let hover_group = SharedString::from(format!("tab-{}", t.id));
        let icon = self.tab_icon(&t, 14.0, c, cx, || Icon::Terminal.svg().size(px(14.)).text_color(c.ui.muted).into_any_element());
        let icon = quiet_icon(icon, t.active, hover_group.clone());
        // Narrow background tabs leave the room to the title (middle-click
        // or the menu still close them).
        let close = (t.active || width >= NARROW_TAB).then(|| self.close_button(&t, 18.0, c, cx));
        let title = self.tab_title(&t, cx);
        let marker = self.tab_marker(&t, 13.0, c);
        let waiting = !t.active && is_waiting(&t);
        let panes = (t.panes > 1).then(|| pane_count(t.panes, c));
        let shell = self
            .tab_shell(&t, Axis::Row, cx)
            .flex_none()
            .w(px(width))
            .gap(px(if width < NARROW_TAB { 6. } else { 8. }))
            .text_size(px(text::LABEL))
            .child(icon)
            .child(title)
            .children(marker)
            .children(panes)
            .children(close);
        let shell = if t.active {
            // Outlined in its group's color (continuing the group's line up
            // its flared corners and around it), else by the bar's seam; it
            // reaches down and merges into the panes.
            // Its outline takes room: the padding gives it back, so the
            // content sits where a background tab's does (#312).
            let (w, line) = outline(&t, c);
            shell
                .h(px(MODERN_TAB_HEIGHT))
                .pl(px(10. - w))
                .pr(px(6. - w))
                .pb(px(2. + w))
                .rounded_t(px(10.))
                .bg(c.terminal)
                .border_t(px(w))
                .border_l(px(w))
                .border_r(px(w))
                .border_color(line)
                .text_color(c.ui.text)
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(flare(true, w, line, c))
                .child(flare(false, w, line, c))
        } else {
            // Background tabs float above the bar's bottom edge, where their
            // group's line runs; the line makes room for the active tab's
            // flared corners next to it.
            let room = FLARE - 2.0 - ACTIVE_GROUP_LINE;
            let left = if t.prev_active { room } else { 0. };
            let right = if t.group_end {
                2.
            } else if t.next_active {
                room
            } else {
                -2.
            };
            shell
                .pl(px(10.))
                .pr(px(6.))
                .h(px(MODERN_BACK_HEIGHT))
                .mb(px(MODERN_LIFT))
                .rounded(px(radius::CONTROL - 2.))
                .text_color(c.ui.muted)
                .hover(|s| s.bg(c.hover).text_color(c.ui.text))
                .when(waiting, |d| d.bg(c.ui.warning.opacity(0.14)).text_color(c.ui.text))
                .when(t.divider, |d| d.child(divider(hover_group, -1.5, c)))
                .when_some(t.group, |d, color| d.child(group_line(color, -MODERN_LIFT, left, right, false, t.group_end)))
        };
        shell.children(t.progress.map(|bar| progress_line(bar, ("tab-progress", t.id), 10.0, c))).into_any_element()
    }

    fn compact_tab(&self, t: TabView, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let mono = cx.global::<AppSettings>().client.font.family.clone().unwrap_or_else(|| "monospace".into());
        let hover_group = SharedString::from(format!("tab-{}", t.id));
        let fg = if t.active { c.ui.text } else { c.ui.muted };
        let icon = self.agent_logo(&t, 12.0, fg, cx).or_else(|| self.process_icon(t.process.as_deref(), 12.0, None, c, cx));
        let icon = icon.map(|i| quiet_icon(i, t.active, hover_group.clone()));
        let title = self.tab_title(&t, cx);
        let marker = self.tab_marker(&t, 11.0, c);
        let waiting = !t.active && is_waiting(&t);
        let number = SharedString::from(format!("{}", t.index + 1));
        let number_color = if t.active { t.group.unwrap_or(c.ui.accent_text) } else { c.ui.muted.opacity(0.75) };
        let (w, line) = outline(&t, c);
        self.tab_shell(&t, Axis::Row, cx)
            .flex_none()
            .max_w(px(220.))
            .gap(px(6.))
            .font_family(mono)
            .text_color(fg)
            .when(t.active, |d| {
                // Like the modern tab: outlined in its group's color (or by the
                // seam) and merging into the panes.
                // Inset from the window's top edge, so the outline's top shows.
                // The padding gives back the room the inset and the outline
                // take, so the content doesn't move when the tab activates (#312).
                d.bg(c.terminal)
                    .mt(px(2.))
                    .px(px(10. - w))
                    .pb(px(2. + w))
                    .rounded_t(px(4.))
                    .border_t(px(w))
                    .border_l(px(w))
                    .border_r(px(w))
                    .border_color(line)
                    .font_weight(gpui::FontWeight::MEDIUM)
            })
            .when(!t.active, |d| d.px(px(10.)).hover(|s| s.bg(c.hover).text_color(c.ui.text)))
            .when(waiting, |d| d.bg(c.ui.warning.opacity(0.16)).text_color(c.ui.text))
            .when(t.divider, |d| d.child(divider(hover_group, -0.5, c)))
            .child(div().flex_none().text_color(number_color).child(number))
            .children(icon)
            .child(title)
            .children(marker)
            .when(t.panes > 1, |d| d.child(div().flex_none().opacity(0.7).child(SharedString::from(format!("[{}]", t.panes)))))
            .when_some(t.group.filter(|_| !t.active), |d, color| {
                d.child(group_line(color, 0.0, 0.0, if t.group_end { 2. } else { 0. }, false, t.group_end))
            })
            .children(t.progress.map(|bar| progress_line(bar, ("tab-progress", t.id), 4.0, c)))
            .into_any_element()
    }

    /// A host group's chip, as the strip and the sidebar show it: the
    /// host's name, solid in its color while the group is open (this
    /// computer's chip is a quiet neutral one); folded, a tinted chip with
    /// a chevron, how many tabs it holds and their activity. `small` for
    /// the compact strip. Click: fold or unfold (Shift: detach);
    /// right-click: the group's menu.
    pub(super) fn group_chip(&self, g: &Group, small: bool, c: Colors, cx: &mut Context<'_, Self>) -> gpui::Stateful<gpui::Div> {
        let key = g.key.clone();
        let menu_key = g.key.clone();
        let folded = g.folded;
        let tip = group_tooltip(g);
        let mark = group_mark(g, c).filter(|_| folded).map(|color| div().flex_none().size(px(6.)).rounded_full().bg(color));
        let has_mark = mark.is_some();
        let count = folded.then(|| {
            let size = if small { 13. } else { 16. };
            div()
                .flex_none()
                .min_w(px(size))
                .h(px(size))
                .px(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(g.color.opacity(0.24))
                .text_size(px(if small { 9.5 } else { 10.5 }))
                .child(SharedString::from(g.tabs.len().to_string()))
        });
        let (fg, glyph) = if folded {
            (c.ui.text.opacity(0.9), Some((Icon::ChevronRight, g.color)))
        } else if g.local {
            (c.ui.text.opacity(0.85), Some((Icon::Monitor, c.ui.muted)))
        } else {
            (on_color(g.color), None)
        };
        let pl = if glyph.is_some() { 7. } else { 9. };
        let pr = match (folded, has_mark) {
            (false, _) => 9.,
            (true, true) => 7.,
            (true, false) => 4.,
        };
        let color = g.color;
        let chip = div()
            .id(gpui::ElementId::NamedInteger("tab-group".into(), g.ids[0]))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.))
            .max_w(px(160.))
            .rounded_full()
            .cursor_pointer()
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(fg)
            .map(|d| match (folded, g.local) {
                (true, _) => d.bg(color.opacity(0.14)).border_1().border_color(color.opacity(0.45)).hover(|s| s.bg(color.opacity(0.22))),
                // Opaque (blended onto the bar), like its group's line.
                (false, true) => d
                    .bg(c.bar.blend(c.ui.text.opacity(0.08)))
                    .border_1()
                    .border_color(c.bar.blend(c.ui.text.opacity(0.14)))
                    .hover(|s| s.bg(c.bar.blend(c.ui.text.opacity(0.14)))),
                (false, false) => d.bg(color).hover(|s| s.opacity(0.85)),
            })
            .tooltip(tooltip(tip))
            .children(glyph.map(|(icon, color)| icon.svg().flex_none().size(px(if small { 10. } else { 11. })).text_color(color)))
            .child(div().min_w_0().truncate().child(g.label.clone()))
            .children(count)
            .children(mark)
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                cx.stop_propagation();
                if ev.modifiers().shift {
                    this.detach_group(&key, window, cx);
                } else {
                    this.fold_group(&key, !folded, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_group_menu(menu_key.clone(), ev.position, window, cx);
                    cx.stop_propagation();
                }),
            );
        if small {
            chip.h(px(19.)).pl(px(pl - 2.)).pr(px(pr - 1.)).text_size(px(10.5))
        } else {
            chip.h(px(22.)).pl(px(pl)).pr(px(pr)).text_size(px(text::CAPTION))
        }
    }

    /// A host group's label in the strip: its chip (`group_chip`), with the
    /// group's line along the bar's bottom edge starting under it.
    fn group_label(&self, g: Group, modern: bool, first: bool, next_active: bool, c: Colors, cx: &mut Context<'_, Self>) -> AnyElement {
        let folded = g.folded;
        let chip = self.group_chip(&g, !modern, c, cx);
        // The chip sits in a slot as tall as a background tab, so the
        // group's line starts under it.
        let (slot_h, lift, gap) = if modern { (MODERN_BACK_HEIGHT, MODERN_LIFT, 2.) } else { (COMPACT_HEIGHT, 0., 0.) };
        let right = if next_active && modern { FLARE - 2. - ACTIVE_GROUP_LINE } else { -gap };
        div()
            .flex_none()
            .relative()
            .flex()
            .items_center()
            .h(px(slot_h))
            .mb(px(lift))
            // Groups keep a little apart from each other.
            .when(!first, |d| d.ml(px(if modern { 8. } else { 6. })))
            .pl(px(6.))
            .pr(px(4.))
            .child(chip)
            .when(!folded, |d| d.child(group_line(g.color, -lift, 6., right, true, false)))
            .into_any_element()
    }
}

/// A host group's label, hovered: the host, its tabs, what a click does.
pub(super) fn group_tooltip(g: &Group) -> String {
    let mut tip = g.label.to_string();
    if let Some(d) = &g.detail {
        tip.push_str(&format!(" ({d})"));
    }
    let n = g.tabs.len();
    let tabs = if n == 1 { "1 tab".to_string() } else { format!("{n} tabs") };
    let click = if g.folded { "click to show them" } else { "click to fold them" };
    format!("{tip}\n{tabs} · {click} · Shift+click to detach · right-click for more")
}

/// How wide the active tab's outline is when it's in a host group (as
/// wide as the group's line, which it continues).
const ACTIVE_GROUP_LINE: f32 = 2.0;

/// A stretch of a host group's line along the bottom of the bar, under
/// its label and background tabs. `bottom` places it at the bar's edge
/// from an item lifted above it; `left`/`right` are insets (negative
/// bridges the gap to the next item); it rounds off where the group
/// starts and ends.
fn group_line(color: Hsla, bottom: f32, left: f32, right: f32, start: bool, end: bool) -> impl IntoElement {
    div()
        .absolute()
        .bottom(px(bottom))
        .h(px(2.))
        .left(px(left))
        .right(px(right))
        .bg(color)
        .when(start, |d| d.rounded_l(px(1.)))
        .when(end, |d| d.rounded_r(px(1.)))
}

/// The active tab's outline: its host group's color, as wide as the
/// group's line, else the bar's seam.
pub(super) fn outline(t: &TabView, c: Colors) -> (f32, Hsla) {
    match t.group {
        Some(color) => (ACTIVE_GROUP_LINE, color),
        None => (1.0, seam(c)),
    }
}

/// The line between the bar and the panes, which the active tab opens.
/// Opaque (blended onto the bar): it meets the active tab's outline and
/// flared corners, where a translucent line would show darker overlaps.
pub(super) fn seam(c: Colors) -> Hsla {
    c.bar.blend(c.ui.text.opacity(if c.ui.dark { 0.1 } else { 0.12 }))
}

/// The faint line between two background tabs, `left` px off the tab's
/// left edge; it hides while the tab is hovered.
fn divider(hover_group: SharedString, left: f32, c: Colors) -> impl IntoElement {
    div()
        .absolute()
        .left(px(left))
        .top(gpui::relative(0.5))
        .mt(px(-7.))
        .h(px(14.))
        .w(px(1.))
        .bg(c.ui.text.opacity(0.12))
        .group_hover(hover_group, |s| s.invisible())
}

/// A background tab's icon recedes a little, until the tab is hovered.
pub(super) fn quiet_icon(icon: AnyElement, active: bool, hover_group: SharedString) -> AnyElement {
    if active {
        return icon;
    }
    div().flex_none().flex().opacity(0.65).group_hover(hover_group, |s| s.opacity(1.)).child(icon).into_any_element()
}

/// One of the active modern tab's flared bottom corners, curving its
/// sides out into the bar like a browser tab. Its edge, `w` px of `line`,
/// carries the tab's outline on into the bar's seam or group line.
fn flare(left: bool, w: f32, line: Hsla, c: Colors) -> impl IntoElement {
    let cut = div().size_full().bg(c.bar).border_b(px(w)).border_color(line);
    let cut = if left { cut.border_r(px(w)).rounded_br(px(FLARE)) } else { cut.border_l(px(w)).rounded_bl(px(FLARE)) };
    let corner = div().absolute().bottom_0().size(px(FLARE)).bg(c.terminal).child(cut);
    if left { corner.left(px(-FLARE)) } else { corner.right(px(-FLARE)) }
}

/// How many panes a tab holds, as a small badge.
pub(super) fn pane_count(n: usize, c: Colors) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(3.))
        .h(px(16.))
        .px(px(5.))
        .rounded(px(5.))
        .bg(c.ui.text.opacity(0.07))
        .text_size(px(10.5))
        .text_color(c.ui.muted)
        .child(Icon::Columns2.svg().size(px(10.)).text_color(c.ui.muted))
        .child(SharedString::from(n.to_string()))
}

/// Text that reads on `bg`: near-black on light colors, else near-white.
/// The cut is above WCAG's even-contrast point (about 0.18): its luminance
/// weighs green so heavily that mid-tone greens would get muddy dark text.
fn on_color(bg: Hsla) -> Hsla {
    if luminance(bg) > 0.3 { gpui::hsla(bg.h, 0.25, 0.1, 1.0) } else { gpui::hsla(bg.h, 0.2, 0.98, 1.0) }
}

/// WCAG relative luminance.
fn luminance(c: Hsla) -> f32 {
    let rgb = c.to_rgb();
    let lin = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    0.2126 * lin(rgb.r) + 0.7152 * lin(rgb.g) + 0.0722 * lin(rgb.b)
}

/// About how wide a host group's chip is in the modern strip (kept free
/// when sizing the tabs).
fn group_label_width(g: &Group) -> f32 {
    let text = g.label.chars().count() as f32 * 6.0;
    let extras = if g.folded {
        44.0
    } else if g.local {
        14.0
    } else {
        0.0
    };
    // The chip's padding, its slot's, the room between groups.
    (text + extras + 18.0 + 10.0 + 8.0).min(180.0)
}

/// The dot a folded host group shows: its tabs' waiting agents first,
/// then their activity.
pub(super) fn group_mark(g: &Group, c: Colors) -> Option<Hsla> {
    if g.waiting {
        return Some(c.ui.warning);
    }
    g.activity.map(|a| activity_look(a, c).1)
}

/// Does the tab have an agent waiting for the user?
pub(super) fn is_waiting(t: &TabView) -> bool {
    t.status.as_ref().is_some_and(|s| s.state == StatusState::Blocked)
}

/// The color of an agent's state.
pub(super) fn status_color(state: StatusState, c: Colors) -> Hsla {
    match state {
        StatusState::Blocked => c.ui.warning,
        StatusState::Error => c.ui.danger,
        StatusState::Done => c.ui.success,
        StatusState::Working => c.ui.accent,
        StatusState::Idle => c.ui.muted,
    }
}

/// An agent's state, `size` px across: a spinner while it works, a
/// pulsing badge while it waits for the user, a check or a cross when it's
/// done or failed. Its tooltip says who and what (and the agent's words).
pub(super) fn status_marker(status: &AgentStatus, id: (&'static str, u64), size: f32, c: Colors) -> AnyElement {
    let color = status_color(status.state, c);
    let icon = status.icon().unwrap_or(Icon::Bot);
    let base = div()
        .id(gpui::ElementId::NamedInteger(id.0.into(), id.1))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(tooltip(status.tooltip()));
    match status.state {
        StatusState::Working => base
            .size(px(size))
            .child(Icon::Loader.svg().size(px(size)).text_color(color).with_animation(
                gpui::ElementId::NamedInteger("status-spin".into(), id.1),
                Animation::new(Duration::from_millis(1100)).repeat(),
                |s, t| s.with_transformation(Transformation::rotate(percentage(t))),
            ))
            .into_any_element(),
        StatusState::Blocked => base
            .size(px(size + 6.))
            .rounded_full()
            .bg(color.opacity(0.2))
            .child(icon.svg().size(px(size - 1.)).text_color(color))
            .with_animation(
                gpui::ElementId::NamedInteger("status-pulse".into(), id.1),
                Animation::new(Duration::from_millis(1600)).repeat().with_easing(gpui::pulsating_between(0.45, 1.0)),
                |s, t| s.opacity(t),
            )
            .into_any_element(),
        _ => base.size(px(size)).child(icon.svg().size(px(size)).text_color(color)).into_any_element(),
    }
}

/// A thin progress line along the bottom of a tab, `inset` px from its
/// sides: filled as far as the work is, or a moving segment while it
/// can't tell.
pub(super) fn progress_line(bar: Bar, id: (&'static str, u64), inset: f32, c: Colors) -> AnyElement {
    let color = match bar.look {
        BarLook::Normal => c.ui.accent,
        BarLook::Error => c.ui.danger,
        BarLook::Paused => c.ui.warning,
    };
    let track = div().absolute().bottom_0().left(px(inset)).right(px(inset)).h(px(2.)).rounded_full().bg(c.ui.text.opacity(0.08));
    let fill = div().absolute().top_0().bottom_0().rounded_full().bg(color);
    let fill = match bar.fill {
        Some(f) => fill.left_0().w(gpui::relative(f.clamp(0.0, 1.0))).into_any_element(),
        None => fill
            .w(gpui::relative(0.3))
            .with_animation(
                gpui::ElementId::NamedInteger(id.0.into(), id.1),
                Animation::new(Duration::from_millis(1400)).repeat().with_easing(gpui::ease_in_out),
                |s, t| s.left(gpui::relative(t * 0.7)),
            )
            .into_any_element(),
    };
    track.child(fill).into_any_element()
}

/// What an activity marker says and its color.
pub(super) fn activity_look(activity: Activity, c: Colors) -> (&'static str, Hsla) {
    match activity {
        Activity::Bell => ("Rang the bell", c.ui.accent),
        Activity::Failed => ("A command failed", c.ui.danger),
        Activity::Finished => ("A command finished", c.ui.success),
        Activity::Output => ("New output", c.ui.muted),
    }
}

/// A modern tab's width in a window `window` px wide with `tabs` tabs:
/// they share the strip (less the + button and some empty space to
/// double-click), between [`MIN_TAB`] and [`MAX_TAB`]; past that the strip
/// scrolls.
fn modern_tab_width(window: f32, tabs: usize) -> f32 {
    const RESERVED: f32 = 12.0 + 2.0 * FLARE + 30.0 + 48.0;
    let gaps = 2.0 * tabs.saturating_sub(1) as f32;
    ((window - RESERVED - gaps) / tabs.max(1) as f32).clamp(MIN_TAB, MAX_TAB)
}

#[cfg(test)]
mod tests {
    #[test]
    fn modern_tabs_share_the_strip() {
        use super::modern_tab_width as w;
        assert_eq!(w(1200.0, 1), 220.0);
        assert_eq!(w(1200.0, 0), 220.0);
        // 1200 - 106 reserved - 18 gaps = 1076 for 10 tabs.
        assert!((w(1200.0, 10) - 107.6).abs() < 0.01);
        assert_eq!(w(1200.0, 40), super::MIN_TAB, "then the strip scrolls");
    }

    #[test]
    fn chip_text_reads_on_its_color() {
        use super::{luminance, on_color};
        let yellow: gpui::Hsla = gpui::rgb(0xe5c07b).into();
        let navy: gpui::Hsla = gpui::rgb(0x1e3a8a).into();
        assert!(luminance(on_color(yellow)) < 0.1, "dark text on a light chip");
        assert!(luminance(on_color(navy)) > 0.8, "light text on a dark chip");
        let green: gpui::Hsla = gpui::rgb(0x40a02b).into();
        assert!(luminance(on_color(green)) > 0.8, "light text on a mid-tone green");
    }
}
