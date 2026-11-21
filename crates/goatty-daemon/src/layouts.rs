//! Window layouts (#118): what each GUI window showed (tabs, splits, which
//! session sits where), so starting goatty again restores the whole window
//! instead of one session.
//!
//! The layout itself is the client's JSON; the daemon only knows which
//! sessions it shows and which connection holds it. A layout belongs to at
//! most one connection: the one that saved it last or claimed it. When that
//! connection ends, the layout is free and the next window that asks
//! ([`Layouts::claim`]) gets it. Layouts live as long as the daemon (their
//! sessions do too), and carry over to the next one on a restart
//! ([`Layouts::export`]).

use std::collections::HashMap;

use goatty_proto::{LayoutId, LayoutInfo, SessionId};
use serde::{Deserialize, Serialize};

/// Larger layouts are ignored (a window with hundreds of panes is still far
/// below this).
pub(crate) const MAX_JSON: usize = 256 << 10;
/// Free layouts beyond this many are forgotten, oldest first.
const MAX_FREE: usize = 32;

struct Record {
    json: String,
    sessions: Vec<SessionId>,
    owner: Option<u64>,
    saved_unix: u64,
    /// Save order (newest wins a claim).
    seq: u64,
}

#[derive(Default)]
pub(crate) struct Layouts {
    map: HashMap<LayoutId, Record>,
    seq: u64,
}

/// A layout as the next goattyd gets it (see `crate::restore`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedLayout {
    pub id: LayoutId,
    pub json: String,
    pub sessions: Vec<SessionId>,
    pub saved_unix: u64,
    pub seq: u64,
}

impl Layouts {
    /// Every layout, for the next goattyd.
    pub(crate) fn export(&self) -> Vec<SavedLayout> {
        let mut list: Vec<SavedLayout> = self
            .map
            .iter()
            .map(|(&id, r)| SavedLayout { id, json: r.json.clone(), sessions: r.sessions.clone(), saved_unix: r.saved_unix, seq: r.seq })
            .collect();
        list.sort_by_key(|l| l.seq);
        list
    }

    /// The previous goattyd's layouts, all free: its connections are gone.
    pub(crate) fn import(list: Vec<SavedLayout>) -> Self {
        let seq = list.iter().map(|l| l.seq).max().unwrap_or(0);
        let map = list
            .into_iter()
            .map(|l| (l.id, Record { json: l.json, sessions: l.sessions, owner: None, saved_unix: l.saved_unix, seq: l.seq }))
            .collect();
        Self { map, seq }
    }

    /// Store `conn`'s layout `id` (and give it to `conn`). No sessions:
    /// forget it.
    pub(crate) fn save(&mut self, conn: u64, id: LayoutId, sessions: Vec<SessionId>, json: String, now_unix: u64) {
        if sessions.is_empty() {
            self.map.remove(&id);
            return;
        }
        if json.len() > MAX_JSON {
            log::warn!("connection {conn}: layout of {} bytes ignored", json.len());
            return;
        }
        self.seq += 1;
        self.map.insert(id, Record { json, sessions, owner: Some(conn), saved_unix: now_unix, seq: self.seq });
        self.trim();
    }

    /// Give `conn` the newest free layout that still shows a running
    /// session, with only those sessions listed. Free layouts without any
    /// are dropped.
    pub(crate) fn claim(&mut self, conn: u64, alive: impl Fn(SessionId) -> bool) -> Option<LayoutInfo> {
        self.map.retain(|_, r| r.owner.is_some() || r.sessions.iter().any(|&s| alive(s)));
        let (&id, record) = self.map.iter_mut().filter(|(_, r)| r.owner.is_none()).max_by_key(|(_, r)| r.seq)?;
        record.owner = Some(conn);
        record.sessions.retain(|&s| alive(s));
        Some(LayoutInfo { id, json: record.json.clone(), sessions: record.sessions.clone(), saved_unix: record.saved_unix })
    }

