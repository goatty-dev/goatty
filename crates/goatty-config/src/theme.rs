//! Color themes: built-in palettes plus per-color overrides.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An opaque sRGB color, always written as `#rrggbb`. Reading also takes
/// `#rgb`, `rgb()`, `hsl()`, `hsv()`, `oklch()` and `oklab()`
/// ([`Color::parse`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgb(self) -> (u8, u8, u8) {
        ((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
    }

    /// Perceived lightness, 0.0 (black) to 1.0 (white).
    pub fn luma(self) -> f32 {
        let (r, g, b) = self.rgb();
        (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
    }

    /// Read a color as settings files and `goatty settings set` write it:
    /// hex (`#rrggbb`, `rrggbb`, `#rgb`, an alpha is ignored) or a CSS
    /// function (`rgb()`, `hsl()`, `hsv()`, `oklch()`, `oklab()`; see
    /// [`crate::colorspace::parse_css_color`], which also says what was lost).
    pub fn parse(s: &str) -> Option<Self> {
        crate::colorspace::parse_css_color(s).map(|p| p.color)
    }
}

impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:06x}", self.0)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:06x}", self.0)
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("{s:?} is not a color (#rrggbb, rgb(), hsl(), hsv(), oklch() or oklab())")))
    }
}

/// Every color a terminal theme defines (also the `colors` object of a
/// user theme file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeColors {
    /// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan,
    /// white, then their bright variants.
    pub ansi: [Color; 16],
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection_foreground: Color,
    pub selection_background: Color,
    /// Settings UI accent; `None` uses `ansi[4]` (blue).
    #[serde(default, alias = "primary", skip_serializing_if = "Option::is_none")]
    pub accent: Option<Color>,
}

/// One editable color of a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSlot {
    Foreground,
    Background,
    Cursor,
    SelectionForeground,
    SelectionBackground,
    Accent,
    /// ANSI color 0–15.
    Ansi(u8),
}

impl ColorSlot {
    /// All 22 slots, in editor order.
    pub fn all() -> Vec<ColorSlot> {
        let mut v =
            vec![Self::Background, Self::Foreground, Self::Cursor, Self::SelectionBackground, Self::SelectionForeground, Self::Accent];
        v.extend((0..16).map(Self::Ansi));
        v
    }

    pub fn label(self) -> String {
        const ANSI: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
        match self {
            Self::Foreground => "Text".into(),
            Self::Background => "Background".into(),
            Self::Cursor => "Cursor".into(),
            Self::SelectionForeground => "Selected text".into(),
            Self::SelectionBackground => "Selection".into(),
            Self::Accent => "Accent".into(),
            Self::Ansi(i) if i < 8 => ANSI[i as usize].into(),
            Self::Ansi(i) => format!("bright {}", ANSI[(i as usize - 8) % 8]),
        }
    }

    /// The colors this slot should stay readable against, for the color
    /// editor's contrast check (#70). Text on the background wants WCAG's
    /// 4.5:1. The chromatic ANSI colors and the cursor warn below 3:1 (large
    /// text and UI parts): many well-liked themes keep their ANSI colors
    /// between 3 and 4.5, and a warning on every theme would stop meaning
    /// anything. Black, white and bright black/white are often backdrops or
    /// dimmed on purpose: their ratio is shown without a warning.
    pub fn contrast_checks(self) -> Vec<ContrastCheck> {
        let check = |against, edited_is_text, min| ContrastCheck { against, edited_is_text, min };
        match self {
            Self::Foreground => vec![check(Self::Background, true, Some(4.5))],
            Self::Background => vec![check(Self::Foreground, false, Some(4.5))],
            Self::SelectionForeground => vec![check(Self::SelectionBackground, true, Some(4.5))],
            Self::SelectionBackground => vec![check(Self::SelectionForeground, false, Some(4.5)), check(Self::Background, false, None)],
            Self::Cursor => vec![check(Self::Background, true, Some(3.0))],
            Self::Accent => vec![check(Self::Background, true, None)],
            Self::Ansi(0 | 7 | 8 | 15) => vec![check(Self::Background, true, None), check(Self::Foreground, true, None)],
            Self::Ansi(_) => vec![check(Self::Background, true, Some(3.0))],
        }
    }
}

