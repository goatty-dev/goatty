//! A color picker: hue ring with a saturation/value square (or a perceptual
//! OKLCH disc: hue × chroma at one lightness), sliders for RGB, HSL, HSV and
//! OKLCH, a field that takes whatever you paste (hex, `rgb()`, `hsl()`,
//! `hsv()`, `oklch()`, `oklab()`), the previous color to go back to,
//! lighter/darker nudges, the contrast against colors it must stay readable
//! on, and a history of recent picks.
//!
//! The wheel, the disc and every slider are tab stops: arrow keys move them
//! (Shift: bigger steps), PageUp/PageDown and Home/End work on sliders.
//!
//! The picker keeps its own float state ([`Pick`]), so dragging never loses
//! precision to 8-bit rounding and hue/saturation/chroma survive passing
//! through gray, black and white. It emits [`ColorPickerEvent::Changed`] on
//! every change (previews, typing, dragging) and
//! [`ColorPickerEvent::Committed`] when the user settles (mouse released,
//! Enter, a history click, a paste): that is when the color joins the
//! history. Opaque sRGB only, like the terminal themes it edits.
//!
//! Everything about colors lives in `goatty_config::colorspace`; this file is
//! the interaction and the painting.

use std::cell::RefCell;
use std::f32::consts::SQRT_2;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use goatty_config::colorspace::{ColorFormat, Hsl, Hsv, Oklch, Rgbf, contrast_ratio, from_rgbf, number, parse_color, to_rgbf};
use goatty_config::{Color, ColorHistory};
use gpui::{
    App, BorderStyle, Bounds, ClickEvent, ClipboardItem, Context, Corners, CursorStyle, DispatchPhase, Edges, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, Hsla, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, Pixels, Point, RenderImage, Rgba, Role, SharedString, Window, actions, black, canvas, div, fill, linear_color_stop,
    linear_gradient, point, prelude::*, px, quad, rgb, size, transparent_black, white,
};

use super::controls::{ButtonKind, button, segmented};
use super::focus::{self, Ring, Scroller};
use super::style::Ui;
use super::text_input::{TextInput, TextInputEvent};

pub const CONTEXT: &str = "ColorPicker";

actions!(color_picker, [PasteColor, CopyColor]);

pub fn bindings() -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    vec![KeyBinding::new("ctrl-v", PasteColor, c), KeyBinding::new("ctrl-c", CopyColor, c)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerEvent {
    /// The color changed (a preview while dragging or typing).
    Changed(Color),
    /// The user settled on this color.
    Committed(Color),
}

// ---- The picked color ---------------------------------------------------------------

/// Chroma below this has no meaningful hue.
const MIN_C: f32 = 0.002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Rgb,
    Hsl,
    Hsv,
    Oklch,
}

/// One slider of a [`Model`], in display units.
#[derive(Debug, Clone, Copy)]
struct Channel {
    label: &'static str,
    unit: &'static str,
    max: f32,
    decimals: usize,
    /// One arrow key press (Shift: ten).
    step: f32,
    /// Wraps around instead of clamping.
    hue: bool,
}

impl Model {
    const ALL: [(Model, &'static str); 4] = [(Model::Rgb, "RGB"), (Model::Hsl, "HSL"), (Model::Hsv, "HSV"), (Model::Oklch, "OKLCH")];

    /// What bare numbers in the paste field mean.
    fn hint(self) -> ColorFormat {
        match self {
            Model::Rgb => ColorFormat::Rgb,
            Model::Hsl => ColorFormat::Hsl,
            Model::Hsv => ColorFormat::Hsv,
            Model::Oklch => ColorFormat::Oklch,
        }
    }

    fn channels(self) -> [Channel; 3] {
        let c = |label, unit, max, decimals, hue| Channel { label, unit, max, decimals, step: 1.0, hue };
        match self {
            Model::Rgb => [c("R", "", 255.0, 0, false), c("G", "", 255.0, 0, false), c("B", "", 255.0, 0, false)],
            Model::Hsl => [c("H", "°", 360.0, 1, true), c("S", "%", 100.0, 1, false), c("L", "%", 100.0, 1, false)],
            Model::Hsv => [c("H", "°", 360.0, 1, true), c("S", "%", 100.0, 1, false), c("V", "%", 100.0, 1, false)],
            Model::Oklch => {
                [c("L", "%", 100.0, 1, false), Channel { step: CHROMA_STEP, ..c("C", "", 0.4, 3, false) }, c("H", "°", 360.0, 1, true)]
            }
        }
    }

    /// Channel `i` at `v` after the key press `n`.
    fn stepped(self, i: usize, v: f32, n: Nudge) -> f32 {
        let ch = self.channels()[i];
        match n {
            Nudge::Min => 0.0,
            // 360° is 0° again: the end of a hue track is a step before it.
            Nudge::Max if ch.hue => ch.max - ch.step,
            Nudge::Max => ch.max,
            Nudge::By(dx, dy) if ch.hue => (v + (dx + dy) * ch.step).rem_euclid(360.0),
            Nudge::By(dx, dy) => (v + (dx + dy) * ch.step).clamp(0.0, ch.max),
        }
    }
}

/// The color being edited, with the memory that keeps hue, saturation and
/// chroma when the color passes through gray, black or white (where they
/// can't be read back from the RGB value).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pick {
    hsv: Hsv,
    ok_h: f32,
    ok_c: f32,
    hsl_s: f32,
}

impl Pick {
    pub fn from_color(c: Color) -> Self {
        let mut p = Self { hsv: Hsv::from_color(c), ok_h: 0.0, ok_c: 0.0, hsl_s: 0.0 };
        p.remember();
        p
    }

    pub fn color(&self) -> Color {
        self.hsv.to_color()
    }

    fn rgbf(&self) -> Rgbf {
        self.hsv.to_rgbf()
    }

    /// Refresh the memory from the current color wherever it is meaningful.
    fn remember(&mut self) {
        let rgb = self.rgbf();
        let o = Oklch::from_rgbf(rgb);
        if o.l > 0.002 && o.l < 0.998 {
            self.ok_c = o.c;
            if o.c >= MIN_C {
                self.ok_h = o.h;
            }
        }
        let l = Hsl::from_rgbf(rgb);
        if l.l > 0.001 && l.l < 0.999 {
            self.hsl_s = l.s;
        }
    }

    /// A new color from RGB, keeping what RGB can't tell (hue of a gray, ...).
    fn keep(&self, rgb: Rgbf) -> Self {
        let mut hsv = Hsv::from_rgbf(rgb);
        if hsv.s < 1e-5 {
            hsv.h = self.hsv.h;
        }
        if hsv.v < 1e-5 {
            hsv.s = self.hsv.s;
        }
        let mut p = Self { hsv, ..*self };
        p.remember();
        p
    }

    fn keep_hsv(&self, hsv: Hsv) -> Self {
        let mut p = Self { hsv, ..*self };
        p.remember();
        p
    }

    /// `c` picked from outside (typed, pasted, history).
    pub fn keep_color(&self, c: Color) -> Self {
        self.keep(to_rgbf(c))
    }

    fn oklch(&self) -> Oklch {
        let o = Oklch::from_rgbf(self.rgbf());
        let flat = o.l < 0.002 || o.l > 0.998; // black/white: chroma and hue are undefined
        Oklch { l: o.l, c: if flat { self.ok_c } else { o.c }, h: if flat || o.c < MIN_C { self.ok_h } else { o.h } }
    }

    fn hsl(&self) -> Hsl {
        let l = Hsl::from_rgbf(self.rgbf());
        Hsl { h: self.hsv.h, s: if l.l < 0.001 || l.l > 0.999 { self.hsl_s } else { l.s }, l: l.l }
    }

    /// Channel `i` of `m`, in the units of [`Model::channels`].
    fn get(&self, m: Model, i: usize) -> f32 {
        match m {
            Model::Rgb => {
                let (r, g, b) = self.color().rgb();
                [r as f32, g as f32, b as f32][i]
            }
            Model::Hsl => {
                let h = self.hsl();
                [h.h, h.s * 100.0, h.l * 100.0][i]
            }
            Model::Hsv => [self.hsv.h, self.hsv.s * 100.0, self.hsv.v * 100.0][i],
            Model::Oklch => {
                let o = self.oklch();
                [o.l * 100.0, o.c, o.h][i]
            }
        }
    }

