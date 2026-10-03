//! Port of test/unit/match.conduct.test.js. The lobby's flow (cache in front of the store, one
//! store transaction per incident) is reproduced by [`Harness`].

use std::collections::HashMap;
use std::convert::Infallible;

use super::*;
use crate::config::Config;

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = CONDUCT_WINDOW_MS;

#[derive(Default)]
struct Calls {
    cooldown: u32,
    count_since: u32,
    set_cooldown: u32,
}

// In-memory stand-in for the store's conduct tables.
#[derive(Default)]
struct FakeStore {
    incidents: Vec<(UserId, IncidentKind, i64)>,
    cooldowns: HashMap<UserId, Cooldown>,
    calls: Calls,
}

impl ConductStore for FakeStore {
    type Error = Infallible;

    fn record(&mut self, user_id: UserId, kind: IncidentKind, at: i64) -> Result<(), Infallible> {
        self.incidents.push((user_id, kind, at));
        Ok(())
    }

    fn count_since(&mut self, user_id: UserId, since: i64) -> Result<IncidentCounts, Infallible> {
        self.calls.count_since += 1;
        let mut out = IncidentCounts::default();
        for &(u, kind, at) in &self.incidents {
            if u == user_id && at >= since {
                match kind {
                    IncidentKind::Abandon => out.abandon += 1,
                    IncidentKind::Abort => out.abort += 1,
                    IncidentKind::NoShow => out.noshow += 1,
                }
            }
        }
        Ok(out)
    }

    fn cooldown(&mut self, user_id: UserId) -> Result<Option<Cooldown>, Infallible> {
        self.calls.cooldown += 1;
        Ok(self.cooldowns.get(&user_id).copied())
    }

    fn set_cooldown(&mut self, user_id: UserId, cooldown: Cooldown) -> Result<(), Infallible> {
        self.calls.set_cooldown += 1;
        self.cooldowns.insert(user_id, cooldown);
        Ok(())
    }
}

// The lobby: a Conduct cache in front of the store.
struct Harness {
    store: FakeStore,
    conduct: Conduct,
    limit: i64,
}

impl Harness {
    fn new() -> Harness {
        Harness::with(FakeStore::default(), Config::for_tests().conduct_abandon_limit)
    }

    fn with(store: FakeStore, limit: i64) -> Harness {
        Harness { store, conduct: Conduct::new(), limit }
    }

    fn record(&mut self, user_id: UserId, kind: IncidentKind, now: i64) -> IncidentOutcome {
        let Ok(out) = record_incident(&mut self.store, user_id, kind, self.limit, now);
        self.conduct.remember(user_id, out.state, now);
        out
    }

    fn load(&mut self, user_id: UserId, now: i64) {
        if self.conduct.state(user_id).is_none() {
            let Ok(row) = self.store.cooldown(user_id);
            self.conduct.remember(user_id, row.unwrap_or_default(), now);
        }
    }

    fn cooldown_until(&mut self, user_id: UserId, now: i64) -> i64 {
        self.load(user_id, now);
        self.conduct.cooldown_until(user_id, now).unwrap()
    }

    fn state(&mut self, user_id: UserId) -> Cooldown {
        self.load(user_id, 0);
        self.conduct.state(user_id).unwrap()
    }
}

use IncidentKind::{Abandon, Abort, NoShow};

#[test]
fn below_the_limit_nothing_happens() {
    let mut h = Harness::new();
    let r = h.record(1, Abandon, 0);
    assert_eq!(
        (r.until, r.level, r.incidents, r.started),
        (0, 0, 1, false),
        "the limit of the test configuration is 3"
    );
    let r = h.record(1, Abort, 10 * MIN);
    assert!(!r.started);
    assert_eq!(h.cooldown_until(1, 10 * MIN), 0);
    assert_eq!(h.store.incidents.len(), 2);
    assert_eq!(h.store.calls.set_cooldown, 0);
}

