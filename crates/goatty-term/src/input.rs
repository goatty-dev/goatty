//! Keyboard and mouse → byte encoding (xterm conventions).
//!
//! UI-agnostic: the GPUI layer maps its keystrokes into [`Key`] + [`Mods`]
//! (a [`KeyEvent`]). Keys are encoded the xterm way until the application
//! turns on kitty keyboard protocol flags (`CSI > flags u`, #22; the flags
//! are in [`TermMode::kitty_flags`]), then as the
//! [kitty protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) says.
//!
//! Not reported: modifier keys on their own (GPUI sends no key events for
//! them), Caps Lock and Num Lock, keypad keys as their own keys (they arrive
//! as the keys they type), and the base-layout key of alternate key reports.

use crate::TermMode;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl Mods {
    /// xterm modifier parameter (1 + bitmask), or None when no modifiers.
    fn param(self) -> Option<u8> {
        let m = (self.shift as u8) | (self.alt as u8) << 1 | (self.ctrl as u8) << 2 | (self.logo as u8) << 3;
        (m != 0).then_some(m + 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key<'a> {
    /// Printable text as produced by the keyboard layout (already shifted).
    /// Shift in the [`Mods`] is ignored except by the kitty protocol.
    Text(&'a str),
    Enter,
    Backspace,
    Tab,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

/// Pressed, held down (auto-repeat), or let go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// One key event, as the kitty protocol needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent<'a> {
    pub key: Key<'a>,
    pub mods: Mods,
    pub kind: KeyKind,
    /// For [`Key::Text`]: the key's own character, unshifted (`a` for
    /// Shift+A), which the kitty protocol reports as the key. `None`: the
    /// text lowercased, if it is one character.
    pub base: Option<char>,
}

impl<'a> KeyEvent<'a> {
    pub fn press(key: Key<'a>, mods: Mods) -> Self {
        Self { key, mods, kind: KeyKind::Press, base: None }
    }
}

/// Encode a key press. `None` means "not something the terminal handles".
pub fn encode_key(key: &Key<'_>, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    encode_key_event(&KeyEvent::press(key.clone(), mods), mode)
}

/// Encode a key event. `None` means "nothing to send": not a key the
/// terminal handles, or a repeat/release the application didn't ask for.
pub fn encode_key_event(ev: &KeyEvent<'_>, mode: TermMode) -> Option<Vec<u8>> {
    if mode.kitty_flags() != 0 {
        return kitty::encode(ev, mode.kitty_flags());
    }
    // Legacy: a repeat is another press; releases aren't reported.
    match ev.kind {
        KeyKind::Release => None,
        KeyKind::Press | KeyKind::Repeat => encode_legacy(&ev.key, ev.mods, mode),
    }
}

/// The xterm encoding.
fn encode_legacy(key: &Key<'_>, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let esc = |mut v: Vec<u8>| {
        if mods.alt {
            v.insert(0, 0x1b);
        }
        v
    };
    let csi_letter = |letter: u8| -> Vec<u8> {
        match mods.param() {
            Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
            None if app_cursor => vec![0x1b, b'O', letter],
            None => vec![0x1b, b'[', letter],
        }
    };
    let csi_tilde = |n: u8| -> Vec<u8> {
        match mods.param() {
            Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
            None => format!("\x1b[{n}~").into_bytes(),
        }
    };
    let ss3_f = |letter: u8| -> Vec<u8> {
        match mods.param() {
            Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
            None => vec![0x1b, b'O', letter],
        }
    };

    Some(match key {
        Key::Enter => esc(vec![b'\r']),
        Key::Backspace => {
            if mods.ctrl {
                esc(vec![0x08])
            } else {
                esc(vec![0x7f])
            }
        }
        Key::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                esc(vec![b'\t'])
            }
        }
        Key::Escape => esc(vec![0x1b]),
        Key::Space => {
            if mods.ctrl {
                esc(vec![0])
            } else {
                esc(vec![b' '])
            }
        }
        Key::Up => csi_letter(b'A'),
        Key::Down => csi_letter(b'B'),
        Key::Right => csi_letter(b'C'),
        Key::Left => csi_letter(b'D'),
        Key::Home => csi_letter(b'H'),
        Key::End => csi_letter(b'F'),
        Key::PageUp => csi_tilde(5),
        Key::PageDown => csi_tilde(6),
        Key::Insert => csi_tilde(2),
        Key::Delete => csi_tilde(3),
        Key::F(n) => match n {
            1 => ss3_f(b'P'),
            2 => ss3_f(b'Q'),
            3 => ss3_f(b'R'),
            4 => ss3_f(b'S'),
            5 => csi_tilde(15),
            6 => csi_tilde(17),
            7 => csi_tilde(18),
            8 => csi_tilde(19),
            9 => csi_tilde(20),
            10 => csi_tilde(21),
            11 => csi_tilde(23),
            12 => csi_tilde(24),
            _ => return None,
        },
        Key::Text(s) => {
            if s.is_empty() {
                return None;
            }
            if mods.ctrl {
                let mut chars = s.chars();
                let c = chars.next()?;
                if chars.next().is_none()
                    && let Some(b) = ctrl_byte(c)
                {
                    return Some(esc(vec![b]));
                }
            }
            esc(s.as_bytes().to_vec())
        }
    })
}