    /// This color with channel `i` of `m` set to `v`.
    fn with(&self, m: Model, i: usize, v: f32) -> Self {
        let mut vals = [self.get(m, 0), self.get(m, 1), self.get(m, 2)];
        vals[i] = v;
        match m {
            Model::Rgb => self.keep([vals[0] / 255.0, vals[1] / 255.0, vals[2] / 255.0].map(|x| x.clamp(0.0, 1.0))),
            Model::Hsl => {
                let hsl = Hsl::new(vals[0], vals[1] / 100.0, vals[2] / 100.0);
                let mut p = self.keep(hsl.to_rgbf());
                p.hsv.h = hsl.h;
                if i == 1 {
                    p.hsl_s = hsl.s;
                }
                p
            }
            Model::Hsv => self.keep_hsv(Hsv::new(vals[0], vals[1] / 100.0, vals[2] / 100.0)),
            Model::Oklch => {
                let o = Oklch::new(vals[0] / 100.0, vals[1], vals[2]);
                let mut p = self.keep(o.to_rgbf());
                match i {
                    1 => p.ok_c = o.c,
                    2 => p.ok_h = o.h,
                    _ => {}
                }
                p
            }
        }
    }

    /// This color at OKLCH `o` (gamut mapped by lowering chroma), keeping
    /// `o`'s hue (and its chroma at black and white, where the color can't).
    fn with_oklch(&self, o: Oklch) -> Self {
        let o = Oklch::new(o.l, o.c, o.h);
        let mut p = self.keep(o.to_rgbf());
        p.ok_h = o.h;
        if o.l <= 0.002 || o.l >= 0.998 {
            p.ok_c = o.c;
        }
        p
    }

    /// The color at `t` (0..=1) along the track of channel `i`.
    fn track(&self, m: Model, i: usize, t: f32) -> Color {
        self.with(m, i, t * m.channels()[i].max).color()
    }
}

// ---- Geometry (pure) ----------------------------------------------------------------

const WHEEL: f32 = 216.0;
const RING_WIDTH: f32 = 20.0;
const RING_SEGMENTS: usize = 120;
/// Between the inner edge of the ring and the square.
const SQUARE_GAP: f32 = 6.0;
/// Slider thumb radius: the track is inset by this much on both sides.
const THUMB: f32 = 8.0;
const TRACK_SEGMENTS: usize = 36;

/// A hue ring with a saturation/value square inside, in a `size`-sized box.
#[derive(Debug, Clone, Copy)]
struct Wheel {
    size: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    Ring,
    Square,
}

impl Wheel {
    fn center(self) -> f32 {
        self.size / 2.0
    }

    fn outer(self) -> f32 {
        self.size / 2.0 - 1.0
    }

    fn inner(self) -> f32 {
        self.outer() - RING_WIDTH
    }

    /// Half the side of the square.
    fn half(self) -> f32 {
        (self.inner() - SQUARE_GAP) / SQRT_2
    }

    fn hit(self, x: f32, y: f32) -> Option<Region> {
        let (dx, dy) = (x - self.center(), y - self.center());
        if dx.abs() <= self.half() && dy.abs() <= self.half() {
            return Some(Region::Square);
        }
        let d = dx.hypot(dy);
        (d >= self.inner() - 3.0 && d <= self.outer() + 3.0).then_some(Region::Ring)
    }

    /// Hue (0 at the top, clockwise) of the direction from the center to `(x, y)`.
    fn hue_at(self, x: f32, y: f32) -> f32 {
        let (dx, dy) = (x - self.center(), y - self.center());
        let h = dx.atan2(-dy).to_degrees();
        if h < 0.0 { h + 360.0 } else { h }
    }

    /// Saturation and value under `(x, y)` (clamped to the square).
    fn sv_at(self, x: f32, y: f32) -> (f32, f32) {
        let (c, h) = (self.center(), self.half());
        (((x - c + h) / (2.0 * h)).clamp(0.0, 1.0), (1.0 - (y - c + h) / (2.0 * h)).clamp(0.0, 1.0))
    }

    /// The point at `hue` and `radius` from the center.
    fn polar(self, hue: f32, radius: f32) -> (f32, f32) {
        let a = hue.to_radians();
        (self.center() + radius * a.sin(), self.center() - radius * a.cos())
    }

    fn sv_point(self, s: f32, v: f32) -> (f32, f32) {
        let (c, h) = (self.center(), self.half());
        (c - h + s * 2.0 * h, c - h + (1.0 - v) * 2.0 * h)
    }
}

/// Chroma at the rim of the OKLCH disc: a little more than sRGB's most
/// colorful color (magenta, 0.32).
const DISC_CHROMA: f32 = 0.33;
/// One arrow key press on an OKLCH chroma (Shift: ten).
const CHROMA_STEP: f32 = 0.002;
/// What "Lighter"/"Darker" change OKLCH lightness by.
const NUDGE_L: f32 = 0.04;
/// How out-of-gamut parts of the disc fade into the picker's background.
const DISC_DIM: f32 = 0.6;

/// The OKLCH disc in a `size`-sized box: hue around (0 at the top,
/// clockwise, like the HSV ring), chroma outwards, at one lightness.
#[derive(Debug, Clone, Copy)]
struct Disc {
    size: f32,
}

impl Disc {
    fn center(self) -> f32 {
        self.size / 2.0
    }

    fn radius(self) -> f32 {
        self.size / 2.0 - 1.0
    }

    fn hit(self, x: f32, y: f32) -> bool {
        (x - self.center()).hypot(y - self.center()) <= self.radius() + 3.0
    }

    /// Hue and chroma under `(x, y)`; outside the rim, the rim's chroma.
    fn hc_at(self, x: f32, y: f32) -> (f32, f32) {
        let w = Wheel { size: self.size };
        let d = (x - self.center()).hypot(y - self.center());
        (w.hue_at(x, y), (d / self.radius()).min(1.0) * DISC_CHROMA)
    }

    fn point(self, h: f32, c: f32) -> (f32, f32) {
        (Wheel { size: self.size }).polar(h, c.clamp(0.0, DISC_CHROMA) / DISC_CHROMA * self.radius())
    }
}

/// The disc at lightness `l` as `n`×`n` BGRA pixels (what `RenderImage`
/// takes). Colors sRGB can't show are drawn at the most chroma it can,
/// faded towards `backdrop`, which also fills the corners.
fn disc_pixels(n: u32, l: f32, backdrop: Rgbf) -> Vec<u8> {
    // The most chroma that fits, per degree of hue, interpolated between.
    let limit: Vec<f32> = (0..=360).map(|h| Oklch::max_chroma(l, h as f32)).collect();
    let r = n as f32 / 2.0;
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - r, y as f32 + 0.5 - r);
            let d = dx.hypot(dy) / r;
            let rgb = if d > 1.0 {
                backdrop
            } else {
                let h = dx.atan2(-dy).to_degrees().rem_euclid(360.0);
                let (k, t) = (h.floor() as usize % 360, h.fract());
                let lim = limit[k] + (limit[k + 1] - limit[k]) * t;
                let c = d * DISC_CHROMA;
                let shown = Oklch { l, c: c.min(lim), h }.to_rgbf_clamped();
                if c > lim { [0, 1, 2].map(|i| shown[i] + (backdrop[i] - shown[i]) * DISC_DIM) } else { shown }
            };
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            out.extend([q(rgb[2]), q(rgb[1]), q(rgb[0]), 255]);
        }
    }
    out
}

/// A key press on a knob: a move in small steps (`x` right, `y` up), or to
/// one end of a slider.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Nudge {
    By(f32, f32),
    Min,
    Max,
}

/// What `key` does to a knob (Shift: ten steps at a time).
fn key_nudge(key: &str, shift: bool) -> Option<Nudge> {
    let m = if shift { 10.0 } else { 1.0 };
    Some(match key {
        "left" => Nudge::By(-m, 0.0),
        "right" => Nudge::By(m, 0.0),
        "up" => Nudge::By(0.0, m),
        "down" => Nudge::By(0.0, -m),
        "pageup" => Nudge::By(0.0, 10.0),
        "pagedown" => Nudge::By(0.0, -10.0),
        "home" => Nudge::Min,
        "end" => Nudge::Max,
        _ => return None,
    })
}

/// Where along a slider track (left edge `left`, `width` wide, both
/// without the thumb inset) is `x`, 0..=1.
fn track_fraction(x: f32, left: f32, width: f32) -> f32 {
    if width <= 0.0 { 0.0 } else { ((x - left) / width).clamp(0.0, 1.0) }
}

// ---- The view -----------------------------------------------------------------------

/// A painted area that takes the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Wheel,
    Disc,
    Lightness,
    Slider(usize),
}