/// One readability check of a theme color (see [`ColorSlot::contrast_checks`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastCheck {
    /// The other color.
    pub against: ColorSlot,
    /// The edited color is drawn on `against` (text, cursor), rather than
    /// being the backdrop for it.
    pub edited_is_text: bool,
    /// Warn below this WCAG 2 contrast ratio; `None` only shows the ratio.
    pub min: Option<f32>,
}

impl ThemeColors {
    /// The UI accent color: `accent` when set, else `ansi[4]` (blue).
    pub fn accent(&self) -> Color {
        self.accent.unwrap_or(self.ansi[4])
    }

    pub fn get(&self, slot: ColorSlot) -> Color {
        match slot {
            ColorSlot::Foreground => self.foreground,
            ColorSlot::Background => self.background,
            ColorSlot::Cursor => self.cursor,
            ColorSlot::SelectionForeground => self.selection_foreground,
            ColorSlot::SelectionBackground => self.selection_background,
            ColorSlot::Accent => self.accent(),
            ColorSlot::Ansi(i) => self.ansi[(i as usize).min(15)],
        }
    }

    pub fn set(&mut self, slot: ColorSlot, c: Color) {
        match slot {
            ColorSlot::Foreground => self.foreground = c,
            ColorSlot::Background => self.background = c,
            ColorSlot::Cursor => self.cursor = c,
            ColorSlot::SelectionForeground => self.selection_foreground = c,
            ColorSlot::SelectionBackground => self.selection_background = c,
            ColorSlot::Accent => self.accent = Some(c),
            ColorSlot::Ansi(i) => self.ansi[(i as usize).min(15)] = c,
        }
    }

    pub fn is_dark(&self) -> bool {
        self.background.luma() < 0.5
    }

    /// Flattened for the wire (`SetPalette`): ansi[16], fg, bg, cursor, sel fg, sel bg.
    pub fn to_vec(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.ansi.iter().map(|c| c.0).collect();
        v.extend([self.foreground, self.background, self.cursor, self.selection_foreground, self.selection_background].map(|c| c.0));
        v
    }

    pub fn from_slice(v: &[u32]) -> Option<Self> {
        if v.len() != 21 {
            return None;
        }
        let mut ansi = [Color(0); 16];
        for (a, &c) in ansi.iter_mut().zip(v) {
            *a = Color(c & 0xff_ffff);
        }
        let c = |i: usize| Color(v[i] & 0xff_ffff);
        Some(Self {
            ansi,
            foreground: c(16),
            background: c(17),
            cursor: c(18),
            selection_foreground: c(19),
            selection_background: c(20),
            accent: None,
        })
    }
}

/// A built-in theme.
#[derive(Debug)]
pub struct Theme {
    /// Stable id stored in settings.
    pub id: &'static str,
    pub name: &'static str,
    pub colors: ThemeColors,
}

const fn c(v: u32) -> Color {
    Color(v)
}

#[allow(clippy::too_many_arguments)]
const fn theme(id: &'static str, name: &'static str, ansi: [u32; 16], fg: u32, bg: u32, cursor: u32, sel_fg: u32, sel_bg: u32) -> Theme {
    let mut a = [Color(0); 16];
    let mut i = 0;
    while i < 16 {
        a[i] = Color(ansi[i]);
        i += 1;
    }
    Theme {
        id,
        name,
        colors: ThemeColors {
            ansi: a,
            foreground: c(fg),
            background: c(bg),
            cursor: c(cursor),
            selection_foreground: c(sel_fg),
            selection_background: c(sel_bg),
            accent: None,
        },
    }
}

