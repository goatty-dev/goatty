//! The goatty brand mark, recolored to follow the active terminal theme.
//!
//! The source SVG (`assets/brand/goatty.svg`) uses five fixed color slots:
//! horns/ears/goatee (`#ff5a00`), inner ear shade (`#8f3200`), head fill
//! (`#16161b`), head outline (`#2f2f38`) and face glyphs (`#f2f2f0`). We swap
//! those slots from the active [`ThemeColors`] and rasterize at exact device
//! pixels so the mascot stays sharp on any DPI and adapts immediately as the
//! user changes themes or overrides the accent color.

use std::fmt::Write;
use std::sync::Arc;

use goatty_config::{Color, ThemeColors};
use gpui::{AnyElement, App, DevicePixels, Global, ImageCacheError, IntoElement, RenderImage, Styled, SvgSize, Window, img, px};

use super::style::mix;

const LOGO_SVG: &str = include_str!("../../assets/brand/goatty.svg");

const MAX_CACHE: usize = 16;

/// The five color slots of `assets/brand/goatty.svg`, resolved for one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrandPalette {
    pub accent: Color,
    pub ear_shadow: Color,
    pub head_fill: Color,
    pub head_stroke: Color,
    pub face: Color,
}

impl BrandPalette {
    /// Derive the mascot's colors from the active terminal theme.
    pub fn from_theme(c: &ThemeColors) -> Self {
        let accent = c.accent();
        // Matches `#ff5a00` → `#8f3200` in the brand SVG.
        let ear_shadow = mix(accent, Color(0x000000), 0.44);
        let (head_fill, head_stroke, face) = if c.is_dark() {
            (mix(c.background, c.foreground, 0.04), mix(c.background, c.foreground, 0.16), c.foreground)
        } else {
            // Keep the mascot's dark terminal head on light themes too, tinted to
            // the theme's ink and dark enough for the light prompt face to read.
            let luma = c.foreground.luma();
            let keep = if luma > 0.0 { (0.16 / luma).min(0.75) } else { 0.75 };
            let head = mix(c.foreground, Color(0x000000), 1.0 - keep);
            (head, mix(head, c.background, 0.18), c.background)
        };
        Self { accent, ear_shadow, head_fill, head_stroke, face }
    }
}

/// Return the brand SVG with its five color slots recolored for `palette`.
pub fn themed_svg(palette: BrandPalette) -> String {
    let mut out = String::with_capacity(LOGO_SVG.len());
    let mut rest = LOGO_SVG;
    while let Some(pos) = rest.find('#') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let replacement = if rest.len() >= 7 {
            match &rest[..7] {
                "#ff5a00" => Some(palette.accent),
                "#8f3200" => Some(palette.ear_shadow),
                "#16161b" => Some(palette.head_fill),
                "#2f2f38" => Some(palette.head_stroke),
                "#f2f2f0" => Some(palette.face),
                _ => None,
            }
        } else {
            None
        };
        if let Some(color) = replacement {
            let _ = write!(out, "{color}");
            rest = &rest[7..];
        } else {
            out.push('#');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Rasterized themed logos, bounded so dragging the color picker can't grow
/// memory without limit.
#[derive(Default)]
struct BrandCache {
    entries: Vec<((BrandPalette, u32), Arc<RenderImage>)>,
}

impl Global for BrandCache {}

/// The goatty logo at `size` logical pixels, colored for `colors`.
pub fn logo(size: f32, colors: &ThemeColors) -> AnyElement {
    let palette = BrandPalette::from_theme(colors);
    img(move |window: &mut Window, cx: &mut App| -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let device = (size * window.scale_factor()).round().max(1.0) as u32;
        let key = (palette, device);
        if let Some(image) = cx.default_global::<BrandCache>().entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()) {
            return Some(Ok(image));
        }
        let svg_text = themed_svg(palette);
        let renderer = cx.svg_renderer();
        let side = DevicePixels(device as i32);
        let image = renderer
            .parse_svg(svg_text.as_bytes())
            .and_then(|svg| renderer.render_parsed(&svg, SvgSize::ExactSize(gpui::size(side, side))));
        Some(match image {
            Ok(image) => {
                let entries = &mut cx.default_global::<BrandCache>().entries;
                if entries.len() >= MAX_CACHE {
                    entries.remove(0);
                }
                entries.push((key, image.clone()));
                Ok(image)
            }
            Err(e) => Err(e.into()),
        })
    })
    .flex_none()
    .size(px(size))
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_svg_has_all_expected_color_slots() {
        for slot in ["#ff5a00", "#8f3200", "#16161b", "#2f2f38", "#f2f2f0"] {
            assert!(LOGO_SVG.contains(slot), "brand SVG is missing expected color slot {slot}");
        }
    }

    #[test]
    fn default_theme_matches_brand_orange_and_ear_shade() {
        let night = goatty_config::find_theme("goatty-night").unwrap().colors;
        let p = BrandPalette::from_theme(&night);
        assert_eq!(p.accent, Color(0xff5a00));
        assert_eq!(p.ear_shadow, Color(0x8f3200));
        assert_eq!(p.face, night.foreground);
    }

    #[test]
    fn light_themes_keep_dark_head_and_light_face() {
        for id in ["goatty-day", "solarized-light", "catppuccin-latte"] {
            let c = goatty_config::find_theme(id).unwrap().colors;
            let p = BrandPalette::from_theme(&c);
            assert!(p.head_fill.luma() <= 0.18, "{id} head should stay dark: {:?}", p.head_fill);
            assert_eq!(p.face, c.background, "{id} face should match light background");
        }
    }

    #[test]
    fn themed_svg_replaces_in_one_pass_without_clobbering() {
        // Use original token colors in different slots to verify single-pass substitution.
        let p = BrandPalette {
            accent: Color(0x16161b),
            ear_shadow: Color(0xf2f2f0),
            head_fill: Color(0x112233),
            head_stroke: Color(0x445566),
            face: Color(0x778899),
        };
        let rendered = themed_svg(p);
        assert_eq!(rendered.matches("#16161b").count(), 5, "horns, ears and goatee keep accent color");
        assert_eq!(rendered.matches("#f2f2f0").count(), 2, "inner ears keep ear shadow color");
        assert_eq!(rendered.matches("#112233").count(), 1);
        assert_eq!(rendered.matches("#445566").count(), 1);
        assert_eq!(rendered.matches("#778899").count(), 3);
        assert!(!rendered.contains("#ff5a00"));
        assert!(!rendered.contains("#8f3200"));
        assert!(!rendered.contains("#2f2f38"));
    }
}