/// Something the mouse drags or the arrow keys move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Knob {
    /// The HSV hue ring.
    Ring,
    /// The HSV saturation/value square.
    Square,
    /// The OKLCH disc (hue × chroma).
    Disc,
    /// The OKLCH lightness bar under the disc.
    Lightness,
    Slider(usize),
}

impl Knob {
    fn target(self) -> Target {
        match self {
            Knob::Ring | Knob::Square => Target::Wheel,
            Knob::Disc => Target::Disc,
            Knob::Lightness => Target::Lightness,
            Knob::Slider(i) => Target::Slider(i),
        }
    }

    fn id(self) -> ElementId {
        match self {
            Knob::Ring => "picker-ring".into(),
            Knob::Square => "picker-square".into(),
            Knob::Disc => "picker-disc".into(),
            Knob::Lightness => "picker-lightness".into(),
            Knob::Slider(i) => ("picker-slider", i).into(),
        }
    }
}

/// The big area left of the sliders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// Hue ring with a saturation/value square.
    Hsv,
    /// Hue × chroma at one OKLCH lightness: evenly bright colors sit on a
    /// circle.
    Oklch,
}

impl Area {
    const ALL: [(Area, &'static str); 2] = [(Area::Hsv, "HSV"), (Area::Oklch, "OKLCH")];
}

/// A color the edited one is drawn on or under, with the WCAG contrast it
/// should keep (#70). The settings window fills these in per theme slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastRef {
    /// The other color's name ("Background").
    pub label: SharedString,
    pub color: Color,
    /// The edited color is drawn on `color` (text), rather than under it.
    pub edited_is_text: bool,
    /// Warn below this ratio; `None` only shows it.
    pub min: Option<f32>,
}

/// The disc's bitmap for one lightness, size and backdrop.
struct DiscImage {
    key: (u32, u32, u32),
    image: Arc<RenderImage>,
}

/// After arrow keys or a nudge, the color joins the history once they stop.
const KEY_COMMIT_DELAY: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Any,
    Num(usize),
}

struct Status {
    text: String,
    ok: bool,
}

pub struct ColorPicker {
    focus: FocusHandle,
    pick: Pick,
    /// The color when this edit started: what "Previous" goes back to.
    original: Color,
    /// The last color announced with `Changed`.
    emitted: Color,
    model: Model,
    area: Area,
    drag: Option<(Knob, Bounds<Pixels>)>,
    /// The OKLCH color the last disc, lightness or nudge change aimed at,
    /// with the color it produced. While the color is still that one, the
    /// next change starts from the aim, so lightness doesn't drift with
    /// 8-bit rounding and a lighter-then-darker nudge gets its chroma back.
    aim: Option<(Color, Oklch)>,
    /// Bumped by every commit: a scheduled one only runs if it's the latest.
    commit_gen: u64,
    contrast: Vec<ContrastRef>,
    disc: Rc<RefCell<Option<DiscImage>>>,
    any: Entity<TextInput>,
    nums: [Entity<TextInput>; 3],
    history: ColorHistory,
    history_path: Option<PathBuf>,
    status: Option<Status>,
    _subs: Vec<gpui::Subscription>,
}

impl EventEmitter<ColorPickerEvent> for ColorPicker {}

impl Focusable for ColorPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn hsla(c: Color) -> Hsla {
    rgb(c.0).into()
}

/// Text that reads on top of `bg`.
fn contrast_text(bg: Color) -> Hsla {
    if bg.luma() > 0.55 { hsla(Color(0x101010)) } else { hsla(Color(0xffffff)) }
}

impl ColorPicker {
    /// A picker starting at `color`, with the user's history file.
    pub fn new(color: Color, cx: &mut Context<'_, Self>) -> Self {
        Self::with_history(color, goatty_config::color_history::history_file(), cx)
    }

    /// A picker that keeps its history in `history_path` (none: not saved).
    pub fn with_history(color: Color, history_path: Option<PathBuf>, cx: &mut Context<'_, Self>) -> Self {
        let any = cx.new(|cx| TextInput::new("Paste a color: #1e1e2e, rgb(30 30 46), oklch(0.27 0.03 285)…", "", cx).label("Color"));
        let nums = [0, 1, 2].map(|_| cx.new(|cx| TextInput::new("", "", cx).label("Channel")));
        let mut subs = vec![cx.subscribe(&any, |this, _, ev: &TextInputEvent, cx| this.on_any(ev, cx))];
        for (i, n) in nums.iter().enumerate() {
            subs.push(cx.subscribe(n, move |this, _, ev: &TextInputEvent, cx| this.on_num(i, ev, cx)));
        }
        let history = history_path.as_deref().map(ColorHistory::load).unwrap_or_default();
        let mut this = Self {
            focus: cx.focus_handle(),
            pick: Pick::from_color(color),
            original: color,
            emitted: color,
            model: Model::Rgb,
            area: Area::Hsv,
            drag: None,
            aim: None,
            commit_gen: 0,
            contrast: Vec::new(),
            disc: Rc::new(RefCell::new(None)),
            any,
            nums,
            history,
            history_path,
            status: None,
            _subs: subs,
        };
        this.sync_fields(None, cx);
        this
    }

    /// Start editing `color`: it becomes the "Previous" color. Emits nothing.
    pub fn open(&mut self, color: Color, cx: &mut Context<'_, Self>) {
        self.pick = Pick::from_color(color);
        self.original = color;
        self.emitted = color;
        self.drag = None;
        self.aim = None;
        self.commit_gen += 1;
        self.status = None;
        self.sync_fields(None, cx);
        cx.notify();
    }

    /// The colors the edited one should stay readable against (none: no
    /// contrast row).
    pub fn set_contrast(&mut self, refs: Vec<ContrastRef>, cx: &mut Context<'_, Self>) {
        if refs != self.contrast {
            self.contrast = refs;
            cx.notify();
        }
    }

    /// Follow a color changed from outside; a no-op when it's already the
    /// picker's own (so the picker's float state isn't rounded to 8 bits
    /// by its own echo).
    pub fn sync(&mut self, color: Color, cx: &mut Context<'_, Self>) {
        if color != self.pick.color() {
            self.open(color, cx);
        }
    }

    fn set_model(&mut self, model: Model, cx: &mut Context<'_, Self>) {
        self.model = model;
        self.sync_fields(None, cx);
        cx.notify();
    }

    fn set_area(&mut self, area: Area, cx: &mut Context<'_, Self>) {
        self.area = area;
        self.drag = None;
        cx.notify();
    }

    /// Where OKLCH changes start from: the last aim while the color is still
    /// the one it produced, else the color as it is.
    fn aimed_oklch(&self) -> Oklch {
        match self.aim {
            Some((c, o)) if c == self.pick.color() => o,
            _ => self.pick.oklch(),
        }
    }

    /// Go to OKLCH `o` and remember it as the aim.
    fn aim_at(&mut self, o: Oklch, cx: &mut Context<'_, Self>) {
        let p = self.pick.with_oklch(o);
        self.take(p, cx);
        self.aim = Some((self.pick.color(), o));
    }

    /// Take `p` from a drag or key, which may move a marker without
    /// changing the 8-bit color (the hue of a gray, say): keep that too.
    /// Drops the OKLCH aim ([`Self::aim_at`] sets it again afterwards).
    fn take(&mut self, p: Pick, cx: &mut Context<'_, Self>) {
        self.aim = None;
        if p.color() != self.pick.color() {
            self.set(p, None, cx);
        } else if p != self.pick {
            self.pick = p;
            self.sync_fields(None, cx);
            cx.notify();
        }
    }

    fn sync_fields(&mut self, skip: Option<Field>, cx: &mut Context<'_, Self>) {
        if skip != Some(Field::Any) {
            let hex = self.pick.color().to_string();
            self.any.update(cx, |i, cx| i.set_text(hex, cx));
        }
        for (i, ch) in self.model.channels().iter().enumerate() {
            if skip != Some(Field::Num(i)) {
                let text = number(self.pick.get(self.model, i), ch.decimals);
                self.nums[i].update(cx, |t, cx| t.set_text(text, cx));
            }
        }
    }

    /// Take `pick` (if it's a different color) and tell everyone.
    fn set(&mut self, pick: Pick, skip: Option<Field>, cx: &mut Context<'_, Self>) {
        if pick.color() == self.pick.color() {
            return;
        }
        self.pick = pick;
        self.sync_fields(skip, cx);
        let c = pick.color();
        if c != self.emitted {
            self.emitted = c;
            cx.emit(ColorPickerEvent::Changed(c));
        }
        cx.notify();
    }

