//! Images in the terminal: the kitty graphics protocol (direct placements
//! and Unicode placeholders), sixel and iTerm2 inline images.
//!
//! rio-vt parses the protocols, keeps the placements and answers the
//! queries. This module is goatty's side of it:
//!   * [`Store`] keeps the pixels of every image that is placed somewhere,
//!     fed by rio-vt's upload queues (`RioEvent::UpdateGraphics`),
//!   * [`collect`] resolves what's visible into [`FrameImage`]s, in cells,
//!     for the renderer,
//!   * [`Replay`] writes the images and placements into snapshots, so a
//!     client that attaches or resyncs sees them too.
//!
//! The renderer only sees [`Image`] and [`FrameImage`]: plain data.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rio_graphics::{ColorType, GraphicData};
use rio_vt::ansi::graphics::{
    AtlasPlacement, KittyPlacement, OverlayViewport, UpdateQueues, atlas_overlay_geometry, kitty_display_size, kitty_overlay_geometry,
    resolve_source_rect,
};
use rio_vt::ansi::kitty_virtual::{IncompletePlacement, PLACEHOLDER, compute_run_geometry, resolve_virtual_placement};
use rio_vt::config::colors::AnsiColor;

use crate::terminal::Term;

/// Decoded pixels of one image, RGBA8 with straight alpha. `id` is unique
/// for the life of the process and changes whenever the pixels do, so a
/// renderer can cache its texture by it.
pub struct Image {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image#{} {}x{}", self.id, self.width, self.height)
    }
}

static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

impl Image {
    fn from_graphic(g: GraphicData) -> Self {
        let rgba = match g.color_type {
            ColorType::Rgba => g.pixels,
            ColorType::Rgb => {
                let mut out = Vec::with_capacity(g.pixels.len() / 3 * 4);
                for &[r, g, b] in g.pixels.as_chunks::<3>().0 {
                    out.extend_from_slice(&[r, g, b, 255]);
                }
                out
            }
        };
        Self { id: NEXT_IMAGE.fetch_add(1, Ordering::Relaxed), width: g.width as u32, height: g.height as u32, rgba }
    }
}

/// Where an image is drawn relative to the text (kitty's z-index rules;
/// sixel and placeholder images are part of the text layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageLayer {
    /// Under cells that have a background color (z < -2^30).
    BelowBackground,
    /// Over cell backgrounds, under the text (z < 0, sixel, placeholders).
    BelowText,
    /// Over the text (z >= 0).
    AboveText,
}

impl ImageLayer {
    fn of(z: i32) -> Self {
        if z < -1_073_741_824 {
            Self::BelowBackground
        } else if z < 0 {
            Self::BelowText
        } else {
            Self::AboveText
        }
    }
}

/// One visible piece of an image. Position and size are in cells from the
/// viewport's top-left corner (fractional: images aren't cell-aligned);
/// parts outside the viewport are for the renderer to clip.
#[derive(Debug, Clone)]
pub struct FrameImage {
    pub image: Arc<Image>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The part of the image shown, normalized: `[u0, v0, u1, v1]`.
    pub source: [f32; 4],
    pub layer: ImageLayer,
    /// Order within the layer (higher on top).
    pub z: i32,
}

/// The pixels of the images rio-vt placed, by its texture key
/// (`kitty_image_key` / `atlas_image_key`). Shared with the listener.
#[derive(Default)]
pub(crate) struct Store {
    images: HashMap<u64, Arc<Image>>,
}

impl Store {
    pub(crate) fn apply(&mut self, queues: UpdateQueues) {
        // Removals first: a retransmitted image is dropped and queued again
        // in the same batch.
        for key in queues.remove_queue {
            self.images.remove(&key);
        }
        for (id, data) in queues.pending_images {
            self.images.insert(rio_graphics::kitty_image_key(id), Arc::new(Image::from_graphic(data)));
        }
        for data in queues.pending {
            let key = rio_graphics::atlas_image_key(data.id.get());
            self.images.insert(key, Arc::new(Image::from_graphic(data)));
        }
    }

    fn kitty(&self, id: u32) -> Option<&Arc<Image>> {
        self.images.get(&rio_graphics::kitty_image_key(id))
    }

