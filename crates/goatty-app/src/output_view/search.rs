//! Searching the output viewer's lines (pure, unit-tested): plain text or a
//! regular expression, ignoring case unless asked not to. Plain text goes
//! through the regex engine too (escaped), which folds case correctly for
//! any script and reports byte offsets into the original line.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

/// Stop collecting matches past this many; the count says "10000+".
pub const MAX_MATCHES: usize = 10_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub case_sensitive: bool,
    pub regex: bool,
}

/// One match: a line (index into the transcript) and the bytes in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub line: usize,
    pub range: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct Matcher(Regex);

impl Matcher {
    /// `Ok(None)` for an empty query, `Err` with a short reason for an
    /// invalid regular expression.
    pub fn new(q: &Query) -> Result<Option<Self>, String> {
        if q.text.is_empty() {
            return Ok(None);
        }
        let pattern = if q.regex { q.text.clone() } else { regex::escape(&q.text) };
        RegexBuilder::new(&pattern)
            .case_insensitive(!q.case_sensitive)
            .size_limit(1 << 22)
            .build()
            .map(|r| Some(Self(r)))
            .map_err(|e| short_error(&e))
    }

    pub fn is_match(&self, line: &str) -> bool {
        self.0.is_match(line)
    }

    /// Every match in `line`, skipping empty ones (`a*` matches everywhere).
    pub fn find_in<'a>(&'a self, line: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
        self.0.find_iter(line).map(|m| m.range()).filter(|r| !r.is_empty())
    }
}

/// Matches in the given lines, in order, up to [`MAX_MATCHES`]. Returns
/// whether it stopped early.
pub fn find_all<'a>(m: &Matcher, lines: impl Iterator<Item = (usize, &'a str)>, out: &mut Vec<Match>) -> bool {
    out.clear();
    for (line, text) in lines {
        for range in m.find_in(text) {
            if out.len() == MAX_MATCHES {
                return true;
            }
            out.push(Match { line, range });
        }
    }
    false
}

/// regex's errors are multi-line with a caret diagram; keep the reason.
fn short_error(e: &regex::Error) -> String {
    match e {
        regex::Error::Syntax(s) => s.lines().rev().find_map(|l| l.strip_prefix("error: ")).unwrap_or("invalid pattern").to_string(),
        regex::Error::CompiledTooBig(_) => "pattern too large".into(),
        _ => "invalid pattern".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> Query {
        Query { text: text.into(), ..Query::default() }
    }

    fn ranges(m: &Matcher, s: &str) -> Vec<Range<usize>> {
        m.find_in(s).collect()
    }

    #[test]
    fn plain_text_ignores_case_and_special_characters() {
        let m = Matcher::new(&q("Error.")).unwrap().unwrap();
        assert_eq!(ranges(&m, "error. ERROR. errorX"), [0..6, 7..13]);
        let m = Matcher::new(&Query { case_sensitive: true, ..q("Error") }).unwrap().unwrap();
        assert_eq!(ranges(&m, "error Error"), vec![6..11]);
        // Case folding that changes byte lengths still reports the original bytes.
        let m = Matcher::new(&q("straße")).unwrap().unwrap();
        assert_eq!(ranges(&m, "Die STRAßE"), vec![4..11]);
        assert!(Matcher::new(&q("")).unwrap().is_none());
    }

    #[test]
    fn regular_expressions() {
        let m = Matcher::new(&Query { regex: true, ..q(r"\d{3}") }).unwrap().unwrap();
        assert_eq!(ranges(&m, "GET /x 404 12ms 500"), [7..10, 16..19]);
        // Empty matches don't count.
        let m = Matcher::new(&Query { regex: true, ..q("x*") }).unwrap().unwrap();
        assert_eq!(ranges(&m, "axxb"), vec![1..3]);
        let err = Matcher::new(&Query { regex: true, ..q("(unclosed") }).unwrap_err();
        assert!(err.contains("unclosed"), "{err}");
        assert!(!err.contains('\n'));
    }

    #[test]
    fn find_all_stops_at_the_limit() {
        let m = Matcher::new(&q("a")).unwrap().unwrap();
        let line = "a".repeat(MAX_MATCHES + 5);
        let mut out = Vec::new();
        assert!(find_all(&m, [(3, line.as_str())].into_iter(), &mut out));
        assert_eq!(out.len(), MAX_MATCHES);
        assert_eq!(out[1], Match { line: 3, range: 1..2 });
        assert!(!find_all(&m, [(0, "bab"), (1, "aa")].into_iter(), &mut out));
        assert_eq!(out, [Match { line: 0, range: 1..2 }, Match { line: 1, range: 0..1 }, Match { line: 1, range: 1..2 }]);
    }
}