    /// The user settled on the current color: remember it.
    fn commit(&mut self, cx: &mut Context<'_, Self>) {
        self.commit_gen += 1;
        let c = self.pick.color();
        if self.history.push(c)
            && let Some(path) = &self.history_path
            && let Err(e) = self.history.save(path)
        {
            log::warn!("couldn't save the color history to {}: {e}", path.display());
        }
        self.sync_fields(None, cx);
        cx.emit(ColorPickerEvent::Committed(c));
        cx.notify();
    }

    /// Commit once the keys or nudges stop for a moment, so holding an arrow
    /// key adds one color to the history, not one per step.
    fn commit_soon(&mut self, cx: &mut Context<'_, Self>) {
        self.commit_gen += 1;
        let generation = self.commit_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(KEY_COMMIT_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.commit_gen == generation {
                    this.commit(cx);
                }
            });
        })
        .detach();
    }

    /// "Lighter" (`dir` 1) or "Darker" (-1): a step of OKLCH lightness, so
    /// the steps look even, keeping hue and (where sRGB allows) chroma.
    fn nudge_lightness(&mut self, dir: f32, cx: &mut Context<'_, Self>) {
        let o = self.aimed_oklch();
        self.status = None;
        self.aim_at(Oklch { l: (o.l + dir * NUDGE_L).clamp(0.0, 1.0), ..o }, cx);
        self.commit_soon(cx);
    }

    // ---- Keyboard -------------------------------------------------------------------

    fn on_knob_key(&mut self, knob: Knob, ev: &KeyDownEvent, cx: &mut Context<'_, Self>) {
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        let Some(n) = key_nudge(&ev.keystroke.key, m.shift) else { return };
        cx.stop_propagation();
        self.status = None;
        let by = |n: Nudge| if let Nudge::By(x, y) = n { Some((x, y)) } else { None };
        match knob {
            Knob::Ring => {
                let Some((x, y)) = by(n) else { return };
                let p = self.pick.keep_hsv(Hsv::new(self.pick.hsv.h + x + y, self.pick.hsv.s, self.pick.hsv.v));
                self.take(p, cx);
            }
            Knob::Square => {
                let Some((x, y)) = by(n) else { return };
                let hsv = self.pick.hsv;
                let p = self.pick.keep_hsv(Hsv { s: (hsv.s + x / 100.0).clamp(0.0, 1.0), v: (hsv.v + y / 100.0).clamp(0.0, 1.0), ..hsv });
                self.take(p, cx);
            }
            Knob::Disc => {
                let o = self.aimed_oklch();
                let o = match n {
                    Nudge::By(x, y) => Oklch { h: (o.h + x).rem_euclid(360.0), c: (o.c + y * CHROMA_STEP).max(0.0), ..o },
                    Nudge::Min => Oklch { c: 0.0, ..o },
                    Nudge::Max => Oklch { c: Oklch::max_chroma(o.l, o.h), ..o },
                };
                // Chroma beyond the gamut would be an aim nothing shows.
                let o = Oklch { c: o.c.min(Oklch::max_chroma(o.l, o.h).max(0.0)), ..o };
                self.aim_at(o, cx);
            }
            Knob::Lightness => {
                let o = self.aimed_oklch();
                let l = match n {
                    Nudge::By(x, y) => o.l + (x + y) / 100.0,
                    Nudge::Min => 0.0,
                    Nudge::Max => 1.0,
                };
                self.aim_at(Oklch { l: l.clamp(0.0, 1.0), ..o }, cx);
            }
            Knob::Slider(i) => {
                let v = self.model.stepped(i, self.pick.get(self.model, i), n);
                let p = self.pick.with(self.model, i, v);
                self.take(p, cx);
            }
        }
        self.commit_soon(cx);
    }

    /// Pick `c` as a whole (history, paste, previous) and commit it.
    fn pick_color(&mut self, c: Color, cx: &mut Context<'_, Self>) {
        let p = self.pick.keep_color(c);
        self.set(p, None, cx);
        self.commit(cx);
    }

    fn say(&mut self, ok: bool, text: impl Into<String>, cx: &mut Context<'_, Self>) {
        self.status = Some(Status { text: text.into(), ok });
        cx.notify();
    }

    // ---- Text fields ----------------------------------------------------------------

    fn on_any(&mut self, ev: &TextInputEvent, cx: &mut Context<'_, Self>) {
        match ev {
            TextInputEvent::Changed(text) => {
                // Typing owns the commit now (Enter): a pending key commit would reset the field.
                self.commit_gen += 1;
                // Take it as soon as it reads as a color; leave the field alone while typing.
                if let Some(c) = parse_color(text, self.model.hint()) {
                    self.status = None;
                    let p = self.pick.keep_color(c);
                    self.set(p, Some(Field::Any), cx);
                }
            }
            TextInputEvent::Submitted(text) => match parse_color(text, self.model.hint()) {
                Some(c) => {
                    self.status = None;
                    self.pick_color(c, cx);
                }
                None => {
                    let shown: String = text.chars().take(40).collect();
                    self.say(false, format!("Can't read {shown:?} as a color. Try #rrggbb, rgb(), hsl(), hsv(), oklch() or oklab()."), cx);
                }
            },
        }
    }

    fn on_num(&mut self, i: usize, ev: &TextInputEvent, cx: &mut Context<'_, Self>) {
        let ch = self.model.channels()[i];
        match ev {
            TextInputEvent::Changed(text) => {
                self.commit_gen += 1;
                let t = text.trim().trim_end_matches(['%', '°']).trim_end_matches("deg").trim();
                let Some(v) = t.parse::<f32>().ok().filter(|v| v.is_finite()) else { return };
                let v = if ch.hue { v.rem_euclid(360.0) } else { v.clamp(0.0, ch.max) };
                let p = self.pick.with(self.model, i, v);
                self.set(p, Some(Field::Num(i)), cx);
            }
            TextInputEvent::Submitted(_) => self.commit(cx),
        }
    }

    // ---- Dragging -------------------------------------------------------------------

    fn begin_drag(&mut self, target: Target, bounds: Bounds<Pixels>, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (x, y) = rel(pos, bounds);
        let knob = match target {
            Target::Wheel => match (Wheel { size: f32::from(bounds.size.width) }).hit(x, y) {
                Some(Region::Ring) => Knob::Ring,
                Some(Region::Square) => Knob::Square,
                None => return,
            },
            Target::Disc if (Disc { size: f32::from(bounds.size.width) }).hit(x, y) => Knob::Disc,
            Target::Disc => return,
            Target::Lightness => Knob::Lightness,
            Target::Slider(i) => Knob::Slider(i),
        };
        // A knob took focus on mouse down (it's a tab stop); otherwise the
        // picker takes it, so Ctrl+V/C work.
        if !self.focus.contains_focused(window, cx) {
            window.focus(&self.focus, cx);
        }
        // The drag commits when it's released; a pending key commit is moot.
        self.commit_gen += 1;
        self.drag = Some((knob, bounds));
        self.status = None;
        self.drag_to(pos, cx);
    }

    fn drag_to(&mut self, pos: Point<Pixels>, cx: &mut Context<'_, Self>) {
        let Some((knob, b)) = self.drag else { return };
        let (x, y) = rel(pos, b);
        let width = f32::from(b.size.width);
        let along = || track_fraction(f32::from(pos.x), f32::from(b.origin.x) + THUMB, width - 2.0 * THUMB);
        let p = match knob {
            Knob::Ring => {
                let hue = (Wheel { size: width }).hue_at(x, y);
                self.pick.keep_hsv(Hsv { h: hue, ..self.pick.hsv })
            }
            Knob::Square => {
                let (s, v) = (Wheel { size: width }).sv_at(x, y);
                self.pick.keep_hsv(Hsv { s, v, ..self.pick.hsv })
            }
            Knob::Disc => {
                let (h, c) = (Disc { size: width }).hc_at(x, y);
                let o = self.aimed_oklch();
                // Beyond the gamut, the marker stays on its edge.
                return self.aim_at(Oklch { h, c: c.min(Oklch::max_chroma(o.l, h)), ..o }, cx);
            }
            Knob::Lightness => {
                let o = self.aimed_oklch();
                return self.aim_at(Oklch { l: along(), ..o }, cx);
            }
            Knob::Slider(i) => self.pick.with(self.model, i, along() * self.model.channels()[i].max),
        };
        self.take(p, cx);
    }

    fn end_drag(&mut self, cx: &mut Context<'_, Self>) {
        if self.drag.take().is_some() {
            self.commit(cx);
        }
    }

    // ---- Clipboard ------------------------------------------------------------------

    fn paste(&mut self, cx: &mut Context<'_, Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return self.say(false, "The clipboard has no text.", cx);
        };
        match parse_color(&text, self.model.hint()) {
            Some(c) => {
                self.pick_color(c, cx);
                self.say(true, format!("Pasted {c}"), cx);
            }
            None => {
                let shown: String = text.trim().chars().take(40).collect();
                self.say(false, format!("Can't read {shown:?} from the clipboard as a color."), cx);
            }
        }
    }

    fn paste_action(&mut self, _: &PasteColor, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.paste(cx);
    }

    fn copy_action(&mut self, _: &CopyColor, _: &mut Window, cx: &mut Context<'_, Self>) {
        let text = self.pick.color().to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.say(true, format!("Copied {text}"), cx);
    }

    fn clear_history(&mut self, cx: &mut Context<'_, Self>) {
        self.history.clear();
        if let Some(path) = &self.history_path
            && let Err(e) = self.history.save(path)
        {
            log::warn!("couldn't save the color history to {}: {e}", path.display());
        }
        cx.notify();
    }
}