    /// Drop images neither screen refers to any more (rio-vt doesn't
    /// queue a removal for every way an image can go away).
    fn gc(&mut self, t: &Term) {
        let g = &t.graphics;
        let live = g.kitty_images.len()
            + g.kitty_inactive_screen.kitty_images.len()
            + g.atlas_key_refs.len()
            + g.kitty_inactive_screen.atlas_key_refs.len();
        if self.images.len() <= live {
            return;
        }
        self.images.retain(|key, _| {
            let kitty = u32::try_from(*key).ok();
            kitty.is_some_and(|id| g.kitty_images.contains_key(&id) || g.kitty_inactive_screen.kitty_images.contains_key(&id))
                || g.atlas_key_refs.contains_key(key)
                || g.kitty_inactive_screen.atlas_key_refs.contains_key(key)
        });
    }
}

/// The terminal's cell size in pixels, as rio-vt places images with it.
/// None until the terminal knows it (no pixel size, no images).
fn cell_px(t: &Term) -> Option<(f32, f32)> {
    let (w, h) = (t.graphics.cell_width, t.graphics.cell_height);
    (w >= 1.0 && h >= 1.0).then_some((w, h))
}

fn viewport(t: &Term, cw: f32, ch: f32) -> OverlayViewport {
    OverlayViewport {
        cell_width: cw,
        cell_height: ch,
        origin_x: 0.0,
        origin_y: 0.0,
        // rio-vt's absolute rows count the lines that left the scrollback too.
        history_size: t.grid.lines_evicted() as i64 + t.history_size() as i64,
        display_offset: t.display_offset() as i64,
        screen_lines: t.screen_lines() as i64,
    }
}

/// A placeholder cell seen while copying the frame: (viewport row, column,
/// fg, underline color, combining marks).
pub(crate) struct PlaceholderCell {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) fg: AnsiColor,
    pub(crate) underline: Option<AnsiColor>,
    pub(crate) marks: [char; 3],
    pub(crate) n_marks: u8,
}

pub(crate) fn is_placeholder(c: char) -> bool {
    c == PLACEHOLDER
}

/// Everything visible, into `out` (cleared first), ordered for painting.
/// `placeholders`: the Unicode placeholder cells of the frame, in order.
pub(crate) fn collect(t: &Term, store: &mut Store, placeholders: &[PlaceholderCell], out: &mut Vec<FrameImage>) {
    out.clear();
    store.gc(t);
    let Some((cw, ch)) = cell_px(t) else { return };
    let vp = viewport(t, cw, ch);
    let piece = |image: &Arc<Image>, x: f32, y: f32, w: f32, h: f32, source: [f32; 4], layer, z| FrameImage {
        image: image.clone(),
        x: x / cw,
        y: y / ch,
        width: w / cw,
        height: h / ch,
        source,
        layer,
        z,
    };

    // In a fixed order, so overlapping images on one z don't swap.
    let mut kitty: Vec<_> = t.graphics.kitty_placements.iter().collect();
    kitty.sort_unstable_by_key(|(k, _)| **k);
    for (_, p) in kitty {
        let Some(image) = store.kitty(p.image_id) else { continue };
        if let Some(g) = kitty_overlay_geometry(p, image.width as usize, image.height as usize, &vp) {
            out.push(piece(image, g.x, g.y, g.width, g.height, g.source_rect, ImageLayer::of(p.z_index), p.z_index));
        }
    }
    for p in &t.graphics.atlas_placements {
        let Some(image) = store.images.get(&p.image_key) else { continue };
        if let Some(g) = atlas_overlay_geometry(p, &vp) {
            out.push(piece(image, g.x, g.y, g.width, g.height, g.source_rect, ImageLayer::BelowText, 0));
        }
    }

    // Placeholder cells: runs of consecutive cells of one image row.
    let mut run: Option<(usize, usize, IncompletePlacement)> = None;
    let flush = |run: Option<(usize, usize, IncompletePlacement)>, out: &mut Vec<FrameImage>| {
        let Some((row, col, cell)) = run else { return };
        let run = cell.complete();
        let Some(v) = resolve_virtual_placement(&t.graphics.kitty_virtual_placements, run.image_id, run.placement_id) else { return };
        let Some(image) = store.kitty(run.image_id) else { return };
        let g = compute_run_geometry(
            &run,
            v.columns,
            v.rows,
            image.width,
            image.height,
            (v.x, v.y, v.width, v.height),
            cw,
            ch,
            0.0,
            0.0,
            row,
            col,
        );
        if let Some(g) = g {
            out.push(piece(image, g.x, g.y, g.width, g.height, g.source_rect, ImageLayer::BelowText, 0));
        }
    };
    let mut last: Option<(usize, usize)> = None;
    for cell in placeholders {
        let next = IncompletePlacement::from_cell(cell.fg, cell.underline, &cell.marks[..cell.n_marks as usize]);
        let adjacent = last == Some((cell.row, cell.col.wrapping_sub(1)));
        last = Some((cell.row, cell.col));
        if adjacent
            && let Some((_, _, r)) = &mut run
            && r.can_append(&next)
        {
            r.append();
            continue;
        }
        flush(run.take(), out);
        run = Some((cell.row, cell.col, next));
    }
    flush(run.take(), out);

    out.sort_by_key(|i| (i.layer, i.z));
}

