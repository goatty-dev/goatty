//! The terminal grid as one custom GPUI element.
//!
//! Per frame: lock the terminal once to copy a resolved [`Frame`], then build
//! a small number of draw calls:
//!   * one quad per run of identical non-default background,
//!   * one shaped text line per row *segment* (a maximal stretch of simple
//!     narrow cells, possibly mixing colors/weights via multiple `TextRun`s),
//!     shaped with `force_width = cell_width` so glyphs land on the grid,
//!   * wide / combining cells shaped individually at their exact cell.
//!
//! GPUI caches shaped lines across frames keyed by (text, runs), so unchanged
//! rows cost a hash lookup, not a reshape.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use goatty_term::{CellFlags, CursorKind, Frame, LinkSpan, Rgb, TermSize};
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, FocusHandle, Font, FontStyle, FontWeight, GlobalElementId,
    Hsla, InspectorElementId, IntoElement, LayoutId, Pixels, Point, ShapedLine, SharedString, Size, StrikethroughStyle, Style, TextRun,
    UnderlineStyle, Window, fill, point, px, relative, size,
};
use unicode_width::UnicodeWidthStr;

use super::box_drawing::{self, CellGrid, DpRect, PathDraw};
use super::glyphs::{Glyph, GlyphCache};
use super::stats::Stats;
use super::view::TerminalView;
use crate::session::Session;

pub fn hsla(c: Rgb) -> Hsla {
    gpui::rgb(c.to_u32()).into()
}

/// Geometry of the last painted grid, for mouse → cell mapping.
#[derive(Clone, Copy, Debug, Default)]
pub struct Geom {
    pub origin: Point<Pixels>,
    pub cell: Size<Pixels>,
    pub cols: usize,
    pub rows: usize,
    /// The cursor cell (col, row), kept while the cursor is hidden: where
    /// IME composition shows and the input method puts its popup.
    pub caret: (usize, usize),
    /// The part of the grid the pane shows, when another client sizes the
    /// session (#12) and the grid doesn't fill the pane or overflows it.
    pub visible: Option<Bounds<Pixels>>,
}

impl Geom {
    /// (col, row, right_half_of_cell)
    pub fn cell_at(&self, p: Point<Pixels>) -> (usize, usize, bool) {
        let x = ((p.x - self.origin.x) / self.cell.width).max(0.0);
        let y = ((p.y - self.origin.y) / self.cell.height).max(0.0);
        let col = (x as usize).min(self.cols.saturating_sub(1));
        let row = (y as usize).min(self.rows.saturating_sub(1));
        (col, row, x.fract() > 0.5)
    }

    /// Where IME composition text `cells` wide starts: at the caret, moved
    /// left so it stays inside the shown part of the grid when it's too
    /// long for the line.
    pub fn preedit_origin(&self, cells: usize) -> Point<Pixels> {
        let (col, row) = self.caret;
        let (first, end) = match self.visible {
            Some(b) => {
                let first = ((b.left() - self.origin.x) / self.cell.width).ceil().max(0.0) as usize;
                let end = ((b.right() - self.origin.x) / self.cell.width).floor().max(0.0) as usize;
                (first, end.min(self.cols))
            }
            None => (0, self.cols),
        };
        let col = col.min(end.saturating_sub(cells)).max(first);
        point(self.origin.x + self.cell.width * col as f32, self.origin.y + self.cell.height * row as f32)
    }

    /// Whether `p` is on the part of the grid the pane shows (not in the
    /// letterbox around it).
    pub fn in_grid(&self, p: Point<Pixels>) -> bool {
        self.visible.is_none_or(|b| b.contains(&p))
    }

    /// Selection auto-scroll: lines to scroll per tick for a drag at height
    /// `y`, positive towards history. 0 over the grid; past the top or
    /// bottom edge of what the pane shows it grows with the distance, one
    /// line per cell height.
    pub fn edge_scroll(&self, y: Pixels) -> i32 {
        const MAX: f32 = 10.0;
        let h = self.cell.height.max(px(1.0));
        let (top, bottom) = match self.visible {
            Some(b) => (b.top(), b.bottom()),
            None => (self.origin.y, self.origin.y + h * self.rows as f32),
        };
        if y < top {
            (1.0 + (top - y) / h).min(MAX) as i32
        } else if y >= bottom {
            -((1.0 + (y - bottom) / h).min(MAX) as i32)
        } else {
            0
        }
    }
}

