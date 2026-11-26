//! How far a mouse-wheel notch scrolls (#310, `mouse.scroll_lines`).
//!
//! GPUI reports a wheel as [`ScrollDelta::Lines`], already scaled by the
//! platform: 3 lines per notch on Linux (`SCROLL_LINES` in `gpui_linux`, X11
//! and Wayland alike), the system's "lines per notch" setting on Windows (3
//! by default), and the raw, accelerated delta on macOS (about 1 per notch).
//! [`wheel_lines`] turns that back into notches and applies the setting, so
//! one notch scrolls the same everywhere. Touchpads report
//! [`ScrollDelta::Pixels`] and scroll by distance instead.

use gpui::Point;

/// Lines GPUI reports for one wheel notch on this platform.
#[cfg(target_os = "macos")]
const PLATFORM_LINES_PER_NOTCH: f32 = 1.0;
#[cfg(not(target_os = "macos"))]
const PLATFORM_LINES_PER_NOTCH: f32 = 3.0;

/// The lines (and, sideways, columns) to scroll for GPUI's line delta `p`,
/// at `per_notch` lines per wheel notch.
pub(crate) fn wheel_lines(p: Point<f32>, per_notch: u32) -> Point<f32> {
    p.map(|v| v / PLATFORM_LINES_PER_NOTCH * per_notch as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    #[test]
    fn one_notch_scrolls_the_configured_lines() {
        let notch = point(0.0, PLATFORM_LINES_PER_NOTCH);
        assert_eq!(wheel_lines(notch, 3), point(0.0, 3.0));
        assert_eq!(wheel_lines(notch, 5), point(0.0, 5.0));
        let down_left = point(PLATFORM_LINES_PER_NOTCH, -2.0 * PLATFORM_LINES_PER_NOTCH);
        assert_eq!(wheel_lines(down_left, 1), point(1.0, -2.0));
    }
}
