//! The timers of a host: one deadline per game, in an ordered set.
//!
//! A pass collects every timer due at its time ([`TimerSet::due`]); the host then fires each one
//! it can still [`claim`](TimerSet::claim): a timer cancelled or rescheduled by an earlier firing
//! of the same pass is skipped, and a timer rescheduled at or before the pass's time fires on the
//! next pass, never twice in one (no loop). Firing order within a pass is by deadline, which no
//! caller relies on.
//!
//! Scheduling, cancelling and claiming are O(log n).

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ids::GameId;

/// One deadline per game.
#[derive(Debug, Default)]
pub struct TimerSet {
    /// Scheduled timers by deadline.
    queue: BTreeSet<(i64, GameId)>,
    /// Deadline of each scheduled timer.
    deadlines: HashMap<GameId, i64>,
    /// Timers collected by the current pass and not fired yet.
    due: HashSet<GameId>,
}

impl TimerSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        TimerSet::default()
    }

    /// Number of pending timers (scheduled, or due and not fired yet).
    #[must_use]
    pub fn len(&self) -> usize {
        self.deadlines.len() + self.due.len()
    }

    /// No pending timer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `game` has a pending timer.
    #[must_use]
    pub fn is_scheduled(&self, game: GameId) -> bool {
        self.deadlines.contains_key(&game) || self.due.contains(&game)
    }

    /// Deadline of `game`'s scheduled timer.
    #[must_use]
    pub fn deadline_of(&self, game: GameId) -> Option<i64> {
        self.deadlines.get(&game).copied()
    }

    /// The earliest scheduled deadline.
    #[must_use]
    pub fn next_deadline(&self) -> Option<i64> {
        self.queue.first().map(|&(d, _)| d)
    }

    /// (Re)schedules `game` at `deadline`; `None` cancels its timer.
    pub fn schedule(&mut self, game: GameId, deadline: Option<i64>) {
        self.cancel(game);
        if let Some(d) = deadline {
            self.deadlines.insert(game, d);
            self.queue.insert((d, game));
        }
    }

    /// Cancels the timer of `game`; returns whether one was pending.
    pub fn cancel(&mut self, game: GameId) -> bool {
        if let Some(d) = self.deadlines.remove(&game) {
            self.queue.remove(&(d, game));
            return true;
        }
        self.due.remove(&game)
    }

    /// Starts a pass at `now`: removes and returns every timer due at `now` (deadline <= now), in
    /// deadline order, each to [`claim`](TimerSet::claim) before firing it. Timers left unclaimed
    /// by an earlier pass (a firing that failed) are forgotten.
    pub fn due(&mut self, now: i64) -> Vec<(GameId, i64)> {
        self.due.clear();
        let mut fired = Vec::new();
        while let Some(&(d, game)) = self.queue.first() {
            if d > now {
                break;
            }
            self.queue.pop_first();
            self.deadlines.remove(&game);
            self.due.insert(game);
            fired.push((game, d));
        }
        fired
    }

    /// Takes a timer collected by the current pass: false when an earlier firing of the pass
    /// cancelled or rescheduled it (then it must not fire).
    pub fn claim(&mut self, game: GameId) -> bool {
        self.due.remove(&game)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pass at `now`: the games fired.
    fn collect(timers: &mut TimerSet, now: i64) -> Vec<GameId> {
        let mut got = Vec::new();
        for (game, _) in timers.due(now) {
            if timers.claim(game) {
                got.push(game);
            }
        }
        got.sort_unstable();
        got
    }

    const A: GameId = 1;
    const B: GameId = 2;
    const C: GameId = 3;

    #[test]
    fn fires_at_the_deadline_not_before_and_cancel_and_reschedule_are_exact() {
        let mut w = TimerSet::new();
        w.schedule(A, Some(105));
        w.schedule(B, Some(105));
        w.schedule(C, Some(300));
        assert_eq!(w.len(), 3);
        assert!(collect(&mut w, 104).is_empty());
        assert_eq!(collect(&mut w, 105), [A, B]);
        assert_eq!(w.len(), 1);
        assert!(w.cancel(C));
        assert!(!w.cancel(C));
        assert!(w.is_empty());
        w.schedule(A, Some(400));
        w.schedule(A, Some(350)); // reschedule replaces
        assert_eq!(w.len(), 1);
        assert_eq!(w.deadline_of(A), Some(350));
        assert_eq!(w.next_deadline(), Some(350));
        assert!(collect(&mut w, 349).is_empty());
        assert_eq!(collect(&mut w, 351), [A]);
        w.schedule(B, None);
        assert!(!w.is_scheduled(B));
        assert_eq!(w.deadline_of(B), None);
    }

    #[test]
    fn far_deadlines_and_catch_up_after_a_pause() {
        let mut w = TimerSet::new();
        w.schedule(10, Some(1003));
        w.schedule(11, Some(45));
        let mut fired = Vec::new();
        for t in (0..=1100).step_by(10) {
            fired.extend(collect(&mut w, t).into_iter().map(|g| (g, t)));
        }
        assert_eq!(fired, [(11, 50), (10, 1010)]);
        // A long pause: everything due fires on the next pass.
        for i in 0..20 {
            w.schedule(100 + i, Some(2000 + i as i64 * 37));
        }
        assert_eq!(collect(&mut w, 10_000).len(), 20);
        assert!(w.is_empty());
    }

    #[test]
    fn a_deadline_in_the_past_fires_on_the_next_pass_and_firings_may_reschedule_without_looping() {
        let mut w = TimerSet::new();
        w.schedule(A, Some(5)); // long past
        assert_eq!(collect(&mut w, 1000), [A]);
        for g in [A, B, C] {
            w.schedule(g, Some(1010));
        }
        // The first one fired reschedules itself in the past and cancels the two others while
        // they are due.
        let mut fired = Vec::new();
        for (game, _) in w.due(1010) {
            if !w.claim(game) {
                continue;
            }
            fired.push(game);
            w.schedule(game, Some(1000));
            for x in [A, B, C] {
                if x != game {
                    w.cancel(x);
                }
            }
        }
        assert_eq!(fired.len(), 1);
        assert_eq!(w.len(), 1);
        assert_eq!(collect(&mut w, 1011), fired);
    }

    #[test]
    fn an_unclaimed_timer_of_a_failed_pass_is_forgotten() {
        let mut w = TimerSet::new();
        w.schedule(A, Some(10));
        w.schedule(B, Some(10));
        assert_eq!(w.due(10).len(), 2);
        assert!(w.claim(A));
        assert_eq!(w.len(), 1);
        assert!(collect(&mut w, 20).is_empty());
        assert!(w.is_empty());
    }

    #[test]
    fn random_operations_agree_with_a_naive_model() {
        let mut seed: u32 = 12345;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            seed % n
        };
        let mut w = TimerSet::new();
        let mut model: HashMap<GameId, i64> = HashMap::new();
        let mut t: i64 = 0;
        for _ in 0..4000 {
            let op = rnd(10);
            let game = GameId::from(rnd(3000));
            if op < 5 {
                let d = t + i64::from(rnd(20000));
                w.schedule(game, Some(d));
                model.insert(game, d);
            } else if op < 6 {
                w.cancel(game);
                model.remove(&game);
            } else {
                t += i64::from(rnd(50));
                let got = collect(&mut w, t);
                let mut want: Vec<GameId> = model.iter().filter(|&(_, &d)| d <= t).map(|(&g, _)| g).collect();
                want.sort_unstable();
                for g in &want {
                    model.remove(g);
                }
                assert_eq!(got, want);
            }
            assert_eq!(w.len(), model.len());
        }
    }
}