pub struct TerminalElement {
    pub session: Arc<Session>,
    /// The view's token: only the view that owns the session's size sets
    /// it (two windows can show the same session, #215).
    pub token: u64,
    pub frame: Rc<RefCell<Frame>>,
    pub geom: Rc<StdCell<Geom>>,
    pub stats: Rc<RefCell<Stats>>,
    pub font: Font,
    pub font_size: Pixels,
    pub line_height_factor: f32,
    /// Space around the grid (top, right, bottom, left).
    pub padding: gpui::Edges<Pixels>,
    pub focused: bool,
    /// true: per-cell glyph cache (default). false: per-segment line shaping
    /// (supports ligatures, slower when many lines change).
    pub glyph_mode: bool,
    pub glyphs: Rc<RefCell<GlyphCache>>,
    /// Blink phase; when false the focused cursor is not drawn.
    pub cursor_visible: bool,
    /// The hovered link, underlined.
    pub link_spans: Vec<LinkSpan>,
    /// The pane, which takes text from the input method (IME, compose).
    pub view: Entity<TerminalView>,
    pub focus: FocusHandle,
    /// Text the input method is composing, drawn over the cursor until
    /// it's committed (empty: none).
    pub preedit: SharedString,
    pub preedit_fg: Rgb,
}

struct GlyphDraw {
    origin: Point<Pixels>,
    glyph: Glyph,
    color: Hsla,
}

enum Deco {
    Underline(Point<Pixels>, Pixels, UnderlineStyle),
    Strike(Point<Pixels>, Pixels, StrikethroughStyle),
}

/// Cursor bounds, shape, and the glyph under a block cursor (shaped, cached, or geometric).
struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorKind,
    line: Option<ShapedLine>,
    glyphs: Vec<GlyphDraw>,
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    paths: Vec<PathDraw>,
}

pub struct Layout {
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    fg_rects: Vec<(Bounds<Pixels>, Hsla)>,
    paths: Vec<PathDraw>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    glyphs: Vec<GlyphDraw>,
    decos: Vec<Deco>,
    cursor: Option<CursorLayout>,
    /// IME composition: its background and the shaped, underlined text.
    preedit: Option<(Bounds<Pixels>, Hsla, ShapedLine)>,
    cursor_color: Hsla,
    line_height: Pixels,
    started: Instant,
    /// Another client controls the session's size (#12): the grid's bounds,
    /// which may be smaller or larger than the pane. Painting is clipped to
    /// the pane and the space around the grid is shaded.
    letterbox: Option<Bounds<Pixels>>,
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    fg: Rgb,
    flags: CellFlags,
}

impl TerminalElement {
    fn run_font(&self, flags: CellFlags) -> Font {
        let mut f = self.font.clone();
        if flags.contains(CellFlags::BOLD) {
            f.weight = FontWeight::BOLD;
        }
        if flags.contains(CellFlags::ITALIC) {
            f.style = FontStyle::Italic;
        }
        f
    }

    fn text_run(&self, len: usize, st: RunStyle) -> TextRun {
        let color = hsla(st.fg);
        let underline = st.flags.contains(CellFlags::UNDERLINE).then(|| UnderlineStyle {
            thickness: px(1.0),
            color: Some(color),
            wavy: st.flags.contains(CellFlags::UNDERCURL),
        });
        let strikethrough = st.flags.contains(CellFlags::STRIKE).then(|| StrikethroughStyle { thickness: px(1.0), color: Some(color) });
        TextRun { len, font: self.run_font(st.flags), color, background_color: None, underline, strikethrough }
    }
}