/// Map ctrl+<char> to its C0 control byte.
fn ctrl_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '-' | '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

mod kitty {
    //! The kitty keyboard protocol (`CSI key[:shifted] ; mods[:event] ;
    //! text u` and the legacy-compatible `~` and letter forms), following
    //! kitty's own encoder.

    use std::fmt::Write as _;

    use super::{Key, KeyEvent, KeyKind, Mods, encode_legacy};
    use crate::TermMode;

    const DISAMBIGUATE: u8 = 1;
    const EVENT_TYPES: u8 = 2;
    const ALTERNATE_KEYS: u8 = 4;
    const ALL_KEYS: u8 = 8;
    const ASSOCIATED_TEXT: u8 = 16;

    /// The code a functional key is reported with, and the final byte.
    fn functional(key: &Key<'_>) -> Option<(u32, char)> {
        Some(match key {
            Key::Escape => (27, 'u'),
            Key::Enter => (13, 'u'),
            Key::Tab => (9, 'u'),
            Key::Backspace => (127, 'u'),
            Key::Up => (1, 'A'),
            Key::Down => (1, 'B'),
            Key::Right => (1, 'C'),
            Key::Left => (1, 'D'),
            Key::Home => (1, 'H'),
            Key::End => (1, 'F'),
            Key::Insert => (2, '~'),
            Key::Delete => (3, '~'),
            Key::PageUp => (5, '~'),
            Key::PageDown => (6, '~'),
            Key::F(n) => match n {
                1 => (1, 'P'),
                2 => (1, 'Q'),
                3 => (13, '~'),
                4 => (1, 'S'),
                5 => (15, '~'),
                6 => (17, '~'),
                7 => (18, '~'),
                8 => (19, '~'),
                9 => (20, '~'),
                10 => (21, '~'),
                11 => (23, '~'),
                12 => (24, '~'),
                13..=35 => (57376 + u32::from(n - 13), 'u'),
                _ => return None,
            },
            Key::Text(_) | Key::Space => return None,
        })
    }

