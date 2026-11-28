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
//!   supports; on Windows, native top-right caption buttons wired to
//!   `WindowControlArea` for Aero Snap and Windows 11 Snap Layouts).
//!   Dragging its empty space moves the window, double-clicking it maximizes,
//!   and right-clicking it opens the window menu.
//! - Vertical tabs and a hidden tab bar get a slim header that does that
//!   ([`Workspace::render_title_header`]).
//! - On Linux, a window that isn't tiled gets a shadow, a hairline border,
//!   rounded top corners and resize edges ([`Workspace::window_frame`]).
//!   On Windows, DWM provides the native frame, shadow, Windows 11 rounded
//!   corners, and resize borders automatically when `appears_transparent` is
//!   set on `TitlebarOptions`.
//!
//! The settings and welcome windows get the same on Linux
//! ([`secondary_window`]): a slim header with their title and the window
//! buttons, in the same frame. On Windows and macOS the system draws theirs.

use goatty_config::TitleBar;
use gpui::{
    AnyElement, App, Bounds, BoxShadow, Context, CursorStyle, Decorations, Hitbox, HitboxBehavior, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, ResizeEdge, Size, StatefulInteractiveElement, Styled, Tiling, TitlebarOptions, Window,
    WindowButton, WindowButtonLayout, WindowControlArea, WindowDecorations, WindowOptions, canvas, div, point, prelude::*, px, rgb,
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
/// Native Windows 10/11 caption button width (Minimize, Maximize, Close).
const WIN_BUTTON_WIDTH: f32 = 46.;

/// When the desktop doesn't say where the window buttons go.
const DEFAULT_LAYOUT: WindowButtonLayout =
    WindowButtonLayout { left: [None; 3], right: [Some(WindowButton::Minimize), Some(WindowButton::Maximize), Some(WindowButton::Close)] };

/// The decorations to ask the desktop for (Linux Wayland/X11).
pub fn requested(title_bar: TitleBar) -> WindowDecorations {
    match title_bar {
        TitleBar::System => WindowDecorations::Server,
        TitleBar::Goatty => WindowDecorations::Client,
    }
}

/// Titlebar options passed to `WindowOptions` (Windows uses
/// `appears_transparent` to extend the DWM client area into the title bar).
pub fn titlebar_options(title_bar: TitleBar) -> TitlebarOptions {
    titlebar_options_for(title_bar, cfg!(target_os = "windows"))
}

fn titlebar_options_for(title_bar: TitleBar, windows: bool) -> TitlebarOptions {
    TitlebarOptions { title: Some("goatty".into()), appears_transparent: windows && title_bar == TitleBar::Goatty, ..Default::default() }
}

/// Options for a secondary window (settings, welcome): the decorations
/// `window.title_bar` asks for. On Windows and macOS the system draws its
/// title bar; on Linux, [`secondary_window`] draws one where the desktop
/// leaves that to goatty.
pub(crate) fn secondary_window_options(title: &'static str, cx: &App) -> WindowOptions {
    let title_bar = cx.global::<AppSettings>().client.window.title_bar;
    WindowOptions {
        app_id: Some("dev.goatty.goatty".into()),
        titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
        window_decorations: Some(requested(title_bar)),
        ..Default::default()
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
pub(super) fn buttons_width(size: f32, windows_bar: bool, window: &Window, cx: &App) -> f32 {
    if windows_bar {
        return 3.0 * WIN_BUTTON_WIDTH;
    }
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
pub(super) fn window_buttons(side: Side, size: f32, windows_bar: bool, c: Colors, window: &Window, cx: &App) -> Option<AnyElement> {
    if windows_bar {
        return matches!(side, Side::Right).then(|| windows_caption_buttons(c, window));
    }
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

/// Native-style Windows 10/11 top-right caption buttons wired to GPUI's
/// `WindowControlArea` (`WM_NCHITTEST` -> `HTMINBUTTON` / `HTMAXBUTTON` /
/// `HTCLOSE`), enabling native minimize, maximize/restore toggle, Windows 11
/// Snap Layouts on maximize hover, and close without stopping propagation.
fn windows_caption_buttons(c: Colors, window: &Window) -> AnyElement {
    let maximized = window.is_maximized();
    let min_ok = window.is_minimizable();
    let max_ok = window.is_resizable();
    div()
        .id("windows-window-controls")
        .flex_none()
        .h_full()
        .flex()
        .items_stretch()
        .child(windows_caption_button("win-minimize", Icon::WindowMinimize, 11., WindowControlArea::Min, min_ok, false, c))
        .child(windows_caption_button(
            "win-maximize",
            if maximized { Icon::WindowRestore } else { Icon::WindowMaximize },
            11.,
            WindowControlArea::Max,
            max_ok,
            false,
            c,
        ))
        .child(windows_caption_button("win-close", Icon::Close, 13., WindowControlArea::Close, true, true, c))
        .into_any_element()
}

fn windows_caption_button(
    id: &'static str,
    icon: Icon,
    icon_size: f32,
    area: WindowControlArea,
    enabled: bool,
    is_close: bool,
    c: Colors,
) -> AnyElement {
    let icon_color = if enabled { c.ui.text } else { c.ui.muted };
    let mut el = div()
        .id(id)
        .group(id)
        .flex_none()
        .w(px(WIN_BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .occlude()
        .window_control_area(area);
    if enabled {
        if is_close {
            el = el.hover(|s| s.bg(rgb(0xc42b1c))).active(|s| s.bg(rgb(0xb22719)));
        } else {
            el = el.hover(|s| s.bg(c.ui.text.opacity(0.08))).active(|s| s.bg(c.ui.text.opacity(0.14)));
        }
    }
    let mut glyph = icon.svg().size(px(icon_size)).text_color(icon_color);
    if is_close && enabled {
        glyph = glyph.group_hover(id, |s| s.text_color(gpui::white()));
    }
    el.child(glyph).into_any_element()
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

/// What goatty's title bar and frame keep between frames, in any window
/// that draws them (the terminal window, the settings and welcome windows).
#[derive(Default)]
pub(crate) struct ChromeState {
    /// The left button went down on the title bar's empty space: the next
    /// move starts moving the window.
    title_drag: bool,
    /// The window edge under the pointer, when goatty draws the frame.
    resize_edge: Option<ResizeEdge>,
    /// The frame last told to the platform: goatty's (with the tiling) or none.
    frame_applied: Option<Option<Tiling>>,
}

/// A view whose window may get goatty's title bar and frame.
pub(crate) trait Chromed: 'static + Sized {
    fn chrome(&mut self) -> &mut ChromeState;
}

/// Keep a window that may draw its own decorations current: re-ask the
/// desktop when `window.title_bar` changes (not on Windows, where that needs
/// a new window), and repaint when the desktop changes what it granted or
/// where the window buttons go.
pub(crate) fn observe<V: Chromed>(window: &mut Window, cx: &mut Context<'_, V>) -> Vec<gpui::Subscription> {
    let mut title_bar = cx.global::<AppSettings>().client.window.title_bar;
    vec![
        cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
        cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()),
        cx.observe_global_in::<AppSettings>(window, move |_, window, cx| {
            let wanted = cx.global::<AppSettings>().client.window.title_bar;
            if wanted != title_bar {
                title_bar = wanted;
                if !cfg!(target_os = "windows") {
                    window.request_decorations(requested(wanted));
                }
            }
        }),
    ]
}

/// Make `el` the title bar's empty space: drag to move the window,
/// double-click to maximize, right-click for the window menu.
/// `windows_bar`: goatty's own title bar on Windows (DWM handles it all).
pub(crate) fn title_bar_area<V: Chromed>(
    el: gpui::Stateful<gpui::Div>,
    windows_bar: bool,
    window: &Window,
    cx: &mut Context<'_, V>,
) -> gpui::Stateful<gpui::Div> {
    if windows_bar {
        // On Windows, `WindowControlArea::Drag` returns `HTCAPTION` from
        // `WM_NCHITTEST`, so Windows natively handles dragging, Aero Snap,
        // double-click maximize/restore, and right-click system menu.
        return el.window_control_area(WindowControlArea::Drag);
    }
    let controls = window.window_controls();
    let zoom = controls.maximize && window.is_resizable();
    el.on_mouse_down(MouseButton::Left, cx.listener(|this: &mut V, _, _, _| this.chrome().title_drag = true))
        .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut V, _, _, _| this.chrome().title_drag = false))
        .on_mouse_up_out(MouseButton::Left, cx.listener(|this: &mut V, _, _, _| this.chrome().title_drag = false))
        .on_mouse_move(cx.listener(|this: &mut V, ev: &gpui::MouseMoveEvent, window, _| {
            // The compositor takes over: no mouse-up may follow.
            if std::mem::take(&mut this.chrome().title_drag) && ev.dragging() {
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

/// A secondary window's (settings, welcome) decorations when goatty draws
/// them: a slim header with `title` and the window buttons above `content`,
/// in goatty's frame. `content` as is when the desktop decorates the window.
pub(crate) fn secondary_window<V: Chromed>(
    view: &mut V,
    title: &'static str,
    bg: gpui::Hsla,
    content: AnyElement,
    window: &mut Window,
    cx: &mut Context<'_, V>,
) -> AnyElement {
    if csd(window).is_none() {
        return window_frame(view, content, window, cx);
    }
    let c = Colors::get(cx);
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
    let area = title_bar_area(area, false, window, cx);
    let header = div()
        .id("title-header")
        .flex_none()
        .w_full()
        .h(px(HEADER_HEIGHT))
        .flex()
        .bg(bg)
        .font_family(cx.global::<AppSettings>().ui_font.clone())
        .children(window_buttons(Side::Left, 22., false, c, window, cx))
        .child(area)
        .children(window_buttons(Side::Right, 22., false, c, window, cx));
    let header = round_top(header, window);
    let body = div().size_full().flex().flex_col().child(header).child(div().flex_1().min_h_0().child(content)).into_any_element();
    window_frame(view, body, window, cx)
}

impl Workspace {
    pub(super) fn title_bar_area(
        &self,
        el: gpui::Stateful<gpui::Div>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        title_bar_area(el, self.windows_custom_title_bar, window, cx)
    }

    /// The header goatty draws when it draws the title bar but the tab bar
    /// can't be it (vertical tabs, or the tab bar is hidden): the window
    /// buttons around the active tab's title.
    pub(super) fn render_title_header(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let c = Colors::get(cx);
        let title = self.active_tab().map(|t| self.tab_label(t, cx)).unwrap_or_default();
        let win_bar = self.windows_custom_title_bar;
        let area = div()
            .id("title-header-drag")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .when(win_bar, |d| d.pl(px(12.)).pr(px(8.)))
            .when(!win_bar, |d| d.justify_center().px(px(8.)))
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
            .children(window_buttons(Side::Left, 22., win_bar, c, window, cx))
            .child(area)
            .children(window_buttons(Side::Right, 22., win_bar, c, window, cx));
        round_top(header, window).into_any_element()
    }
}

/// Wrap the window's content in the frame goatty draws with its own
/// decorations: room for the shadow, a hairline border, and edges that
/// resize the window. As is without them.
pub(crate) fn window_frame<V: Chromed>(view: &mut V, content: AnyElement, window: &mut Window, cx: &mut Context<'_, V>) -> AnyElement {
    let tiling = csd(window);
    // Tell the platform only when it changes (X11 sets a property).
    if view.chrome().frame_applied != Some(tiling) {
        view.chrome().frame_applied = Some(tiling);
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
            d.on_mouse_move(cx.listener(move |this: &mut V, ev: &gpui::MouseMoveEvent, window, cx| {
                // Repaint when the pointer enters or leaves an edge (its cursor).
                let edge = resize_edge(ev.position, window.viewport_size(), tiling);
                if edge != this.chrome().resize_edge {
                    this.chrome().resize_edge = edge;
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
        assert!(!titlebar_options_for(TitleBar::System, true).appears_transparent);
        assert!(titlebar_options_for(TitleBar::Goatty, true).appears_transparent);
        assert!(!titlebar_options_for(TitleBar::Goatty, false).appears_transparent);
    }
}