fn rel(pos: Point<Pixels>, b: Bounds<Pixels>) -> (f32, f32) {
    (f32::from(pos.x - b.origin.x), f32::from(pos.y - b.origin.y))
}

// ---- Painting -----------------------------------------------------------------------

/// A round marker with a white ring and a dark outline, readable on anything.
fn paint_marker(window: &mut Window, center: Point<Pixels>, inside: Hsla) {
    let at = |r: f32| Bounds::new(point(center.x - px(r), center.y - px(r)), size(px(r * 2.0), px(r * 2.0)));
    window.paint_quad(quad(
        at(9.0),
        Corners::all(px(9.0)),
        transparent_black(),
        Edges::all(px(1.0)),
        black().opacity(0.55),
        BorderStyle::Solid,
    ));
    window.paint_quad(quad(at(8.0), Corners::all(px(8.0)), inside, Edges::all(px(2.5)), white(), BorderStyle::Solid));
}

fn paint_wheel(bounds: Bounds<Pixels>, pick: &Pick, ui: &Ui, window: &mut Window) {
    let g = Wheel { size: f32::from(bounds.size.width).min(f32::from(bounds.size.height)) };
    let o = bounds.origin;
    let at = |(x, y): (f32, f32)| point(o.x + px(x), o.y + px(y));

    // The ring: thin wedges, each a little longer than its step so no seams show.
    let step = 360.0 / RING_SEGMENTS as f32;
    for k in 0..RING_SEGMENTS {
        let (a0, a1) = (k as f32 * step, (k as f32 + 1.0) * step + if k + 1 < RING_SEGMENTS { 0.8 } else { 0.0 });
        let mut pb = PathBuilder::fill();
        pb.move_to(at(g.polar(a0, g.outer())));
        pb.line_to(at(g.polar(a1, g.outer())));
        pb.line_to(at(g.polar(a1, g.inner())));
        pb.line_to(at(g.polar(a0, g.inner())));
        pb.close();
        if let Ok(path) = pb.build() {
            window.paint_path(path, hsla(Hsv::new(a0 + step / 2.0, 1.0, 1.0).to_color()));
        }
    }

    // The square: white to the pure hue across, transparent to black down.
    let (c, h) = (g.center(), g.half());
    let square = Bounds::new(at((c - h, c - h)), size(px(2.0 * h), px(2.0 * h)));
    let pure = hsla(Hsv::new(pick.hsv.h, 1.0, 1.0).to_color());
    window.paint_quad(fill(square, linear_gradient(90.0, linear_color_stop(white(), 0.0), linear_color_stop(pure, 1.0))));
    window.paint_quad(fill(square, linear_gradient(180.0, linear_color_stop(transparent_black(), 0.0), linear_color_stop(black(), 1.0))));
    window.paint_quad(quad(square, Corners::all(px(0.0)), transparent_black(), Edges::all(px(1.0)), ui.border, BorderStyle::Solid));

    paint_marker(window, at(g.polar(pick.hsv.h, (g.outer() + g.inner()) / 2.0)), pure);
    paint_marker(window, at(g.sv_point(pick.hsv.s, pick.hsv.v)), hsla(pick.color()));
}

/// The OKLCH disc at the aimed lightness `o.l`, with a marker at the
/// color. The bitmap is cached per lightness, size and backdrop; a replaced
/// one is dropped from the window's atlas.
fn paint_disc(
    bounds: Bounds<Pixels>,
    o: Oklch,
    color: Color,
    ui: &Ui,
    cache: &RefCell<Option<DiscImage>>,
    window: &mut Window,
    cx: &mut App,
) {
    let side = bounds.size.width.min(bounds.size.height);
    let bounds = Bounds::new(bounds.origin, size(side, side));
    let n = (f32::from(side) * window.scale_factor().min(2.0)).round().max(8.0) as u32;
    let backdrop: Rgba = ui.surface.into();
    let backdrop = [backdrop.r, backdrop.g, backdrop.b];
    let key = ((o.l.clamp(0.0, 1.0) * 1000.0).round() as u32, n, from_rgbf(backdrop).0);
    let mut cache = cache.borrow_mut();
    if cache.as_ref().is_none_or(|d| d.key != key) {
        let Some(buf) = image::RgbaImage::from_raw(n, n, disc_pixels(n, key.0 as f32 / 1000.0, backdrop)) else { return };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buf)]));
        if let Some(old) = cache.replace(DiscImage { key, image }) {
            cx.drop_image(old.image, Some(window));
        }
    }
    if let Some(d) = cache.as_ref()
        && let Err(e) = window.paint_image(bounds, bounds, Corners::all(side / 2.0), d.image.clone(), 0, false)
    {
        log::warn!("couldn't paint the OKLCH disc: {e}");
    }
    let g = Disc { size: f32::from(side) };
    window.paint_quad(quad(bounds, Corners::all(side / 2.0), transparent_black(), Edges::all(px(1.0)), ui.border, BorderStyle::Solid));
    let (x, y) = g.point(o.h, Oklch::from_color(color).c);
    paint_marker(window, point(bounds.origin.x + px(x), bounds.origin.y + px(y)), hsla(color));
}

/// The gradient bar of a slider: `n` segments, each a two-stop gradient
/// between sampled colors, with rounded ends and a border.
fn paint_track(window: &mut Window, bar: Bounds<Pixels>, ui: &Ui, color_at: impl Fn(f32) -> Color) {
    let n = TRACK_SEGMENTS;
    let stops: Vec<Hsla> = (0..=n).map(|k| hsla(color_at(k as f32 / n as f32))).collect();
    let r = bar.size.height / 2.0;
    for k in 0..n {
        let x0 = bar.left() + bar.size.width * (k as f32 / n as f32);
        let x1 = bar.left() + bar.size.width * ((k + 1) as f32 / n as f32);
        let mut q = fill(
            Bounds::from_corners(point(x0, bar.top()), point(x1, bar.bottom())),
            linear_gradient(90.0, linear_color_stop(stops[k], 0.0), linear_color_stop(stops[k + 1], 1.0)),
        );
        let (left, right) = (px(if k == 0 { f32::from(r) } else { 0.0 }), px(if k + 1 == n { f32::from(r) } else { 0.0 }));
        q.corner_radii = Corners { top_left: left, bottom_left: left, top_right: right, bottom_right: right };
        window.paint_quad(q);
    }
    window.paint_quad(quad(bar, Corners::all(r), transparent_black(), Edges::all(px(1.0)), ui.border, BorderStyle::Solid));
}

/// Mouse handling for a draggable area. Down starts a drag when it lands in
/// `bounds`; move and up are heard anywhere in the window while it lasts.
fn drag_listeners(window: &mut Window, picker: &Entity<ColorPicker>, bounds: Bounds<Pixels>, target: Target) {
    let p = picker.clone();
    window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && bounds.contains(&ev.position) {
            p.update(cx, |this, cx| this.begin_drag(target, bounds, ev.position, window, cx));
        }
    });
    let p = picker.clone();
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        p.update(cx, |this, cx| {
            if this.drag.is_some_and(|(k, _)| k.target() == target) {
                if ev.pressed_button == Some(MouseButton::Left) {
                    this.drag_to(ev.position, cx);
                } else {
                    // The button was released somewhere we couldn't hear.
                    this.end_drag(cx);
                }
            }
        });
    });
    let p = picker.clone();
    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
            p.update(cx, |this, cx| {
                if this.drag.is_some_and(|(k, _)| k.target() == target) {
                    this.end_drag(cx);
                }
            });
        }
    });
}