    pub(super) fn encode(ev: &KeyEvent<'_>, flags: u8) -> Option<Vec<u8>> {
        let all_keys = flags & ALL_KEYS != 0;
        let disambiguate = flags & DISAMBIGUATE != 0 || all_keys;
        let events = flags & EVENT_TYPES != 0;
        if ev.kind == KeyKind::Release && !events {
            return None;
        }
        let mods = ev.mods;
        let release = ev.kind == KeyKind::Release;
        // Shift alone still types text: keys with ctrl, alt or super don't.
        let typing = !(mods.ctrl || mods.alt || mods.logo);

        let text = match &ev.key {
            Key::Text(t) => Some(*t),
            Key::Space => Some(" "),
            _ => None,
        };
        if let Some(text) = text {
            if text.is_empty() {
                return None;
            }
            if !all_keys && !release {
                if typing {
                    return Some(text.as_bytes().to_vec());
                }
                if !disambiguate {
                    // Only event types asked for: presses stay legacy.
                    return encode_legacy(&ev.key, mods, TermMode::empty());
                }
            }
            let base = match ev.base {
                Some(c) => c,
                None => {
                    let mut chars = text.chars();
                    let c = chars.next()?;
                    if chars.next().is_some() {
                        // Composed text with no key: only text can carry it.
                        return (!release).then(|| text.as_bytes().to_vec());
                    }
                    c.to_lowercase().next().unwrap_or(c)
                }
            };
            let shifted = (flags & ALTERNATE_KEYS != 0 && mods.shift).then(|| single(text)).flatten().filter(|&c| c != base);
            let assoc = (flags & ASSOCIATED_TEXT != 0 && all_keys && typing && !release)
                .then_some(text)
                .filter(|t| !t.chars().any(char::is_control));
            return Some(csi(u32::from(base), shifted, 'u', mods, ev.kind, events, assoc));
        }

        let (code, end) = functional(&ev.key)?;
        let legacy_ok = !all_keys && mods == Mods::default();
        match ev.key {
            // These keep their legacy bytes so a shell stays usable after a
            // program that set the flags dies (kitty does the same).
            Key::Enter | Key::Tab | Key::Backspace if legacy_ok => {
                return (!release).then(|| encode_legacy(&ev.key, mods, TermMode::empty())).flatten();
            }
            Key::Escape if legacy_ok && !disambiguate => {
                return (!release).then(|| vec![0x1b]);
            }
            _ => {}
        }
        if !disambiguate && !release && end == 'u' && !matches!(ev.key, Key::F(_)) {
            // Event types only: modified Enter/Tab/Backspace/Escape stay legacy.
            return encode_legacy(&ev.key, mods, TermMode::empty());
        }
        Some(csi(code, None, end, mods, ev.kind, events, None))
    }

    fn single(s: &str) -> Option<char> {
        let mut chars = s.chars();
        let c = chars.next()?;
        chars.next().is_none().then_some(c)
    }

    /// `CSI code[:shifted] [; mods[:event] [; text]] end`. A code of 1 with
    /// nothing after it is left out (`CSI A`), as in the legacy forms.
    fn csi(code: u32, shifted: Option<char>, end: char, mods: Mods, kind: KeyKind, events: bool, text: Option<&str>) -> Vec<u8> {
        let event = match kind {
            KeyKind::Press => None,
            _ if !events => None,
            KeyKind::Repeat => Some(2),
            KeyKind::Release => Some(3),
        };
        let m = mods.param();
        let mut out = String::from("\x1b[");
        let params = m.is_some() || event.is_some() || text.is_some();
        if code != 1 || params || shifted.is_some() {
            let _ = write!(out, "{code}");
        }
        if let Some(s) = shifted {
            let _ = write!(out, ":{}", u32::from(s));
        }
        if params {
            let _ = write!(out, ";{}", m.unwrap_or(1));
            if let Some(e) = event {
                let _ = write!(out, ":{e}");
            }
        }
        if let Some(t) = text {
            out.push(';');
            for (i, c) in t.chars().enumerate() {
                if i > 0 {
                    out.push(':');
                }
                let _ = write!(out, "{}", u32::from(c));
            }
        }
        out.push(end);
        out.into_bytes()
    }
}

