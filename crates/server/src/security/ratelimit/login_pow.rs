//! The global login failure-rate detector that turns on the login proof of work (DESIGN.md
//! section 8, "Brute force and stuffing"): when the server sees `POW_LOGIN_TRIGGER_PER_MIN`
//! failed password checks within a minute, `POST /auth/login` and the password step of a Google
//! link ask for a proof of work of `POW_LOGIN_BITS` for the next 5 minutes (each new trigger
//! extends it).

use std::sync::Arc;

use parking_lot::Mutex;

use super::control::LocalControl;
use super::counters::SlidingWindowCounter;
use crate::clock::SharedClock;
use crate::config::Config;

/// How long the login proof of work stays on after the last trigger, in ms.
pub const POW_LOGIN_HOLD_MS: i64 = 300_000;

/// The control key of the server-wide failed-login count.
pub const LOGIN_FAILURES_KEY: &str = "auth:login-failures";

/// Which count turned the login proof of work on (the `source` of the `login_pow_on` event).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowSource {
    /// The detector's own sliding count (not rounded).
    Local,
    /// The control's `auth:login-failures` window (rounded up, so usually the first to trigger).
    Global,
}

impl PowSource {
    /// The `source` value of the `login_pow_on` event.
    pub fn as_str(self) -> &'static str {
        match self {
            PowSource::Local => "local",
            PowSource::Global => "global",
        }
    }
}

/// The detector: a sliding count of failed password checks over a minute and the time until
/// which the login proof of work is required.
pub struct LoginPowDetector {
    bits: u32,
    trigger: u32,
    clock: SharedClock,
    recent: SlidingWindowCounter,
    control: Arc<LocalControl>,
    until: Mutex<i64>,
}

impl std::fmt::Debug for LoginPowDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginPowDetector").field("bits", &self.bits).field("trigger", &self.trigger).finish()
    }
}

impl LoginPowDetector {
    /// A detector requiring `bits` (0: never) once `trigger_per_min` failures are seen in a
    /// minute.
    pub fn new(
        bits: u32,
        trigger_per_min: u32,
        control: Arc<LocalControl>,
        clock: SharedClock,
    ) -> LoginPowDetector {
        LoginPowDetector {
            bits,
            trigger: trigger_per_min,
            recent: SlidingWindowCounter::new(60_000, clock.clone()),
            clock,
            control,
            until: Mutex::new(i64::MIN),
        }
    }

    /// The detector of `POW_LOGIN_BITS` and `POW_LOGIN_TRIGGER_PER_MIN`.
    pub fn from_config(config: &Config, control: Arc<LocalControl>, clock: SharedClock) -> LoginPowDetector {
        let clamp = |v: i64| u32::try_from(v.max(0)).unwrap_or(u32::MAX);
        LoginPowDetector::new(
            clamp(config.pow_login_bits),
            clamp(config.pow_login_trigger_per_min),
            control,
            clock,
        )
    }

    /// The difficulty asked while active.
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// True while the login proof of work is required.
    pub fn active(&self) -> bool {
        self.bits > 0 && self.clock.now_ms() < *self.until.lock()
    }

    /// Turns the proof of work on (or extends it); returns the source when it was off and this
    /// turned it on (the caller records `login_pow_on`).
    fn activate(&self, source: PowSource) -> Option<PowSource> {
        let was = self.active();
        *self.until.lock() = self.clock.now_ms() + POW_LOGIN_HOLD_MS;
        (!was && self.bits > 0).then_some(source)
    }

    /// Counts one failed password check (a login or a Google link step). Returns the source when
    /// this failure turned the login proof of work on: the caller records the security event
    /// `login_pow_on {source}`.
    pub fn note_failure(&self) -> Option<PowSource> {
        let trigger = f64::from(self.trigger);
        let mut turned_on = None;
        if self.recent.add(1.0) >= trigger {
            turned_on = self.activate(PowSource::Local);
        }
        let r = self.control.take(LOGIN_FAILURES_KEY, self.trigger, 60_000, 1);
        if !r.allowed || r.count >= i64::from(self.trigger) {
            // Always called: a global trigger extends the hold even when the local one fired.
            let global = self.activate(PowSource::Global);
            turned_on = turned_on.or(global);
        }
        turned_on
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn detector(bits: u32, trigger: u32) -> (Arc<ManualClock>, LoginPowDetector) {
        let clock = ManualClock::new(1_790_596_800_000.0, 1_790_596_800_000);
        let control = Arc::new(LocalControl::new(clock.clone()));
        (clock.clone(), LoginPowDetector::new(bits, trigger, control, clock))
    }

    #[test]
    fn a_wave_of_failures_turns_the_proof_of_work_on_for_five_minutes() {
        let (clock, d) = detector(16, 30);
        assert!(!d.active());
        let mut events = vec![];
        for _ in 0..40 {
            if let Some(s) = d.note_failure() {
                events.push(s);
            }
        }
        assert!(d.active());
        assert_eq!(events.len(), 1, "one login_pow_on event per activation");
        clock.advance((POW_LOGIN_HOLD_MS - 1) as f64);
        assert!(d.active());
        clock.advance(1.0);
        assert!(!d.active(), "off 5 minutes after the last trigger");
    }

    #[test]
    fn the_local_count_is_checked_first() {
        let (_, d) = detector(16, 3);
        assert_eq!(d.note_failure(), None);
        assert_eq!(d.note_failure(), None);
        // The third failure reaches the trigger in both counts; the local one is checked first
        // and the global one only extends the hold.
        assert_eq!(d.note_failure(), Some(PowSource::Local));
        assert_eq!(PowSource::Local.as_str(), "local");
    }

    #[test]
    fn the_rounded_global_count_can_trigger_alone() {
        let (clock, d) = detector(16, 3);
        assert_eq!(d.note_failure(), None);
        assert_eq!(d.note_failure(), None);
        // One second into the next window the two earlier failures weigh 59/60 each: the local
        // count (2.97) stays below the trigger, the global one rounds up to 3.
        clock.advance(61_000.0);
        assert_eq!(d.note_failure(), Some(PowSource::Global));
        assert!(d.active());
        assert_eq!(PowSource::Global.as_str(), "global");
    }

    #[test]
    fn bits_zero_never_asks() {
        let (_, d) = detector(0, 1);
        for _ in 0..5 {
            assert_eq!(d.note_failure(), None);
        }
        assert!(!d.active());
        let cfg = Config::for_tests();
        let clock = crate::clock::system();
        let d = LoginPowDetector::from_config(&cfg, Arc::new(LocalControl::new(clock.clone())), clock);
        assert_eq!((d.bits(), d.trigger), (0, 30));
    }
}