// ---- Rendering ----------------------------------------------------------------------

impl ColorPicker {
    /// A transparent tab stop over part of a painted area: it holds the
    /// focus ring and takes the arrow keys for `knob`.
    fn knob_stop(&self, knob: Knob, ui: Ui, cx: &mut Context<'_, Self>) -> gpui::Stateful<gpui::Div> {
        let id = knob.id();
        let el = div().id(id.clone()).absolute().on_key_down(cx.listener(move |p, ev: &KeyDownEvent, _, cx| p.on_knob_key(knob, ev, cx)));
        focus::tab(el, &id, ui, Ring::Outer, Scroller::Page)
    }

    /// The HSV wheel or the OKLCH disc (with its lightness bar), and the
    /// switch between them.
    fn area(&self, ui: Ui, cx: &mut Context<'_, Self>) -> gpui::AnyElement {
        let this = cx.entity();
        let pick = self.pick;
        let canvas_el = match self.area {
            Area::Hsv => {
                let this = this.clone();
                let g = Wheel { size: WHEEL };
                let (c, h) = (g.center(), g.half());
                let hsv = pick.hsv;
                div()
                    .relative()
                    .size(px(WHEEL))
                    .cursor(CursorStyle::Crosshair)
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                paint_wheel(bounds, &pick, &ui, window);
                                drag_listeners(window, &this, bounds, Target::Wheel);
                            },
                        )
                        .size_full(),
                    )
                    .child(
                        self.knob_stop(Knob::Ring, ui, cx)
                            .size_full()
                            .rounded_full()
                            .role(Role::Slider)
                            .aria_label("Hue")
                            .aria_numeric_value(f64::from(hsv.h))
                            .aria_min_numeric_value(0.0)
                            .aria_max_numeric_value(360.0)
                            .aria_numeric_value_step(1.0),
                    )
                    .child(
                        self.knob_stop(Knob::Square, ui, cx)
                            .left(px(c - h))
                            .top(px(c - h))
                            .size(px(2.0 * h))
                            .role(Role::Slider)
                            .aria_label("Saturation (left and right) and brightness (up and down)")
                            .aria_value(format!("saturation {}%, brightness {}%", number(hsv.s * 100.0, 0), number(hsv.v * 100.0, 0))),
                    )
                    .into_any_element()
            }
            Area::Oklch => {
                let o = self.aimed_oklch();
                let cache = self.disc.clone();
                let color = pick.color();
                let disc = {
                    let this = this.clone();
                    div()
                        .relative()
                        .size(px(WHEEL))
                        .cursor(CursorStyle::Crosshair)
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, cx| {
                                    paint_disc(bounds, o, color, &ui, &cache, window, cx);
                                    drag_listeners(window, &this, bounds, Target::Disc);
                                },
                            )
                            .size_full(),
                        )
                        .child(
                            self.knob_stop(Knob::Disc, ui, cx)
                                .size_full()
                                .rounded_full()
                                .role(Role::Slider)
                                .aria_label("Hue (left and right) and chroma (up and down)")
                                .aria_value(format!("hue {}°, chroma {}", number(o.h, 0), number(Oklch::from_color(color).c, 3))),
                        )
                };
                let bar = {
                    let this = this.clone();
                    div()
                        .relative()
                        .w(px(WHEEL))
                        .h(px(22.0))
                        .cursor(CursorStyle::PointingHand)
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, _| {
                                    let bar = Bounds::new(
                                        point(bounds.left() + px(THUMB), bounds.center().y - px(5.0)),
                                        size(bounds.size.width - px(2.0 * THUMB), px(10.0)),
                                    );
                                    paint_track(window, bar, &ui, |t| pick.with_oklch(Oklch { l: t, ..o }).color());
                                    paint_marker(window, point(bar.left() + bar.size.width * o.l, bar.center().y), hsla(color));
                                    drag_listeners(window, &this, bounds, Target::Lightness);
                                },
                            )
                            .size_full(),
                        )
                        .child(
                            self.knob_stop(Knob::Lightness, ui, cx)
                                .size_full()
                                .rounded_full()
                                .role(Role::Slider)
                                .aria_label("Lightness")
                                .aria_numeric_value(f64::from(o.l * 100.0))
                                .aria_min_numeric_value(0.0)
                                .aria_max_numeric_value(100.0)
                                .aria_numeric_value_step(1.0),
                        )
                };
                div().flex().flex_col().gap_2().child(disc).child(bar).into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .child(canvas_el)
            .child(segmented("picker-area", &Area::ALL, &self.area, ui, move |a, _, cx| this.update(cx, |p, cx| p.set_area(a, cx))))
            .into_any_element()
    }

    fn slider(&self, i: usize, ui: Ui, cx: &mut Context<'_, Self>) -> gpui::AnyElement {
        let (pick, model, this) = (self.pick, self.model, cx.entity());
        let ch = model.channels()[i];
        let value = pick.get(model, i);
        let t = (value / ch.max).clamp(0.0, 1.0);
        div()
            .relative()
            .flex_1()
            .h(px(22.0))
            .cursor(CursorStyle::PointingHand)
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let bar = Bounds::new(
                            point(bounds.left() + px(THUMB), bounds.center().y - px(5.0)),
                            size(bounds.size.width - px(2.0 * THUMB), px(10.0)),
                        );
                        paint_track(window, bar, &ui, |t| pick.track(model, i, t));
                        paint_marker(window, point(bar.left() + bar.size.width * t, bar.center().y), hsla(pick.color()));
                        drag_listeners(window, &this, bounds, Target::Slider(i));
                    },
                )
                .size_full(),
            )
            .child(
                self.knob_stop(Knob::Slider(i), ui, cx)
                    .size_full()
                    .rounded_full()
                    .role(Role::Slider)
                    .aria_label(SharedString::from(ch.label))
                    .aria_numeric_value(f64::from(value))
                    .aria_min_numeric_value(0.0)
                    .aria_max_numeric_value(f64::from(ch.max))
                    .aria_numeric_value_step(f64::from(ch.step)),
            )
            .into_any_element()
    }

    /// The contrast against each [`ContrastRef`]: a sample, the ratio, and
    /// a warning when it's below the minimum.
    fn contrast_row(&self, ui: Ui) -> Option<gpui::AnyElement> {
        if self.contrast.is_empty() {
            return None;
        }
        let cur = self.pick.color();
        let items = self.contrast.iter().map(|r| {
            let ratio = contrast_ratio(cur, r.color);
            let low = r.min.is_some_and(|m| ratio < m);
            let (text, bg) = if r.edited_is_text { (cur, r.color) } else { (r.color, cur) };
            let sample = div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(30.0))
                .h(px(20.0))
                .rounded_sm()
                .border_1()
                .border_color(ui.border)
                .bg(hsla(bg))
                .text_color(hsla(text))
                .text_xs()
                .child("Aa");
            let mut line = format!("{}:1 against {}", number(ratio, 1), r.label);
            if let Some(m) = r.min.filter(|_| low) {
                line.push_str(&format!(" · hard to read, aim for {}:1", number(m, 1)));
            }
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(sample)
                .child(div().text_xs().text_color(if low { ui.warning } else { ui.muted }).child(SharedString::from(line)))
        });
        Some(div().flex().flex_col().gap_1().children(items).into_any_element())
    }
}