#[test]
fn the_limit_in_24_h_pauses_rated_matchmaking_15_min_then_1_h_then_6_h() {
    let mut h = Harness::new();
    h.record(1, Abandon, 0);
    h.record(1, NoShow, HOUR);
    let r = h.record(1, Abort, 2 * HOUR);
    assert_eq!(
        r,
        IncidentOutcome {
            until: 2 * HOUR + 15 * MIN,
            level: 1,
            incidents: 3,
            started: true,
            state: Cooldown { until: 2 * HOUR + 15 * MIN, level: 1 }
        }
    );
    assert_eq!(h.store.cooldowns[&1], Cooldown { until: 2 * HOUR + 15 * MIN, level: 1 });
    assert_eq!(h.cooldown_until(1, 2 * HOUR), 2 * HOUR + 15 * MIN);
    assert_eq!(h.cooldown_until(1, 2 * HOUR + 15 * MIN), 0);
    // Repeated offences.
    let r = h.record(1, Abandon, 5 * HOUR);
    assert_eq!((r.until, r.level), (6 * HOUR, 2));
    let r = h.record(1, Abandon, 7 * HOUR);
    assert_eq!((r.until, r.level), (13 * HOUR, 3));
    let r = h.record(1, Abandon, 14 * HOUR);
    assert_eq!((r.until, r.level), (20 * HOUR, 3));
    assert_eq!(CONDUCT_COOLDOWNS_MS, [15 * MIN, HOUR, 6 * HOUR]);
    // Other users are not affected.
    assert_eq!(h.cooldown_until(2, 14 * HOUR), 0);
}

#[test]
fn incidents_older_than_24_h_do_not_count() {
    let mut h = Harness::new();
    h.record(1, Abandon, 0);
    h.record(1, Abandon, 12 * HOUR);
    let r = h.record(1, Abandon, DAY + 1); // the first one left the window
    assert_eq!(r.incidents, 2);
    assert!(!r.started);
    assert_eq!(h.cooldown_until(1, DAY + 1), 0);
}

#[test]
fn an_active_cooldown_is_never_shortened() {
    let mut h = Harness::new();
    for i in 0..5 {
        h.record(1, Abandon, i * MIN); // levels 1, 2, 3: 6 h from 4 min
    }
    assert_eq!(h.cooldown_until(1, 5 * MIN), 4 * MIN + 6 * HOUR);
    let mut store = FakeStore::default();
    store.cooldowns.insert(7, Cooldown { until: 10 * DAY, level: 0 }); // e.g. set by an administrator
    let mut c = Harness::with(store, 3);
    for i in 0..3 {
        c.record(7, NoShow, i);
    }
    assert_eq!(c.cooldown_until(7, 3), 10 * DAY);
    assert_eq!(c.state(7).level, 1);
}

#[test]
fn the_level_decays_by_one_per_full_day_without_incident() {
    let mut h = Harness::new();
    // Three cooldowns in a row: level 3.
    for i in 0..5 {
        h.record(1, Abandon, i * HOUR);
    }
    assert_eq!(h.state(1).level, 3);
    // Two clean days (the last incident was at 4 h): one more incident brings the level to 1.
    let mut t = 4 * HOUR + 2 * DAY + HOUR;
    let r = h.record(1, Abandon, t);
    assert_eq!(r.level, 1);
    assert!(!r.started);
    assert_eq!(h.store.cooldowns[&1].level, 1, "decay persisted");
    // Two more incidents within 24 h: limit reached at level 1 -> 1 h, level 2.
    h.record(1, Abandon, t + HOUR);
    let r = h.record(1, Abandon, t + 2 * HOUR);
    assert!(r.started);
    assert_eq!(r.until, t + 3 * HOUR);
    assert_eq!(r.level, 2);
    // Three clean days or more: back to the first cooldown (15 min).
    t += 2 * HOUR + 5 * DAY;
    h.record(1, Abandon, t);
    h.record(1, Abandon, t + 1);
    let r = h.record(1, Abandon, t + 2);
    assert_eq!(r.until, t + 2 + 15 * MIN);
    assert_eq!(r.level, 1);
    // An incident inside the last 24 h stops the decay.
    let mut b = Harness::new();
    for i in 0..4 {
        b.record(2, Abandon, i * HOUR); // level 2
    }
    b.record(2, Abandon, 30 * HOUR); // 27 h clean: level 1
    assert_eq!(b.state(2).level, 1);
    b.record(2, Abandon, 40 * HOUR); // 10 h since the last: stays 1
    assert_eq!(b.state(2).level, 1);
}

