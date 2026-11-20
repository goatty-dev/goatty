//! Crisp geometric rendering for terminal graphics characters:
//!   * Box Drawing (`U+2500..=U+257F`, all 128 code points)
//!   * Block Elements (`U+2580..=U+259F`, all 32 code points)
//!   * Braille Patterns (`U+2800..=U+28FF`, all 256 code points)
//!   * Powerline separators (`U+E0B0..=U+E0BF`, 16 code points)
//!   * Symbols for Legacy Computing sextants & eighth blocks
//!     (`U+1FB00..=U+1FB3B`, `U+1FB70..=U+1FB7B`, `U+1FB82..=U+1FB8B`)
//!
//! Font glyphs for these characters are designed for a single fixed line height
//! and rasterize with anti-aliased edges against fractional cell widths, which
//! leaves visible seams between adjacent blocks and bright/thick overlaps where
//! vertical box-drawing lines meet across rows (#258). Drawing them on an
//! integer device-pixel cell grid guarantees seamless joins at every font size,
//! line height, font family, and display scale factor.

use gpui::{Bounds, Hsla, Path, PathBuilder, PathStyle, Pixels, Point, StrokeOptions, point, px};

/// Cubic Bézier handle factor for a circular quarter-arc (`4/3 * (sqrt(2) - 1)`).
const KAPPA: f32 = 0.552_284_8;

/// A half-open rectangle `[x0, x1) × [y0, y1)` in integer device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl CellRect {
    pub fn width(self) -> i32 {
        (self.x1 - self.x0).max(0)
    }

    pub fn height(self) -> i32 {
        (self.y1 - self.y0).max(0)
    }

    pub fn to_bounds(self, scale: f32) -> Bounds<Pixels> {
        Bounds::from_corners(
            point(px(self.x0 as f32 / scale), px(self.y0 as f32 / scale)),
            point(px(self.x1 as f32 / scale), px(self.y1 as f32 / scale)),
        )
    }
}

/// Grid geometry in device pixels so adjacent cells share exact pixel edges.
#[derive(Clone, Copy, Debug)]
pub struct CellGrid {
    origin_x_dp: f32,
    origin_y_dp: f32,
    cell_w_dp: f32,
    cell_h_dp: f32,
    pub scale: f32,
    /// Light box-drawing stroke width in device pixels (>= 1).
    pub light: i32,
    /// Heavy box-drawing stroke width in device pixels (>= 2 * light).
    pub heavy: i32,
}

impl CellGrid {
    pub fn new(origin: Point<Pixels>, cell_w: Pixels, line_height: Pixels, font_size: Pixels, scale: f32) -> Self {
        let scale = scale.max(0.01);
        let light = (f32::from(font_size) * scale * 0.085).round().max(1.0) as i32;
        let heavy = light * 2;
        Self {
            origin_x_dp: f32::from(origin.x) * scale,
            origin_y_dp: f32::from(origin.y) * scale,
            cell_w_dp: f32::from(cell_w) * scale,
            cell_h_dp: f32::from(line_height) * scale,
            scale,
            light,
            heavy,
        }
    }

    #[inline]
    pub fn col_x(&self, col: usize) -> i32 {
        (self.origin_x_dp + self.cell_w_dp * col as f32).round() as i32
    }

    #[inline]
    pub fn row_y(&self, row: usize) -> i32 {
        (self.origin_y_dp + self.cell_h_dp * row as f32).round() as i32
    }

    #[inline]
    pub fn cell(&self, col: usize, row: usize) -> CellRect {
        CellRect { x0: self.col_x(col), y0: self.row_y(row), x1: self.col_x(col + 1), y1: self.row_y(row + 1) }
    }

    #[inline]
    pub fn span_bounds(&self, col_start: usize, col_end: usize, row: usize) -> Bounds<Pixels> {
        CellRect { x0: self.col_x(col_start), y0: self.row_y(row), x1: self.col_x(col_end), y1: self.row_y(row + 1) }.to_bounds(self.scale)
    }
}

/// An integer device-pixel quad accumulated during prepaint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DpRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub color: Hsla,
}

impl DpRect {
    pub fn to_bounds(self, scale: f32) -> Bounds<Pixels> {
        CellRect { x0: self.x0, y0: self.y0, x1: self.x1, y1: self.y1 }.to_bounds(scale)
    }
}

/// Push a rectangle, coalescing horizontally adjacent rectangles of identical
/// vertical span and color into one quad (so `████`, `────`, `░░░░`, etc.
/// draw in a single quad per run).
fn push_rect(out: &mut Vec<DpRect>, x0: i32, y0: i32, x1: i32, y1: i32, color: Hsla) {
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.color == color
        && last.y0 == y0
        && last.y1 == y1
        && last.x1 == x0
    {
        last.x1 = x1;
        return;
    }
    out.push(DpRect { x0, y0, x1, y1, color });
}

/// Whether `ch` is drawn geometrically instead of via the font rasterizer.
pub fn is_custom(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2500..=0x257F
            | 0x2580..=0x259F
            | 0x2800..=0x28FF
            | 0xE0B0..=0xE0BF
            | 0x1FB00..=0x1FB3B
            | 0x1FB70..=0x1FB7B
            | 0x1FB82..=0x1FB8B
    )
}