// ---- snapshots ------------------------------------------------------------

/// Encoded image transmissions, by what they encode, so a client that
/// resyncs often doesn't make the daemon compress the same pixels again.
#[derive(Default)]
pub(crate) struct EncodeCache {
    entries: HashMap<CacheKey, Arc<str>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum CacheKey {
    Kitty(u32, rio_vt::time::Instant),
    Atlas(u64),
}

/// Encoded image bytes one snapshot carries at most (both screens); images
/// past it are left out, with their placements. The snapshot travels in one
/// frame (`goatty_proto::MAX_FRAME`, 64 MiB), and the rest of it (the rows)
/// needs room too.
const MAX_SNAPSHOT_IMAGES: usize = 40 * 1024 * 1024;

/// What a snapshot may still spend on images, and the transmissions it
/// encoded (they become the [`EncodeCache`] when it's done).
pub(crate) struct Budget {
    left: usize,
    used: HashMap<CacheKey, Arc<str>>,
}

impl Default for Budget {
    fn default() -> Self {
        Self { left: MAX_SNAPSHOT_IMAGES, used: HashMap::new() }
    }
}

impl Budget {
    /// The transmission for `key`, encoded now unless a snapshot did
    /// before; None when it doesn't fit in what's left.
    fn take(&mut self, cache: &EncodeCache, key: CacheKey, encode: impl FnOnce() -> String) -> Option<Arc<str>> {
        let command = match self.used.get(&key).or_else(|| cache.entries.get(&key)) {
            Some(c) => c.clone(),
            None => encode().into(),
        };
        // Kept even when it doesn't fit: the next snapshot won't encode it again.
        self.used.insert(key, command.clone());
        self.left = self.left.checked_sub(command.len())?;
        Some(command)
    }

