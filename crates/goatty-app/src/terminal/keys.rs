//! GPUI keystrokes → terminal keys. Pure mapping, no I/O; the byte encoding
//! itself lives in `goatty_term::input`.

use goatty_term::input::{Key, KeyEvent, KeyKind, Mods};
use gpui::{Keystroke, Modifiers};

pub fn mods(m: &Modifiers) -> Mods {
    Mods { ctrl: m.control, alt: m.alt, shift: m.shift, logo: m.platform }
}

/// The terminal key event for `ks`, or `None` for modifier-only and
/// unknown keys and for Super combinations (those belong to the compositor).
pub fn terminal_key(ks: &Keystroke, kind: KeyKind) -> Option<KeyEvent<'_>> {
    let m = ks.modifiers;
    if m.platform {
        return None;
    }
    let key = match ks.key.as_str() {
        "enter" => Key::Enter,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "escape" => Key::Escape,
        "space" => Key::Space,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        k if k.len() >= 2 && k.starts_with('f') && k[1..].parse::<u8>().is_ok() => Key::F(k[1..].parse().ok()?),
        k => match &ks.key_char {
            // Typed text from the layout (already shifted). With ctrl the
            // layout often gives nothing; fall back to the key name.
            Some(t) if !m.control => Key::Text(t.as_str()),
            _ if k.chars().count() == 1 => Key::Text(k),
            _ => return None,
        },
    };
    // GPUI names a letter key by its lowercase letter (keeping Shift) and
    // other keys by what they type (dropping Shift): the kitty protocol
    // reports that as the key.
    let base = match key {
        Key::Text(_) => single_char(&ks.key),
        _ => None,
    };
    Some(KeyEvent { key, mods: mods(&m), kind, base })
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(Debug of the key, mods)` for a keystroke spec plus layout text.
    fn map(src: &'static str, key_char: Option<&str>) -> Option<(String, Mods)> {
        let mut ks = Keystroke::parse(src).unwrap();
        ks.key_char = key_char.map(String::from);
        terminal_key(&ks, KeyKind::Press).map(|e| (format!("{:?}", e.key), e.mods))
    }

    #[test]
    fn named_and_function_keys() {
        assert_eq!(map("enter", None).unwrap().0, "Enter");
        assert_eq!(map("f5", None).unwrap().0, "F(5)");
        let (key, m) = map("shift-up", None).unwrap();
        assert_eq!((key.as_str(), m.shift), ("Up", true));
    }

    #[test]
    fn text_uses_layout_char_and_keeps_the_key() {
        let mut ks = Keystroke::parse("shift-a").unwrap();
        ks.key_char = Some("A".into());
        let ev = terminal_key(&ks, KeyKind::Repeat).unwrap();
        assert_eq!(ev.key, Key::Text("A"));
        // Shift stays for the kitty protocol (legacy encoding ignores it
        // for text), and the key is `a`.
        assert!(ev.mods.shift);
        assert_eq!(ev.base, Some('a'));
        assert_eq!(ev.kind, KeyKind::Repeat);
        // Ctrl combos often have no key_char: fall back to the key name.
        let (key, m) = map("ctrl-r", None).unwrap();
        assert_eq!((key.as_str(), m.ctrl), (r#"Text("r")"#, true));
    }

    #[test]
    fn super_and_modifier_only_are_ignored() {
        assert!(map("super-t", None).is_none());
        assert!(map("shift", None).is_none());
    }
}