    /// Give `conn` layout `id` when it's free and still shows a running
    /// session (with only those sessions listed).
    pub(crate) fn claim_id(&mut self, conn: u64, id: LayoutId, alive: impl Fn(SessionId) -> bool) -> Option<LayoutInfo> {
        let record = self.map.get_mut(&id).filter(|r| r.owner.is_none())?;
        record.sessions.retain(|&s| alive(s));
        if record.sessions.is_empty() {
            self.map.remove(&id);
            return None;
        }
        record.owner = Some(conn);
        Some(LayoutInfo { id, json: record.json.clone(), sessions: record.sessions.clone(), saved_unix: record.saved_unix })
    }

    /// The free layouts that still show a running session, newest first,
    /// with only those sessions listed. Claims nothing.
    pub(crate) fn list_free(&self, alive: impl Fn(SessionId) -> bool) -> Vec<LayoutInfo> {
        let mut free: Vec<(u64, LayoutInfo)> = self
            .map
            .iter()
            .filter(|(_, r)| r.owner.is_none())
            .filter_map(|(&id, r)| {
                let sessions: Vec<SessionId> = r.sessions.iter().copied().filter(|&s| alive(s)).collect();
                (!sessions.is_empty()).then(|| (r.seq, LayoutInfo { id, json: r.json.clone(), sessions, saved_unix: r.saved_unix }))
            })
            .collect();
        free.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        free.into_iter().map(|(_, l)| l).collect()
    }

    /// `conn` lets go of layout `id` (detached tabs): free, and the newest.
    pub(crate) fn release_one(&mut self, conn: u64, id: LayoutId, now_unix: u64) {
        let Some(record) = self.map.get_mut(&id).filter(|r| r.owner == Some(conn)) else { return };
        record.owner = None;
        self.seq += 1;
        record.seq = self.seq;
        record.saved_unix = now_unix;
        self.trim();
    }

    /// `conn` ended: its layouts are free for the next window.
    pub(crate) fn release(&mut self, conn: u64) {
        for r in self.map.values_mut().filter(|r| r.owner == Some(conn)) {
            r.owner = None;
        }
        self.trim();
    }

