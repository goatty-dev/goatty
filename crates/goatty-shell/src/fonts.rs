//! Whether an installed font draws the prompt's Nerd Font glyphs, for
//! `prompt.icons_elsewhere = auto` (#302).
//!
//! Terminals that render through fontconfig (VTE, Konsole, Alacritty, foot,
//! VS Code on Linux, …) fall back to any installed font for a glyph the
//! configured one lacks, so with a Nerd Font installed they draw the icons
//! whatever font they're set to. macOS and Windows don't fall back for the
//! private use area: there only the terminal itself counts (the startup
//! files check that).

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The Nerd Font (private use area) code points in `text`.
pub fn glyphs(text: &str) -> BTreeSet<char> {
    text.chars().filter(|&c| matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..)).collect()
}

/// The fontconfig pattern that matches fonts with every one of `glyphs`.
fn pattern(glyphs: &BTreeSet<char>) -> String {
    let hex: Vec<String> = glyphs.iter().map(|&c| format!("{:x}", u32::from(c))).collect();
    format!(":charset={}", hex.join(" "))
}

/// Whether one installed font has every Nerd Font glyph in `text` (no
/// glyphs: true). Runs `fc-list` (~0.1 s), remembered for a minute, since
/// every generation build asks.
pub fn installed_for(text: &str) -> bool {
    let glyphs = glyphs(text);
    if glyphs.is_empty() {
        return true;
    }
    if cfg!(any(windows, target_os = "macos")) {
        return false;
    }
    static CACHE: Mutex<Option<(Instant, String, bool)>> = Mutex::new(None);
    let pattern = pattern(&glyphs);
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, p, found)) = cache.as_ref()
        && *p == pattern
        && at.elapsed() < Duration::from_secs(60)
    {
        return *found;
    }
    let found = std::process::Command::new("fc-list")
        .args([pattern.as_str(), "family"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.iter().any(|b| !b.is_ascii_whitespace()));
    *cache = Some((Instant::now(), pattern, found));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pattern_lists_the_private_use_glyphs() {
        let g = glyphs("a \u{e0b0} \u{f015} ❯ \u{f0001} \u{e0b0}");
        assert_eq!(g.len(), 3);
        assert_eq!(pattern(&g), ":charset=e0b0 f015 f0001");
        assert!(installed_for("only ❯ text"), "nothing to draw");
    }
}