    /// Keep this snapshot's transmissions for the next one.
    pub(crate) fn finish(&mut self, cache: &mut EncodeCache) {
        cache.entries = std::mem::take(&mut self.used);
    }
}

/// Ids for sixel and iTerm2 images replayed as kitty images, from here up,
/// skipping the ids of the screen's kitty images.
const ATLAS_REPLAY_ID: u32 = 0xFF00_0000;

/// A screen's images for a snapshot: the transmissions and virtual
/// placements (written before the rows), and the direct placements, written
/// while the row they start on is replayed so they land on that row in the
/// client too (in its scrollback if they're there).
#[derive(Default)]
pub(crate) struct Replay {
    pub(crate) prelude: String,
    /// Grid line → placements starting there (each a complete sequence).
    rows: BTreeMap<i32, Vec<Place>>,
}

struct Place {
    col: usize,
    command: String,
}

impl Replay {
    /// The active screen's images. `first_line`: the topmost grid line the
    /// snapshot replays; placements starting above it are cropped to it.
    pub(crate) fn build(t: &Term, store: &Store, cache: &EncodeCache, budget: &mut Budget, first_line: i32) -> Self {
        let mut replay = Replay::default();
        let Some((cw, ch)) = cell_px(t) else { return replay };
        let g = &t.graphics;
        if g.kitty_images.is_empty() && g.atlas_placements.is_empty() {
            return replay;
        }
        let mut sent = HashSet::new();

        // Placed images first, then the ones a program sent to place later.
        let mut placed: Vec<u32> = g.kitty_placements.keys().chain(g.kitty_virtual_placements.keys()).map(|(id, _)| *id).collect();
        placed.sort_unstable();
        placed.dedup();
        let mut unplaced: Vec<u32> = g.kitty_images.keys().filter(|id| placed.binary_search(id).is_err()).copied().collect();
        unplaced.sort_unstable();
        for id in placed.into_iter().chain(unplaced) {
            let Some(stored) = g.kitty_images.get(&id) else { continue };
            let data = &stored.data;
            let format = if data.color_type == ColorType::Rgba { 32 } else { 24 };
            let key = CacheKey::Kitty(id, stored.transmission_time);
            let Some(command) = budget.take(cache, key, || transmit(id, format, data.width, data.height, &data.pixels)) else { continue };
            replay.prelude.push_str(&command);
            sent.insert(id);
        }
        for v in g.kitty_virtual_placements.values().filter(|v| sent.contains(&v.image_id)) {
            let _ = write!(replay.prelude, "\x1b_Ga=p,U=1,q=2,i={}", v.image_id);
            if v.placement_id != 0 {
                let _ = write!(replay.prelude, ",p={}", v.placement_id);
            }
            let _ = write!(replay.prelude, ",c={},r={},x={},y={},w={},h={}\x1b\\", v.columns, v.rows, v.x, v.y, v.width, v.height);
        }

        let base = t.grid.lines_evicted() as i64 + t.history_size() as i64;
        for p in g.kitty_placements.values().filter(|p| sent.contains(&p.image_id)) {
            let Some(stored) = g.kitty_images.get(&p.image_id) else { continue };
            let line = (p.dest_row - base).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            if let Some((line, command)) = place_kitty(p, stored.data.width, stored.data.height, line, first_line, cw, ch) {
                replay.rows.entry(line).or_default().push(Place { col: p.dest_col, command });
            }
        }

        let mut atlas_ids: HashMap<u64, Option<u32>> = HashMap::new();
        let mut next_id = ATLAS_REPLAY_ID;
        for p in &g.atlas_placements {
            let Some(image) = store.images.get(&p.image_key) else { continue };
            let id = *atlas_ids.entry(p.image_key).or_insert_with(|| {
                let key = CacheKey::Atlas(image.id);
                // Encoded with the base id (so the cache holds one form); given its own.
                let command =
                    budget.take(cache, key, || transmit(ATLAS_REPLAY_ID, 32, image.width as usize, image.height as usize, &image.rgba))?;
                while g.kitty_images.contains_key(&next_id) {
                    next_id = next_id.wrapping_add(1).max(ATLAS_REPLAY_ID);
                }
                let id = next_id;
                next_id = next_id.wrapping_add(1).max(ATLAS_REPLAY_ID);
                replay.prelude.push_str(&command.replacen(&format!("i={ATLAS_REPLAY_ID},"), &format!("i={id},"), 1));
                Some(id)
            });
            let Some(id) = id else { continue };
            let line = (p.abs_row - base).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            if let Some((line, command)) = place_atlas(p, image, id, line, first_line, cw, ch) {
                replay.rows.entry(line).or_default().push(Place { col: p.col, command });
            }
        }
        replay
    }

    /// The placements starting on grid line `line`, written with the
    /// client's cursor on that line (anywhere in it, even at a pending
    /// wrap: the cursor is saved and restored around them).
    pub(crate) fn write_row(&mut self, line: i32, out: &mut String) {
        if let Some(places) = self.rows.remove(&line) {
            for p in places {
                let _ = write!(out, "\x1b7\x1b[{}G{}\x1b8", p.col + 1, p.command);
            }
        }
    }

