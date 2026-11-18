//! The window's title bar and frame, when goatty draws them (#120,
//! `window.title_bar`).
//!
//! goatty asks the desktop to decorate the window (`WindowDecorations::Server`)
//! unless `window.title_bar` is `goatty`. What counts is what the desktop
//! granted ([`csd`]): tiling compositors (niri, Hyprland, Sway) grant
//! server-side decorations and draw none, so goatty adds none either. GNOME
//! on Wayland has no xdg-decoration and leaves every window to draw its
//! own, so goatty does there whatever was asked. X11 without a compositor
//! can't do client-side decorations, so the window manager's stay.
//!
//! When goatty draws them:
//! - The horizontal tab bar is the title bar: the window buttons sit at its
//!   ends (the desktop's `button-layout`, only the ones the compositor
//!   supports). Dragging its empty space moves the window, double-clicking
//!   it maximizes, and right-clicking it opens the window menu.
//! - Vertical tabs and a hidden tab bar get a slim header that does that
//!   ([`Workspace::render_title_header`]).
//! - A window that isn't tiled gets a shadow, a hairline border, rounded
//!   top corners and resize edges ([`Workspace::window_frame`]).
//!
//! macOS and Windows aren't built yet; there the desktop's title bar stays.

use goatty_config::TitleBar;
use gpui::{
    AnyElement, App, Bounds, BoxShadow, Context, CursorStyle, Decorations, Hitbox, HitboxBehavior, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, ResizeEdge, Size, StatefulInteractiveElement, Styled, Tiling, Window, WindowButton,
    WindowButtonLayout, WindowDecorations, canvas, div, point, prelude::*, px,
};

use super::Workspace;
use super::tab_bar::Colors;
use crate::settings::AppSettings;
use crate::ui::controls::tooltip;
use crate::ui::icons::Icon;
use crate::ui::style::text;

/// How far the shadow reaches outside the window; also the band that
/// resizes it.
const SHADOW: Pixels = px(10.);
const ROUNDING: Pixels = px(10.);
const BORDER: Pixels = px(1.);
const HEADER_HEIGHT: f32 = 34.;
const BUTTON_GAP: f32 = 8.;
const BUTTON_PAD: f32 = 8.;

/// When the desktop doesn't say where the window buttons go.
const DEFAULT_LAYOUT: WindowButtonLayout =
    WindowButtonLayout { left: [None; 3], right: [Some(WindowButton::Minimize), Some(WindowButton::Maximize), Some(WindowButton::Close)] };

/// The decorations to ask the desktop for.
pub fn requested(title_bar: TitleBar) -> WindowDecorations {
    match title_bar {
        TitleBar::System => WindowDecorations::Server,
        TitleBar::Goatty => WindowDecorations::Client,
    }
}

/// How the window is tiled, when goatty draws its decorations; `None` when
/// the desktop does (or draws none on purpose).
pub(super) fn csd(window: &Window) -> Option<Tiling> {
    match window.window_decorations() {
        Decorations::Client { tiling } => Some(tiling),
        Decorations::Server => None,
    }
}

/// Which end of the title bar.
#[derive(Clone, Copy)]
pub(super) enum Side {
    Left,
    Right,
}

/// The window buttons at one end of the title bar, when goatty draws the
/// decorations: where the desktop puts them, those the compositor supports.
fn side_buttons(side: Side, window: &Window, cx: &App) -> Vec<WindowButton> {
    if csd(window).is_none() {
        return Vec::new();
    }
    let layout = cx.button_layout().unwrap_or(DEFAULT_LAYOUT);
    let supported = window.window_controls();
    match side {
        Side::Left => layout.left,
        Side::Right => layout.right,
    }
    .into_iter()
    .flatten()
    .filter(|b| match b {
        WindowButton::Minimize => supported.minimize,
        WindowButton::Maximize => supported.maximize,
        WindowButton::Close => true,
    })
    .collect()
}

/// How wide [`window_buttons`] are at both ends, `size` px buttons.
pub(super) fn buttons_width(size: f32, window: &Window, cx: &App) -> f32 {
    [Side::Left, Side::Right]
        .into_iter()
        .map(|side| match side_buttons(side, window, cx).len() {
            0 => 0.0,
            n => n as f32 * size + (n - 1) as f32 * BUTTON_GAP + 2.0 * BUTTON_PAD,
        })
        .sum()
}

/// The window buttons for one end of the title bar, `size` px each, when
/// goatty draws the decorations and the desktop puts some there.
pub(super) fn window_buttons(side: Side, size: f32, c: Colors, window: &Window, cx: &App) -> Option<AnyElement> {
    let maximized = window.is_maximized();
    let buttons: Vec<AnyElement> = side_buttons(side, window, cx).into_iter().map(|b| window_button(b, maximized, size, c)).collect();
    if buttons.is_empty() {
        return None;
    }
    Some(
        div()
            .flex_none()
            .h_full()
            .flex()
            .items_center()
            .gap(px(BUTTON_GAP))
            .px(px(BUTTON_PAD))
            // Not the start of a window move.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .children(buttons)
            .into_any_element(),
    )
}

