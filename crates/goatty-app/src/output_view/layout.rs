//! Where text goes in the output viewer, in terminal columns (pure,
//! unit-tested). The viewer draws a monospace font, so a character's width
//! is its column count: 1, 2 for wide characters (CJK, most emoji), 0 for
//! combining marks.
//!
//! Unwrapped, every line is one row and the view scrolls sideways. Wrapped,
//! a line is cut into rows of at most the view's width, after a space when
//! the cut would split a word that starts in the row's second half.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// The byte offsets where the rows of `s` start when wrapped at `cols`
/// columns. Always starts with 0; an empty line is one row. Spaces never
/// start a row: they hang past the end of the one before (column-aligned
/// output like `docker ps` would otherwise wrap into rows of blanks).
pub fn wrap_points(s: &str, cols: usize) -> Vec<usize> {
    let cols = cols.max(1);
    let mut points = vec![0];
    let mut row_start = 0;
    let mut width = 0;
    // The last place in this row the next row could start after a space,
    // with the width up to it.
    let mut soft: Option<(usize, usize)> = None;
    for (i, c) in s.char_indices() {
        let w = char_width(c);
        if c == ' ' {
            width += w;
            soft = Some((i + 1, width));
            continue;
        }
        if width + w > cols && i > row_start {
            let (at, used) = match soft {
                Some((at, used)) if used * 2 >= cols && at > row_start => (at, used),
                _ => (i, width),
            };
            points.push(at);
            row_start = at;
            width -= used;
            soft = None;
        }
        width += w;
    }
    points
}

/// How many rows `s` takes wrapped at `cols` columns.
pub fn row_count(s: &str, cols: usize) -> usize {
    if s.len() <= cols {
        // No character is wider in columns than in UTF-8 bytes.
        return 1;
    }
    wrap_points(s, cols).len()
}

/// The bytes of `s` that show in columns `from..to`, and the column the
/// first of them starts at (before `from` when a wide character is cut).
pub fn column_window(s: &str, from: usize, to: usize) -> (Range<usize>, usize) {
    let mut col = 0;
    let mut start = None;
    for (i, c) in s.char_indices() {
        let w = char_width(c);
        if start.is_none() && col + w > from {
            start = Some((i, col));
        }
        if col >= to {
            let (s0, c0) = start.unwrap_or((i, col));
            return (s0..i, c0);
        }
        col += w;
    }
    let (s0, c0) = start.unwrap_or((s.len(), col));
    (s0..s.len(), c0)
}

/// The first visual row of each shown line (prefix sums), for mapping
/// between rows and lines.
#[derive(Debug, Clone, Default)]
pub struct Rows {
    /// `starts[i]`: the first row of line `i`; one more entry at the end.
    starts: Vec<usize>,
}

impl Rows {
    /// One row per line.
    pub fn unwrapped(lines: usize) -> Self {
        Self { starts: (0..=lines).collect() }
    }

    pub fn from_counts(counts: impl Iterator<Item = usize>) -> Self {
        let mut starts = vec![0];
        let mut total = 0;
        for n in counts {
            total += n.max(1);
            starts.push(total);
        }
        Self { starts }
    }

    pub fn total(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    pub fn lines(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The first row of line `i`.
    pub fn first_row(&self, i: usize) -> usize {
        self.starts[i.min(self.lines())]
    }

    /// The line a row belongs to, and which of its rows it is.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total() {
            return None;
        }
        let i = self.starts.partition_point(|&s| s <= row) - 1;
        Some((i, row - self.starts[i]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(s: &str, cols: usize) -> Vec<&str> {
        let p = wrap_points(s, cols);
        p.iter().enumerate().map(|(i, &a)| &s[a..p.get(i + 1).copied().unwrap_or(s.len())]).collect()
    }

    #[test]
    fn wraps_long_words_by_character_and_text_after_spaces() {
        assert_eq!(rows("", 4), [""]);
        assert_eq!(rows("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(rows("error: connection refused", 12), ["error: ", "connection ", "refused"]);
        // A space early in the row isn't worth a short row.
        assert_eq!(rows("a bcdefghij", 6), ["a bcde", "fghij"]);
        assert_eq!(rows("abcd", 4), ["abcd"]);
        // Runs of spaces hang at the end of a row instead of making rows of their own.
        assert_eq!(rows("PORTS          NAMES", 8), ["PORTS          ", "NAMES"]);
        assert_eq!(rows("ab      cdefgh", 4), ["ab      ", "cdef", "gh"]);
        assert_eq!(rows("    indented", 8), ["    ", "indented"]);
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(display_width("日本a"), 5);
        assert_eq!(rows("日本語です", 4), ["日本", "語で", "す"]);
        // A wide character doesn't fit in the last column.
        assert_eq!(rows("a日本", 2), ["a", "日", "本"]);
        assert_eq!(row_count("日本語です", 4), 3);
        assert_eq!(row_count("short", 80), 1);
    }

    #[test]
    fn column_windows() {
        assert_eq!(column_window("abcdef", 2, 4), (2..4, 2));
        assert_eq!(column_window("abc", 5, 9), (3..3, 3));
        assert_eq!(column_window("abcdef", 0, 100), (0..6, 0));
        // Column 1 is the right half of 日: start with it, at column 0.
        assert_eq!(column_window("日本", 1, 3), (0..6, 0));
        assert_eq!(column_window("日本", 2, 4), (3..6, 2));
    }

    #[test]
    fn rows_map_to_lines() {
        let r = Rows::from_counts([1, 3, 0, 2].into_iter());
        assert_eq!(r.total(), 7);
        assert_eq!(r.locate(0), Some((0, 0)));
        assert_eq!(r.locate(3), Some((1, 2)));
        assert_eq!(r.locate(4), Some((2, 0)));
        assert_eq!(r.locate(6), Some((3, 1)));
        assert_eq!(r.locate(7), None);
        assert_eq!(r.first_row(3), 5);
        assert_eq!(Rows::unwrapped(3).locate(2), Some((2, 0)));
    }
}