    /// The placements on screen rows the snapshot didn't write (empty rows
    /// at the bottom), positioned absolutely. Rows outside the screen are
    /// dropped.
    pub(crate) fn write_rest(&mut self, screen_lines: usize, out: &mut String) {
        for (line, places) in std::mem::take(&mut self.rows) {
            if line < 0 || line as usize >= screen_lines {
                continue;
            }
            for p in places {
                let _ = write!(out, "\x1b7\x1b[{};{}H{}\x1b8", line + 1, p.col + 1, p.command);
            }
        }
    }
}

/// `a=t` for an image: zlib-compressed pixels, base64, one sequence.
fn transmit(id: u32, format: u8, width: usize, height: usize, pixels: &[u8]) -> String {
    let mut z = flate2::write::ZlibEncoder::new(Vec::with_capacity(pixels.len() / 4), flate2::Compression::fast());
    let _ = z.write_all(pixels);
    let compressed = z.finish().unwrap_or_default();
    let mut out = format!("\x1b_Ga=t,q=2,i={id},f={format},s={width},v={height},o=z;");
    base64_into(&compressed, &mut out);
    out.push_str("\x1b\\");
    out
}

/// The `a=p` for a direct placement whose top row is grid line `line`,
/// moved down to `first_line` (and cropped) when it starts above it.
fn place_kitty(p: &KittyPlacement, image_w: usize, image_h: usize, line: i32, first_line: i32, cw: f32, ch: f32) -> Option<(i32, String)> {
    let skip = first_line.saturating_sub(line).max(0) as u32;
    if p.rows == 0 || skip >= p.rows {
        return None;
    }
    let (mut x, mut y, mut w, mut h) = (p.source_x, p.source_y, p.source_width, p.source_height);
    let (mut rows, mut y_off) = (p.requested_rows, p.cell_y_offset);
    if skip > 0 {
        let (sx, sy, sw, sh) = resolve_source_rect(x, y, w, h, image_w, image_h)?;
        let (_, dh) = kitty_display_size(sw, sh, p.requested_columns, p.requested_rows, cw.round() as usize, ch.round() as usize);
        // The display pixels above the first line, in source pixels.
        let hidden = (skip as f32 * ch - p.cell_y_offset as f32).max(0.0);
        let cut = ((hidden * sh as f32 / dh.max(1) as f32).round() as usize).min(sh.saturating_sub(1));
        (x, y, w, h) = (sx as u32, (sy + cut) as u32, sw as u32, (sh - cut) as u32);
        rows = rows.saturating_sub(skip);
        y_off = 0;
    }
    let mut cmd = format!("\x1b_Ga=p,q=2,C=1,i={},x={x},y={y},w={w},h={h}", p.image_id);
    // Placement ids from 2^31 up are rio-vt's own (the program sent none).
    if p.placement_id != 0 && p.placement_id < 0x8000_0000 {
        let _ = write!(cmd, ",p={}", p.placement_id);
    }
    if p.requested_columns > 0 {
        let _ = write!(cmd, ",c={}", p.requested_columns);
    }
    if rows > 0 {
        let _ = write!(cmd, ",r={rows}");
    }
    let _ = write!(cmd, ",X={},Y={y_off},z={}\x1b\\", p.cell_x_offset, p.z_index);
    Some((line.max(first_line), cmd))
}

/// A sixel/iTerm2 placement replayed as a kitty placement of image `id`.
fn place_atlas(p: &AtlasPlacement, image: &Image, id: u32, line: i32, first_line: i32, cw: f32, ch: f32) -> Option<(i32, String)> {
    let skip = first_line.saturating_sub(line).max(0) as usize;
    if p.rows == 0 || skip >= p.rows || p.total_width == 0 || p.total_height == 0 {
        return None;
    }
    // The placement's crop is in display pixels at insert time; the
    // image's own pixels may be scaled from those.
    let sx = image.width as f32 / p.total_width as f32;
    let sy = image.height as f32 / p.total_height as f32;
    let top = p.src_y as f32 + skip as f32 * p.insert_cell_h as f32;
    let bottom = (p.src_y + p.src_height) as f32;
    if bottom <= top {
        return None;
    }
    let (x, y) = ((p.src_x as f32 * sx).round(), (top * sy).round());
    let (w, h) = ((p.src_width as f32 * sx).round().max(1.0), ((bottom - top) * sy).round().max(1.0));
    let rows = p.rows - skip;
    // Shown at the size it was inserted at, scaled to the live cell size.
    let display_w = p.src_width as f32 * cw / p.insert_cell_w.max(1) as f32;
    let display_h = (bottom - top) * ch / p.insert_cell_h.max(1) as f32;
    let native = (display_w - w).abs() < 1.0 && (display_h - h).abs() < 1.0;
    // z=-1: under the text, like the sixel it was.
    let mut cmd = format!("\x1b_Ga=p,q=2,C=1,z=-1,i={id},x={x},y={y},w={w},h={h}");
    if !native {
        let _ = write!(cmd, ",c={},r={rows}", p.columns);
    }
    cmd.push_str("\x1b\\");
    Some((line.max(first_line), cmd))
}

/// Standard base64 with padding, appended to `out`.
pub(crate) fn base64_into(data: &[u8], out: &mut String) {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode_string(data, out);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Frame, Palette, TermSize, Terminal};

    /// 1x1 red PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8/5+hHgAHggJ/PchI7wAAAABJRU5ErkJggg==";

    fn term(answer: bool) -> (Terminal, crate::TerminalChannels) {
        Terminal::new(TermSize { cols: 20, rows: 6, cell_width: 10, cell_height: 20 }, answer, Arc::new(Palette::default()))
    }

    fn frame(t: &Terminal) -> Frame {
        let mut f = Frame::default();
        t.snapshot_into(&mut f);
        f
    }