impl Theme {
    const fn with_accent(mut self, accent: u32) -> Self {
        self.colors.accent = Some(c(accent));
        self
    }
}

pub const DEFAULT_THEME: &str = "goatty-night";

/// Built-in themes. Palettes follow each scheme's published terminal colors.
pub static THEMES: &[Theme] = &[
    theme(
        "goatty-night",
        "goatty night",
        [
            0x1c1c22, 0xd6655c, 0x82b384, 0xe0a958, 0x6e9fd4, 0xb688c9, 0x68b0ad, 0xc8c8c6, //
            0x6b6b76, 0xe8837a, 0x9bc99d, 0xf2be72, 0x8cb8e6, 0xcfa4de, 0x85cac7, 0xf2f2f0,
        ],
        0xecece8,
        0x0c0c0f,
        0xff5a00,
        0xf2f2f0,
        0x3d2418,
    )
    .with_accent(0xff5a00),
    theme(
        "goatty-day",
        "goatty day",
        [
            0x18181d, 0xbd443c, 0x3e7d47, 0xa86a10, 0x356ea8, 0x884f9c, 0x2a7d7a, 0xb8b8be, //
            0x6c6c76, 0xd4554c, 0x4d9457, 0xc27d18, 0x4683c2, 0xa163b5, 0x369490, 0xe6e5e0,
        ],
        0x18181d,
        0xf7f6f3,
        0xe04f00,
        0x18181d,
        0xf3d8c6,
    )
    .with_accent(0xe04f00),
    theme(
        "solarized-dark",
        "Solarized Dark",
        [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, //
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
        0x839496,
        0x002b36,
        0x93a1a1,
        0xfdf6e3,
        0x073642,
    ),
    theme(
        "solarized-light",
        "Solarized Light",
        [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, //
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
        0x657b83,
        0xfdf6e3,
        0x586e75,
        0x002b36,
        0xeee8d5,
    ),
    theme(
        "gruvbox-dark",
        "Gruvbox Dark",
        [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, //
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
        0xebdbb2,
        0x282828,
        0xebdbb2,
        0xebdbb2,
        0x504945,
    ),
    theme(
        "dracula",
        "Dracula",
        [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2, //
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
        0xf8f8f2,
        0x282a36,
        0xf8f8f2,
        0xf8f8f2,
        0x44475a,
    ),
    theme(
        "nord",
        "Nord",
        [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0, //
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
        0xd8dee9,
        0x2e3440,
        0xd8dee9,
        0xeceff4,
        0x434c5e,
    ),
    theme(
        "catppuccin-mocha",
        "Catppuccin Mocha",
        [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de, //
            0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
        ],
        0xcdd6f4,
        0x1e1e2e,
        0xf5e0dc,
        0xcdd6f4,
        0x585b70,
    ),
    theme(
        "catppuccin-latte",
        "Catppuccin Latte",
        [
            0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be, //
            0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
        ],
        0x4c4f69,
        0xeff1f5,
        0xdc8a78,
        0x4c4f69,
        0xacb0be,
    ),
    theme(
        "tokyo-night",
        "Tokyo Night",
        [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6, //
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
        0xc0caf5,
        0x1a1b26,
        0xc0caf5,
        0xc0caf5,
        0x283457,
    ),
];

pub fn find_theme(id: &str) -> Option<&'static Theme> {
    THEMES.iter().find(|t| t.id == id)
}

pub const DEFAULT_LIGHT_THEME: &str = "goatty-day";

/// The `theme` section of the client settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    /// Theme id: a built-in ([`THEMES`]) or a user theme
    /// (`themes/<id>.json`, see [`crate::themes`]).
    pub name: String,
    /// Use `light` / `dark` depending on the desktop's appearance instead
    /// of `name`.
    pub follow_system: bool,
    pub light: String,
    pub dark: String,
    /// Render bold text in the bright variant of colors 0–7 (xterm behavior).
    pub bold_is_bright: bool,
    /// Per-theme color overrides, keyed by theme id.
    #[serde(serialize_with = "serialize_non_empty_overrides")]
    pub overrides: BTreeMap<String, ThemeOverrides>,
}

fn serialize_non_empty_overrides<S: Serializer>(map: &BTreeMap<String, ThemeOverrides>, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let non_empty: Vec<_> = map.iter().filter(|(_, v)| !v.is_empty()).collect();
    let mut m = s.serialize_map(Some(non_empty.len()))?;
    for (k, v) in non_empty {
        m.serialize_entry(k, v)?;
    }
    m.end()
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            name: DEFAULT_THEME.into(),
            follow_system: false,
            light: DEFAULT_LIGHT_THEME.into(),
            dark: DEFAULT_THEME.into(),
            bold_is_bright: true,
            overrides: BTreeMap::new(),
        }
    }
}

/// Individual colors replacing the theme's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_foreground: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_background: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<Color>,
    /// Index `i` replaces ANSI color `i` (0–15); `null` keeps the theme's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ansi: Vec<Option<Color>>,
}

impl ThemeOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default() || ColorSlot::all().into_iter().all(|s| self.get(s).is_none())
    }

    pub fn get(&self, slot: ColorSlot) -> Option<Color> {
        match slot {
            ColorSlot::Foreground => self.foreground,
            ColorSlot::Background => self.background,
            ColorSlot::Cursor => self.cursor,
            ColorSlot::SelectionForeground => self.selection_foreground,
            ColorSlot::SelectionBackground => self.selection_background,
            ColorSlot::Accent => self.accent,
            ColorSlot::Ansi(i) => self.ansi.get(i as usize).copied().flatten(),
        }
    }

    /// Set (or with `None` clear) one override, keeping the ANSI list short.
    pub fn set(&mut self, slot: ColorSlot, c: Option<Color>) {
        match slot {
            ColorSlot::Foreground => self.foreground = c,
            ColorSlot::Background => self.background = c,
            ColorSlot::Cursor => self.cursor = c,
            ColorSlot::SelectionForeground => self.selection_foreground = c,
            ColorSlot::SelectionBackground => self.selection_background = c,
            ColorSlot::Accent => self.accent = c,
            ColorSlot::Ansi(i) => {
                let i = (i as usize).min(15);
                if self.ansi.len() <= i {
                    self.ansi.resize(i + 1, None);
                }
                self.ansi[i] = c;
                while self.ansi.last() == Some(&None) {
                    self.ansi.pop();
                }
            }
        }
    }

    /// `base` with these overrides applied.
    pub fn apply(&self, mut base: ThemeColors) -> ThemeColors {
        for slot in ColorSlot::all() {
            if let Some(c) = self.get(slot) {
                base.set(slot, c);
            }
        }
        base
    }
}