    fn trim(&mut self) {
        let mut free: Vec<(u64, LayoutId)> = self.map.iter().filter(|(_, r)| r.owner.is_none()).map(|(&id, r)| (r.seq, id)).collect();
        if free.len() <= MAX_FREE {
            return;
        }
        free.sort_unstable();
        for (_, id) in &free[..free.len() - MAX_FREE] {
            self.map.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(l: &mut Layouts, conn: u64, id: LayoutId, sessions: &[SessionId]) {
        l.save(conn, id, sessions.to_vec(), format!("layout {id}"), 0);
    }

    #[test]
    fn a_layout_is_claimed_once_its_window_is_gone() {
        let mut l = Layouts::default();
        save(&mut l, 1, 10, &[1, 2]);
        // Still held by connection 1.
        assert!(l.claim(2, |_| true).is_none());
        l.release(1);
        let got = l.claim(2, |_| true).unwrap();
        assert_eq!((got.id, got.json.as_str(), got.sessions), (10, "layout 10", vec![1, 2]));
        // A second window doesn't get the same layout.
        assert!(l.claim(3, |_| true).is_none());
    }

    #[test]
    fn newest_free_layout_wins_and_dead_sessions_drop_out() {
        let mut l = Layouts::default();
        save(&mut l, 1, 10, &[1]);
        save(&mut l, 2, 20, &[2, 3]);
        save(&mut l, 3, 30, &[4]);
        for c in 1..=3 {
            l.release(c);
        }
        // 30 is newest but its only session ended: dropped.
        let got = l.claim(9, |s| s != 4 && s != 3).unwrap();
        assert_eq!((got.id, got.sessions), (20, vec![2]));
        assert_eq!(l.claim(9, |s| s != 4).unwrap().id, 10);
        assert!(l.claim(9, |_| true).is_none());
    }

    #[test]
    fn listing_shows_free_layouts_and_claiming_by_id_takes_that_one() {
        let mut l = Layouts::default();
        save(&mut l, 1, 10, &[1, 2]);
        save(&mut l, 2, 20, &[3]);
        save(&mut l, 3, 30, &[4]);
        l.release(1);
        l.release(2);
        // 30 is still held; 20's session ended.
        let list = l.list_free(|s| s != 3);
        assert_eq!(list.iter().map(|i| i.id).collect::<Vec<_>>(), [10]);
        l.release(3);
        let list = l.list_free(|s| s != 2);
        assert_eq!(list.iter().map(|i| (i.id, i.sessions.clone())).collect::<Vec<_>>(), [(30, vec![4]), (20, vec![3]), (10, vec![1])]);
        // Listing claims nothing; claiming by id skips the newer ones.
        let got = l.claim_id(9, 10, |s| s != 2).unwrap();
        assert_eq!((got.id, got.sessions), (10, vec![1]));
        assert!(l.claim_id(8, 10, |_| true).is_none(), "held by 9 now");
        assert!(l.claim_id(8, 77, |_| true).is_none(), "unknown");
        // A layout whose sessions all ended is dropped.
        assert!(l.claim_id(8, 20, |s| s != 3).is_none());
        assert!(!l.map.contains_key(&20));
        assert_eq!(l.claim(8, |_| true).unwrap().id, 30);
    }

    #[test]
    fn releasing_one_layout_frees_it_as_the_newest() {
        let mut l = Layouts::default();
        save(&mut l, 1, 10, &[1]);
        save(&mut l, 2, 20, &[2]);
        l.release(2);
        // Only the owner can let go.
        l.release_one(2, 10, 5);
        assert_eq!(l.list_free(|_| true).iter().map(|i| i.id).collect::<Vec<_>>(), [20]);
        l.release_one(1, 10, 5);
        let free = l.list_free(|_| true);
        assert_eq!(free.iter().map(|i| (i.id, i.saved_unix)).collect::<Vec<_>>(), [(10, 5), (20, 0)]);
    }

    #[test]
    fn saving_without_sessions_forgets_and_saving_reclaims() {
        let mut l = Layouts::default();
        save(&mut l, 1, 10, &[1]);
        save(&mut l, 1, 10, &[]);
        l.release(1);
        assert!(l.claim(2, |_| true).is_none());
        // A reconnected window saving again holds its layout again.
        save(&mut l, 1, 11, &[1]);
        l.release(1);
        save(&mut l, 5, 11, &[1]);
        assert!(l.claim(2, |_| true).is_none());
    }

    #[test]
    fn layouts_carry_over_free() {
        let mut l = Layouts::default();
        save(&mut l, 1, 10, &[1]);
        save(&mut l, 2, 20, &[2]);
        let mut next = Layouts::import(l.export());
        // The newest one first, though both were held before.
        assert_eq!(next.claim(5, |_| true).unwrap().id, 20);
        assert_eq!(next.claim(6, |_| true).unwrap().id, 10);
        save(&mut next, 7, 30, &[3]);
        assert_eq!(next.export().last().unwrap().id, 30, "save order continues");
    }

    #[test]
    fn oversized_and_surplus_layouts_are_not_kept() {
        let mut l = Layouts::default();
        l.save(1, 1, vec![1], "x".repeat(MAX_JSON + 1), 0);
        assert!(l.map.is_empty());
        for id in 0..(MAX_FREE as u64 + 5) {
            save(&mut l, id, id, &[1]);
            l.release(id);
        }
        assert_eq!(l.map.len(), MAX_FREE);
        assert_eq!(l.claim(99, |_| true).unwrap().id, MAX_FREE as u64 + 4);
    }
}