    fn rgba(w: usize, h: usize, px: [u8; 4]) -> String {
        let mut out = String::new();
        base64_into(&px.repeat(w * h), &mut out);
        out
    }

    /// (x, y, width, height) in cells, rounded to hundredths.
    fn rect(i: &FrameImage) -> (f32, f32, f32, f32) {
        let r = |v: f32| (v * 100.0).round() / 100.0;
        (r(i.x), r(i.y), r(i.width), r(i.height))
    }

    #[test]
    fn a_direct_placement_shows_at_the_cursor_and_moves_it() {
        let (t, _ch) = term(false);
        t.feed(format!("ab\x1b_Ga=T,f=32,s=20,v=40,i=7;{}\x1b\\", rgba(20, 40, [1, 2, 3, 255])).as_bytes());
        let f = frame(&t);
        assert_eq!(f.images.len(), 1);
        let img = &f.images[0];
        assert_eq!(rect(img), (2.0, 0.0, 2.0, 2.0), "20x40 px in 10x20 px cells");
        assert_eq!((img.image.width, img.image.height), (20, 40));
        assert_eq!(&img.image.rgba[..4], &[1, 2, 3, 255]);
        assert_eq!(img.layer, ImageLayer::AboveText);
        // C=0: the cursor goes below the image... rio-vt leaves it on the last row, after the image.
        assert_eq!(f.cursor.map(|c| c.row), Some(1));
    }

    #[test]
    fn png_is_decoded_and_z_picks_the_layer() {
        let (t, _ch) = term(false);
        t.feed(format!("\x1b_Ga=T,f=100,i=1,c=3,r=2,z=-5,C=1;{PNG}\x1b\\").as_bytes());
        let f = frame(&t);
        assert_eq!(f.images.len(), 1);
        assert_eq!(rect(&f.images[0]), (0.0, 0.0, 3.0, 2.0), "stretched to c x r cells");
        assert_eq!((f.images[0].image.width, f.images[0].image.height), (1, 1));
        assert_eq!(f.images[0].layer, ImageLayer::BelowText);
        assert_eq!(f.cursor.map(|c| (c.col, c.row)), Some((0, 0)), "C=1 leaves the cursor");
    }

    #[test]
    fn the_daemon_answers_the_support_probe_and_the_mirror_does_not() {
        let probe = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
        let (t, ch) = term(true);
        t.feed(probe);
        let replies: Vec<u8> = ch.replies.try_iter().flatten().collect();
        assert_eq!(replies, b"\x1b_Gi=31;OK\x1b\\");
        let (m, ch) = term(false);
        m.feed(probe);
        assert_eq!(ch.replies.try_iter().count(), 0);
    }

    #[test]
    fn the_text_area_size_in_pixels_is_reported() {
        let (t, ch) = term(true);
        t.feed(b"\x1b[14t\x1b[16t");
        let replies: Vec<u8> = ch.replies.try_iter().flatten().collect();
        assert_eq!(String::from_utf8(replies).unwrap(), "\x1b[4;120;200t\x1b[6;20;10t");
    }

    #[test]
    fn placeholder_cells_draw_the_image_not_text() {
        let (t, _ch) = term(false);
        // A virtual placement 2 cells wide, 1 high, of image 42 (fg 42).
        t.feed(format!("\x1b_Ga=T,U=1,q=2,f=32,s=20,v=20,i=42,c=2,r=1;{}\x1b\\", rgba(20, 20, [9, 9, 9, 255])).as_bytes());
        t.feed("x\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39my".as_bytes());
        let f = frame(&t);
        let text: String = f.row(0).iter().take(4).map(|c| c.ch).collect();
        assert_eq!(text, "x  y", "the placeholders are blank cells");
        assert_eq!(f.images.len(), 1, "one run");
        // 2x1 cells of 10x20 px: the square image fills the box.
        assert_eq!(rect(&f.images[0]), (1.0, 0.0, 2.0, 1.0));
        // In a 4x1 box it's fitted and centred.
        t.feed(b"\x1b_Ga=p,U=1,q=2,i=42,c=4,r=1\x1b\\");
        // The two cells printed show the box's left half: padding, then the image.
        assert_eq!(rect(&frame(&t).images[0]), (2.0, 0.0, 1.0, 1.0));
        assert_eq!(f.images[0].layer, ImageLayer::BelowText);
    }