impl Render for ColorPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // This view renders while the window lays out: its controls need the focus pool opened again.
        let scope = focus::enter(window);
        let ui = Ui::get(cx);
        let this = cx.entity();
        let model = self.model;
        let cur = self.pick.color();
        let chans = model.channels();
        for (n, ch) in self.nums.iter().zip(chans.iter()) {
            n.update(cx, |n, _| n.set_label(format!("{} channel", ch.label)));
        }

        let sliders: Vec<_> = (0..3)
            .map(|i| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(14.0)).text_sm().text_color(ui.muted).child(chans[i].label))
                    .child(self.slider(i, ui, cx))
                    .child(div().w(px(64.0)).child(self.nums[i].clone()))
                    .child(div().w(px(12.0)).text_xs().text_color(ui.muted).child(chans[i].unit))
            })
            .collect();
        let area = self.area(ui, cx);

        let swatch = |id: &'static str, label: &'static str, c: Color| {
            div()
                .id(id)
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .bg(hsla(c))
                .text_color(contrast_text(c))
                .child(label)
        };
        let before_after = div()
            .flex()
            .flex_1()
            .min_w_0()
            .h(px(34.0))
            .rounded_md()
            .border_1()
            .border_color(ui.border)
            .overflow_hidden()
            .child(focus::tab(
                swatch("picker-previous", "Previous", self.original)
                    .role(Role::Button)
                    .aria_label(format!("Go back to the previous color, {}", self.original))
                    .cursor_pointer()
                    .on_click(cx.listener(|p, _: &ClickEvent, _, cx| p.pick_color(p.original, cx))),
                &"picker-previous".into(),
                ui,
                Ring::Inner,
                Scroller::Page,
            ))
            .child(swatch("picker-new", "New", cur));
        let nudge = |id: &'static str, label: &'static str, dir: f32| {
            let this = this.clone();
            button(id, label, ButtonKind::Normal, ui, move |_, cx| this.update(cx, |p, cx| p.nudge_lightness(dir, cx)))
                .aria_label(format!("{label} (a step of OKLCH lightness)"))
        };
        let compare = div().flex().items_center().gap_2().child(before_after).child(nudge("picker-darker", "Darker", -1.0)).child(nudge(
            "picker-lighter",
            "Lighter",
            1.0,
        ));

        let controls = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(div().flex().items_center().gap_2().child(div().flex_1().child(self.any.clone())).child(button(
                "picker-paste",
                "Paste",
                ButtonKind::Normal,
                ui,
                {
                    let this = this.clone();
                    move |_, cx| this.update(cx, |p, cx| p.paste(cx))
                },
            )))
            .when_some(self.status.as_ref(), |d, s| {
                d.child(div().text_xs().text_color(if s.ok { ui.success } else { ui.danger }).child(SharedString::from(s.text.clone())))
            })
            .child(div().flex().child(segmented("picker-model", &Model::ALL, &model, ui, {
                let this = this.clone();
                move |m, _, cx| this.update(cx, |p, cx| p.set_model(m, cx))
            })))
            .children(sliders)
            .child(compare)
            .children(self.contrast_row(ui));

        let history = if self.history.is_empty() {
            div().text_xs().text_color(ui.muted).child("Colors you settle on show up here.").into_any_element()
        } else {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(5.0))
                .children(self.history.colors().iter().enumerate().map(|(i, &c)| {
                    let id = gpui::ElementId::from(("picker-history", i));
                    let swatch = div()
                        .id(id.clone())
                        .role(Role::Button)
                        .aria_label(format!("Use {c}"))
                        .size(px(22.0))
                        .rounded_sm()
                        .border_1()
                        .border_color(if c == cur { ui.accent } else { ui.border })
                        .bg(hsla(c))
                        .cursor_pointer()
                        .hover(|s| s.border_color(ui.text))
                        .on_click(cx.listener(move |p, _: &ClickEvent, _, cx| p.pick_color(c, cx)));
                    focus::tab(swatch, &id, ui, Ring::Outer, Scroller::Page)
                }))
                .into_any_element()
        };

        let picker = div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::copy_action))
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(ui.border)
            .bg(ui.surface)
            .child(div().flex().gap_4().child(area).child(controls))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().flex().items_center().justify_between().child(div().text_xs().text_color(ui.muted).child("RECENT")).when(
                        !self.history.is_empty(),
                        |d| {
                            d.child(focus::tab(
                                div()
                                    .id("picker-clear-history")
                                    .role(Role::Button)
                                    .aria_label("Clear the recent colors")
                                    .text_xs()
                                    .text_color(ui.muted)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(ui.danger))
                                    .child("Clear")
                                    .on_click(cx.listener(|p, _: &ClickEvent, _, cx| p.clear_history(cx))),
                                &"picker-clear-history".into(),
                                ui,
                                Ring::Outer,
                                Scroller::Page,
                            ))
                        },
                    ))
                    .child(history),
            );
        focus::leave(scope, window);
        picker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Wheel = Wheel { size: WHEEL };

    #[test]
    fn hue_runs_clockwise_from_the_top() {
        let c = W.center();
        assert!((W.hue_at(c, 0.0) - 0.0).abs() < 1e-3, "top");
        assert!((W.hue_at(WHEEL, c) - 90.0).abs() < 1e-3, "right");
        assert!((W.hue_at(c, WHEEL) - 180.0).abs() < 1e-3, "bottom");
        assert!((W.hue_at(0.0, c) - 270.0).abs() < 1e-3, "left");
        // polar() and hue_at() are inverses.
        for h in [0.0, 30.0, 123.0, 200.0, 359.0] {
            let (x, y) = W.polar(h, 80.0);
            assert!((W.hue_at(x, y) - h).abs() < 1e-3, "{h}");
        }
    }

    #[test]
    fn the_square_fits_inside_the_ring() {
        let corner = W.half() * SQRT_2;
        assert!(corner < W.inner(), "{corner} vs {}", W.inner());
        assert!(W.half() > 40.0, "big enough to aim in: {}", W.half());
    }

    #[test]
    fn hit_testing_tells_ring_from_square() {
        let c = W.center();
        assert_eq!(W.hit(c, c), Some(Region::Square));
        assert_eq!(W.hit(c + W.half() - 1.0, c - W.half() + 1.0), Some(Region::Square));
        let (x, y) = W.polar(45.0, (W.inner() + W.outer()) / 2.0);
        assert_eq!(W.hit(x, y), Some(Region::Ring));
        assert_eq!(W.hit(0.0, 0.0), None, "the corner of the box");
        // The gap between the square and the ring belongs to neither.
        let (x, y) = W.polar(0.0, W.inner() - SQUARE_GAP / 2.0 - 8.0);
        if W.hit(x, y).is_some() {
            assert_eq!(W.hit(x, y), Some(Region::Square));
        }
        let (x, y) = W.polar(0.0, W.half() + (W.inner() - 3.0 - W.half()) / 2.0 + 20.0);
        assert!(matches!(W.hit(x, y), None | Some(Region::Ring)));
    }

    #[test]
    fn sv_square_maps_corners() {
        let (c, h) = (W.center(), W.half());
        assert_eq!(W.sv_at(c - h, c - h), (0.0, 1.0), "top-left: white");
        assert_eq!(W.sv_at(c + h, c - h), (1.0, 1.0), "top-right: pure hue");
        assert_eq!(W.sv_at(c - h, c + h), (0.0, 0.0), "bottom: black");
        assert_eq!(W.sv_at(c + 500.0, c - 500.0), (1.0, 1.0), "clamped outside");
        // sv_point and sv_at are inverses.
        let (x, y) = W.sv_point(0.3, 0.8);
        let (s, v) = W.sv_at(x, y);
        assert!((s - 0.3).abs() < 1e-4 && (v - 0.8).abs() < 1e-4);
    }

    #[test]
    fn track_fraction_clamps() {
        assert_eq!(track_fraction(10.0, 10.0, 100.0), 0.0);
        assert_eq!(track_fraction(60.0, 10.0, 100.0), 0.5);
        assert_eq!(track_fraction(-50.0, 10.0, 100.0), 0.0);
        assert_eq!(track_fraction(900.0, 10.0, 100.0), 1.0);
        assert_eq!(track_fraction(5.0, 10.0, 0.0), 0.0);
    }

    #[test]
    fn channels_read_and_write_every_model() {
        let p = Pick::from_color(Color(0x336699));
        for (m, _) in Model::ALL {
            for i in 0..3 {
                // Writing back what was read changes nothing.
                let q = p.with(m, i, p.get(m, i));
                assert!(
                    q.color().rgb().0.abs_diff(p.color().rgb().0) <= 1 && q.color().rgb().2.abs_diff(p.color().rgb().2) <= 1,
                    "{m:?} {i}: {q:?}"
                );
            }
        }
        let p = Pick::from_color(Color(0x000000));
        assert_eq!(p.with(Model::Rgb, 0, 255.0).color(), Color(0xff0000));
        assert_eq!(p.with(Model::Rgb, 2, 128.0).color(), Color(0x000080));
        assert_eq!(Pick::from_color(Color(0xff0000)).with(Model::Hsv, 0, 120.0).color(), Color(0x00ff00));
        assert_eq!(Pick::from_color(Color(0xff0000)).with(Model::Hsl, 2, 100.0).color(), Color(0xffffff));
        assert_eq!(Pick::from_color(Color(0x808080)).with(Model::Oklch, 0, 100.0).color(), Color(0xffffff));
    }

    #[test]
    fn saturated_colors_survive_the_extremes() {
        // HSL lightness to white and back: the saturation comes back.
        let p = Pick::from_color(Color(0x3366cc));
        let (h, s) = (p.get(Model::Hsl, 0), p.get(Model::Hsl, 1));
        let white = p.with(Model::Hsl, 2, 100.0);
        assert_eq!(white.color(), Color(0xffffff));
        let back = white.with(Model::Hsl, 2, p.get(Model::Hsl, 2));
        assert!((back.get(Model::Hsl, 0) - h).abs() < 0.5 && (back.get(Model::Hsl, 1) - s).abs() < 0.5, "{back:?}");
        assert!(back.color().rgb().0.abs_diff(0x33) <= 1 && back.color().rgb().2.abs_diff(0xcc) <= 1, "{back:?}");

        // OKLCH lightness to black and back keeps chroma and hue.
        let o = p.get(Model::Oklch, 1);
        let oh = p.get(Model::Oklch, 2);
        let black = p.with(Model::Oklch, 0, 0.0);
        assert_eq!(black.color(), Color(0x000000));
        let back = black.with(Model::Oklch, 0, p.get(Model::Oklch, 0));
        assert!((back.get(Model::Oklch, 1) - o).abs() < 0.01 && (back.get(Model::Oklch, 2) - oh).abs() < 1.0, "{back:?}");

        // HSV value to zero and back keeps the saturation.
        let dark = p.with(Model::Hsv, 2, 0.0);
        assert_eq!(dark.color(), Color(0x000000));
        let back = dark.with(Model::Hsv, 2, p.get(Model::Hsv, 2));
        assert!((back.get(Model::Hsv, 1) - p.get(Model::Hsv, 1)).abs() < 0.01);
    }

    #[test]
    fn hue_is_kept_through_gray() {
        // Dragging saturation to 0 and the hue slider while gray still works.
        let p = Pick::from_color(Color(0x3366cc));
        let gray = p.with(Model::Hsv, 1, 0.0);
        assert_eq!(gray.color().rgb().0, gray.color().rgb().1);
        assert!((gray.get(Model::Hsv, 0) - p.get(Model::Hsv, 0)).abs() < 1e-3);
        let turned = gray.with(Model::Hsv, 0, 10.0);
        assert!((turned.get(Model::Hsv, 0) - 10.0).abs() < 1e-3);
        let colored = turned.with(Model::Hsv, 1, 100.0);
        assert!(colored.color().rgb().0 > colored.color().rgb().2, "hue 10 is reddish: {:?}", colored.color());
        // Same for the OKLCH hue slider on a gray.
        let g = Pick::from_color(Color(0x808080)).with(Model::Oklch, 2, 200.0);
        assert!((g.get(Model::Oklch, 2) - 200.0).abs() < 1e-3);
        let tinted = g.with(Model::Oklch, 1, 0.1);
        let o = Oklch::from_color(tinted.color());
        assert!((o.h - 200.0).abs() < 3.0, "{o:?}");
    }

    #[test]
    fn keys_move_knobs_in_steps() {
        assert_eq!(key_nudge("right", false), Some(Nudge::By(1.0, 0.0)));
        assert_eq!(key_nudge("down", true), Some(Nudge::By(0.0, -10.0)));
        assert_eq!(key_nudge("pageup", false), Some(Nudge::By(0.0, 10.0)));
        assert_eq!(key_nudge("home", false), Some(Nudge::Min));
        assert_eq!(key_nudge("tab", false), None, "Tab still moves focus");
        assert_eq!(key_nudge("a", false), None);

        assert_eq!(Model::Rgb.stepped(0, 100.0, Nudge::By(1.0, 0.0)), 101.0);
        assert_eq!(Model::Rgb.stepped(0, 250.0, Nudge::By(0.0, 10.0)), 255.0, "clamped");
        assert_eq!(Model::Rgb.stepped(0, 3.0, Nudge::By(-10.0, 0.0)), 0.0);
        assert_eq!(Model::Rgb.stepped(1, 3.0, Nudge::Max), 255.0);
        assert_eq!(Model::Hsv.stepped(0, 355.0, Nudge::By(10.0, 0.0)), 5.0, "hue wraps");
        assert_eq!(Model::Hsv.stepped(0, 2.0, Nudge::By(-10.0, 0.0)), 352.0);
        assert_eq!(Model::Hsv.stepped(0, 2.0, Nudge::Max), 359.0, "End isn't Home on a hue");
        assert!((Model::Oklch.stepped(1, 0.1, Nudge::By(0.0, 1.0)) - (0.1 + CHROMA_STEP)).abs() < 1e-6, "chroma steps are small");
    }

    #[test]
    fn disc_maps_hue_around_and_chroma_outwards() {
        let d = Disc { size: WHEEL };
        let c = d.center();
        assert!(d.hit(c, c) && d.hit(c, 1.0) && !d.hit(0.0, 0.0), "a circle, not its box");
        let (h, ch) = d.hc_at(c, c);
        assert_eq!(ch, 0.0, "gray in the middle (hue {h})");
        let (h, ch) = d.hc_at(c + d.radius(), c);
        assert!((h - 90.0).abs() < 1e-3 && (ch - DISC_CHROMA).abs() < 1e-5, "{h} {ch}");
        assert_eq!(d.hc_at(c + 500.0, c).1, DISC_CHROMA, "clamped to the rim");
        for (h, ch) in [(0.0, 0.1), (123.0, 0.05), (300.0, 0.3)] {
            let (x, y) = d.point(h, ch);
            let (h2, c2) = d.hc_at(x, y);
            assert!((h2 - h).abs() < 1e-2 && (c2 - ch).abs() < 1e-4, "{h} {ch} -> {h2} {c2}");
        }
    }

    #[test]
    fn disc_pixels_show_the_gamut() {
        let n = 64;
        let backdrop = [0.2, 0.2, 0.2];
        let px = disc_pixels(n, 0.7, backdrop);
        assert_eq!(px.len(), (n * n * 4) as usize);
        let at = |x: u32, y: u32| {
            let i = ((y * n + x) * 4) as usize;
            (px[i + 2], px[i + 1], px[i], px[i + 3])
        };
        // Corners are the backdrop, the middle a gray at the disc's lightness.
        assert_eq!(at(0, 0), (51, 51, 51, 255));
        let (r, g, b, _) = at(n / 2, n / 2);
        let o = Oklch::from_color(goatty_config::colorspace::rgb8(r, g, b));
        assert!((o.l - 0.7).abs() < 0.02 && o.c < 0.015, "nearly gray: {o:?}");
        // The rim at L 0.7 is far outside sRGB: faded towards the backdrop, so
        // darker than the same hue just inside the gamut.
        let rim = at(n / 2, 1);
        let inside = Oklch::new(0.7, Oklch::max_chroma(0.7, 0.0), 0.0).to_color().rgb();
        assert!(rim.0 < inside.0, "{rim:?} vs {inside:?}");
    }

    #[test]
    fn with_oklch_keeps_hue_and_maps_into_gamut() {
        let p = Pick::from_color(Color(0x808080));
        let q = p.with_oklch(Oklch { l: 0.6, c: 0.5, h: 140.0 });
        let o = Oklch::from_color(q.color());
        assert!((o.l - 0.6).abs() < 0.01 && (o.h - 140.0).abs() < 2.0 && o.c < 0.5, "{o:?}");
        // Black keeps the hue and chroma it was given.
        let black = p.with_oklch(Oklch { l: 0.0, c: 0.1, h: 200.0 });
        assert_eq!(black.color(), Color(0));
        assert!((black.oklch().h - 200.0).abs() < 1e-3 && (black.oklch().c - 0.1).abs() < 1e-6);
    }

    #[test]
    fn tracks_run_from_one_end_to_the_other() {
        let p = Pick::from_color(Color(0x3366cc));
        assert_eq!(p.track(Model::Rgb, 0, 0.0).rgb().0, 0);
        assert_eq!(p.track(Model::Rgb, 0, 1.0).rgb().0, 255);
        assert_eq!(p.track(Model::Hsl, 2, 0.0), Color(0x000000));
        assert_eq!(p.track(Model::Hsl, 2, 1.0), Color(0xffffff));
        assert_eq!(p.track(Model::Hsv, 1, 0.0).rgb().0, p.track(Model::Hsv, 1, 0.0).rgb().1, "no saturation: gray");
        assert_eq!(p.track(Model::Oklch, 0, 1.0), Color(0xffffff));
    }
}
