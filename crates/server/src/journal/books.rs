//! Bookkeeping of the segments (memory only: open() recomputes it by scanning them).
//!
//! Per segment: the games it mentions and how many of them still need it (`pins`). Per game: the
//! segments that mention it, the first segment it needs (the one of its first record, or of its
//! latest durable snapshot) and the one holding its `committed` record. A game not committed
//! needs every segment from its first one on. A segment can go when no game needs it, except that
//! a segment holding a game's `committed` record outlives every other segment mentioning that
//! game, so that a recovery never sees a committed game's records without that record.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;

use indexmap::IndexSet;

use crate::ids::GameId;

/// One segment file.
#[derive(Debug)]
pub(super) struct Segment {
    pub path: PathBuf,
    /// The games it mentions, in order of first mention.
    pub games: IndexSet<GameId>,
    /// Games not committed that still need it.
    pub pins: u32,
    /// The segment being written to (never deleted).
    pub active: bool,
    /// Size on disk.
    pub bytes: u64,
}

/// What the journal knows of one game.
#[derive(Debug)]
pub(super) struct GameBook {
    /// The segments that mention it.
    pub segs: BTreeSet<u64>,
    /// The first segment it needs.
    pub first: u64,
    /// Its `committed` record is durable.
    pub committed: bool,
    /// The segment holding its `committed` record (the latest one, if several).
    pub commit_seg: Option<u64>,
}

/// Segments, games, and the durability counters of the directory.
#[derive(Debug, Default)]
pub(super) struct Books {
    /// By number, ascending.
    pub segs: BTreeMap<u64, Segment>,
    pub games: HashMap<GameId, GameBook>,
    /// Highest segment number used.
    pub seq: u64,
    /// Bytes this process wrote to the active segment.
    pub seg_size: u64,
    /// Sum of the segment sizes.
    pub disk_bytes: u64,
    /// The next batch goes to a new segment (after a failed write).
    pub force_rotate: bool,
    /// Segments deleted so far.
    pub unlinks: u64,
    /// ... of which a directory fsync made the deletion durable.
    pub synced_unlinks: u64,
    /// Highest segment whose directory entry a directory fsync made durable.
    pub synced_seq: u64,
}

impl Books {
    /// Notes that segment `seq` mentions `game`.
    pub fn track(&mut self, game: GameId, seq: u64) {
        let g = self.games.entry(game).or_insert_with(|| GameBook {
            segs: BTreeSet::new(),
            first: seq,
            committed: false,
            commit_seg: None,
        });
        if g.segs.insert(seq)
            && let Some(seg) = self.segs.get_mut(&seq)
        {
            seg.games.insert(game);
            if !g.committed && seq >= g.first {
                seg.pins += 1;
            }
        }
    }

    /// A snapshot of `game` is durable in segment `seq`: the game no longer needs the segments
    /// before it. Returns the segments it released.
    pub fn mark_snapshot(&mut self, game: GameId, seq: u64) -> Vec<u64> {
        let Some(g) = self.games.get_mut(&game) else { return Vec::new() };
        if g.committed || seq <= g.first {
            return Vec::new();
        }
        let released: Vec<u64> = g.segs.range(g.first..seq).copied().collect();
        for s in &released {
            unpin(&mut self.segs, *s);
        }
        g.first = seq;
        released
    }

    /// The `committed` record of `game` is durable in segment `seq`. Returns the segments that
    /// mention the game.
    pub fn mark_committed(&mut self, game: GameId, seq: u64) -> Vec<u64> {
        let Some(g) = self.games.get_mut(&game) else { return Vec::new() };
        g.commit_seg = Some(g.commit_seg.map_or(seq, |c| c.max(seq)));
        if g.committed {
            return Vec::new();
        }
        g.committed = true;
        for s in g.segs.range(g.first..) {
            unpin(&mut self.segs, *s);
        }
        g.segs.iter().copied().collect()
    }

    /// Whether segment `seq` may be deleted now.
    pub fn deletable(&self, seq: u64) -> bool {
        let Some(seg) = self.segs.get(&seq) else { return false };
        if seg.active || seg.pins > 0 {
            return false;
        }
        !seg.games
            .iter()
            .any(|id| self.games.get(id).is_some_and(|g| g.commit_seg == Some(seq) && g.segs.len() > 1))
    }

    /// Whether segment `seq` holds the `committed` record of a game it mentions.
    pub fn holds_commit(&self, seq: u64) -> bool {
        self.segs.get(&seq).is_some_and(|seg| {
            seg.games.iter().any(|id| self.games.get(id).is_some_and(|g| g.commit_seg == Some(seq)))
        })
    }

