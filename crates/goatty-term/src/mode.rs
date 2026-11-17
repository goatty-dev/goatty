//! goatty's own terminal mode bitflags.
//!
//! Only the flags the UI, input encoders, and snapshots inspect live here;
//! `rio_vt::crosswords::Mode` stays inside `goatty-term`.

use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

use rio_vt::crosswords::Mode;

/// Terminal mode bits (DECCKM, mouse reporting, alt screen, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TermMode(u32);

impl TermMode {
    pub const SHOW_CURSOR: Self = Self(1 << 0);
    pub const APP_CURSOR: Self = Self(1 << 1);
    pub const APP_KEYPAD: Self = Self(1 << 2);
    pub const MOUSE_REPORT_CLICK: Self = Self(1 << 3);
    pub const MOUSE_REPORT_X10: Self = Self(1 << 4);
    pub const MOUSE_DRAG: Self = Self(1 << 5);
    pub const MOUSE_MOTION: Self = Self(1 << 6);
    pub const SGR_MOUSE: Self = Self(1 << 7);
    pub const UTF8_MOUSE: Self = Self(1 << 8);
    pub const BRACKETED_PASTE: Self = Self(1 << 9);
    pub const FOCUS_IN_OUT: Self = Self(1 << 10);
    pub const LINE_WRAP: Self = Self(1 << 11);
    pub const ALT_SCREEN: Self = Self(1 << 12);
    pub const ALTERNATE_SCROLL: Self = Self(1 << 13);
    /// Kitty keyboard protocol flags (`CSI > flags u`, #22): the active
    /// screen's top of the flag stack, one bit per progressive enhancement.
    /// Read them with [`TermMode::kitty_flags`].
    pub const KITTY_DISAMBIGUATE: Self = Self(1 << 14);
    pub const KITTY_EVENT_TYPES: Self = Self(1 << 15);
    pub const KITTY_ALTERNATE_KEYS: Self = Self(1 << 16);
    pub const KITTY_ALL_KEYS: Self = Self(1 << 17);
    pub const KITTY_ASSOCIATED_TEXT: Self = Self(1 << 18);

    /// Any mouse reporting protocol enabled.
    pub const MOUSE_MODE: Self = Self(Self::MOUSE_REPORT_CLICK.0 | Self::MOUSE_REPORT_X10.0 | Self::MOUSE_DRAG.0 | Self::MOUSE_MOTION.0);

    /// Any kitty keyboard enhancement enabled.
    pub const KITTY_KEYBOARD: Self = Self(
        Self::KITTY_DISAMBIGUATE.0
            | Self::KITTY_EVENT_TYPES.0
            | Self::KITTY_ALTERNATE_KEYS.0
            | Self::KITTY_ALL_KEYS.0
            | Self::KITTY_ASSOCIATED_TEXT.0,
    );
    const KITTY_SHIFT: u32 = 14;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    /// The kitty keyboard flags as the protocol numbers them (the byte
    /// `CSI ? u` reports: 1 disambiguate, 2 event types, 4 alternate keys,
    /// 8 all keys as escapes, 16 associated text).
    pub const fn kitty_flags(self) -> u8 {
        ((self.0 & Self::KITTY_KEYBOARD.0) >> Self::KITTY_SHIFT) as u8
    }

    /// The modes of `t`. rio-vt's kitty bits in `Mode` go stale (switching
    /// screens and RIS don't update them), so the flags come from the
    /// keyboard mode stack.
    pub(crate) fn of(t: &crate::terminal::Term) -> Self {
        let kitty = (t.keyboard_mode().bits() as u32) << Self::KITTY_SHIFT;
        Self::from_rio(t.mode()) | Self(kitty & Self::KITTY_KEYBOARD.0)
    }

    pub(crate) fn from_rio(mode: Mode) -> Self {
        const MAP: &[(Mode, TermMode)] = &[
            (Mode::SHOW_CURSOR, TermMode::SHOW_CURSOR),
            (Mode::APP_CURSOR, TermMode::APP_CURSOR),
            (Mode::APP_KEYPAD, TermMode::APP_KEYPAD),
            (Mode::MOUSE_REPORT_CLICK, TermMode::MOUSE_REPORT_CLICK),
            (Mode::MOUSE_REPORT_X10, TermMode::MOUSE_REPORT_X10),
            (Mode::MOUSE_DRAG, TermMode::MOUSE_DRAG),
            (Mode::MOUSE_MOTION, TermMode::MOUSE_MOTION),
            (Mode::SGR_MOUSE, TermMode::SGR_MOUSE),
            (Mode::UTF8_MOUSE, TermMode::UTF8_MOUSE),
            (Mode::BRACKETED_PASTE, TermMode::BRACKETED_PASTE),
            (Mode::FOCUS_IN_OUT, TermMode::FOCUS_IN_OUT),
            (Mode::LINE_WRAP, TermMode::LINE_WRAP),
            (Mode::ALT_SCREEN, TermMode::ALT_SCREEN),
            (Mode::ALTERNATE_SCROLL, TermMode::ALTERNATE_SCROLL),
        ];
        let mut out = Self::empty();
        for &(src, dst) in MAP {
            if mode.contains(src) {
                out |= dst;
            }
        }
        out
    }
}