fn window_button(button: WindowButton, maximized: bool, size: f32, c: Colors) -> AnyElement {
    let (icon, tip) = match button {
        WindowButton::Minimize => (Icon::WindowMinimize, "Minimize"),
        WindowButton::Maximize if maximized => (Icon::WindowRestore, "Restore"),
        WindowButton::Maximize => (Icon::WindowMaximize, "Maximize"),
        WindowButton::Close => (Icon::Close, "Close the window"),
    };
    div()
        .id(button.id())
        .flex_none()
        .size(px(size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(c.ui.text.opacity(0.08))
        .hover(|s| s.bg(c.ui.text.opacity(0.16)))
        .tooltip(tooltip(tip))
        .child(icon.svg().size(px(size * 0.62)).text_color(c.ui.text))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            match button {
                WindowButton::Minimize => window.minimize_window(),
                WindowButton::Maximize => window.zoom_window(),
                // Like the desktop's close button: the sessions keep running.
                WindowButton::Close => window.remove_window(),
            }
        })
        .into_any_element()
}

/// How much of the window's width goatty's frame takes (the shadow and
/// border along untiled sides); 0 without it.
pub(super) fn frame_width(window: &Window) -> f32 {
    let Some(t) = csd(window) else { return 0.0 };
    let side = f32::from(SHADOW + BORDER);
    [t.left, t.right].iter().filter(|tiled| !**tiled).count() as f32 * side
}

/// Round `el`'s top corners like the window's, when goatty draws its frame
/// (the topmost bar paints over the frame's corners).
pub(super) fn round_top<E: Styled>(el: E, window: &Window) -> E {
    let Some(tiling) = csd(window) else { return el };
    let el = if !tiling.top && !tiling.left { el.rounded_tl(ROUNDING - BORDER) } else { el };
    if !tiling.top && !tiling.right { el.rounded_tr(ROUNDING - BORDER) } else { el }
}

