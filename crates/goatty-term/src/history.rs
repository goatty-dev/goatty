//! Scrollback on demand: line numbers that mean the same row on the daemon
//! and in a client's mirror, the daemon serving any range of its history,
//! and the client inserting it where it belongs.
//!
//! **Host lines.** Every row of a session's primary screen has a number on
//! the daemon: rows count from the session's start (rio-vt's evicted row
//! count plus the position in the scrollback), so a number keeps meaning
//! the same row while output scrolls and old rows drop off.
//!
//! **The client's map.** The mirror holds only part of that history: the
//! attach snapshot replays the newest lines, and a resync skips lines. After
//! each snapshot (and after a resize) the daemon says which host line its
//! screen's top row is ([`Terminal::set_top_line`]); the mirror's screen
//! matches the daemon's at that point, so this pins its rows to host lines.
//! Rows that came in as live output since continue the numbering. Missing
//! lines show up as host lines older than the oldest mapped row, and as
//! jumps between two pinned runs (what a resync skipped).
//! [`Terminal::wanted_history`] says what to fetch when the viewport comes
//! near one, and [`Terminal::insert_history`] puts the daemon's answer in.
//!
//! Inserting rebuilds the primary grid from a snapshot of the old one with
//! the new rows spliced in (rio-vt can't insert rows into its scrollback),
//! then swaps the grid in. Modes, the cursor and the rest of the terminal's
//! state stay as they were; the viewport keeps showing the same rows.

use std::sync::Arc;

use rio_vt::crosswords::grid::{Dimensions, Grid, Scroll};

use crate::snapshot::write_rows;
use crate::terminal::Term;
use crate::{DEFAULT_SCROLLBACK, Palette, TermMode, TermSize, Terminal};

/// Why the daemon sent the screen's top line (mirrors the wire's reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopLineCause {
    /// After an attach snapshot: the mirror was reset, every row of it is
    /// the daemon's.
    Attach,
    /// After a resync snapshot: the screen is current, the history above it
    /// may miss what was skipped.
    Resync,
    /// After the session was resized (rows reflowed on both sides).
    Resize,
}

/// A range of the daemon's history, as VT bytes ([`Terminal::history_rows`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryRows {
    /// The host line of the first row in `data`.
    pub first: u64,
    /// The oldest host line the daemon still has.
    pub oldest: u64,
    /// The rows `first..before`, each ending with a newline.
    pub data: Vec<u8>,
}

/// What a client should ask the daemon for ([`Terminal::wanted_history`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryWanted {
    /// Fetch the lines just before this host line…
    pub before: u64,
    /// …at most this many.
    pub count: u32,
}

/// Lines per fetch: about 100 KB of VT text, a few frames to insert.
pub const FETCH_CHUNK: u32 = 1_000;

/// The most rows one fetch returns.
pub const MAX_FETCH: usize = 5_000;

/// Which host line each row of the mirror shows.
#[derive(Debug, Default)]
pub(crate) struct LineMap {
    /// `(row, host line)` pairs sorted by row, where `row` is the mirror's
    /// own absolute row number (evicted rows + position). A pair maps its
    /// row and the rows after it, up to the next pair.
    anchors: Vec<(u64, u64)>,
    /// Host lines before this are gone on the daemon.
    oldest: u64,
    /// The columns changed and rows reflowed: the map is unknown until the
    /// daemon's next top line. `true` if the history had a gap, which a
    /// reflow loses track of.
    resized: Option<bool>,
}