/// Wrap pasted text, honoring bracketed paste and neutralizing an embedded
/// end-marker (a classic paste-injection trick).
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let clean = text.replace("\x1b[201~", "");
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(clean.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        // Without bracketed paste, newlines would execute immediately; xterm
        // converts LF to CR.
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion with no button held.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Encode a mouse event for applications that enabled mouse reporting.
/// `col`/`row` are 0-based viewport cells.
pub fn encode_mouse(button: MouseButton, action: MouseAction, col: usize, row: usize, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    if action == MouseAction::Motion {
        let dragging = button != MouseButton::None;
        let ok = mode.contains(TermMode::MOUSE_MOTION) || (dragging && mode.contains(TermMode::MOUSE_DRAG));
        if !ok {
            return None;
        }
    }
    let mut code: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if action == MouseAction::Motion {
        code += 32;
    }
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }
    let (x, y) = (col + 1, row + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{suffix}").into_bytes());
    }
    // Legacy X10 encoding: release is button 3, coordinates capped at 223.
    if action == MouseAction::Release {
        code = 3 + (code & !3);
    }
    if x > 223 || y > 223 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', 32 + code as u8, 32 + x as u8, 32 + y as u8])
}

/// What goatty can do to the command line typed at a managed zsh's prompt
/// (#186). Each is a private key sequence that goatty's shell integration
/// binds to a zle widget (`goatty-shell/zsh/integration.zsh`); send it only
/// while `Terminal::line_editing` says the widgets are there, since another
/// shell or program would take it as typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEdit {
    /// Report the line (`OSC 7337;line=`, arriving as `UiEvent::CommandLine`).
    Report,
    /// Put `sudo ` in front of the line, or take it away; on an empty line,
    /// the previous command with `sudo`.
    ToggleSudo,
    /// Edit the line in `$VISUAL`/`$EDITOR` (zsh's `edit-command-line`).
    OpenEditor,
    /// Empty the line (it goes to the kill ring: Ctrl+Y brings it back).
    Clear,
}

impl LineEdit {
    pub const ALL: [LineEdit; 4] = [LineEdit::Report, LineEdit::ToggleSudo, LineEdit::OpenEditor, LineEdit::Clear];