#[test]
fn survives_a_restart_through_the_store_reads_are_cached() {
    let mut a = Harness::new();
    for i in 0..3 {
        a.record(1, Abandon, i);
    }
    let until = a.cooldown_until(1, 3);
    assert_eq!(until, 2 + 15 * MIN);
    // A new process on the same database.
    let mut b = Harness::with(std::mem::take(&mut a.store), 3);
    assert_eq!(b.cooldown_until(1, 5), until);
    assert_eq!(b.state(1).level, 1);
    let reads = b.store.calls.cooldown;
    for _ in 0..100 {
        b.cooldown_until(1, 10);
    }
    for _ in 0..100 {
        b.cooldown_until(2, 10);
    }
    assert_eq!(b.store.calls.cooldown, reads + 1, "user 2 read once, user 1 cached");
    // invalidate() forgets the cache (e.g. after an administrator cleared the cooldown).
    b.store.cooldowns.remove(&1);
    assert_eq!(b.cooldown_until(1, 10), until);
    b.conduct.invalidate(1);
    assert_eq!(b.cooldown_until(1, 10), 0);
    b.conduct.invalidate_all();
    assert!(b.conduct.is_empty());
    assert_eq!(b.cooldown_until(2, 10), 0);
}

#[test]
fn an_incident_reads_the_stored_state_even_when_the_cache_is_stale() {
    let mut h = Harness::new();
    for i in 0..3 {
        h.record(1, Abandon, i);
    }
    // An administrator clears the cooldown in the database; the cache still has it.
    h.store.cooldowns.remove(&1);
    assert_eq!(h.cooldown_until(1, 10), 2 + 15 * MIN);
    // The next incident starts from the stored state (level 0): 15 min again, not 1 h.
    let r = h.record(1, Abandon, HOUR);
    assert_eq!((r.until, r.level), (HOUR + 15 * MIN, 1));
    assert_eq!(h.cooldown_until(1, HOUR), HOUR + 15 * MIN);
}

#[test]
fn stored_rows_kinds_and_configurable_limit() {
    assert_eq!(Cooldown::normalized(5000, 7), Cooldown { until: 5000, level: 3 });
    assert_eq!(Cooldown::normalized(5000, -2), Cooldown { until: 5000, level: 0 });
    let mut store = FakeStore::default();
    store.cooldowns.insert(1, Cooldown { until: 5000, level: 9 });
    let mut c = Harness::with(store, 3);
    assert_eq!(c.cooldown_until(1, 0), 5000);
    let r = c.record(1, Abandon, 0);
    assert_eq!(r.level, 0, "an out-of-range stored level is clamped, then decays over clean days");

    assert_eq!(IncidentKind::parse("rage"), None);
    for kind in IncidentKind::ALL {
        assert_eq!(IncidentKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(IncidentKind::NoShow.to_string(), "noshow");

    let mut one = Harness::with(FakeStore::default(), 1);
    let r = one.record(9, NoShow, 100);
    assert!(r.started);
    assert_eq!(r.until, 100 + 15 * MIN);
}

#[test]
fn a_full_cache_drops_idle_states_first() {
    let mut c = Conduct::with_cap(4);
    c.remember(1, Cooldown { until: 1000, level: 1 }, 0);
    c.remember(2, Cooldown::default(), 0);
    c.remember(3, Cooldown { until: 50, level: 0 }, 0);
    c.remember(4, Cooldown::default(), 0);
    c.remember(4, Cooldown { until: 70, level: 0 }, 0); // an update never trims
    assert_eq!(c.len(), 4);
    c.remember(5, Cooldown { until: 200, level: 0 }, 60); // 2 and 3 are idle at 60
    assert_eq!(c.len(), 3);
    assert_eq!((c.state(2), c.state(3)), (None, None));
    assert_eq!(c.cooldown_until(4, 60), Some(70));
    c.remember(6, Cooldown { until: 0, level: 2 }, 60);
    c.remember(7, Cooldown::default(), 60); // nothing idle: everything goes
    assert_eq!(c.len(), 1);
    assert_eq!(c.cooldown_until(7, 60), Some(0));
}