/// Bits of CellFlags that affect glyph rendering (not WIDE/SPACER).
fn style_bits(f: CellFlags) -> CellFlags {
    let mut out = CellFlags::empty();
    for b in [CellFlags::BOLD, CellFlags::ITALIC, CellFlags::UNDERLINE, CellFlags::UNDERCURL, CellFlags::STRIKE] {
        if f.contains(b) {
            out |= b;
        }
    }
    out
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Layout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Layout {
        let started = Instant::now();
        let ts = window.text_system().clone();
        let font_id = ts.resolve_font(&self.font);
        let cell_w = ts.advance(font_id, self.font_size, 'm').map(|s| s.width).unwrap_or(self.font_size * 0.6);
        let line_height = (self.font_size * self.line_height_factor).round();
        let pad = self.padding;
        let inner_w = bounds.size.width - pad.left - pad.right;
        let inner_h = bounds.size.height - pad.top - pad.bottom;
        let cols = ((inner_w / cell_w).floor() as u16).max(2);
        let rows = ((inner_h / line_height).floor() as u16).max(1);
        let mut origin = bounds.origin + point(pad.left, pad.top);

        if self.session.view_differs(cols, rows) && self.session.may_resize(self.token) {
            let scale = window.scale_factor();
            self.session.resize(TermSize {
                cols,
                rows,
                cell_width: (f32::from(cell_w) * scale) as u16,
                cell_height: (f32::from(line_height) * scale) as u16,
            });
        }

        let mut frame = self.frame.borrow_mut();
        let snap_start = Instant::now();
        self.session.terminal.snapshot_into(&mut frame);
        self.session.overlay_predictions(&mut frame);
        self.stats.borrow_mut().snapshot_us += snap_start.elapsed().as_micros() as u64;
        if frame.display_offset > 0 {
            // Scrolled up: get what the daemon has above (or in between).
            self.session.fill_history();
        }
        let caret = frame.caret.unwrap_or(self.geom.get().caret);
        let caret = (caret.0.min(frame.cols.saturating_sub(1)), caret.1.min(frame.rows.saturating_sub(1)));
        // Another client sizes the session, or another window of this goatty
        // does (`may_resize`): the grid keeps its size here.
        let fixed =
            self.session.watching() || (!self.session.may_resize(self.token) && (frame.cols, frame.rows) != (cols as usize, rows as usize));
        let letterbox = fixed.then(|| {
            // A grid larger than the pane pans so the cursor's cell shows.
            let pan = |cells: usize, fits: u16, at: usize| (at + 1).saturating_sub(fits as usize).min(cells.saturating_sub(fits as usize));
            origin.x -= cell_w * pan(frame.cols, cols, caret.0) as f32;
            origin.y -= line_height * pan(frame.rows, rows, caret.1) as f32;
            Bounds::new(origin, size(cell_w * frame.cols as f32, line_height * frame.rows as f32))
        });
        let visible = letterbox.map(|grid| grid.intersect(&bounds));
        let geom = Geom { origin, cell: size(cell_w, line_height), cols: frame.cols, rows: frame.rows, caret, visible };
        self.geom.set(geom);

        let scale = window.scale_factor();
        let grid = CellGrid::new(origin, cell_w, line_height, self.font_size, scale);
        let default_bg = frame.background;
        let mut cache = self.glyphs.borrow_mut();
        cache.ensure(&ts, &self.font, self.font_size);
        let mut glyph_draws: Vec<GlyphDraw> = Vec::new();
        let mut decos: Vec<Deco> = Vec::new();
        let mut rects = Vec::new();
        let mut fg_dp_rects: Vec<DpRect> = Vec::new();
        let mut paths: Vec<PathDraw> = Vec::new();
        let mut lines = Vec::with_capacity(frame.rows * 2);
        let mut seg_text = String::with_capacity(frame.cols);
        let mut runs: Vec<TextRun> = Vec::new();

        for r in 0..frame.rows {
            let row = frame.row(r);
            let y = origin.y + line_height * r as f32;

            // Backgrounds: merge runs of equal non-default color.
            let mut c = 0;
            while c < row.len() {
                let bg = row[c].bg;
                let start = c;
                while c < row.len() && row[c].bg == bg {
                    c += 1;
                }
                if bg != default_bg {
                    rects.push((grid.span_bounds(start, c, r), hsla(bg)));
                }
            }

            if self.glyph_mode {
                let baseline = y + (line_height - cache.ascent - cache.descent) / 2.0 + cache.ascent;
                let mut deco_run: Option<(usize, Rgb, CellFlags)> = None;
                for (col, cell) in row.iter().enumerate() {
                    let deco_flags = deco_bits(cell.flags);
                    // Underline/strike runs (merged across cells).
                    match deco_run {
                        Some((_, fg, f)) if f == deco_flags && fg == cell.fg => {}
                        _ => {
                            if let Some((start, fg, f)) = deco_run.take() {
                                push_deco(&mut decos, &grid, baseline, &cache, start, col, fg, f);
                            }
                            if deco_flags != CellFlags::empty() {
                                deco_run = Some((col, cell.fg, deco_flags));
                            }
                        }
                    }
                    if cell.flags.contains(CellFlags::SPACER) || cell.ch == ' ' {
                        continue;
                    }
                    let color = hsla(cell.fg);
                    let extra = frame.extra(cell);
                    if extra.is_empty()
                        && !cell.flags.contains(CellFlags::WIDE)
                        && box_drawing::draw(cell.ch, grid.cell(col, r), &grid, color, &mut fg_dp_rects, &mut paths)
                    {
                        continue;
                    }
                    let x = origin.x + cell_w * col as f32;
                    if extra.is_empty() {
                        if let Some(g) = cache.simple(&ts, cell.ch, cell.flags) {
                            glyph_draws.push(GlyphDraw { origin: point(x + g.x, baseline), glyph: g, color });
                        }
                    } else {
                        let mut text = String::new();
                        text.push(cell.ch);
                        text.extend(extra);
                        for g in cache.complex(&ts, &text, cell.flags) {
                            glyph_draws.push(GlyphDraw { origin: point(x + g.x, baseline), glyph: *g, color });
                        }
                    }
                }
                if let Some((start, fg, f)) = deco_run.take() {
                    push_deco(&mut decos, &grid, baseline, &cache, start, row.len(), fg, f);
                }
                continue;
            }

            // Text segments.
            let baseline = y + (line_height - cache.ascent - cache.descent) / 2.0 + cache.ascent;
            let mut geom_deco_run: Option<(usize, Rgb, CellFlags)> = None;
            let mut seg_start = 0usize;
            let mut cur: Option<RunStyle> = None;
            let mut run_len = 0usize;
            let flush = |seg_text: &mut String,
                         runs: &mut Vec<TextRun>,
                         cur: &mut Option<RunStyle>,
                         run_len: &mut usize,
                         seg_start: usize,
                         lines: &mut Vec<(Point<Pixels>, ShapedLine)>| {
                if let Some(st) = cur.take() {
                    runs.push(self.text_run(*run_len, st));
                }
                *run_len = 0;
                // Drop trailing spaces that draw nothing.
                let trimmed = seg_text.trim_end_matches(' ').len();
                let mut cut = seg_text.len() - trimmed;
                while cut > 0 {
                    let last = runs.last_mut().unwrap();
                    if last.underline.is_some() || last.strikethrough.is_some() {
                        break;
                    }
                    let take = cut.min(last.len);
                    last.len -= take;
                    cut -= take;
                    if last.len == 0 {
                        runs.pop();
                    }
                    seg_text.truncate(seg_text.len() - take);
                }
                if !seg_text.is_empty() && !runs.is_empty() {
                    let line = ts.shape_line(SharedString::from(seg_text.clone()), self.font_size, runs, Some(cell_w));
                    lines.push((point(origin.x + cell_w * seg_start as f32, y), line));
                }
                seg_text.clear();
                runs.clear();
            };

            for (col, cell) in row.iter().enumerate() {
                if cell.flags.contains(CellFlags::SPACER) {
                    if let Some((start, fg, f)) = geom_deco_run.take() {
                        push_deco(&mut decos, &grid, baseline, &cache, start, col, fg, f);
                    }
                    flush(&mut seg_text, &mut runs, &mut cur, &mut run_len, seg_start, &mut lines);
                    continue;
                }
                let extra = frame.extra(cell);
                let wide = cell.flags.contains(CellFlags::WIDE);
                let st = RunStyle { fg: cell.fg, flags: style_bits(cell.flags) };
                if !wide && extra.is_empty() && box_drawing::is_custom(cell.ch) {
                    flush(&mut seg_text, &mut runs, &mut cur, &mut run_len, seg_start, &mut lines);
                    box_drawing::draw(cell.ch, grid.cell(col, r), &grid, hsla(cell.fg), &mut fg_dp_rects, &mut paths);
                    let deco_flags = deco_bits(cell.flags);
                    match geom_deco_run {
                        Some((_, fg, f)) if f == deco_flags && fg == cell.fg => {}
                        _ => {
                            if let Some((start, fg, f)) = geom_deco_run.take() {
                                push_deco(&mut decos, &grid, baseline, &cache, start, col, fg, f);
                            }
                            if deco_flags != CellFlags::empty() {
                                geom_deco_run = Some((col, cell.fg, deco_flags));
                            }
                        }
                    }
                    continue;
                }
                if let Some((start, fg, f)) = geom_deco_run.take() {
                    push_deco(&mut decos, &grid, baseline, &cache, start, col, fg, f);
                }
                if wide || !extra.is_empty() {
                    // Complex cell: shape alone, anchored at its column.
                    flush(&mut seg_text, &mut runs, &mut cur, &mut run_len, seg_start, &mut lines);
                    let mut text = String::new();
                    text.push(cell.ch);
                    text.extend(extra);
                    let run = self.text_run(text.len(), st);
                    let line = ts.shape_line(SharedString::from(text), self.font_size, &[run], None);
                    lines.push((point(origin.x + cell_w * col as f32, y), line));
                    continue;
                }
                if seg_text.is_empty() {
                    if cell.ch == ' ' {
                        continue; // leading blanks: start segment later
                    }
                    seg_start = col;
                } else if col != seg_start + seg_text.chars().count() {
                    // gap (skipped leading blanks never happen mid-seg, but be safe)
                    flush(&mut seg_text, &mut runs, &mut cur, &mut run_len, seg_start, &mut lines);
                    seg_start = col;
                }
                // A plain space can join the current run whatever its color.
                let joins = match cur {
                    Some(c) => c == st || (cell.ch == ' ' && st.flags == CellFlags::empty() && c.flags == CellFlags::empty()),
                    None => false,
                };
                if !joins {
                    if let Some(prev) = cur.take() {
                        runs.push(self.text_run(run_len, prev));
                    }
                    cur = Some(st);
                    run_len = 0;
                }
                seg_text.push(cell.ch);
                run_len += cell.ch.len_utf8();
            }
            if let Some((start, fg, f)) = geom_deco_run.take() {
                push_deco(&mut decos, &grid, baseline, &cache, start, row.len(), fg, f);
            }
            flush(&mut seg_text, &mut runs, &mut cur, &mut run_len, seg_start, &mut lines);
        }

        let fg_rects = fg_dp_rects.into_iter().map(|r| (r.to_bounds(scale), r.color)).collect();

        // Hovered link: underline in each stretch's text color.
        for span in &self.link_spans {
            if span.row >= frame.rows || span.cols.start >= frame.cols {
                continue;
            }
            let end = span.cols.end.min(frame.cols);
            let y = origin.y + line_height * span.row as f32;
            let baseline = y + (line_height - cache.ascent - cache.descent) / 2.0 + cache.ascent;
            let color = Some(hsla(frame.row(span.row)[span.cols.start].fg));
            decos.push(Deco::Underline(
                point(origin.x + cell_w * span.cols.start as f32, baseline + cache.descent * 0.618),
                cell_w * (end - span.cols.start) as f32,
                UnderlineStyle { thickness: px(1.0), color, wavy: false },
            ));
        }

        // Cursor.
        let cursor = frame.cursor.map(|cur| {
            let end_col = cur.col + if cur.wide { 2 } else { 1 };
            let b = grid.span_bounds(cur.col, end_col, cur.row);
            let mut cursor_glyphs = Vec::new();
            let mut cursor_dp_rects = Vec::new();
            let mut cursor_paths = Vec::new();
            let mut cursor_line = None;
            if self.focused && cur.shape == CursorKind::Block {
                let cell = frame.row(cur.row)[cur.col];
                if cell.ch != ' ' {
                    let extra = frame.extra(&cell);
                    let fg = hsla(frame.background);
                    if extra.is_empty()
                        && !cell.flags.contains(CellFlags::WIDE)
                        && box_drawing::draw(cell.ch, grid.cell(cur.col, cur.row), &grid, fg, &mut cursor_dp_rects, &mut cursor_paths)
                    {
                        // Painted geometrically over the block cursor.
                    } else if self.glyph_mode {
                        let y = origin.y + line_height * cur.row as f32;
                        let baseline = y + (line_height - cache.ascent - cache.descent) / 2.0 + cache.ascent;
                        let x = origin.x + cell_w * cur.col as f32;
                        let mut text = String::new();
                        text.push(cell.ch);
                        text.extend(extra);
                        for g in cache.complex(&ts, &text, cell.flags) {
                            cursor_glyphs.push(GlyphDraw { origin: point(x + g.x, baseline), glyph: *g, color: fg });
                        }
                    } else {
                        let mut text = String::new();
                        text.push(cell.ch);
                        text.extend(extra);
                        let st = RunStyle { fg: frame.background, flags: style_bits(cell.flags) };
                        let run = self.text_run(text.len(), st);
                        cursor_line = Some(ts.shape_line(SharedString::from(text), self.font_size, &[run], None));
                    }
                }
            }
            let cursor_rects = cursor_dp_rects.into_iter().map(|r| (r.to_bounds(scale), r.color)).collect();
            CursorLayout { bounds: b, shape: cur.shape, line: cursor_line, glyphs: cursor_glyphs, rects: cursor_rects, paths: cursor_paths }
        });

        let preedit = (!self.preedit.is_empty()).then(|| {
            let color = hsla(self.preedit_fg);
            let underline = Some(UnderlineStyle { thickness: px(1.0), color: Some(color), wavy: false });
            let run =
                TextRun { len: self.preedit.len(), font: self.font.clone(), color, background_color: None, underline, strikethrough: None };
            let line = ts.shape_line(self.preedit.clone(), self.font_size, &[run], None);
            let cells = self.preedit.width().max(1);
            let at = geom.preedit_origin(cells);
            // A composition longer than the line is cut off at the grid's edge.
            let width = line.width.max(cell_w * cells as f32).min(origin.x + cell_w * frame.cols as f32 - at.x);
            (Bounds::new(at, size(width, line_height)), hsla(frame.background), line)
        });

        self.stats.borrow_mut().shaped_segments += if self.glyph_mode { glyph_draws.len() } else { lines.len() } as u64;
        Layout {
            rects,
            fg_rects,
            paths,
            lines,
            glyphs: glyph_draws,
            decos,
            cursor,
            preedit,
            cursor_color: hsla(frame.cursor_color),
            line_height,
            started,
            letterbox,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut Layout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let paint_start = Instant::now();
        window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
        let mask = layout.letterbox.map(|_| ContentMask { bounds });
        window.with_content_mask(mask, |window| self.paint_grid(layout, window, cx));
        if let Some(grid) = layout.letterbox {
            paint_letterbox(bounds, grid, window);
        }
        let mut st = self.stats.borrow_mut();
        let pre = paint_start.duration_since(layout.started).as_micros() as u64;
        let paint = paint_start.elapsed().as_micros() as u64;
        st.frames += 1;
        st.prepaint_us += pre;
        st.paint_us += paint;
        st.max_frame_us = st.max_frame_us.max(pre + paint);
    }
}

/// Shade the pane outside the grid of a session another client sizes, and
/// draw the grid's right and bottom edges where they fall inside the pane.
fn paint_letterbox(pane: Bounds<Pixels>, grid: Bounds<Pixels>, window: &mut Window) {
    let shade = gpui::hsla(0., 0., 0.5, 0.12);
    let edge = gpui::hsla(0., 0., 0.5, 0.45);
    let (right, bottom) = (grid.right().min(pane.right()), grid.bottom().min(pane.bottom()));
    if grid.right() < pane.right() {
        window.paint_quad(fill(Bounds::from_corners(point(right, pane.top()), pane.bottom_right()), shade));
        window.paint_quad(fill(Bounds::new(point(right, pane.top()), size(px(1.), bottom - pane.top())), edge));
    }
    if grid.bottom() < pane.bottom() {
        window.paint_quad(fill(Bounds::from_corners(point(pane.left(), bottom), point(right, pane.bottom())), shade));
        window.paint_quad(fill(Bounds::new(point(pane.left(), bottom), size(right + px(1.) - pane.left(), px(1.))), edge));
    }
}

impl TerminalElement {
    fn paint_grid(&mut self, layout: &mut Layout, window: &mut Window, cx: &mut App) {
        for (b, color) in layout.rects.drain(..) {
            window.paint_quad(fill(b, color));
        }
        for (b, color) in layout.fg_rects.drain(..) {
            window.paint_quad(fill(b, color));
        }
        let paint_path = |p: PathDraw, window: &mut Window| {
            let mask = p.clip.map(|bounds| ContentMask { bounds });
            window.with_content_mask(mask, |window| window.paint_path(p.path, p.color));
        };
        for p in layout.paths.drain(..) {
            paint_path(p, window);
        }
        for (origin, line) in &layout.lines {
            let _ = line.paint(*origin, layout.line_height, gpui::TextAlign::Left, None, window, cx);
        }
        let font_size = self.font_size;
        let paint_glyph = |d: &GlyphDraw, window: &mut Window| {
            let _ = if d.glyph.emoji {
                window.paint_emoji(d.origin, d.glyph.font_id, d.glyph.id, font_size)
            } else {
                window.paint_glyph(d.origin, d.glyph.font_id, d.glyph.id, font_size, d.color)
            };
        };
        for d in &layout.glyphs {
            paint_glyph(d, window);
        }
        for d in layout.decos.drain(..) {
            match d {
                Deco::Underline(o, w, st) => window.paint_underline(o, w, &st),
                Deco::Strike(o, w, st) => window.paint_strikethrough(o, w, &st),
            }
        }
        // While composing, the composition stands where the cursor is.
        let cursor = layout.cursor.take().filter(|_| (self.cursor_visible || !self.focused) && layout.preedit.is_none());
        if let Some(cur) = cursor {
            let b = cur.bounds;
            let c = layout.cursor_color;
            let t = px(2.0);
            match (cur.shape, self.focused) {
                (CursorKind::Block, true) => {
                    window.paint_quad(fill(b, c));
                    for (r, color) in cur.rects {
                        window.paint_quad(fill(r, color));
                    }
                    for p in cur.paths {
                        paint_path(p, window);
                    }
                    if let Some(g) = cur.line {
                        let _ = g.paint(b.origin, layout.line_height, gpui::TextAlign::Left, None, window, cx);
                    }
                    for d in &cur.glyphs {
                        paint_glyph(d, window);
                    }
                }
                (CursorKind::Beam, _) => window.paint_quad(fill(Bounds::new(b.origin, size(t, b.size.height)), c)),
                (CursorKind::Underline, _) => {
                    window.paint_quad(fill(Bounds::new(point(b.origin.x, b.origin.y + b.size.height - t), size(b.size.width, t)), c))
                }
                (CursorKind::Block, false) => {
                    // Hollow block when unfocused.
                    let (o, s) = (b.origin, b.size);
                    window.paint_quad(fill(Bounds::new(o, size(s.width, px(1.))), c));
                    window.paint_quad(fill(Bounds::new(point(o.x, o.y + s.height - px(1.)), size(s.width, px(1.))), c));
                    window.paint_quad(fill(Bounds::new(o, size(px(1.), s.height)), c));
                    window.paint_quad(fill(Bounds::new(point(o.x + s.width - px(1.), o.y), size(px(1.), s.height)), c));
                }
            }
        }
        if let Some((bg, bg_color, line)) = layout.preedit.take() {
            window.with_content_mask(Some(ContentMask { bounds: bg }), |window| {
                window.paint_quad(fill(bg, bg_color));
                let _ = line.paint(bg.origin, layout.line_height, gpui::TextAlign::Left, None, window, cx);
            });
        }
    }
}

fn deco_bits(f: CellFlags) -> CellFlags {
    let mut out = CellFlags::empty();
    for b in [CellFlags::UNDERLINE, CellFlags::UNDERCURL, CellFlags::STRIKE] {
        if f.contains(b) {
            out |= b;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn push_deco(
    decos: &mut Vec<Deco>,
    grid: &CellGrid,
    baseline: Pixels,
    cache: &GlyphCache,
    start: usize,
    end: usize,
    fg: Rgb,
    f: CellFlags,
) {
    let color = Some(hsla(fg));
    let x0 = grid.col_x(start);
    let x1 = grid.col_x(end);
    let x = px(x0 as f32 / grid.scale);
    let w = px((x1 - x0) as f32 / grid.scale);
    if f.contains(CellFlags::UNDERLINE) {
        let wavy = f.contains(CellFlags::UNDERCURL);
        decos.push(Deco::Underline(point(x, baseline + cache.descent * 0.618), w, UnderlineStyle { thickness: px(1.0), color, wavy }));
    }
    if f.contains(CellFlags::STRIKE) {
        decos.push(Deco::Strike(point(x, baseline - cache.ascent * 0.32), w, StrikethroughStyle { thickness: px(1.0), color }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_scroll_grows_past_the_edges() {
        let g = Geom { origin: point(px(5.0), px(10.0)), cell: size(px(8.0), px(20.0)), cols: 80, rows: 10, caret: (0, 0), visible: None };
        assert_eq!(g.edge_scroll(px(10.0)), 0);
        assert_eq!(g.edge_scroll(px(209.0)), 0);
        assert_eq!(g.edge_scroll(px(9.0)), 1);
        assert_eq!(g.edge_scroll(px(-35.0)), 3);
        assert_eq!(g.edge_scroll(px(210.0)), -1);
        assert_eq!(g.edge_scroll(px(250.0)), -3);
        assert_eq!(g.edge_scroll(px(-10_000.0)), 10);
        assert_eq!(g.edge_scroll(px(10_000.0)), -10);
    }

    #[test]
    fn a_letterboxed_grid_scrolls_and_clicks_by_what_the_pane_shows() {
        // A 60-row grid panned up by 15 rows in a pane showing 24 of them.
        let (cell, rows) = (size(px(8.0), px(20.0)), 60);
        let origin = point(px(0.0), px(-300.0));
        let visible = Some(Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(480.0))));
        let g = Geom { origin, cell, cols: 40, rows, caret: (0, 44), visible };
        assert_eq!(g.edge_scroll(px(479.0)), 0);
        assert_eq!(g.edge_scroll(px(481.0)), -1);
        assert_eq!(g.edge_scroll(px(-1.0)), 1);
        assert!(g.in_grid(point(px(10.0), px(10.0))));
        assert!(!g.in_grid(point(px(330.0), px(10.0))), "right of a 40-column grid");
        assert!(Geom { visible: None, ..g }.in_grid(point(px(330.0), px(10.0))));
    }

    #[test]
    fn preedit_stays_inside_the_grid() {
        let g = Geom { origin: point(px(0.0), px(0.0)), cell: size(px(10.0), px(20.0)), cols: 80, rows: 10, caret: (5, 2), visible: None };
        assert_eq!(g.preedit_origin(3), point(px(50.0), px(40.0)));
        let g = Geom { caret: (78, 2), ..g };
        assert_eq!(g.preedit_origin(6), point(px(740.0), px(40.0)));
        assert_eq!(g.preedit_origin(200), point(px(0.0), px(40.0)));
    }

    #[test]
    fn preedit_stays_inside_what_the_pane_shows() {
        // An 80-column grid panned left by 10 columns in a pane showing 40.
        let visible = Some(Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(200.0))));
        let g = Geom { origin: point(px(-100.0), px(0.0)), cell: size(px(10.0), px(20.0)), cols: 80, rows: 10, caret: (48, 2), visible };
        assert_eq!(g.preedit_origin(4), point(px(360.0), px(40.0)), "columns 46..50 end at the right edge");
        assert_eq!(g.preedit_origin(200), point(px(0.0), px(40.0)), "never left of the first shown column");
    }
}