    #[test]
    fn sixel_shows_as_an_image() {
        let (t, _ch) = term(false);
        // 2 columns x 6 rows of color 1.
        t.feed(b"\x1bPq#1;2;100;0;0#1~~\x1b\\");
        let f = frame(&t);
        assert_eq!(f.images.len(), 1);
        assert_eq!((f.images[0].image.width, f.images[0].image.height), (2, 6));
        assert_eq!(f.images[0].image.rgba[..3], [255, 0, 0]);
    }

    /// What a fresh client shows after attaching (the snapshot fed into it).
    fn attached(t: &Terminal) -> Frame {
        let (c, _ch) = term(false);
        c.feed(&t.reattach_snapshot());
        frame(&c)
    }

    fn rects(f: &Frame) -> Vec<(f32, f32, f32, f32)> {
        f.images.iter().map(rect).collect()
    }

    #[test]
    fn an_attaching_client_gets_the_images() {
        let (t, _ch) = term(true);
        t.feed(b"line 1\r\nline 2\r\n  ");
        t.feed(format!("\x1b_Ga=T,f=32,s=20,v=40,i=7,p=3;{}\x1b\\ after", rgba(20, 40, [5, 6, 7, 255])).as_bytes());
        t.feed(format!("\r\n\x1b_Ga=T,U=1,q=2,f=32,s=10,v=20,i=42,c=1,r=1;{}\x1b\\", rgba(10, 20, [1, 1, 1, 255])).as_bytes());
        t.feed("\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\x1b[39m".as_bytes());
        let want = frame(&t);
        assert_eq!(want.images.len(), 2);
        let got = attached(&t);
        assert_eq!(rects(&got), rects(&want));
        assert_eq!(got.images[0].image.rgba[..4], want.images[0].image.rgba[..4]);
        let text = |f: &Frame| f.cells.iter().map(|c| c.ch).collect::<String>();
        assert_eq!(text(&got), text(&want), "the placement didn't disturb the text");
        assert_eq!(got.cursor.map(|c| (c.col, c.row)), want.cursor.map(|c| (c.col, c.row)));
    }

