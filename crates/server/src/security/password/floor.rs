//! The padding floor of failed login checks (DESIGN.md section 8): the slowest recent password
//! check, so that a failed login takes the same time whatever account, hash or dummy was behind
//! it.

use parking_lot::Mutex;

use crate::clock::SharedClock;

/// The slowest check recorded in the current period of `period_ms` or in the one before (a value
/// is remembered for one to two periods), never less than the baseline (which does not decay), at
/// most `cap_ms`. Periods follow the monotonic clock.
pub struct CheckFloor {
    cap_ms: f64,
    period_ms: f64,
    clock: SharedClock,
    state: Mutex<FloorState>,
}

struct FloorState {
    origin: f64,
    period: i64,
    cur: f64,
    prev: f64,
    baseline: f64,
}

impl std::fmt::Debug for CheckFloor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckFloor")
            .field("cap_ms", &self.cap_ms)
            .field("period_ms", &self.period_ms)
            .finish()
    }
}

impl CheckFloor {
    /// The cap of the server: 2 s.
    pub const CAP_MS: f64 = 2000.0;
    /// The period of the server: 10 minutes (a value counts for 10 to 20 minutes).
    pub const PERIOD_MS: f64 = 600_000.0;

    /// The floor of the server (2 s cap, 10-minute periods) on `clock`.
    pub fn new(clock: SharedClock) -> CheckFloor {
        CheckFloor::with_limits(Self::CAP_MS, Self::PERIOD_MS, clock)
    }

    /// A floor with its own cap and period (tests).
    pub fn with_limits(cap_ms: f64, period_ms: f64, clock: SharedClock) -> CheckFloor {
        let origin = clock.mono_ms();
        CheckFloor {
            cap_ms,
            period_ms,
            clock,
            state: Mutex::new(FloorState { origin, period: 0, cur: 0.0, prev: 0.0, baseline: 0.0 }),
        }
    }

    fn roll(&self, st: &mut FloorState) {
        let p = ((self.clock.mono_ms() - st.origin) / self.period_ms).floor() as i64;
        if p == st.period {
            return;
        }
        st.prev = if p == st.period + 1 { st.cur } else { 0.0 };
        st.cur = 0.0;
        st.period = p;
    }

    /// Records the duration of one check, in ms.
    pub fn record(&self, ms: f64) {
        let mut st = self.state.lock();
        self.roll(&mut st);
        if ms > st.cur {
            st.cur = ms;
        }
    }

    /// Sets the part of the floor that does not decay (the warm-up's measure of the slowest kind
    /// of verification), in ms; a value below the current baseline (or not finite) is ignored.
    pub fn set_baseline(&self, ms: f64) {
        let mut st = self.state.lock();
        if ms.is_finite() && ms > st.baseline {
            st.baseline = ms;
        }
    }

    /// The baseline, in ms.
    pub fn baseline_ms(&self) -> f64 {
        self.state.lock().baseline
    }

    /// The duration a failed check is padded to, in ms.
    pub fn floor_ms(&self) -> f64 {
        let mut st = self.state.lock();
        self.roll(&mut st);
        st.cur.max(st.prev).max(st.baseline).min(self.cap_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn the_slowest_check_of_the_current_or_previous_period_capped() {
        let clock = ManualClock::new(1000.0, 0);
        let f = CheckFloor::with_limits(500.0, 100.0, clock.clone());
        assert_eq!(f.floor_ms(), 0.0);
        f.record(30.0);
        f.record(80.0);
        f.record(40.0);
        assert_eq!(f.floor_ms(), 80.0);
        clock.advance(100.0); // next period: the previous one still counts
        f.record(20.0);
        assert_eq!(f.floor_ms(), 80.0);
        clock.advance(100.0); // one period later again: only the last period's 20
        assert_eq!(f.floor_ms(), 20.0);
        clock.advance(250.0); // a gap of more than one period forgets everything
        assert_eq!(f.floor_ms(), 0.0);
        f.record(9000.0);
        assert_eq!(f.floor_ms(), 500.0, "capped");
    }

    #[test]
    fn the_warm_up_baseline_does_not_decay() {
        let clock = ManualClock::new(1000.0, 0);
        let f = CheckFloor::with_limits(500.0, 100.0, clock.clone());
        assert_eq!(f.baseline_ms(), 0.0);
        f.set_baseline(60.0);
        assert_eq!(f.floor_ms(), 60.0, "the floor of a fresh server, before any check");
        f.record(20.0);
        assert_eq!(f.floor_ms(), 60.0, "a faster check does not lower it");
        f.record(90.0);
        assert_eq!(f.floor_ms(), 90.0, "a slower recent check raises it");
        clock.advance(100.0); // one period roll: the slower check still counts
        assert_eq!(f.floor_ms(), 90.0);
        clock.advance(100.0); // two period rolls: the recent checks are forgotten, the baseline stays
        assert_eq!(f.floor_ms(), 60.0);
        clock.advance(1000.0); // a long quiet time
        assert_eq!(f.floor_ms(), 60.0);
        f.set_baseline(40.0); // a lower measure does not lower it
        f.set_baseline(f64::NAN);
        f.set_baseline(f64::INFINITY);
        assert_eq!((f.baseline_ms(), f.floor_ms()), (60.0, 60.0));
        f.set_baseline(9000.0);
        assert_eq!(f.floor_ms(), 500.0, "capped");
    }

    #[test]
    fn server_limits() {
        let f = CheckFloor::new(ManualClock::new(0.0, 0));
        f.record(5000.0);
        assert_eq!(f.floor_ms(), 2000.0);
    }
}