    /// The key sequence (a CSI no keyboard sends).
    pub fn keys(self) -> &'static [u8] {
        match self {
            LineEdit::Report => b"\x1b[7337;1~",
            LineEdit::ToggleSudo => b"\x1b[7337;2~",
            LineEdit::OpenEditor => b"\x1b[7337;3~",
            LineEdit::Clear => b"\x1b[7337;4~",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_etx() {
        let m = Mods { ctrl: true, ..Default::default() };
        assert_eq!(encode_key(&Key::Text("c"), m, TermMode::empty()).unwrap(), vec![3]);
    }

    #[test]
    fn arrows_respect_decckm() {
        let none = Mods::default();
        assert_eq!(encode_key(&Key::Up, none, TermMode::empty()).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&Key::Up, none, TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        let ctrl = Mods { ctrl: true, ..Default::default() };
        assert_eq!(encode_key(&Key::Right, ctrl, TermMode::APP_CURSOR).unwrap(), b"\x1b[1;5C");
    }

    #[test]
    fn alt_prefixes_escape() {
        let m = Mods { alt: true, ..Default::default() };
        assert_eq!(encode_key(&Key::Text("b"), m, TermMode::empty()).unwrap(), b"\x1bb");
    }

    /// The kitty flags `flags` (`CSI > flags u`) as a mode.
    fn kitty(flags: u8) -> TermMode {
        let (term, _ch) = crate::Terminal::new(crate::TermSize::new(20, 5), false, std::sync::Arc::new(crate::Palette::default()));
        term.feed(format!("\x1b[>{flags}u").as_bytes());
        let mode = term.mode();
        assert_eq!(mode.kitty_flags(), flags);
        mode
    }

    fn ev(key: Key<'_>, mods: Mods, kind: KeyKind) -> KeyEvent<'_> {
        KeyEvent { key, mods, kind, base: None }
    }

    fn enc(key: Key<'_>, mods: Mods, mode: TermMode) -> String {
        String::from_utf8(encode_key(&key, mods, mode).unwrap_or_default()).unwrap()
    }

    const NONE: Mods = Mods { ctrl: false, alt: false, shift: false, logo: false };
    const CTRL: Mods = Mods { ctrl: true, alt: false, shift: false, logo: false };
    const ALT: Mods = Mods { ctrl: false, alt: true, shift: false, logo: false };
    const SHIFT: Mods = Mods { ctrl: false, alt: false, shift: true, logo: false };

    #[test]
    fn legacy_ignores_shift_on_text_and_drops_releases() {
        assert_eq!(enc(Key::Text("A"), SHIFT, TermMode::empty()), "A");
        assert_eq!(encode_key_event(&ev(Key::Text("a"), NONE, KeyKind::Release), TermMode::empty()), None);
        assert_eq!(encode_key_event(&ev(Key::Text("a"), NONE, KeyKind::Repeat), TermMode::empty()).unwrap(), b"a");
    }

    #[test]
    fn kitty_disambiguate() {
        let m = kitty(1);
        // Text and the keys a shell needs stay as they were.
        assert_eq!(enc(Key::Text("a"), NONE, m), "a");
        assert_eq!(enc(Key::Text("A"), SHIFT, m), "A");
        assert_eq!(enc(Key::Enter, NONE, m), "\r");
        assert_eq!(enc(Key::Tab, NONE, m), "\t");
        assert_eq!(enc(Key::Backspace, NONE, m), "\x7f");
        // What legacy can't tell apart gets CSI u.
        assert_eq!(enc(Key::Escape, NONE, m), "\x1b[27u");
        assert_eq!(enc(Key::Text("c"), CTRL, m), "\x1b[99;5u");
        assert_eq!(enc(Key::Text("i"), CTRL, m), "\x1b[105;5u");
        assert_eq!(enc(Key::Text("b"), ALT, m), "\x1b[98;3u");
        assert_eq!(enc(Key::Space, CTRL, m), "\x1b[32;5u");
        assert_eq!(enc(Key::Enter, CTRL, m), "\x1b[13;5u");
        assert_eq!(enc(Key::Tab, SHIFT, m), "\x1b[9;2u");
        let ctrl_shift = Mods { ctrl: true, shift: true, ..NONE };
        let e = KeyEvent { key: Key::Text("A"), mods: ctrl_shift, kind: KeyKind::Press, base: Some('a') };
        assert_eq!(encode_key_event(&e, m).unwrap(), b"\x1b[97;6u");
        // Functional keys: CSI forms, never SS3.
        assert_eq!(enc(Key::Up, NONE, m), "\x1b[A");
        assert_eq!(enc(Key::Up, CTRL, m), "\x1b[1;5A");
        assert_eq!(enc(Key::F(1), NONE, m), "\x1b[P");
        assert_eq!(enc(Key::F(3), NONE, m), "\x1b[13~");
        assert_eq!(enc(Key::F(5), SHIFT, m), "\x1b[15;2~");
        assert_eq!(enc(Key::F(13), NONE, m), "\x1b[57376u");
        assert_eq!(enc(Key::Delete, NONE, m), "\x1b[3~");
        // No releases without event types.
        assert_eq!(encode_key_event(&ev(Key::Up, NONE, KeyKind::Release), m), None);
    }

    #[test]
    fn kitty_event_types() {
        let m = kitty(1 | 2);
        let run = |key, mods, kind| String::from_utf8(encode_key_event(&ev(key, mods, kind), m).unwrap_or_default()).unwrap();
        assert_eq!(run(Key::Text("a"), NONE, KeyKind::Press), "a");
        assert_eq!(run(Key::Text("a"), NONE, KeyKind::Repeat), "a");
        assert_eq!(run(Key::Text("a"), NONE, KeyKind::Release), "\x1b[97;1:3u");
        assert_eq!(run(Key::Up, NONE, KeyKind::Repeat), "\x1b[1;1:2A");
        assert_eq!(run(Key::Up, CTRL, KeyKind::Release), "\x1b[1;5:3A");
        assert_eq!(run(Key::Escape, NONE, KeyKind::Release), "\x1b[27;1:3u");
        // Enter, Tab and Backspace stay legacy, release included.
        assert_eq!(run(Key::Enter, NONE, KeyKind::Release), "");

        // Event types alone: presses keep their legacy bytes.
        let m = kitty(2);
        let run = |key, mods, kind| String::from_utf8(encode_key_event(&ev(key, mods, kind), m).unwrap_or_default()).unwrap();
        assert_eq!(run(Key::Text("c"), CTRL, KeyKind::Press), "\x03");
        assert_eq!(run(Key::Escape, NONE, KeyKind::Press), "\x1b");
        assert_eq!(run(Key::Text("c"), CTRL, KeyKind::Release), "\x1b[99;5:3u");
    }

    #[test]
    fn kitty_all_keys_alternates_and_text() {
        let m = kitty(8);
        assert_eq!(enc(Key::Text("a"), NONE, m), "\x1b[97u");
        assert_eq!(enc(Key::Enter, NONE, m), "\x1b[13u");
        assert_eq!(enc(Key::Backspace, NONE, m), "\x1b[127u");
        assert_eq!(enc(Key::Space, NONE, m), "\x1b[32u");

        let m = kitty(8 | 4 | 16);
        let e = KeyEvent { key: Key::Text("A"), mods: SHIFT, kind: KeyKind::Press, base: Some('a') };
        assert_eq!(encode_key_event(&e, m).unwrap(), b"\x1b[97:65;2;65u");
        assert_eq!(enc(Key::Text("é"), NONE, m), "\x1b[233;1;233u");
        // Ctrl types no text.
        assert_eq!(enc(Key::Text("a"), CTRL, m), "\x1b[97;5u");
    }

    #[test]
    fn kitty_flags_follow_the_stack_and_the_screen() {
        let (term, _ch) = crate::Terminal::new(crate::TermSize::new(20, 5), false, std::sync::Arc::new(crate::Palette::default()));
        term.feed(b"\x1b[>1u");
        assert_eq!(term.mode().kitty_flags(), 1);
        term.feed(b"\x1b[=3;2u");
        assert_eq!(term.mode().kitty_flags(), 3);
        // The alternate screen has its own stack.
        term.feed(b"\x1b[?1049h");
        assert_eq!(term.mode().kitty_flags(), 0);
        term.feed(b"\x1b[>8u");
        assert_eq!(term.mode().kitty_flags(), 8);
        term.feed(b"\x1b[?1049l");
        assert_eq!(term.mode().kitty_flags(), 3);
        term.feed(b"\x1b[<u");
        assert_eq!(term.mode().kitty_flags(), 0);
        assert!(!term.mode().intersects(TermMode::KITTY_KEYBOARD));
        // RIS clears them.
        term.feed(b"\x1b[>31u\x1bc");
        assert_eq!(term.mode().kitty_flags(), 0);
    }

    #[test]
    fn paste_strips_injected_end_marker() {
        let out = encode_paste("rm -rf x\x1b[201~; echo pwned\n", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~rm -rf x; echo pwned\n\x1b[201~");
    }

    #[test]
    fn the_shell_binds_every_line_edit() {
        let zsh = include_str!("../../goatty-shell/zsh/integration.zsh");
        for e in LineEdit::ALL {
            let seq = std::str::from_utf8(e.keys()).unwrap().replace('\x1b', "^[");
            assert!(zsh.contains(&format!("'{seq}'")), "{e:?}: {seq} isn't bound in integration.zsh");
        }
    }
}
