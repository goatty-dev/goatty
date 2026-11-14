//! The notifications shown recently, to read again after their toast is
//! gone (#84). No UI and no handlers (buttons aren't kept): level, text,
//! when, and how often in a row. The overlay that lists them is
//! [`super::recent`].
//!
//! What gets in follows what the toasts did, so the list reads like what
//! was on screen:
//! - Work in progress ([`Level::Loading`]) isn't kept; its result is (an
//!   [`update`](super::update) of the same toast).
//! - A toast updated in place replaces its entry.
//! - A toast with a `key` replaces the last entry with that key ("Text size
//!   16 px" replaces "Text size 15 px"), and counts repeats.
//! - The newest [`CAPACITY`] are kept.

use std::collections::VecDeque;
use std::time::SystemTime;

use gpui::SharedString;

use super::{Level, Toast, ToastId};

/// How many notifications are kept.
pub const CAPACITY: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The toast it came from (the last one, for a key shown repeatedly).
    pub id: ToastId,
    pub level: Level,
    pub title: SharedString,
    pub body: Option<SharedString>,
    pub key: Option<SharedString>,
    /// When it was (last) shown.
    pub at: SystemTime,
    /// How often in a row it said the same thing.
    pub count: u32,
}

impl Entry {
    /// What "Copy" puts on the clipboard: the title, and the body below it.
    pub fn text(&self) -> String {
        match &self.body {
            Some(b) => format!("{}\n{b}", self.title),
            None => self.title.to_string(),
        }
    }

    /// Whether every word of `query` (lowercase) is in its text.
    pub fn matches(&self, query: &str) -> bool {
        let text = self.text().to_lowercase();
        query.split_whitespace().all(|w| text.contains(w))
    }

    fn same_content(&self, t: &Toast) -> bool {
        self.level == t.level && self.title == t.title && self.body == t.body
    }
}

#[derive(Debug, Default)]
pub struct History {
    /// Oldest first.
    entries: VecDeque<Entry>,
    /// Bumped on every change, so a view knows when to refresh.
    version: u64,
}

impl History {
    /// Newest first.
    pub fn newest_first(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Remember `toast`, shown (or updated) as `id` at `at`.
    pub fn record(&mut self, id: ToastId, toast: &Toast, at: SystemTime) {
        if toast.level == Level::Loading {
            return;
        }
        let earlier = self
            .entries
            .iter()
            .rposition(|e| e.id == id)
            .or_else(|| toast.key.as_ref().and_then(|k| self.entries.iter().rposition(|e| e.key.as_ref() == Some(k))));
        let count = match earlier.and_then(|i| self.entries.remove(i)) {
            // The same toast updated in place is one event, not a repeat.
            Some(e) if e.id != id && e.same_content(toast) => e.count + 1,
            _ => 1,
        };
        self.entries.push_back(Entry {
            id,
            level: toast.level,
            title: toast.title.clone(),
            body: toast.body.clone(),
            key: toast.key.clone(),
            at,
            count,
        });
        while self.entries.len() > CAPACITY {
            self.entries.pop_front();
        }
        self.version += 1;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.version += 1;
    }
}

/// "just now", "5 min ago", "3 h ago", "2 d ago": how long before `now`.
pub fn ago(at: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(at).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn titles(h: &History) -> Vec<String> {
        h.newest_first().map(|e| e.title.to_string()).collect()
    }

    #[test]
    fn keeps_results_not_progress() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut h = History::default();
        h.record(ToastId(1), &Toast::loading("Reloading shells…"), t0);
        assert_eq!(h.len(), 0);
        h.record(ToastId(1), &Toast::error("Couldn't reload shells").body("no daemon"), t0);
        h.record(ToastId(2), &Toast::success("Copied"), t0);
        assert_eq!(titles(&h), ["Copied", "Couldn't reload shells"]);
        assert_eq!(h.newest_first().nth(1).unwrap().text(), "Couldn't reload shells\nno daemon");
    }

    #[test]
    fn updates_replace_their_entry() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut h = History::default();
        h.record(ToastId(1), &Toast::info("Step 1"), t0);
        h.record(ToastId(2), &Toast::info("other"), t0);
        h.record(ToastId(1), &Toast::success("Done"), t0 + Duration::from_secs(5));
        assert_eq!(titles(&h), ["Done", "other"]);
        // An update repeating the same text isn't a repeat.
        h.record(ToastId(1), &Toast::success("Done"), t0);
        assert_eq!(h.newest_first().next().unwrap().count, 1);
    }

    #[test]
    fn keys_replace_and_count_repeats() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut h = History::default();
        h.record(ToastId(1), &Toast::info("Text size 15 px").key("zoom"), t0);
        h.record(ToastId(2), &Toast::info("other"), t0);
        h.record(ToastId(3), &Toast::info("Text size 16 px").key("zoom"), t0);
        assert_eq!(titles(&h), ["Text size 16 px", "other"]);
        h.record(ToastId(4), &Toast::info("Text size 16 px").key("zoom"), t0);
        assert_eq!(h.newest_first().next().unwrap().count, 2);
        h.record(ToastId(5), &Toast::info("Text size 17 px").key("zoom"), t0);
        assert_eq!(h.newest_first().next().unwrap().count, 1);
        // Without a key, the same text twice is two entries.
        h.record(ToastId(6), &Toast::info("other"), t0);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn keeps_the_newest() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut h = History::default();
        for i in 0..CAPACITY as u64 + 5 {
            h.record(ToastId(i), &Toast::info(format!("t{i}")), t0);
        }
        assert_eq!(h.len(), CAPACITY);
        assert_eq!(h.newest_first().next().unwrap().title, format!("t{}", CAPACITY + 4));
        assert_eq!(h.newest_first().last().unwrap().title, "t5");
        let v = h.version();
        h.clear();
        assert_eq!(h.len(), 0);
        assert!(h.version() > v);
    }

    #[test]
    fn filtering_and_ages() {
        let e = Entry {
            id: ToastId(1),
            level: Level::Error,
            title: "Couldn't save settings".into(),
            body: Some("Permission denied".into()),
            key: None,
            at: SystemTime::UNIX_EPOCH,
            count: 1,
        };
        assert!(e.matches("save denied"));
        assert!(e.matches(""));
        assert!(!e.matches("theme"));
        let t0 = SystemTime::UNIX_EPOCH;
        assert_eq!(ago(t0, t0 + Duration::from_secs(5)), "just now");
        assert_eq!(ago(t0, t0 + Duration::from_secs(150)), "2 min ago");
        assert_eq!(ago(t0, t0 + Duration::from_secs(7300)), "2 h ago");
        assert_eq!(ago(t0 + Duration::from_secs(9), t0), "just now");
    }
}
