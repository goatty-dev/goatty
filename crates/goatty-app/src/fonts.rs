//! The bundled fonts (`assets/fonts`), registered at start: JetBrainsMono
//! Nerd Font, so Nerd Font symbols (prompt, tab icons) work without
//! installing a font, and Inter, the UI font, as static faces so its weights
//! show (GPUI on Linux draws a variable font at weight 400 whatever weight is
//! asked for, zed#64783). Also finds the installed monospace families for
//! the font picker.

use std::borrow::Cow;
use std::collections::HashSet;

use gpui::{App, Font, FontFallbacks};

pub use goatty_config::FontSettings;

/// The faces the terminal uses (regular, bold, italic, bold italic).
const FACES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/JetBrainsMonoNerdFont-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoNerdFont-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoNerdFont-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoNerdFont-BoldItalic.ttf"),
];

/// The UI font's faces (regular, medium, semibold, bold: the weights the UI uses).
const UI_FACES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
];

/// Register the bundled faces. Returns the font families installed on the
/// system, read before registering (font resolution prefers an installed
/// Nerd Font over the bundled one).
pub fn register(cx: &App) -> Vec<String> {
    let system = cx.text_system().all_font_names();
    if let Err(e) = cx.text_system().add_fonts(FACES.iter().map(|f| Cow::Borrowed(*f)).collect()) {
        log::error!("couldn't load the bundled {}: {e:#}", FontSettings::BUNDLED);
    }
    if let Err(e) = cx.text_system().add_fonts(UI_FACES.iter().map(|f| Cow::Borrowed(*f)).collect()) {
        log::error!("couldn't load the bundled {}: {e:#}", FontSettings::BUNDLED_UI);
    }
    system
}

/// The font families goatty can use, found without a window (for
/// `goatty settings validate`): installed ones (every family name each face
/// declares, matching both fontconfig on Linux and DirectWrite on Windows),
/// plus the bundled ones. Reads every font file's header, so it takes a moment.
/// Empty when no font is found, so font checks are skipped rather than
/// reporting every family missing.
pub fn installed_families() -> Vec<String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut names: Vec<String> = db.faces().flat_map(|f| f.families.iter().map(|(name, _)| name.clone())).collect();
    if names.is_empty() {
        return names;
    }
    names.push(FontSettings::BUNDLED.to_string());
    names.push(FontSettings::BUNDLED_UI.to_string());
    names.sort_unstable();
    names.dedup();
    names
}

/// The terminal font for `family`: glyphs it lacks (Nerd Font symbols in a
/// plain font) come from the bundled font.
pub fn terminal(family: &str) -> Font {
    let mut font = gpui::font(family.to_string());
    if family != FontSettings::BUNDLED {
        font.fallbacks = Some(FontFallbacks::from_fonts(vec![FontSettings::BUNDLED.to_string()]));
    }
    font
}

/// The font for Nerd Font icons in goatty's own UI: always the bundled one,
/// whose code points are checked by the tests in `workspace::icons`.
pub fn icons() -> Font {
    gpui::font(FontSettings::BUNDLED)
}

/// Glyphs whose advances differ in any proportional font: narrow, wide,
/// digits and punctuation.
const PROBE: &[char] = &['i', 'l', 'm', 'M', 'W', '0', '.', ' '];

/// Whether `face` draws Latin text on a fixed grid: every probe glyph exists
/// and has the same advance. Like fontconfig's `spacing=mono`/`dual`, this
/// counts CJK monospace fonts (double-width ideographs) as monospace, and
/// fonts without Latin letters (symbols, emoji) as not. Unlike fontconfig,
/// it skips icon fonts that draw letters on a grid but map both cases to one
/// glyph (Material Symbols): text fonts have distinct `m` and `M`.
pub fn is_monospace(face: &ttf_parser::Face<'_>) -> bool {
    face.glyph_index('m') != face.glyph_index('M')
        && uniform(PROBE.iter().map(|&c| face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g))))
}

/// All advances are present, non-zero and equal.
fn uniform(mut advances: impl Iterator<Item = Option<u16>>) -> bool {
    let Some(Some(first)) = advances.next() else { return false };
    first > 0 && advances.all(|a| a == Some(first))
}

/// The installed font families that are monospace (any face measures as
/// [`is_monospace`]), plus the bundled one. Scans every font file, so call it
/// off the UI thread (0.1 s warm, seconds with a cold disk cache).
pub fn monospace_families() -> HashSet<String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut mono = HashSet::from([FontSettings::BUNDLED.to_string()]);
    for info in db.faces() {
        if info.families.iter().all(|(name, _)| mono.contains(name)) {
            continue;
        }
        let fixed = db
            .with_face_data(info.id, |data, index| ttf_parser::Face::parse(data, index).is_ok_and(|face| is_monospace(&face)))
            .unwrap_or(false);
        if fixed {
            mono.extend(info.families.iter().map(|(name, _)| name.clone()));
        }
    }
    mono
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The bundled faces parse and are the family the settings resolve to.
    #[test]
    fn bundled_faces() {
        for data in FACES {
            let face = ttf_parser::Face::parse(data, 0).expect("bundled font parses");
            let family = face
                .names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
                .and_then(|n| n.to_string())
                .expect("family name");
            // Bold/Italic faces name their family "JetBrainsMono Nerd Font"
            // plus the style in the legacy name; the typographic family is
            // the plain one.
            let typographic = face
                .names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY && n.is_unicode())
                .and_then(|n| n.to_string());
            assert_eq!(typographic.as_deref().unwrap_or(&family), FontSettings::BUNDLED);
        }
    }

    /// The UI font's faces are static faces of one family at the weights
    /// the UI asks for, so GPUI picks each by weight.
    #[test]
    fn bundled_ui_faces() {
        let mut weights = Vec::new();
        for data in UI_FACES {
            let face = ttf_parser::Face::parse(data, 0).expect("bundled UI font parses");
            assert!(!face.is_variable(), "a variable face renders at 400 only");
            let family = face
                .names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY && n.is_unicode())
                .or_else(|| face.names().into_iter().find(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode()))
                .and_then(|n| n.to_string())
                .expect("family name");
            assert_eq!(family, FontSettings::BUNDLED_UI);
            weights.push(face.weight().to_number());
        }
        assert_eq!(weights, [400, 500, 600, 700]);
    }

    /// Every bundled face measures as monospace.
    #[test]
    fn bundled_is_monospace() {
        for data in FACES {
            assert!(is_monospace(&ttf_parser::Face::parse(data, 0).expect("bundled font parses")));
        }
    }

    #[test]
    fn uniform_advances() {
        assert!(uniform([Some(600), Some(600), Some(600)].into_iter()));
        assert!(!uniform([Some(600), Some(500)].into_iter()), "proportional");
        assert!(!uniform([Some(600), None].into_iter()), "a missing glyph");
        assert!(!uniform([Some(0), Some(0)].into_iter()), "zero width");
        assert!(!uniform(std::iter::empty()));
    }

    /// Whether the bundled regular face has a glyph for `c`.
    pub fn has_glyph(c: char) -> bool {
        ttf_parser::Face::parse(FACES[0], 0).expect("bundled font parses").glyph_index(c).is_some()
    }
}