/// Emit quads and/or vector paths for `ch` inside `cell`. Returns `true` if
/// `ch` was handled (including empty Braille `U+2800` which draws nothing).
pub fn draw(
    ch: char,
    cell: CellRect,
    grid: &CellGrid,
    color: Hsla,
    rects: &mut Vec<DpRect>,
    paths: &mut Vec<(Path<Pixels>, Hsla)>,
) -> bool {
    let cp = ch as u32;
    match cp {
        0x2500..=0x257F => draw_box_drawing(cp, cell, grid, color, rects, paths),
        0x2580..=0x259F => draw_block_element(cp, cell, color, rects),
        0x2800..=0x28FF => draw_braille((cp - 0x2800) as u8, cell, color, rects),
        0xE0B0..=0xE0BF => draw_powerline(cp, cell, grid, color, paths),
        0x1FB00..=0x1FB3B => draw_sextant(cp - 0x1FB00, cell, color, rects),
        0x1FB70..=0x1FB75 => {
            // Vertical one-eighth blocks at columns 1..=6 (of 8).
            let i = (cp - 0x1FB70 + 1) as i32;
            let x0 = frac(cell.x0, cell.x1, i, 8);
            let x1 = frac(cell.x0, cell.x1, i + 1, 8).max(x0 + 1).min(cell.x1);
            push_rect(rects, x0, cell.y0, x1, cell.y1, color);
        }
        0x1FB76..=0x1FB7B => {
            // Horizontal one-eighth blocks at rows 1..=6 (of 8).
            let i = (cp - 0x1FB76 + 1) as i32;
            let y0 = frac(cell.y0, cell.y1, i, 8);
            let y1 = frac(cell.y0, cell.y1, i + 1, 8).max(y0 + 1).min(cell.y1);
            push_rect(rects, cell.x0, y0, cell.x1, y1, color);
        }
        0x1FB82..=0x1FB86 => {
            // Upper 2/8..6/8 blocks.
            let eighths = (cp - 0x1FB82 + 2) as i32;
            let y1 = frac(cell.y0, cell.y1, eighths, 8);
            push_rect(rects, cell.x0, cell.y0, cell.x1, y1, color);
        }
        0x1FB87..=0x1FB8B => {
            // Right 2/8..6/8 blocks.
            let eighths = (cp - 0x1FB87 + 2) as i32;
            let x0 = frac(cell.x0, cell.x1, 8 - eighths, 8);
            push_rect(rects, x0, cell.y0, cell.x1, cell.y1, color);
        }
        _ => return false,
    }
    true
}

/// Divide `[start, end]` proportionally at `num / den`, rounded to nearest
/// device pixel (`frac(0) == start`, `frac(den) == end`).
#[inline]
fn frac(start: i32, end: i32, num: i32, den: i32) -> i32 {
    start + ((end - start) * num + den / 2) / den
}

// ---------------------------------------------------------------------------
// Block Elements & Sextants
// ---------------------------------------------------------------------------

fn draw_block_element(cp: u32, cell: CellRect, color: Hsla, rects: &mut Vec<DpRect>) {
    match cp {
        // ▀ upper half
        0x2580 => push_rect(rects, cell.x0, cell.y0, cell.x1, frac(cell.y0, cell.y1, 1, 2), color),
        // ▁▂▃▄▅▆▇█ lower 1/8..8/8
        0x2581..=0x2588 => {
            let eighths = (cp - 0x2580) as i32;
            let y0 = frac(cell.y0, cell.y1, 8 - eighths, 8);
            let y0 = if eighths < 8 { y0.min(cell.y1 - 1) } else { cell.y0 };
            push_rect(rects, cell.x0, y0, cell.x1, cell.y1, color);
        }
        // ▉▊▋▌▍▎▏ left 7/8..1/8
        0x2589..=0x258F => {
            let eighths = (0x2590 - cp) as i32;
            let x1 = frac(cell.x0, cell.x1, eighths, 8).max(cell.x0 + 1).min(cell.x1);
            push_rect(rects, cell.x0, cell.y0, x1, cell.y1, color);
        }
        // ▐ right half
        0x2590 => push_rect(rects, frac(cell.x0, cell.x1, 1, 2), cell.y0, cell.x1, cell.y1, color),
        // ░▒▓ light, medium, dark shades (translucent solid fill: seamless across cells)
        0x2591 => push_rect(rects, cell.x0, cell.y0, cell.x1, cell.y1, color.opacity(0.25)),
        0x2592 => push_rect(rects, cell.x0, cell.y0, cell.x1, cell.y1, color.opacity(0.50)),
        0x2593 => push_rect(rects, cell.x0, cell.y0, cell.x1, cell.y1, color.opacity(0.75)),
        // ▔ upper 1/8
        0x2594 => {
            let y1 = frac(cell.y0, cell.y1, 1, 8).max(cell.y0 + 1).min(cell.y1);
            push_rect(rects, cell.x0, cell.y0, cell.x1, y1, color);
        }
        // ▕ right 1/8
        0x2595 => {
            let x0 = frac(cell.x0, cell.x1, 7, 8).min(cell.x1 - 1).max(cell.x0);
            push_rect(rects, x0, cell.y0, cell.x1, cell.y1, color);
        }
        // ▖▗▘▙▚▛▜▝▞▟ quadrants (bit = row * 2 + col)
        0x2596..=0x259F => {
            let bits: u8 = match cp {
                0x2596 => 0b0100, // ▖ lower left
                0x2597 => 0b1000, // ▗ lower right
                0x2598 => 0b0001, // ▘ upper left
                0x2599 => 0b1101, // ▙ upper left + lower left + lower right
                0x259A => 0b1001, // ▚ upper left + lower right
                0x259B => 0b0111, // ▛ upper left + upper right + lower left
                0x259C => 0b1011, // ▜ upper left + upper right + lower right
                0x259D => 0b0010, // ▝ upper right
                0x259E => 0b0110, // ▞ upper right + lower left
                0x259F => 0b1110, // ▟ upper right + lower left + lower right
                _ => 0,
            };
            let mx = frac(cell.x0, cell.x1, 1, 2);
            let my = frac(cell.y0, cell.y1, 1, 2);
            let xs = [(cell.x0, mx), (mx, cell.x1)];
            let ys = [(cell.y0, my), (my, cell.y1)];
            for (row, &(ry0, ry1)) in ys.iter().enumerate() {
                let left = bits & (1 << (row * 2)) != 0;
                let right = bits & (1 << (row * 2 + 1)) != 0;
                match (left, right) {
                    (true, true) => push_rect(rects, cell.x0, ry0, cell.x1, ry1, color),
                    (true, false) => push_rect(rects, xs[0].0, ry0, xs[0].1, ry1, color),
                    (false, true) => push_rect(rects, xs[1].0, ry0, xs[1].1, ry1, color),
                    (false, false) => {}
                }
            }
        }
        _ => {}
    }
}