impl ThemeSettings {
    /// The theme id in use, given the desktop's appearance.
    pub fn active_id(&self, system_dark: bool) -> &str {
        match (self.follow_system, system_dark) {
            (false, _) => &self.name,
            (true, true) => &self.dark,
            (true, false) => &self.light,
        }
    }

    /// Overrides stored for `id`, if any color is overridden.
    pub fn overrides_for(&self, id: &str) -> Option<&ThemeOverrides> {
        self.overrides.get(id).filter(|o| !o.is_empty())
    }

    /// Overrides stored for the active theme, if any color is overridden.
    pub fn active_overrides(&self, system_dark: bool) -> Option<&ThemeOverrides> {
        self.overrides_for(self.active_id(system_dark))
    }

    /// Set (or with `None` clear) one overridden color for `id`, dropping
    /// the entry once no colors remain overridden.
    pub fn set_override(&mut self, id: &str, slot: ColorSlot, color: Option<Color>) {
        if let Some(c) = color {
            self.overrides.entry(id.to_string()).or_default().set(slot, Some(c));
        } else if let Some(entry) = self.overrides.get_mut(id) {
            entry.set(slot, None);
            if entry.is_empty() {
                self.overrides.remove(id);
            }
        }
    }

    /// Drop every overridden color for `id`, leaving other themes intact.
    pub fn clear_overrides(&mut self, id: &str) {
        self.overrides.remove(id);
    }