impl BitOr for TermMode {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for TermMode {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for TermMode {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for TermMode {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Frame, Palette, TermSize, Terminal};

    #[test]
    fn maps_every_supported_rio_mode_and_ignores_internal_ones() {
        assert_eq!(TermMode::from_rio(Mode::NONE), TermMode::empty());
        assert!(TermMode::from_rio(Mode::NONE).is_empty());

        let pairs = [
            (Mode::SHOW_CURSOR, TermMode::SHOW_CURSOR),
            (Mode::APP_CURSOR, TermMode::APP_CURSOR),
            (Mode::APP_KEYPAD, TermMode::APP_KEYPAD),
            (Mode::MOUSE_REPORT_CLICK, TermMode::MOUSE_REPORT_CLICK),
            (Mode::MOUSE_REPORT_X10, TermMode::MOUSE_REPORT_X10),
            (Mode::MOUSE_DRAG, TermMode::MOUSE_DRAG),
            (Mode::MOUSE_MOTION, TermMode::MOUSE_MOTION),
            (Mode::SGR_MOUSE, TermMode::SGR_MOUSE),
            (Mode::UTF8_MOUSE, TermMode::UTF8_MOUSE),
            (Mode::BRACKETED_PASTE, TermMode::BRACKETED_PASTE),
            (Mode::FOCUS_IN_OUT, TermMode::FOCUS_IN_OUT),
            (Mode::LINE_WRAP, TermMode::LINE_WRAP),
            (Mode::ALT_SCREEN, TermMode::ALT_SCREEN),
            (Mode::ALTERNATE_SCROLL, TermMode::ALTERNATE_SCROLL),
        ];
        for (rio, own) in pairs {
            assert_eq!(TermMode::from_rio(rio), own);
        }

        assert_eq!(TermMode::from_rio(Mode::MOUSE_MODE), TermMode::MOUSE_MODE);

        // Internal rio-vt flags don't leak into TermMode.
        let internal = Mode::VI | Mode::URGENCY_HINTS | Mode::SYNC_UPDATE | Mode::GRAPHEME_CLUSTER | Mode::ORIGIN | Mode::INSERT;
        assert_eq!(TermMode::from_rio(internal), TermMode::empty());
    }

    #[test]
    fn bit_operations() {
        let mut m = TermMode::ALT_SCREEN | TermMode::APP_CURSOR;
        assert!(m.contains(TermMode::ALT_SCREEN));
        assert!(m.contains(TermMode::APP_CURSOR));
        assert!(!m.contains(TermMode::BRACKETED_PASTE));
        assert!(m.intersects(TermMode::ALT_SCREEN | TermMode::SGR_MOUSE));
        assert!(!m.intersects(TermMode::MOUSE_MODE));

        m |= TermMode::MOUSE_DRAG;
        assert!(m.intersects(TermMode::MOUSE_MODE));

        let masked = m & (TermMode::ALT_SCREEN | TermMode::MOUSE_MODE);
        assert_eq!(masked, TermMode::ALT_SCREEN | TermMode::MOUSE_DRAG);

        m &= TermMode::APP_CURSOR;
        assert_eq!(m, TermMode::APP_CURSOR);
        assert_ne!(m.bits(), 0);
    }

    #[test]
    fn terminal_and_frame_convert_modes_from_vt() {
        let (term, _ch) = Terminal::new(TermSize::new(20, 5), false, Arc::new(Palette::default()));
        // Defaults from VT core: cursor visible, autowrap on, alternate scroll on.
        let initial = term.mode();
        assert!(initial.contains(TermMode::SHOW_CURSOR));
        assert!(initial.contains(TermMode::LINE_WRAP));
        assert!(initial.contains(TermMode::ALTERNATE_SCROLL));
        assert!(!initial.contains(TermMode::ALT_SCREEN));

        term.feed(b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?1007l\x1b=");
        let mode = term.mode();
        assert!(mode.contains(TermMode::ALT_SCREEN));
        assert!(mode.contains(TermMode::APP_CURSOR));
        assert!(mode.contains(TermMode::BRACKETED_PASTE));
        assert!(mode.contains(TermMode::MOUSE_DRAG));
        assert!(mode.intersects(TermMode::MOUSE_MODE));
        assert!(mode.contains(TermMode::SGR_MOUSE));
        assert!(mode.contains(TermMode::FOCUS_IN_OUT));
        assert!(mode.contains(TermMode::APP_KEYPAD));
        assert!(!mode.contains(TermMode::ALTERNATE_SCROLL));

        let mut frame = Frame::default();
        assert_eq!(frame.mode, TermMode::empty());
        term.snapshot_into(&mut frame);
        assert_eq!(frame.mode, mode);
    }
}