impl LineMap {
    /// Fold anchors whose rows were evicted into the oldest row.
    fn normalize(&mut self, evicted: u64) {
        while self.anchors.len() > 1 && self.anchors[1].0 <= evicted {
            self.anchors.remove(0);
        }
        if let Some(a) = self.anchors.first_mut()
            && a.0 < evicted
        {
            *a = (evicted, a.1 + (evicted - a.0));
        }
        // Drop anchors that only continue the one before.
        let mut i = 1;
        while i < self.anchors.len() {
            let (c0, h0) = self.anchors[i - 1];
            let (c1, h1) = self.anchors[i];
            if h0 + (c1 - c0) == h1 {
                self.anchors.remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn has_gap(&self) -> bool {
        self.anchors.windows(2).any(|w| w[1].1 > w[0].1 + (w[1].0 - w[0].0))
    }
}

/// Room for older lines above the mirror's oldest row, when the daemon has
/// some and nothing unmapped is above (the map is normalized).
fn older_room(t: &Term, map: &LineMap, evicted: u64) -> Option<u64> {
    let (c0, h0) = *map.anchors.first()?;
    let room = (DEFAULT_SCROLLBACK as u64).saturating_sub(t.history_size() as u64);
    (c0 == evicted && h0 > map.oldest && room > 0).then_some(room)
}

/// The mirror's own absolute numbers: of its oldest row, and of its screen's top row.
fn own_rows(t: &Term) -> (u64, u64) {
    let evicted = t.grid.lines_evicted();
    (evicted, evicted + t.history_size() as u64)
}

impl Terminal {
    // ---- daemon side ------------------------------------------------------

    /// The host line of the primary screen's top row, or `None` while the
    /// alternate screen is active (its rows have no history).
    pub fn top_line(&self) -> Option<u64> {
        self.with_term(|t| (!TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN)).then(|| own_rows(t).1))
    }

    /// Up to `count` (at most [`MAX_FETCH`]) rows just before host line
    /// `before`, as VT bytes for [`Terminal::insert_history`]. Empty when
    /// the daemon has none of them any more, or while the alternate screen
    /// is active.
    pub fn history_rows(&self, before: u64, count: usize) -> HistoryRows {
        self.with_term(|t| {
            let (oldest, top) = own_rows(t);
            if TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN) {
                return HistoryRows { first: before, oldest, data: Vec::new() };
            }
            let end = before.min(top + t.screen_lines() as u64);
            let first = end.saturating_sub(count.min(MAX_FETCH) as u64).max(oldest);
            if first >= end {
                return HistoryRows { first: before, oldest, data: Vec::new() };
            }
            let line = |h: u64| (h as i64 - top as i64) as i32;
            let mut out = String::new();
            write_rows(t, line(first)..line(end), true, &mut out);
            HistoryRows { first, oldest, data: out.into_bytes() }
        })
    }

    // ---- client side ------------------------------------------------------

    /// The daemon says its screen's top row is host line `line`, right after
    /// the bytes fed so far (see the module docs).
    pub fn set_top_line(&self, line: u64, cause: TopLineCause) {
        let t = self.inner.term.lock();
        if TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN) {
            // The primary screen's rows aren't where this applies.
            return;
        }
        let (evicted, top) = own_rows(&t);
        let mut map = self.inner.lines.lock();
        let to_oldest = (evicted, line.saturating_sub(top - evicted));
        match (cause, map.resized.take()) {
            // Every row was replayed by the daemon, in order.
            (TopLineCause::Attach, _) => {
                map.anchors = vec![to_oldest];
                map.oldest = 0;
            }
            // Rows reflowed the same on both sides: one run again.
            (TopLineCause::Resize, Some(false)) => map.anchors = vec![to_oldest],
            // A reflow lost where the gap was (or a resync came first): only
            // the screen is known, and the rows above it can't take older
            // lines in the right place.
            (_, Some(_)) => map.anchors = vec![(top, line)],
            (_, None) => map.anchors.push((top, line)),
        }
        map.anchors.retain(|a| a.0 <= top);
        map.anchors.sort_unstable();
        map.normalize(evicted);
    }

    /// Lines to fetch from the daemon because the viewport is within
    /// `margin` rows of missing history: older lines above the oldest row,
    /// or lines a resync skipped. `None` when nothing's missing nearby, or
    /// the daemon never said where its rows are.
    pub fn wanted_history(&self, margin: usize) -> Option<HistoryWanted> {
        let t = self.inner.term.lock();
        if TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let mut map = self.inner.lines.lock();
        if map.resized.is_some() || map.anchors.is_empty() {
            return None;
        }
        let (evicted, top) = own_rows(&t);
        map.normalize(evicted);
        let view_top = top - t.display_offset() as u64;
        let view_bottom = view_top + t.screen_lines() as u64;
        let margin = margin as u64;
        let near = |row: u64| row + margin >= view_top && row <= view_bottom + margin;
        let chunk = |n: u64| n.min(FETCH_CHUNK as u64) as u32;
        // A skipped stretch between two runs (newest first).
        for w in map.anchors.windows(2).rev() {
            let ((c0, h0), (c1, h1)) = (w[0], w[1]);
            // Not what the daemon dropped meanwhile.
            let expected = (h0 + (c1 - c0)).max(map.oldest);
            if h1 > expected && near(c1) {
                return Some(HistoryWanted { before: h1, count: chunk(h1 - expected) });
            }
        }
        // Older lines, if there's room for them and nothing unmapped above.
        let (c0, h0) = map.anchors[0];
        if let Some(room) = older_room(&t, &map, evicted)
            && near(c0)
        {
            return Some(HistoryWanted { before: h0, count: chunk((h0 - map.oldest).min(room)) });
        }
        None
    }