    /// Built-in themes only (no user themes, fixed theme): the colors with
    /// overrides. Unknown names fall back to the default theme. The app
    /// uses [`crate::themes::ThemeLibrary::resolve`].
    pub fn colors(&self) -> ThemeColors {
        let base = find_theme(&self.name).or_else(|| find_theme(DEFAULT_THEME)).expect("default theme exists").colors;
        match self.overrides_for(&self.name) {
            Some(o) => o.apply(base),
            None => base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_serde() {
        assert_eq!(serde_json::to_string(&Color(0x0a0b0c)).unwrap(), "\"#0a0b0c\"");
        assert_eq!(serde_json::from_str::<Color>("\"#FFaa00\"").unwrap(), Color(0xffaa00));
        assert!(serde_json::from_str::<Color>("\"red\"").is_err());
        assert!(serde_json::from_str::<Color>("\"12 34 56\"").is_err(), "bare numbers could mean any model");
        // Read leniently (#71), always written as #rrggbb.
        for (text, want) in [("#fff", 0xffffff), ("rgb(255 170 0)", 0xffaa00), ("hsl(40deg 100% 50%)", 0xffaa00), ("#ffaa0080", 0xffaa00)] {
            let c = serde_json::from_str::<Color>(&format!("{text:?}")).unwrap();
            assert_eq!(c, Color(want), "{text}");
            assert_eq!(serde_json::to_string(&c).unwrap(), format!("\"#{want:06x}\""));
        }
    }

    #[test]
    fn every_slot_has_a_contrast_check_against_another_slot() {
        for slot in ColorSlot::all() {
            let checks = slot.contrast_checks();
            assert!(!checks.is_empty(), "{slot:?}");
            assert!(checks.iter().all(|c| c.against != slot), "{slot:?}");
        }
        assert_eq!(ColorSlot::Foreground.contrast_checks()[0].min, Some(4.5));
        assert_eq!(ColorSlot::Ansi(0).contrast_checks()[0].min, None, "black is often a backdrop");
    }

    #[test]
    fn themes_are_unique_and_sane() {
        for (i, t) in THEMES.iter().enumerate() {
            assert!(THEMES[..i].iter().all(|o| o.id != t.id), "duplicate {}", t.id);
            assert_ne!(t.colors.foreground, t.colors.background, "{}", t.id);
            assert_eq!(t.colors.accent(), t.colors.accent.unwrap_or(t.colors.ansi[4]), "{}", t.id);
        }
        assert!(find_theme(DEFAULT_THEME).unwrap().colors.is_dark());
        assert_eq!(find_theme(DEFAULT_THEME).unwrap().colors.accent, Some(Color(0xff5a00)));
        assert!(!find_theme("goatty-day").unwrap().colors.is_dark());
        assert_eq!(find_theme("goatty-day").unwrap().colors.accent, Some(Color(0xe04f00)));
        assert_eq!(find_theme("nord").unwrap().colors.accent, None);
    }

    #[test]
    fn overrides_apply() {
        let mut s = ThemeSettings::default();
        s.set_override(DEFAULT_THEME, ColorSlot::Background, Some(Color(0x000000)));
        s.set_override(DEFAULT_THEME, ColorSlot::Ansi(1), Some(Color(0xff0000)));
        let c = s.colors();
        assert_eq!(c.background, Color(0));
        assert_eq!(c.ansi[1], Color(0xff0000));
        assert_eq!(c.ansi[0], find_theme(DEFAULT_THEME).unwrap().colors.ansi[0]);
        assert_eq!(c.accent, Some(Color(0xff5a00)));
        assert_eq!(c.accent(), Color(0xff5a00));
        let round = ThemeColors::from_slice(&c.to_vec()).unwrap();
        // `to_vec`/`from_slice` carry only the 21 terminal colors over the wire, not the UI accent.
        assert_eq!(round, ThemeColors { accent: None, ..c });
        assert_eq!(ThemeSettings { name: "nope".into(), ..Default::default() }.colors(), ThemeSettings::default().colors());

        s.set_override(DEFAULT_THEME, ColorSlot::Accent, Some(Color(0x98c379)));
        let c = s.colors();
        assert_eq!(c.accent, Some(Color(0x98c379)));
        assert_eq!(c.accent(), Color(0x98c379));
        assert_eq!(c.get(ColorSlot::Accent), Color(0x98c379));
        assert_eq!(c.ansi[4], find_theme(DEFAULT_THEME).unwrap().colors.ansi[4]);

        // Switching themes keeps per-theme overrides isolated and preserved.
        s.name = "nord".into();
        assert_eq!(s.colors(), find_theme("nord").unwrap().colors, "nord has no overrides");
        s.set_override("nord", ColorSlot::Foreground, Some(Color(0x123456)));
        assert_eq!(s.colors().foreground, Color(0x123456));
        s.name = DEFAULT_THEME.into();
        assert_eq!(s.colors().background, Color(0));
        assert_eq!(s.colors().foreground, find_theme(DEFAULT_THEME).unwrap().colors.foreground);

        // Clearing or reverting one theme leaves other themes intact.
        s.set_override("nord", ColorSlot::Foreground, None);
        assert!(s.overrides_for("nord").is_none());
        assert!(s.overrides_for(DEFAULT_THEME).is_some());
        s.clear_overrides(DEFAULT_THEME);
        assert!(s.overrides.is_empty());
    }

    #[test]
    fn slots_and_override_editing() {
        assert_eq!(ColorSlot::all().len(), 22);
        assert_eq!(ColorSlot::Accent.label(), "Accent");
        assert_eq!(ColorSlot::Ansi(9).label(), "bright red");
        let mut o = ThemeOverrides::default();
        o.set(ColorSlot::Ansi(5), Some(Color(1)));
        assert_eq!(o.ansi.len(), 6);
        assert_eq!(o.get(ColorSlot::Ansi(5)), Some(Color(1)));
        assert!(!o.is_empty());
        o.set(ColorSlot::Ansi(5), None);
        assert!(o.ansi.is_empty(), "trailing Nones are dropped");
        assert!(o.is_empty());
        o.set(ColorSlot::Accent, Some(Color(0x00ff00)));
        assert_eq!(o.get(ColorSlot::Accent), Some(Color(0x00ff00)));
        assert!(!o.is_empty());
        o.set(ColorSlot::Accent, None);
        assert!(o.is_empty());
        let s = ThemeSettings { follow_system: true, ..Default::default() };
        assert_eq!(s.active_id(true), DEFAULT_THEME);
        assert_eq!(s.active_id(false), DEFAULT_LIGHT_THEME);
        assert_eq!(ThemeSettings::default().active_id(false), DEFAULT_THEME);
    }

    #[test]
    fn colors_round_trip_json() {
        let c = find_theme("nord").unwrap().colors;
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("accent"), "absent accent stays omitted: {json}");
        let back: ThemeColors = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);

        let mut custom = c;
        custom.accent = Some(Color(0xa3be8c));
        let json = serde_json::to_string(&custom).unwrap();
        assert!(json.contains("\"accent\":\"#a3be8c\""), "{json}");
        let back: ThemeColors = serde_json::from_str(&json).unwrap();
        assert_eq!(back, custom);
        assert_eq!(back.accent(), Color(0xa3be8c));

        let from_primary: ThemeColors = serde_json::from_str(&json.replace("\"accent\"", "\"primary\"")).unwrap();
        assert_eq!(from_primary, custom);
    }
}