    /// Forgets a deleted segment; `next` receives the segments holding a `committed` record that
    /// may be deletable now.
    pub fn forget(&mut self, seq: u64, next: &mut Vec<u64>) {
        let Some(seg) = self.segs.remove(&seq) else { return };
        self.disk_bytes = self.disk_bytes.saturating_sub(seg.bytes);
        for id in &seg.games {
            let Some(g) = self.games.get_mut(id) else { continue };
            g.segs.remove(&seq);
            if g.segs.is_empty() {
                self.games.remove(id);
            } else if g.segs.len() == 1
                && let Some(c) = g.commit_seg
                && g.segs.contains(&c)
            {
                next.push(c);
            }
        }
    }

    /// The games not committed, without a snapshot pending, that still need a segment
    /// `compact_segments` or more behind the newest one (old segments first).
    pub fn stale_games(&self, compact_segments: u64) -> Vec<GameId> {
        let Some(limit) = self.seq.checked_sub(compact_segments) else { return Vec::new() };
        let mut out = Vec::new();
        for (_, seg) in self.segs.range(..=limit) {
            if seg.pins == 0 {
                continue;
            }
            for id in &seg.games {
                if self.games.get(id).is_some_and(|g| !g.committed && g.first <= limit) {
                    out.push(*id);
                }
            }
        }
        out
    }
}

fn unpin(segs: &mut BTreeMap<u64, Segment>, seq: u64) {
    if let Some(seg) = segs.get_mut(&seq) {
        debug_assert!(seg.pins > 0, "segment {seq} unpinned below zero");
        seg.pins = seg.pins.saturating_sub(1);
    }
}

/// An insertion-ordered set of games with O(1) insert, remove and pop (a re-inserted game goes to
/// the back, like a JavaScript `Set`).
#[derive(Debug, Default)]
pub(super) struct GameQueue {
    order: VecDeque<(GameId, u64)>,
    live: HashMap<GameId, u64>,
    stamp: u64,
}

impl GameQueue {
    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    pub fn insert(&mut self, game: GameId) {
        if self.live.contains_key(&game) {
            return;
        }
        self.stamp += 1;
        self.live.insert(game, self.stamp);
        self.order.push_back((game, self.stamp));
    }

    pub fn remove(&mut self, game: GameId) {
        if self.live.remove(&game).is_some() && self.order.len() > 2 * self.live.len() + 64 {
            let live = &self.live;
            self.order.retain(|(g, s)| live.get(g) == Some(s));
        }
    }

    pub fn pop_front(&mut self) -> Option<GameId> {
        while let Some((g, s)) = self.order.pop_front() {
            if self.live.get(&g) == Some(&s) {
                self.live.remove(&g);
                return Some(g);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn books(seqs: &[u64]) -> Books {
        let mut b = Books::default();
        for &s in seqs {
            b.segs.insert(
                s,
                Segment { path: PathBuf::new(), games: IndexSet::new(), pins: 0, active: false, bytes: 10 },
            );
            b.seq = s;
        }
        b
    }

    #[test]
    fn pins_follow_tracking_snapshots_and_commits() {
        let mut b = books(&[1, 2, 3]);
        b.track(1, 1);
        b.track(1, 2);
        b.track(2, 2);
        b.track(1, 2);
        assert_eq!((b.segs[&1].pins, b.segs[&2].pins), (1, 2));
        assert_eq!(b.mark_snapshot(1, 2), vec![1]);
        assert_eq!(b.mark_snapshot(1, 2), Vec::<u64>::new(), "not newer than its first segment");
        assert!(b.deletable(1));
        b.track(1, 3);
        assert_eq!(b.mark_committed(1, 3), vec![1, 2, 3]);
        assert_eq!(b.mark_committed(1, 3), Vec::<u64>::new());
        assert_eq!((b.segs[&2].pins, b.segs[&3].pins), (1, 0));
        // Segment 3 holds game 1's committed record: it outlives segments 1 and 2.
        assert!(b.holds_commit(3) && !b.deletable(3));
        let mut next = Vec::new();
        b.forget(1, &mut next);
        assert!(next.is_empty());
        b.games.get_mut(&2).unwrap().committed = true;
        b.segs.get_mut(&2).unwrap().pins = 0;
        b.forget(2, &mut next);
        assert_eq!(next, vec![3]);
        assert!(b.deletable(3));
        b.forget(3, &mut next);
        assert!(b.games.is_empty() && b.segs.is_empty());
        assert_eq!(b.disk_bytes, 0);
    }

    #[test]
    fn game_queue_keeps_insertion_order() {
        let mut q = GameQueue::default();
        for g in [5, 3, 9, 3] {
            q.insert(g);
        }
        q.remove(3);
        q.insert(3);
        assert_eq!(q.len(), 3);
        let mut out = Vec::new();
        while let Some(g) = q.pop_front() {
            out.push(g);
        }
        assert_eq!(out, vec![5, 9, 3]);
        assert!(q.is_empty());
        for g in 0..1000 {
            q.insert(g);
            q.remove(g);
        }
        assert!(q.order.len() <= 64 + 1, "stale entries are swept");
    }
}