    /// Whether the daemon has lines older than this mirror's oldest row
    /// that [`wanted_history`](Self::wanted_history) would fetch once the
    /// view is near that row. Call it with the terminal locked (`t`).
    pub(crate) fn older_history_missing(&self, t: &Term) -> bool {
        if TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let mut map = self.inner.lines.lock();
        if map.resized.is_some() || map.anchors.is_empty() {
            return false;
        }
        let evicted = own_rows(t).0;
        map.normalize(evicted);
        older_room(t, &map, evicted).is_some()
    }

    /// Put rows from the daemon (`rows`: [`HistoryRows`] fetched for host
    /// line `before`) into the history, right above the row showing
    /// `before`. Returns false when the map changed meanwhile and they no
    /// longer fit anywhere (fetch again). Call it on the thread that feeds
    /// the terminal.
    pub fn insert_history(&self, before: u64, rows: &HistoryRows) -> bool {
        // No output gets in between: the grid is rebuilt from a copy.
        let _feeding = self.inner.parser.lock();
        let (bytes, at, size, offset) = {
            let t = self.inner.term.lock();
            let mut map = self.inner.lines.lock();
            map.oldest = map.oldest.max(rows.oldest);
            if TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN) || map.resized.is_some() {
                return false;
            }
            let (evicted, top) = own_rows(&t);
            map.normalize(evicted);
            let Some(&(at, _)) = map.anchors.iter().find(|a| a.1 == before) else { return false };
            if rows.data.is_empty() {
                return true;
            }
            // Old rows above `at`, the new ones, the rest down to the screen's
            // last row, which lands on the bottom (as in an attach snapshot).
            let split = (at as i64 - top as i64) as i32;
            let mut out = String::new();
            let history = -(t.history_size() as i32);
            if split > history {
                write_rows(&t, history..split, true, &mut out);
            }
            let mut bytes = out.into_bytes();
            bytes.extend_from_slice(&rows.data);
            let mut rest = String::new();
            write_rows(&t, split..t.screen_lines() as i32, false, &mut rest);
            bytes.extend_from_slice(rest.as_bytes());
            let size = TermSize::new(t.columns() as u16, t.screen_lines() as u16);
            (bytes, at, size, t.display_offset())
        };

        let (scratch, _chans) = Terminal::new(size, false, Arc::new(Palette::default()));
        scratch.feed(&bytes);
        let mut grid = scratch.with_term(|s| std::mem::replace(&mut s.grid, Grid::new(1, 1, 0)));

        let mut t = self.inner.term.lock();
        let mut map = self.inner.lines.lock();
        // `resize` doesn't wait for the parser lock: a grid of the old size
        // must not replace the resized one.
        if (t.columns() as u16, t.screen_lines() as u16) != (size.cols, size.rows) || map.resized.is_some() {
            return false;
        }
        let (_, old_top) = own_rows(&t);
        let old_rows = t.history_size() as u64 + t.screen_lines() as u64;
        // Keep the cursor, its pen and the saved cursor. Style and extras
        // ids belong to a grid's own tables: re-intern the pen from its
        // value, and drop an open hyperlink (rio-vt's `swap_alt` does the same).
        for (new, old) in [(&mut grid.cursor, &t.grid.cursor), (&mut grid.saved_cursor, &t.grid.saved_cursor)] {
            *new = old.clone();
            new.style_dirty = true;
            new.template.set_extras_id(None);
        }
        grid.sync_template_style();
        let new_evicted = grid.lines_evicted();
        let new_top = new_evicted + grid.history_size() as u64;
        // Rows the new grid ever had, minus the old ones: what got inserted.
        let added = (new_evicted + grid.history_size() as u64 + grid.screen_lines() as u64).saturating_sub(old_rows);
        t.grid = grid;
        t.selection = None;
        // The offset counts from the bottom: it only grows when rows went in
        // below the viewport's top.
        let view_top = old_top - offset as u64;
        let offset = if view_top >= at { offset as u64 } else { offset as u64 + added };
        t.scroll_display(Scroll::Delta(offset.min(i32::MAX as u64) as i32));
        t.mark_fully_damaged();