    #[test]
    fn images_in_the_scrollback_come_back_on_their_line() {
        let (t, _ch) = term(true);
        t.feed(format!("top\r\n\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [1, 2, 3, 255])).as_bytes());
        for i in 0..10 {
            t.feed(format!("\r\nrow {i}").as_bytes());
        }
        assert!(frame(&t).images.is_empty(), "scrolled out of view");
        let (c, _ch) = term(false);
        c.feed(&t.reattach_snapshot());
        assert!(frame(&c).images.is_empty());
        // Scroll both up to the image.
        t.scroll(10);
        c.scroll(10);
        let (want, got) = (frame(&t), frame(&c));
        assert_eq!(want.images.len(), 1);
        assert_eq!(rects(&got), rects(&want));
    }

    #[test]
    fn a_resync_replaces_the_clients_images() {
        let (t, _ch) = term(true);
        let (c, _cch) = term(false);
        let first = format!("\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [1, 1, 1, 255]));
        t.feed(first.as_bytes());
        c.feed(first.as_bytes());
        // The client misses a delete and a new image.
        t.feed(format!("\x1b_Ga=d,d=I,i=1\x1b\\\x1b[3;5H\x1b_Ga=T,f=32,s=30,v=20,i=2;{}\x1b\\", rgba(30, 20, [2, 2, 2, 255])).as_bytes());
        c.feed(&t.resync_snapshot());
        let (want, got) = (frame(&t), frame(&c));
        assert_eq!(want.images.len(), 1);
        assert_eq!(rects(&got), rects(&want));
        assert_eq!(got.images[0].image.rgba[..4], [2, 2, 2, 255]);
    }

    #[test]
    fn a_resync_keeps_an_image_cut_off_at_the_top() {
        let (t, _ch) = term(true);
        t.feed(format!("\x1b[4;1H\x1b_Ga=T,f=32,s=10,v=60,i=1;{}\x1b\\", rgba(10, 60, [3, 3, 3, 255])).as_bytes());
        t.feed(b"\r\n\r\n\r\n\r\n");
        let want = frame(&t);
        assert_eq!(rects(&want), [(0.0, -1.0, 1.0, 3.0)], "one of its three rows above the screen");
        let (c, _ch) = term(false);
        c.feed(&t.resync_snapshot());
        let got = frame(&c);
        assert_eq!(rects(&got), [(0.0, 0.0, 1.0, 2.0)], "the visible part");
        assert_eq!(got.images[0].source, [0.0, 1.0 / 3.0, 1.0, 1.0]);
    }

    #[test]
    fn the_alternate_screen_keeps_its_images_through_a_snapshot() {
        let (t, _ch) = term(true);
        t.feed(format!("\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [1, 1, 1, 255])).as_bytes());
        t.feed(format!("\x1b[?1049h\x1b[2;2H\x1b_Ga=T,f=32,s=10,v=20,i=2;{}\x1b\\", rgba(10, 20, [2, 2, 2, 255])).as_bytes());
        t.feed(b"\x1b[3;3H\x1bPq#1;2;0;100;0#1~~\x1b\\");
        let want = frame(&t);
        assert_eq!(want.images.len(), 2, "the alt screen's kitty image and sixel");
        let got = attached(&t);
        assert_eq!(rects(&got), rects(&want));
        assert_eq!(rects(&frame(&t)), rects(&want), "the session's alt screen still has both");
        // Leaving the alternate screen shows the primary screen's image.
        t.feed(b"\x1b[?1049l");
        let (c, _ch) = term(false);
        c.feed(&t.reattach_snapshot());
        assert_eq!(rects(&frame(&c)), rects(&frame(&t)));
        assert_eq!(frame(&c).images[0].image.rgba[..4], [1, 1, 1, 255]);
    }

    /// Switching screens for the snapshot makes rio-vt queue the other
    /// screen's image (same id, other pixels): the snapshot must not hold
    /// the image lock then (the listener takes it).
    #[test]
    fn a_snapshot_with_one_id_on_both_screens_finishes() {
        let (t, _ch) = term(true);
        t.feed(format!("\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [1, 1, 1, 255])).as_bytes());
        t.feed(format!("\x1b[?1049h\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [2, 2, 2, 255])).as_bytes());
        let got = attached(&t);
        assert_eq!(got.images[0].image.rgba[..4], [2, 2, 2, 255]);
        assert_eq!(frame(&t).images[0].image.rgba[..4], [2, 2, 2, 255]);
    }

    #[test]
    fn an_image_placed_after_attaching_shows() {
        let (t, _ch) = term(true);
        t.feed(format!("\x1b_Ga=t,q=2,f=32,s=10,v=20,i=5;{}\x1b\\", rgba(10, 20, [5, 5, 5, 255])).as_bytes());
        let (c, _cch) = term(false);
        c.feed(&t.reattach_snapshot());
        let place = b"\x1b_Ga=p,q=2,i=5\x1b\\";
        t.feed(place);
        c.feed(place);
        let (want, got) = (frame(&t), frame(&c));
        assert_eq!(want.images.len(), 1);
        assert_eq!(rects(&got), rects(&want));
        assert_eq!(got.images[0].image.rgba[..4], [5, 5, 5, 255]);
    }

    #[test]
    fn a_replayed_sixel_does_not_take_a_kitty_images_id() {
        let (t, _ch) = term(true);
        t.feed(format!("\x1b_Ga=T,f=32,s=10,v=20,i={ATLAS_REPLAY_ID};{}\x1b\\", rgba(10, 20, [1, 1, 1, 255])).as_bytes());
        t.feed(b"\x1b[3;3H\x1bPq#1;2;0;100;0#1~~\x1b\\");
        let want = frame(&t);
        assert_eq!(want.images.len(), 2);
        let got = attached(&t);
        assert_eq!(rects(&got), rects(&want));
        let px = |f: &Frame| f.images.iter().map(|i| i.image.rgba[..4].to_vec()).collect::<Vec<_>>();
        assert_eq!(px(&got), px(&want));
    }

    #[test]
    fn restore_snapshots_leave_images_out() {
        let (t, _ch) = term(true);
        t.feed(format!("\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [1, 1, 1, 255])).as_bytes());
        assert!(!String::from_utf8_lossy(&t.restore_snapshot()).contains("\x1b_G"));
    }

    #[test]
    fn deleted_images_leave_the_store() {
        let (t, _ch) = term(false);
        t.feed(format!("\x1b_Ga=T,f=32,s=10,v=20,i=1;{}\x1b\\", rgba(10, 20, [1, 1, 1, 255])).as_bytes());
        assert_eq!(frame(&t).images.len(), 1);
        t.feed(b"\x1b_Ga=d,d=I,i=1\x1b\\");
        assert!(frame(&t).images.is_empty());
        assert!(t.inner.images.lock().images.is_empty());
    }
}