impl Workspace {
    /// Make `el` the title bar's empty space: drag to move the window,
    /// double-click to maximize, right-click for the window menu.
    pub(super) fn title_bar_area(
        &self,
        el: gpui::Stateful<gpui::Div>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let controls = window.window_controls();
        let zoom = controls.maximize && window.is_resizable();
        el.on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.title_drag = true))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.title_drag = false))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.title_drag = false))
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, window, _| {
                // The compositor takes over: no mouse-up may follow.
                if std::mem::take(&mut this.title_drag) && ev.dragging() {
                    window.start_window_move();
                }
            }))
            .on_click(move |ev, window, _| {
                if zoom && ev.click_count() == 2 {
                    window.zoom_window();
                }
            })
            .when(controls.window_menu, |d| {
                d.on_mouse_down(MouseButton::Right, |ev, window, cx| {
                    window.show_window_menu(ev.position);
                    cx.stop_propagation();
                })
            })
    }

    /// The header goatty draws when it draws the title bar but the tab bar
    /// can't be it (vertical tabs, or the tab bar is hidden): the window
    /// buttons around the active tab's title.
    pub(super) fn render_title_header(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let c = Colors::get(cx);
        let title = self.active_tab().map(|t| self.tab_label(t, cx)).unwrap_or_default();
        let area = div()
            .id("title-header-drag")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .px(px(8.))
            .text_size(px(text::SMALL))
            .text_color(c.ui.muted)
            .child(div().min_w_0().truncate().child(title));
        let area = self.title_bar_area(area, window, cx);
        let header = div()
            .id("title-header")
            .flex_none()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .flex()
            .bg(c.bar)
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .children(window_buttons(Side::Left, 22., c, window, cx))
            .child(area)
            .children(window_buttons(Side::Right, 22., c, window, cx));
        round_top(header, window).into_any_element()
    }

    /// Wrap the window's content in the frame goatty draws with its own
    /// decorations: room for the shadow, a hairline border, and edges that
    /// resize the window. As is without them.
    pub(super) fn window_frame(&mut self, content: AnyElement, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let tiling = csd(window);
        // Tell the platform only when it changes (X11 sets a property).
        if self.frame_applied != Some(tiling) {
            self.frame_applied = Some(tiling);
            let r = |round: bool| if round { ROUNDING } else { px(0.) };
            let (inset, corners) = match tiling {
                Some(t) => (
                    SHADOW,
                    gpui::Corners {
                        top_left: r(!t.top && !t.left),
                        top_right: r(!t.top && !t.right),
                        bottom_left: px(0.),
                        bottom_right: px(0.),
                    },
                ),
                None => (px(0.), gpui::Corners::default()),
            };
            window.set_client_inset(inset);
            window.set_client_corner_radii(corners);
        }
        let Some(tiling) = tiling else { return content };
        let c = Colors::get(cx);
        let resizable = window.is_resizable();
        let inner = div()
            .size_full()
            .cursor(CursorStyle::Arrow)
            .border_color(c.edge)
            .when(!tiling.top, |d| d.border_t(BORDER))
            .when(!tiling.bottom, |d| d.border_b(BORDER))
            .when(!tiling.left, |d| d.border_l(BORDER))
            .when(!tiling.right, |d| d.border_r(BORDER))
            .when(!tiling.top && !tiling.left, |d| d.rounded_tl(ROUNDING))
            .when(!tiling.top && !tiling.right, |d| d.rounded_tr(ROUNDING))
            .when(!tiling.is_tiled(), |d| {
                d.shadow(vec![BoxShadow {
                    color: gpui::black().opacity(0.4),
                    offset: point(px(0.), px(0.)),
                    blur_radius: SHADOW / 2.,
                    spread_radius: px(0.),
                    inset: false,
                }])
            })
            .child(content);
        div()
            .id("window-frame")
            .size_full()
            .when(!tiling.top, |d| d.pt(SHADOW))
            .when(!tiling.bottom, |d| d.pb(SHADOW))
            .when(!tiling.left, |d| d.pl(SHADOW))
            .when(!tiling.right, |d| d.pr(SHADOW))
            .child(inner)
            .when(resizable, |d| {
                d.on_mouse_move(cx.listener(move |this, ev: &gpui::MouseMoveEvent, window, cx| {
                    // Repaint when the pointer enters or leaves an edge (its cursor).
                    let edge = resize_edge(ev.position, window.viewport_size(), tiling);
                    if edge != this.resize_edge {
                        this.resize_edge = edge;
                        cx.notify();
                    }
                }))
                .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                    if let Some(edge) = resize_edge(ev.position, window.viewport_size(), tiling) {
                        window.start_window_resize(edge);
                        cx.stop_propagation();
                    }
                })
                .child(
                    canvas(
                        |_, window, _| window.insert_hitbox(Bounds::new(Point::default(), window.viewport_size()), HitboxBehavior::Normal),
                        move |_, hitbox: Hitbox, window, _| {
                            let size = window.viewport_size();
                            if let Some(edge) = resize_edge(window.mouse_position(), size, tiling) {
                                window.set_cursor_style(edge_cursor(edge), &hitbox);
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .into_any_element()
    }
}

fn edge_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// The edge `pos` resizes the window from: in the shadow band around it
/// (none along tiled sides), corners within twice the band of a corner.
fn resize_edge(pos: Point<Pixels>, window: Size<Pixels>, tiling: Tiling) -> Option<ResizeEdge> {
    let (x, y, w, h) = (pos.x, pos.y, window.width, window.height);
    let top = !tiling.top && y < SHADOW;
    let bottom = !tiling.bottom && y > h - SHADOW;
    let left = !tiling.left && x < SHADOW;
    let right = !tiling.right && x > w - SHADOW;
    let corner = SHADOW * 2.;
    let near_left = !tiling.left && x < corner;
    let near_right = !tiling.right && x > w - corner;
    let near_top = !tiling.top && y < corner;
    let near_bottom = !tiling.bottom && y > h - corner;
    Some(match () {
        _ if (top && near_left) || (left && near_top) => ResizeEdge::TopLeft,
        _ if (top && near_right) || (right && near_top) => ResizeEdge::TopRight,
        _ if (bottom && near_left) || (left && near_bottom) => ResizeEdge::BottomLeft,
        _ if (bottom && near_right) || (right && near_bottom) => ResizeEdge::BottomRight,
        _ if top => ResizeEdge::Top,
        _ if bottom => ResizeEdge::Bottom,
        _ if left => ResizeEdge::Left,
        _ if right => ResizeEdge::Right,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_edges() {
        let size = gpui::size(px(800.), px(600.));
        let free = Tiling::default();
        let at = |x: f32, y: f32, t| resize_edge(point(px(x), px(y)), size, t);
        assert_eq!(at(400., 300., free), None, "inside the window");
        assert_eq!(at(400., 12., free), None, "inside, near the top");
        assert_eq!(at(400., 3., free), Some(ResizeEdge::Top));
        assert_eq!(at(797., 300., free), Some(ResizeEdge::Right));
        assert_eq!(at(3., 3., free), Some(ResizeEdge::TopLeft));
        assert_eq!(at(15., 3., free), Some(ResizeEdge::TopLeft), "corners reach a bit along the edges");
        assert_eq!(at(795., 595., free), Some(ResizeEdge::BottomRight));
        let left = Tiling { left: true, top: true, bottom: true, right: false };
        assert_eq!(at(3., 300., left), None, "tiled sides don't resize");
        assert_eq!(at(3., 3., left), None);
        assert_eq!(at(797., 3., left), Some(ResizeEdge::Right));
        assert_eq!(requested(TitleBar::System), WindowDecorations::Server);
        assert_eq!(requested(TitleBar::Goatty), WindowDecorations::Client);
    }
}