        // Rows below `at` kept their place under the screen's top; the rows
        // above it moved up by `added`.
        let moved = |row: u64| {
            let line = row as i64 - old_top as i64;
            let line = if row >= at { line } else { line - added as i64 };
            (new_top as i64 + line).max(0) as u64
        };
        for a in map.anchors.iter_mut() {
            a.0 = moved(a.0);
        }
        let first_row = moved(at).saturating_sub(added);
        map.anchors.push((first_row, rows.first));
        map.anchors.sort_unstable();
        map.normalize(new_evicted);
        true
    }

    /// Note a resize for the line map (see [`LineMap::resized`]).
    pub(crate) fn line_map_resized(&self, cols_changed: bool) {
        if cols_changed {
            let mut map = self.inner.lines.lock();
            let gap = map.has_gap() || map.resized == Some(true);
            map.resized = Some(gap);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn term(size: TermSize, daemon: bool) -> Terminal {
        Terminal::new(size, daemon, Arc::new(Palette::default())).0
    }

    /// The non-empty lines of the whole transcript (history and screen).
    fn lines(t: &Terminal) -> Vec<String> {
        t.transcript().lines.into_iter().map(|l| l.text.trim_end().to_string()).filter(|l| !l.is_empty()).collect()
    }

    fn screen(t: &Terminal) -> Vec<String> {
        let mut f = crate::Frame::default();
        t.snapshot_into(&mut f);
        (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect()
    }

    fn numbered(range: std::ops::Range<usize>) -> Vec<u8> {
        range.flat_map(|i| format!("line {i}\r\n").into_bytes()).collect()
    }

    /// Fetch and insert until the client wants nothing more.
    fn fill(daemon: &Terminal, client: &Terminal) -> usize {
        let mut fetches = 0;
        client.scroll(i32::MAX / 2);
        while let Some(w) = client.wanted_history(100) {
            let rows = daemon.history_rows(w.before, w.count as usize);
            assert!(client.insert_history(w.before, &rows));
            fetches += 1;
            assert!(fetches < 50, "keeps fetching");
            client.scroll(i32::MAX / 2);
        }
        fetches
    }

    #[test]
    fn a_trimmed_attach_snapshot_fills_up_on_scroll() {
        let size = TermSize::new(30, 5);
        let a = term(size, true);
        a.feed(&numbered(0..3000));
        a.feed(b"$ ");
        let b = term(size, false);
        b.feed(&a.reattach_snapshot_with_history(100));
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        assert_eq!(b.history_size(), 100);
        // At the bottom nothing is wanted; near the top it is.
        assert_eq!(b.wanted_history(10), None);
        b.scroll(95);
        let screen_before = screen(&b);
        let w = b.wanted_history(10).expect("older lines");
        assert_eq!(w.before, a.top_line().unwrap() - 100);
        let rows = a.history_rows(w.before, w.count as usize);
        assert!(b.insert_history(w.before, &rows));
        assert_eq!(screen(&b), screen_before, "the viewport shows the same rows");
        assert_eq!(fill(&a, &b) + 1, 3);
        assert_eq!(lines(&b), lines(&a));
        assert_eq!(b.history_size(), a.history_size());
    }

    #[test]
    fn a_prompt_jump_fetches_older_history_until_it_finds_the_prompt() {
        use crate::{PromptJump, PromptJumped};
        let size = TermSize::new(30, 5);
        let a = term(size, true);
        let cmd = |t: &Terminal, name: &str, out: &[u8]| {
            t.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07{name}\r\n\x1b]133;C\x07").as_bytes());
            t.feed(out);
            t.feed(b"\x1b]133;D;0\x07");
        };
        // A prompt, then far more output than one fetch margin, then another.
        cmd(&a, "old", &numbered(0..3000));
        cmd(&a, "new", &numbered(0..5));
        a.feed(b"\x1b]133;A\x07$ ");
        let b = term(size, false);
        b.feed(&a.reattach_snapshot_with_history(100));
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        assert_eq!(b.scroll_to_prompt(PromptJump::Previous), PromptJumped::Moved);
        assert_eq!(screen(&b)[0], "$ new");
        // The app's loop: the redraw fetches near the view, then it jumps again.
        let mut fetches = 0;
        loop {
            match b.scroll_to_prompt(PromptJump::Previous) {
                PromptJumped::Fetching => {
                    let w = b.wanted_history(10).expect("the view is at the oldest row held");
                    assert!(b.insert_history(w.before, &a.history_rows(w.before, w.count as usize)));
                    fetches += 1;
                    assert!(fetches < 10, "keeps fetching");
                }
                PromptJumped::Moved => break,
                other => panic!("{other:?} before reaching the old prompt"),
            }
        }
        assert_eq!(screen(&b)[0], "$ old");
        assert_eq!(fetches, 3);
        assert_eq!(b.scroll_to_prompt(PromptJump::Previous), PromptJumped::Boundary);
    }

    #[test]
    fn lines_skipped_by_a_resync_are_fetched_into_the_gap() {
        let size = TermSize::new(30, 5);
        let a = term(size, true);
        let b = term(size, false);
        b.feed(&a.reattach_snapshot());
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        let start = numbered(0..50);
        a.feed(&start);
        b.feed(&start);
        // The client misses a burst.
        a.feed(&numbered(50..400));
        a.feed(b"$ ");
        b.feed(&a.resync_snapshot());
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Resync);
        assert!(!lines(&b).contains(&"line 200".to_string()));
        b.scroll(10);
        fill(&a, &b);
        assert_eq!(lines(&b), lines(&a));
    }

    #[test]
    fn a_gap_the_daemon_dropped_is_not_asked_for_again() {
        let size = TermSize::new(30, 5);
        let a = term(size, true);
        let b = term(size, false);
        b.feed(&a.reattach_snapshot());
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        let start = numbered(0..50);
        a.feed(&start);
        b.feed(&start);
        a.feed(&numbered(50..400));
        b.feed(&a.resync_snapshot());
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Resync);
        // The daemon's history is cleared before the client asks.
        a.feed(b"\x1b[3J");
        b.scroll(10);
        let w = b.wanted_history(100).expect("the gap");
        let rows = a.history_rows(w.before, w.count as usize);
        assert!(rows.data.is_empty() && rows.oldest >= w.before);
        assert!(b.insert_history(w.before, &rows));
        assert_eq!(b.wanted_history(100), None);
    }

    #[test]
    fn a_resize_keeps_the_numbering() {
        let a = term(TermSize::new(30, 5), true);
        a.feed(&numbered(0..500));
        let b = term(TermSize::new(30, 5), false);
        b.feed(&a.reattach_snapshot_with_history(50));
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        for t in [&a, &b] {
            t.resize(TermSize::new(20, 8));
        }
        b.scroll(1000);
        assert_eq!(b.wanted_history(10), None, "unknown until the daemon says");
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Resize);
        fill(&a, &b);
        assert_eq!(lines(&b), lines(&a));
    }

    #[test]
    fn nothing_is_fetched_without_a_top_line_or_past_the_daemons_oldest() {
        let size = TermSize::new(30, 5);
        let a = term(size, true);
        a.feed(&numbered(0..200));
        let b = term(size, false);
        b.feed(&a.reattach_snapshot());
        b.scroll(1000);
        assert_eq!(b.wanted_history(10), None, "an old daemon never says");
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        assert_eq!(b.wanted_history(10), None, "the client has it all");
        // The daemon's history was cleared: its answer stops the client asking.
        let c = term(size, false);
        c.feed(&a.reattach_snapshot_with_history(20));
        c.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        a.feed(b"\x1b[3J");
        c.scroll(1000);
        let w = c.wanted_history(10).unwrap();
        let rows = a.history_rows(w.before, w.count as usize);
        assert!(rows.data.is_empty());
        assert!(c.insert_history(w.before, &rows));
        assert_eq!(c.wanted_history(10), None);
    }

    #[test]
    fn inserting_keeps_hyperlinks_modes_and_the_pen() {
        let size = TermSize::new(30, 5);
        let a = term(size, true);
        a.feed(&numbered(0..100));
        a.feed(b"\x1b]8;id=x;https://example.com\x1b\\link\x1b]8;;\x1b\\\r\n\x1b[?2004h\x1b[31m");
        let b = term(size, false);
        b.feed(&a.reattach_snapshot_with_history(10));
        b.set_top_line(a.top_line().unwrap(), TopLineCause::Attach);
        // (Snapshots don't carry the pen.)
        b.feed(b"\x1b[31m");
        b.scroll(1000);
        fill(&a, &b);
        assert!(b.mode().contains(TermMode::BRACKETED_PASTE));
        a.feed(b"red");
        b.feed(b"red");
        let style = |t: &Terminal| {
            t.with_term(|t| {
                use rio_vt::crosswords::pos::Column;
                let cur = t.grid.cursor.pos;
                t.grid.style_of(&t.grid[cur.row][Column(cur.col.0 - 1)])
            })
        };
        assert_eq!(style(&b), style(&a), "the pen");
        let link = b.with_term(|t| {
            use rio_vt::crosswords::pos::{Column, Line};
            let row = (0..t.screen_lines() as i32).find(|&r| t.grid[Line(r)][Column(3)].c() == 'k');
            row.and_then(|r| t.cell_hyperlink(Line(r), Column(0))).map(|l| l.uri().to_string())
        });
        assert_eq!(link.as_deref(), Some("https://example.com"));
    }
}