fn draw_sextant(offset: u32, cell: CellRect, color: Hsla, rects: &mut Vec<DpRect>) {
    // U+1FB00..=U+1FB3B skip 0 (empty), 21 (▌), 42 (▐), and 63 (█).
    let bits = (offset + 1 + u32::from(offset >= 20) + u32::from(offset >= 40)) as u8;
    let mx = frac(cell.x0, cell.x1, 1, 2);
    let y1 = frac(cell.y0, cell.y1, 1, 3);
    let y2 = frac(cell.y0, cell.y1, 2, 3);
    let ys = [(cell.y0, y1), (y1, y2), (y2, cell.y1)];
    for (row, &(ry0, ry1)) in ys.iter().enumerate() {
        let left = bits & (1 << (row * 2)) != 0;
        let right = bits & (1 << (row * 2 + 1)) != 0;
        match (left, right) {
            (true, true) => push_rect(rects, cell.x0, ry0, cell.x1, ry1, color),
            (true, false) => push_rect(rects, cell.x0, ry0, mx, ry1, color),
            (false, true) => push_rect(rects, mx, ry0, cell.x1, ry1, color),
            (false, false) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Braille Patterns (U+2800..=U+28FF)
// ---------------------------------------------------------------------------

fn draw_braille(bits: u8, cell: CellRect, color: Hsla, rects: &mut Vec<DpRect>) {
    if bits == 0 {
        return;
    }
    let w = cell.width();
    let h = cell.height();
    if w == 0 || h == 0 {
        return;
    }
    // Keep dots square and uniformly pitched across cell boundaries so
    // braille graphs (btop, bottom, nvtop) form an even dot matrix.
    let dot = ((w as f32 * 0.28).round() as i32).min((h as f32 * 0.15).round() as i32).max(1);
    let dot_w = dot.min((w / 2).max(1));
    let dot_h = dot.min((h / 4).max(1));

    // Bit index for (row, col) in the standard 2×4 Braille layout.
    const MAP: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let xs = [cell.x0 + ((w - dot_w * 2) / 4).max(0), cell.x0 + ((w * 3 - dot_w * 2) / 4).min(w - dot_w).max(0)];
    for (row, &row_bits) in MAP.iter().enumerate() {
        let cy = cell.y0 + ((h * (2 * row as i32 + 1) - dot_h * 4) / 8).clamp(0, h - dot_h);
        for (col, &mask) in row_bits.iter().enumerate() {
            if bits & mask != 0 {
                push_rect(rects, xs[col], cy, xs[col] + dot_w, cy + dot_h, color);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Box Drawing (U+2500..=U+257F)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

#[derive(Clone, Copy, Debug)]
struct Arms {
    up: Weight,
    right: Weight,
    down: Weight,
    left: Weight,
}

const fn arms(up: Weight, right: Weight, down: Weight, left: Weight) -> Arms {
    Arms { up, right, down, left }
}

fn orthogonal_arms(cp: u32) -> Option<Arms> {
    use Weight::{Double as D, Heavy as H, Light as L, None as N};
    Some(match cp {
        0x2500 => arms(N, L, N, L), // ─
        0x2501 => arms(N, H, N, H), // ━
        0x2502 => arms(L, N, L, N), // │
        0x2503 => arms(H, N, H, N), // ┃

        // Corners (down + right, down + left, up + right, up + left)
        0x250C => arms(N, L, L, N), // ┌
        0x250D => arms(N, H, L, N), // ┍
        0x250E => arms(N, L, H, N), // ┎
        0x250F => arms(N, H, H, N), // ┏
        0x2510 => arms(N, N, L, L), // ┐
        0x2511 => arms(N, N, L, H), // ┑
        0x2512 => arms(N, N, H, L), // ┒
        0x2513 => arms(N, N, H, H), // ┓
        0x2514 => arms(L, L, N, N), // └
        0x2515 => arms(L, H, N, N), // ┕
        0x2516 => arms(H, L, N, N), // ┖
        0x2517 => arms(H, H, N, N), // ┗
        0x2518 => arms(L, N, N, L), // ┘
        0x2519 => arms(L, N, N, H), // ┙
        0x251A => arms(H, N, N, L), // ┚
        0x251B => arms(H, N, N, H), // ┛

        // Vertical + right
        0x251C => arms(L, L, L, N), // ├
        0x251D => arms(L, H, L, N), // ┝
        0x251E => arms(H, L, L, N), // ┞
        0x251F => arms(L, L, H, N), // ┟
        0x2520 => arms(H, L, H, N), // ┠
        0x2521 => arms(H, H, L, N), // ┡
        0x2522 => arms(L, H, H, N), // ┢
        0x2523 => arms(H, H, H, N), // ┣

        // Vertical + left
        0x2524 => arms(L, N, L, L), // ┤
        0x2525 => arms(L, N, L, H), // ┥
        0x2526 => arms(H, N, L, L), // ┦
        0x2527 => arms(L, N, H, L), // ┧
        0x2528 => arms(H, N, H, L), // ┨
        0x2529 => arms(H, N, L, H), // ┩
        0x252A => arms(L, N, H, H), // ┪
        0x252B => arms(H, N, H, H), // ┫

        // Horizontal + down
        0x252C => arms(N, L, L, L), // ┬
        0x252D => arms(N, L, L, H), // ┭
        0x252E => arms(N, H, L, L), // ┮
        0x252F => arms(N, H, L, H), // ┯
        0x2530 => arms(N, L, H, L), // ┰
        0x2531 => arms(N, L, H, H), // ┱
        0x2532 => arms(N, H, H, L), // ┲
        0x2533 => arms(N, H, H, H), // ┳

        // Horizontal + up
        0x2534 => arms(L, L, N, L), // ┴
        0x2535 => arms(L, L, N, H), // ┵
        0x2536 => arms(L, H, N, L), // ┶
        0x2537 => arms(L, H, N, H), // ┷
        0x2538 => arms(H, L, N, L), // ┸
        0x2539 => arms(H, L, N, H), // ┹
        0x253A => arms(H, H, N, L), // ┺
        0x253B => arms(H, H, N, H), // ┻

        // Crosses
        0x253C => arms(L, L, L, L), // ┼
        0x253D => arms(L, L, L, H), // ┽
        0x253E => arms(L, H, L, L), // ┾
        0x253F => arms(L, H, L, H), // ┿
        0x2540 => arms(H, L, L, L), // ╀
        0x2541 => arms(L, L, H, L), // ╁
        0x2542 => arms(H, L, H, L), // ╂
        0x2543 => arms(H, L, L, H), // ╃
        0x2544 => arms(H, H, L, L), // ╄
        0x2545 => arms(L, L, H, H), // ╅
        0x2546 => arms(L, H, H, L), // ╆
        0x2547 => arms(H, H, L, H), // ╇
        0x2548 => arms(L, H, H, H), // ╈
        0x2549 => arms(H, L, H, H), // ╉
        0x254A => arms(H, H, H, L), // ╊
        0x254B => arms(H, H, H, H), // ╋

        // Double and mixed single/double
        0x2550 => arms(N, D, N, D), // ═
        0x2551 => arms(D, N, D, N), // ║
        0x2552 => arms(N, D, L, N), // ╒
        0x2553 => arms(N, L, D, N), // ╓
        0x2554 => arms(N, D, D, N), // ╔
        0x2555 => arms(N, N, L, D), // ╕
        0x2556 => arms(N, N, D, L), // ╖
        0x2557 => arms(N, N, D, D), // ╗
        0x2558 => arms(L, D, N, N), // ╘
        0x2559 => arms(D, L, N, N), // ╙
        0x255A => arms(D, D, N, N), // ╚
        0x255B => arms(L, N, N, D), // ╛
        0x255C => arms(D, N, N, L), // ╜
        0x255D => arms(D, N, N, D), // ╝
        0x255E => arms(L, D, L, N), // ╞
        0x255F => arms(D, L, D, N), // ╟
        0x2560 => arms(D, D, D, N), // ╠
        0x2561 => arms(L, N, L, D), // ╡
        0x2562 => arms(D, N, D, L), // ╢
        0x2563 => arms(D, N, D, D), // ╣
        0x2564 => arms(N, D, L, D), // ╤
        0x2565 => arms(N, L, D, L), // ╥
        0x2566 => arms(N, D, D, D), // ╦
        0x2567 => arms(L, D, N, D), // ╧
        0x2568 => arms(D, L, N, L), // ╨
        0x2569 => arms(D, D, N, D), // ╩
        0x256A => arms(L, D, L, D), // ╪
        0x256B => arms(D, L, D, L), // ╫
        0x256C => arms(D, D, D, D), // ╬

        // Half-lines and mixed light/heavy straight lines
        0x2574 => arms(N, N, N, L), // ╴
        0x2575 => arms(L, N, N, N), // ╵
        0x2576 => arms(N, L, N, N), // ╶
        0x2577 => arms(N, N, L, N), // ╷
        0x2578 => arms(N, N, N, H), // ╸
        0x2579 => arms(H, N, N, N), // ╹
        0x257A => arms(N, H, N, N), // ╺
        0x257B => arms(N, N, H, N), // ╻
        0x257C => arms(N, H, N, L), // ╼
        0x257D => arms(L, N, H, N), // ╽
        0x257E => arms(N, L, N, H), // ╾
        0x257F => arms(H, N, L, N), // ╿
        _ => return None,
    })
}

fn draw_box_drawing(cp: u32, cell: CellRect, grid: &CellGrid, color: Hsla, rects: &mut Vec<DpRect>, paths: &mut Vec<(Path<Pixels>, Hsla)>) {
    if let Some(a) = orthogonal_arms(cp) {
        draw_orthogonal(a, cell, grid, color, rects);
        return;
    }
    match cp {
        // Dashed horizontal (2, 3, 4 dashes; light or heavy)
        0x2504 => draw_dashed_h(cell, 3, grid.light, color, rects), // ┄
        0x2505 => draw_dashed_h(cell, 3, grid.heavy, color, rects), // ┅
        0x2508 => draw_dashed_h(cell, 4, grid.light, color, rects), // ┈
        0x2509 => draw_dashed_h(cell, 4, grid.heavy, color, rects), // ┉
        0x254C => draw_dashed_h(cell, 2, grid.light, color, rects), // ╌
        0x254D => draw_dashed_h(cell, 2, grid.heavy, color, rects), // ╍
        // Dashed vertical
        0x2506 => draw_dashed_v(cell, 3, grid.light, color, rects), // ┆
        0x2507 => draw_dashed_v(cell, 3, grid.heavy, color, rects), // ┇
        0x250A => draw_dashed_v(cell, 4, grid.light, color, rects), // ┊
        0x250B => draw_dashed_v(cell, 4, grid.heavy, color, rects), // ┋
        0x254E => draw_dashed_v(cell, 2, grid.light, color, rects), // ╎
        0x254F => draw_dashed_v(cell, 2, grid.heavy, color, rects), // ╏
        // Rounded corners ╭ ╮ ╯ ╰
        0x256D..=0x2570 => draw_rounded_corner(cp, cell, grid, color, paths),
        // Diagonals ╱ ╲ ╳
        0x2571..=0x2573 => draw_diagonal(cp, cell, grid, color, paths),
        _ => {}
    }
}

fn draw_orthogonal(a: Arms, cell: CellRect, grid: &CellGrid, color: Hsla, rects: &mut Vec<DpRect>) {
    let w = cell.width();
    let h = cell.height();
    if w == 0 || h == 0 {
        return;
    }
    let t = grid.light.min(w).min(h);
    let big = grid.heavy.min(w).min(h);
    let cx = cell.x0 + (w - t) / 2;
    let cy = cell.y0 + (h - t) / 2;

    let has_double = matches!(a.up, Weight::Double)
        || matches!(a.right, Weight::Double)
        || matches!(a.down, Weight::Double)
        || matches!(a.left, Weight::Double);
    if has_double {
        draw_double_orthogonal(a, cell, cx, cy, t, color, rects);
        return;
    }

    let chx = cell.x0 + (w - big) / 2;
    let chy = cell.y0 + (h - big) / 2;

    let v_band = |wt: Weight| -> Option<(i32, i32)> {
        match wt {
            Weight::Light => Some((cx, cx + t)),
            Weight::Heavy => Some((chx, chx + big)),
            _ => None,
        }
    };
    let h_band = |wt: Weight| -> Option<(i32, i32)> {
        match wt {
            Weight::Light => Some((cy, cy + t)),
            Weight::Heavy => Some((chy, chy + big)),
            _ => None,
        }
    };

    // Fast path for pure straight lines (enables horizontal coalescing for ─ ━).
    if a.up == Weight::None && a.down == Weight::None && a.left == a.right {
        if let Some((y0, y1)) = h_band(a.left) {
            push_rect(rects, cell.x0, y0, cell.x1, y1, color);
        }
        return;
    }
    if a.left == Weight::None && a.right == Weight::None && a.up == a.down {
        if let Some((x0, x1)) = v_band(a.up) {
            push_rect(rects, x0, cell.y0, x1, cell.y1, color);
        }
        return;
    }

    // Center junction bounds along each axis. When an axis has no arms (half
    // lines or straight mixed light/heavy like ╼ ╽), arms meet at the center.
    let (vx_lo, vx_hi) = match (v_band(a.up), v_band(a.down)) {
        (Some(u), Some(d)) => (u.0.min(d.0), u.1.max(d.1)),
        (Some(u), None) => u,
        (None, Some(d)) => d,
        (None, None) => (cx, cx + t),
    };
    let (hy_lo, hy_hi) = match (h_band(a.left), h_band(a.right)) {
        (Some(l), Some(r)) => (l.0.min(r.0), l.1.max(r.1)),
        (Some(l), None) => l,
        (None, Some(r)) => r,
        (None, None) => (cy, cy + t),
    };

    // Pure straight mixed lines (╼ ╾ ╽ ╿): split cleanly at midpoint.
    let pure_h = a.up == Weight::None && a.down == Weight::None;
    let pure_v = a.left == Weight::None && a.right == Weight::None;

    if let Some((y0, y1)) = h_band(a.left) {
        let x1 = if pure_h && a.right != Weight::None { cx } else { vx_hi };
        push_rect(rects, cell.x0, y0, x1, y1, color);
    }
    if let Some((y0, y1)) = h_band(a.right) {
        let x0 = if pure_h && a.left != Weight::None { cx } else { vx_lo };
        push_rect(rects, x0, y0, cell.x1, y1, color);
    }
    if let Some((x0, x1)) = v_band(a.up) {
        let y1 = if pure_v && a.down != Weight::None { cy } else { hy_hi };
        push_rect(rects, x0, cell.y0, x1, y1, color);
    }
    if let Some((x0, x1)) = v_band(a.down) {
        let y0 = if pure_v && a.up != Weight::None { cy } else { hy_lo };
        push_rect(rects, x0, y0, x1, cell.y1, color);
    }
}

fn draw_double_orthogonal(a: Arms, cell: CellRect, cx: i32, cy: i32, t: i32, color: Hsla, rects: &mut Vec<DpRect>) {
    use Weight::{Double as D, Light as L, None as N};

    // Double tracks sit symmetrically around the single centerline with a gap of `t`:
    //   single: [c, c + t)
    //   double: [c - t, c) and [c + t, c + 2*t)
    let dx0 = (cx - t).max(cell.x0);
    let dx1 = (cx + t).min(cell.x1 - t).max(cell.x0);
    let dy0 = (cy - t).max(cell.y0);
    let dy1 = (cy + t).min(cell.y1 - t).max(cell.y0);

    let h_double = a.left == D || a.right == D;
    let v_double = a.up == D || a.down == D;

    match (h_double, v_double) {
        // Pure double horizontal (═) or double horizontal + single vertical (╒╕╘╛╞╡╤╧╪).
        (true, false) => {
            let (left_end, right_start) = match (a.left, a.right) {
                (D, D) => (cell.x1, cell.x1), // full horizontal pass
                (D, N) => (cx + t, cell.x1),
                (N, D) => (cell.x0, cx),
                _ => (cell.x0, cell.x1),
            };
            if a.left == D {
                push_rect(rects, cell.x0, dy0, left_end, dy0 + t, color);
                push_rect(rects, cell.x0, dy1, left_end, dy1 + t, color);
            } else if a.right == D {
                push_rect(rects, right_start, dy0, cell.x1, dy0 + t, color);
                push_rect(rects, right_start, dy1, cell.x1, dy1 + t, color);
            }
            match (a.up, a.down) {
                (L, L) => push_rect(rects, cx, cell.y0, cx + t, cell.y1, color),
                (L, N) => {
                    // ╧ (both L & R double): stem stops at upper bar so it doesn't cross the gap.
                    // ╘ ╛ (corner): stem closes off the outer lower bar too.
                    let y1 = if a.left == D && a.right == D { dy0 + t } else { dy1 + t };
                    push_rect(rects, cx, cell.y0, cx + t, y1, color);
                }
                (N, L) => {
                    let y0 = if a.left == D && a.right == D { dy1 } else { dy0 };
                    push_rect(rects, cx, y0, cx + t, cell.y1, color);
                }
                _ => {}
            }
        }
        // Pure double vertical (║) or double vertical + single horizontal (╓╖╙╜╟╢╥╨╫).
        (false, true) => {
            let (up_end, down_start) = match (a.up, a.down) {
                (D, D) => (cell.y1, cell.y1),
                (D, N) => (cy + t, cell.y1),
                (N, D) => (cell.y0, cy),
                _ => (cell.y0, cell.y1),
            };
            if a.up == D {
                push_rect(rects, dx0, cell.y0, dx0 + t, up_end, color);
                push_rect(rects, dx1, cell.y0, dx1 + t, up_end, color);
            } else if a.down == D {
                push_rect(rects, dx0, down_start, dx0 + t, cell.y1, color);
                push_rect(rects, dx1, down_start, dx1 + t, cell.y1, color);
            }
            match (a.left, a.right) {
                (L, L) => push_rect(rects, cell.x0, cy, cell.x1, cy + t, color),
                (L, N) => {
                    let x1 = if a.up == D && a.down == D { dx0 + t } else { dx1 + t };
                    push_rect(rects, cell.x0, cy, x1, cy + t, color);
                }
                (N, L) => {
                    let x0 = if a.up == D && a.down == D { dx1 } else { dx0 };
                    push_rect(rects, x0, cy, cell.x1, cy + t, color);
                }
                _ => {}
            }
        }
        // Double on both axes (╔╗╚╝╠╣╦╩╬).
        (true, true) => {
            // Top horizontal bar (dy0): continuous if no `up` arm; else split around `up`.
            if a.up == N {
                let x0 = if a.left == D { cell.x0 } else { dx0 };
                let x1 = if a.right == D { cell.x1 } else { dx1 + t };
                push_rect(rects, x0, dy0, x1, dy0 + t, color);
            } else {
                if a.left == D {
                    push_rect(rects, cell.x0, dy0, dx0 + t, dy0 + t, color);
                }
                if a.right == D {
                    push_rect(rects, dx1, dy0, cell.x1, dy0 + t, color);
                }
            }
            // Bottom horizontal bar (dy1): continuous if no `down` arm; else split around `down`.
            if a.down == N {
                let x0 = if a.left == D { cell.x0 } else { dx0 };
                let x1 = if a.right == D { cell.x1 } else { dx1 + t };
                push_rect(rects, x0, dy1, x1, dy1 + t, color);
            } else {
                if a.left == D {
                    push_rect(rects, cell.x0, dy1, dx0 + t, dy1 + t, color);
                }
                if a.right == D {
                    push_rect(rects, dx1, dy1, cell.x1, dy1 + t, color);
                }
            }
            // Left vertical bar (dx0): continuous if no `left` arm; else split around `left`.
            if a.left == N {
                let y0 = if a.up == D { cell.y0 } else { dy0 };
                let y1 = if a.down == D { cell.y1 } else { dy1 + t };
                push_rect(rects, dx0, y0, dx0 + t, y1, color);
            } else {
                if a.up == D {
                    push_rect(rects, dx0, cell.y0, dx0 + t, dy0 + t, color);
                }
                if a.down == D {
                    push_rect(rects, dx0, dy1, dx0 + t, cell.y1, color);
                }
            }
            // Right vertical bar (dx1): continuous if no `right` arm; else split around `right`.
            if a.right == N {
                let y0 = if a.up == D { cell.y0 } else { dy0 };
                let y1 = if a.down == D { cell.y1 } else { dy1 + t };
                push_rect(rects, dx1, y0, dx1 + t, y1, color);
            } else {
                if a.up == D {
                    push_rect(rects, dx1, cell.y0, dx1 + t, dy0 + t, color);
                }
                if a.down == D {
                    push_rect(rects, dx1, dy1, dx1 + t, cell.y1, color);
                }
            }
        }
        (false, false) => {}
    }
}

fn draw_dashed_h(cell: CellRect, count: i32, thickness: i32, color: Hsla, rects: &mut Vec<DpRect>) {
    let h = cell.height();
    let t = thickness.min(h);
    let cy = cell.y0 + (h - t) / 2;
    for i in 0..count {
        let s0 = frac(cell.x0, cell.x1, i, count);
        let s1 = frac(cell.x0, cell.x1, i + 1, count);
        let len = s1 - s0;
        if len <= 0 {
            continue;
        }
        let gap = if len >= 2 { (len / 3).max(1) } else { 0 };
        let lead = gap / 2;
        let trail = gap - lead;
        push_rect(rects, s0 + lead, cy, (s1 - trail).max(s0 + lead + 1), cy + t, color);
    }
}

fn draw_dashed_v(cell: CellRect, count: i32, thickness: i32, color: Hsla, rects: &mut Vec<DpRect>) {
    let w = cell.width();
    let t = thickness.min(w);
    let cx = cell.x0 + (w - t) / 2;
    for i in 0..count {
        let s0 = frac(cell.y0, cell.y1, i, count);
        let s1 = frac(cell.y0, cell.y1, i + 1, count);
        let len = s1 - s0;
        if len <= 0 {
            continue;
        }
        let gap = if len >= 2 { (len / 3).max(1) } else { 0 };
        let lead = gap / 2;
        let trail = gap - lead;
        push_rect(rects, cx, s0 + lead, cx + t, (s1 - trail).max(s0 + lead + 1), color);
    }
}

fn dp_point(x: f32, y: f32, scale: f32) -> Point<Pixels> {
    point(px(x / scale), px(y / scale))
}

fn stroke_builder(width_dp: f32, scale: f32) -> PathBuilder {
    let width = width_dp / scale;
    PathBuilder::default().with_style(PathStyle::Stroke(StrokeOptions::default().with_line_width(width).with_miter_limit(1.5)))
}

fn draw_rounded_corner(cp: u32, cell: CellRect, grid: &CellGrid, color: Hsla, paths: &mut Vec<(Path<Pixels>, Hsla)>) {
    let w = cell.width();
    let h = cell.height();
    if w == 0 || h == 0 {
        return;
    }
    let t = grid.light.min(w).min(h);
    let cx = cell.x0 + (w - t) / 2;
    let cy = cell.y0 + (h - t) / 2;
    let mx = cx as f32 + t as f32 * 0.5;
    let my = cy as f32 + t as f32 * 0.5;
    let (x0, x1, y0, y1) = (cell.x0 as f32, cell.x1 as f32, cell.y0 as f32, cell.y1 as f32);
    let r = (mx - x0).min(x1 - mx).min(my - y0).min(y1 - my).max(0.5);
    let k = 1.0 - KAPPA;
    let s = grid.scale;

    let mut b = stroke_builder(t as f32, s);
    match cp {
        // ╭ right + down
        0x256D => {
            b.move_to(dp_point(x1, my, s));
            b.line_to(dp_point(mx + r, my, s));
            b.cubic_bezier_to(dp_point(mx, my + r, s), dp_point(mx + r * k, my, s), dp_point(mx, my + r * k, s));
            b.line_to(dp_point(mx, y1, s));
        }
        // ╮ left + down
        0x256E => {
            b.move_to(dp_point(x0, my, s));
            b.line_to(dp_point(mx - r, my, s));
            b.cubic_bezier_to(dp_point(mx, my + r, s), dp_point(mx - r * k, my, s), dp_point(mx, my + r * k, s));
            b.line_to(dp_point(mx, y1, s));
        }
        // ╯ left + up
        0x256F => {
            b.move_to(dp_point(x0, my, s));
            b.line_to(dp_point(mx - r, my, s));
            b.cubic_bezier_to(dp_point(mx, my - r, s), dp_point(mx - r * k, my, s), dp_point(mx, my - r * k, s));
            b.line_to(dp_point(mx, y0, s));
        }
        // ╰ right + up
        0x2570 => {
            b.move_to(dp_point(x1, my, s));
            b.line_to(dp_point(mx + r, my, s));
            b.cubic_bezier_to(dp_point(mx, my - r, s), dp_point(mx + r * k, my, s), dp_point(mx, my - r * k, s));
            b.line_to(dp_point(mx, y0, s));
        }
        _ => return,
    }
    if let Ok(path) = b.build() {
        paths.push((path, color));
    }
}

fn draw_diagonal(cp: u32, cell: CellRect, grid: &CellGrid, color: Hsla, paths: &mut Vec<(Path<Pixels>, Hsla)>) {
    let s = grid.scale;
    let t = grid.light as f32;
    let (x0, x1, y0, y1) = (cell.x0 as f32, cell.x1 as f32, cell.y0 as f32, cell.y1 as f32);
    let mut b = stroke_builder(t, s);
    if cp == 0x2571 || cp == 0x2573 {
        // ╱ bottom-left to top-right
        b.move_to(dp_point(x0, y1, s));
        b.line_to(dp_point(x1, y0, s));
    }
    if cp == 0x2572 || cp == 0x2573 {
        // ╲ top-left to bottom-right
        b.move_to(dp_point(x0, y0, s));
        b.line_to(dp_point(x1, y1, s));
    }
    if let Ok(path) = b.build() {
        paths.push((path, color));
    }
}

// ---------------------------------------------------------------------------
// Powerline Separators (U+E0B0..=U+E0BF)
// ---------------------------------------------------------------------------

fn draw_powerline(cp: u32, cell: CellRect, grid: &CellGrid, color: Hsla, paths: &mut Vec<(Path<Pixels>, Hsla)>) {
    let s = grid.scale;
    let t = grid.light as f32;
    let (x0, x1, y0, y1) = (cell.x0 as f32, cell.x1 as f32, cell.y0 as f32, cell.y1 as f32);
    let mid_y = (y0 + y1) * 0.5;
    let rx = x1 - x0;
    let ry = (y1 - y0) * 0.5;
    let inset = (t * 0.5).min(rx * 0.25);

    let builder = match cp {
        //  filled right-pointing triangle
        0xE0B0 => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x0, y0, s));
            b.line_to(dp_point(x1, mid_y, s));
            b.line_to(dp_point(x0, y1, s));
            b.close();
            b
        }
        //  thin right-pointing chevron
        0xE0B1 => {
            let mut b = stroke_builder(t, s);
            b.move_to(dp_point(x0, y0, s));
            b.line_to(dp_point(x1 - inset, mid_y, s));
            b.line_to(dp_point(x0, y1, s));
            b
        }
        //  filled left-pointing triangle
        0xE0B2 => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x1, y0, s));
            b.line_to(dp_point(x0, mid_y, s));
            b.line_to(dp_point(x1, y1, s));
            b.close();
            b
        }
        //  thin left-pointing chevron
        0xE0B3 => {
            let mut b = stroke_builder(t, s);
            b.move_to(dp_point(x1, y0, s));
            b.line_to(dp_point(x0 + inset, mid_y, s));
            b.line_to(dp_point(x1, y1, s));
            b
        }
        //  filled right semicircle
        0xE0B4 => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x0, y0, s));
            b.cubic_bezier_to(dp_point(x1, mid_y, s), dp_point(x0 + rx * KAPPA, y0, s), dp_point(x1, mid_y - ry * KAPPA, s));
            b.cubic_bezier_to(dp_point(x0, y1, s), dp_point(x1, mid_y + ry * KAPPA, s), dp_point(x0 + rx * KAPPA, y1, s));
            b.close();
            b
        }
        //  thin right semicircle
        0xE0B5 => {
            let ax = x1 - inset;
            let arx = (ax - x0).max(0.5);
            let mut b = stroke_builder(t, s);
            b.move_to(dp_point(x0, y0, s));
            b.cubic_bezier_to(dp_point(ax, mid_y, s), dp_point(x0 + arx * KAPPA, y0, s), dp_point(ax, mid_y - ry * KAPPA, s));
            b.cubic_bezier_to(dp_point(x0, y1, s), dp_point(ax, mid_y + ry * KAPPA, s), dp_point(x0 + arx * KAPPA, y1, s));
            b
        }
        //  filled left semicircle
        0xE0B6 => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x1, y0, s));
            b.cubic_bezier_to(dp_point(x0, mid_y, s), dp_point(x1 - rx * KAPPA, y0, s), dp_point(x0, mid_y - ry * KAPPA, s));
            b.cubic_bezier_to(dp_point(x1, y1, s), dp_point(x0, mid_y + ry * KAPPA, s), dp_point(x1 - rx * KAPPA, y1, s));
            b.close();
            b
        }
        //  thin left semicircle
        0xE0B7 => {
            let ax = x0 + inset;
            let arx = (x1 - ax).max(0.5);
            let mut b = stroke_builder(t, s);
            b.move_to(dp_point(x1, y0, s));
            b.cubic_bezier_to(dp_point(ax, mid_y, s), dp_point(x1 - arx * KAPPA, y0, s), dp_point(ax, mid_y - ry * KAPPA, s));
            b.cubic_bezier_to(dp_point(x1, y1, s), dp_point(ax, mid_y + ry * KAPPA, s), dp_point(x1 - arx * KAPPA, y1, s));
            b
        }
        //  filled lower-left triangle
        0xE0B8 => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x0, y0, s));
            b.line_to(dp_point(x1, y1, s));
            b.line_to(dp_point(x0, y1, s));
            b.close();
            b
        }
        //   thin backslash diagonal
        0xE0B9 | 0xE0BF => {
            let mut b = stroke_builder(t, s);
            b.move_to(dp_point(x0, y0, s));
            b.line_to(dp_point(x1, y1, s));
            b
        }
        //  filled lower-right triangle
        0xE0BA => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x1, y0, s));
            b.line_to(dp_point(x1, y1, s));
            b.line_to(dp_point(x0, y1, s));
            b.close();
            b
        }
        //   thin slash diagonal
        0xE0BB | 0xE0BD => {
            let mut b = stroke_builder(t, s);
            b.move_to(dp_point(x0, y1, s));
            b.line_to(dp_point(x1, y0, s));
            b
        }
        //  filled upper-left triangle
        0xE0BC => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x0, y0, s));
            b.line_to(dp_point(x1, y0, s));
            b.line_to(dp_point(x0, y1, s));
            b.close();
            b
        }
        //  filled upper-right triangle
        0xE0BE => {
            let mut b = PathBuilder::fill();
            b.move_to(dp_point(x0, y0, s));
            b.line_to(dp_point(x1, y0, s));
            b.line_to(dp_point(x1, y1, s));
            b.close();
            b
        }
        _ => return,
    };
    if let Ok(path) = builder.build() {
        paths.push((path, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_grid(cell_w: f32, line_height: f32, font_size: f32, scale: f32) -> CellGrid {
        CellGrid::new(point(px(6.0), px(6.0)), px(cell_w), px(line_height), px(font_size), scale)
    }

    #[test]
    fn adjacent_cells_share_exact_device_pixel_edges() {
        for (cw, lh, scale) in [(8.4, 18.0, 1.0), (28.8, 60.0, 1.0), (8.4, 18.0, 1.25), (8.4, 18.0, 1.5), (8.4, 18.0, 2.0)] {
            let g = test_grid(cw, lh, 14.0, scale);
            for col in 0..40 {
                let a = g.cell(col, 3);
                let b = g.cell(col + 1, 3);
                assert_eq!(a.x1, b.x0, "horizontal seam between col {col} and {}", col + 1);
            }
            for row in 0..20 {
                let a = g.cell(5, row);
                let b = g.cell(5, row + 1);
                assert_eq!(a.y1, b.y0, "vertical seam between row {row} and {}", row + 1);
            }
        }
    }

    #[test]
    fn adjacent_full_blocks_coalesce_with_zero_gaps() {
        let g = test_grid(8.4, 18.0, 14.0, 1.0);
        let c = gpui::white();
        let mut rects = Vec::new();
        let mut paths = Vec::new();
        for col in 0..16 {
            assert!(draw('█', g.cell(col, 0), &g, c, &mut rects, &mut paths));
        }
        assert!(paths.is_empty());
        assert_eq!(rects.len(), 1, "16 adjacent full blocks merge into one quad");
        assert_eq!(rects[0].x0, g.col_x(0));
        assert_eq!(rects[0].x1, g.col_x(16));
        assert_eq!(rects[0].y0, g.row_y(0));
        assert_eq!(rects[0].y1, g.row_y(1));
    }

    #[test]
    fn vertical_lines_meet_without_overlap_at_any_line_height() {
        for lh in [14.0, 18.0, 21.0, 28.0] {
            let g = test_grid(8.4, lh, 14.0, 1.0);
            let c = gpui::white();
            let mut rects = Vec::new();
            let mut paths = Vec::new();
            draw('│', g.cell(4, 0), &g, c, &mut rects, &mut paths);
            draw('│', g.cell(4, 1), &g, c, &mut rects, &mut paths);
            assert_eq!(rects.len(), 2);
            assert_eq!(rects[0].x0, rects[1].x0, "vertical bars stay aligned across rows");
            assert_eq!(rects[0].x1, rects[1].x1);
            assert_eq!(rects[0].y1, rects[1].y0, "exact touch with zero overlap and zero gap");
        }
    }

    #[test]
    fn half_blocks_tile_cell_exactly() {
        let g = test_grid(8.4, 17.0, 14.0, 1.0);
        let c = gpui::white();
        let cell = g.cell(0, 0);
        let (mut r_top, mut r_bot, mut paths) = (Vec::new(), Vec::new(), Vec::new());
        draw('▀', cell, &g, c, &mut r_top, &mut paths);
        draw('▄', cell, &g, c, &mut r_bot, &mut paths);
        assert_eq!(r_top[0].y0, cell.y0);
        assert_eq!(r_top[0].y1, r_bot[0].y0);
        assert_eq!(r_bot[0].y1, cell.y1);

        let (mut r_left, mut r_right) = (Vec::new(), Vec::new());
        draw('▌', cell, &g, c, &mut r_left, &mut paths);
        draw('▐', cell, &g, c, &mut r_right, &mut paths);
        assert_eq!(r_left[0].x0, cell.x0);
        assert_eq!(r_left[0].x1, r_right[0].x0);
        assert_eq!(r_right[0].x1, cell.x1);
    }

    #[test]
    fn every_box_drawing_and_block_codepoint_renders_inside_cell() {
        let g = test_grid(8.4, 18.0, 14.0, 1.0);
        let cell = g.cell(3, 2);
        let c = gpui::white();
        for cp in (0x2500..=0x259F).chain(0x2800..=0x28FF).chain(0xE0B0..=0xE0BF).chain(0x1FB00..=0x1FB3B) {
            let ch = char::from_u32(cp).unwrap();
            assert!(is_custom(ch), "U+{cp:04X} should be custom");
            let mut rects = Vec::new();
            let mut paths = Vec::new();
            assert!(draw(ch, cell, &g, c, &mut rects, &mut paths), "U+{cp:04X} failed to draw");
            if cp != 0x2800 {
                assert!(!rects.is_empty() || !paths.is_empty(), "U+{cp:04X} emitted no primitives");
            }
            for r in &rects {
                assert!(r.x0 >= cell.x0 && r.x1 <= cell.x1, "U+{cp:04X} x out of cell: {r:?} vs {cell:?}");
                assert!(r.y0 >= cell.y0 && r.y1 <= cell.y1, "U+{cp:04X} y out of cell: {r:?} vs {cell:?}");
                assert!(r.x1 > r.x0 && r.y1 > r.y0, "U+{cp:04X} degenerate rect: {r:?}");
            }
        }
    }

    #[test]
    fn double_box_t_junctions_leave_hollow_center_intact() {
        let g = test_grid(12.0, 24.0, 18.0, 1.0);
        let cell = g.cell(0, 0);
        let c = gpui::white();
        let mut rects = Vec::new();
        let mut paths = Vec::new();
        // ╤ has double horizontal bar and single down stem: stem must not cut through top bar.
        draw('╤', cell, &g, c, &mut rects, &mut paths);
        let top_bar_y = rects.iter().map(|r| r.y0).min().unwrap();
        let vertical_stems: Vec<_> = rects.iter().filter(|r| r.y1 == cell.y1).collect();
        assert_eq!(vertical_stems.len(), 1);
        assert!(vertical_stems[0].y0 > top_bar_y, "single stem in ╤ starts at lower bar");
    }

    #[test]
    fn plain_text_is_not_custom() {
        for ch in ['a', 'M', ' ', '0', '-', '|', '_', '😀', '→'] {
            assert!(!is_custom(ch), "{ch:?} should use the font rasterizer");
        }
    }
}
