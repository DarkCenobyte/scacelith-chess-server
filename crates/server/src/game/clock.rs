//! Server-authoritative chess clock of one game (DESIGN 6.1), and the reconnection graces.
//!
//! Everything here is deterministic: the caller passes the time in. Every stored time and clock
//! value is an integer number of milliseconds (the room floors the host's fractional time at its
//! entry points), so the values the journal records are exact and a replay rebuilds identical
//! clocks.
//!
//! Rules implemented:
//! * plies 0 and 1 (each side's first move) run no clock and add no increment; each has
//!   `FIRST_MOVE_TIMEOUT_MS` from its turn start (`NoShow` otherwise), and its deadline has the
//!   same margin as a flag, `min(quota, rtt + 50, LAG_COMP_MAX_MS)`, so that a first move sent in
//!   time over a slow link still counts;
//! * later moves: `elapsed = recv - turn_start`, `lag = elapsed - clamp(think_ms, 0, elapsed)`,
//!   `comp = min(lag, rtt + 50, LAG_COMP_MAX_MS, quota)`, `charged = elapsed - comp`; the move
//!   flags when `remaining - charged <= 0`, otherwise `remaining -= charged` then `+= inc`;
//!   `quota -= comp`, then `quota = min(quota + LAG_QUOTA_GAIN_MS, LAG_QUOTA_MAX_MS)`;
//! * the flag deadline is `turn_start + remaining + min(quota, rtt + 50, LAG_COMP_MAX_MS)`: the
//!   latest moment a move could still arrive in time. A move arriving at or after it flags;
//! * `think_ms > elapsed + 100` is impossible for an honest client (`clock_implausible`);
//!   `think_ms` never adds time: it is clamped to `elapsed` and only bounds the compensation.
//!
//! Every operation is O(1) and allocation-free.

use crate::config::Config;

use super::rules::Side;

/// Margin above the measured elapsed time before a client's `thinkMs` is called implausible.
pub const IMPLAUSIBLE_MARGIN_MS: i64 = 100;
/// Added to the round-trip average to bound the lag compensation of one move.
pub const RTT_EXTRA_MS: i64 = 50;
/// The round-trip average is capped here (a client delaying its pongs gains nothing more).
pub const RTT_EMA_MAX_MS: f64 = 2000.0;
/// Weight of a new round-trip sample in the exponential moving average.
pub const RTT_EMA_ALPHA: f64 = 0.25;
/// Round trip assumed before the first measurement of a player.
pub const INITIAL_RTT_MS: i64 = 100;

/// Clock settings of a game (the defaults are the configuration's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockPolicy {
    /// `FIRST_MOVE_TIMEOUT_MS`.
    pub first_move_ms: i64,
    /// `LAG_COMP_MAX_MS`.
    pub lag_comp_max_ms: i64,
    /// `LAG_QUOTA_INITIAL_MS`.
    pub quota_initial_ms: i64,
    /// `LAG_QUOTA_GAIN_MS`.
    pub quota_gain_ms: i64,
    /// `LAG_QUOTA_MAX_MS`.
    pub quota_max_ms: i64,
}

impl Default for ClockPolicy {
    fn default() -> Self {
        ClockPolicy {
            first_move_ms: 30000,
            lag_comp_max_ms: 1000,
            quota_initial_ms: 2000,
            quota_gain_ms: 100,
            quota_max_ms: 3000,
        }
    }
}

impl ClockPolicy {
    /// The clock settings of the configuration (negative values read as 0).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        ClockPolicy {
            first_move_ms: config.first_move_timeout_ms.max(0),
            lag_comp_max_ms: config.lag_comp_max_ms.max(0),
            quota_initial_ms: config.lag_quota_initial_ms.max(0),
            quota_gain_ms: config.lag_quota_gain_ms.max(0),
            quota_max_ms: config.lag_quota_max_ms.max(0),
        }
    }
}

/// Reconnection graces (DESIGN 6.4) and the clock hold of a restored game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GracePolicy {
    /// `RECONNECT_GRACE_MIN_MS`.
    pub min_ms: i64,
    /// `RECONNECT_GRACE_MAX_MS`.
    pub max_ms: i64,
    /// `RECOVERY_GRACE_MS`.
    pub recovery_ms: i64,
    /// `RECOVERY_CLOCK_HOLD_MS`.
    pub recovery_hold_ms: i64,
}

impl Default for GracePolicy {
    fn default() -> Self {
        GracePolicy { min_ms: 15000, max_ms: 60000, recovery_ms: 90000, recovery_hold_ms: 20000 }
    }
}

impl GracePolicy {
    /// The grace settings of the configuration.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        GracePolicy {
            min_ms: config.reconnect_grace_min_ms,
            max_ms: config.reconnect_grace_max_ms,
            recovery_ms: config.recovery_grace_ms.max(0),
            recovery_hold_ms: config.recovery_clock_hold_ms.max(0),
        }
    }

    /// Reconnection grace of a time control: `clamp(base / 10, min, max)` (floored).
    #[must_use]
    pub fn grace_for(&self, base_ms: i64) -> i64 {
        let lo = self.min_ms;
        let hi = self.max_ms.max(lo);
        // floor(base / 10) for any sign, then the clamp (equal to flooring the clamped quotient).
        base_ms.div_euclid(10).clamp(lo, hi)
    }

    /// Reconnection grace of both players of a game restored from the journal (server restart
    /// or crash): `RECOVERY_GRACE_MS`, or the normal grace when that is longer. The server broke
    /// the connections and every client comes back at once.
    #[must_use]
    pub fn recovery_grace_for(&self, base_ms: i64) -> i64 {
        self.recovery_ms.max(self.grace_for(base_ms))
    }

    /// Clock hold of a restored game: `RECOVERY_CLOCK_HOLD_MS`, at most the recovery grace. The
    /// clock (or first-move timer) of the side to move starts when that player is back, or once
    /// the hold is over.
    #[must_use]
    pub fn recovery_hold_for(&self, base_ms: i64) -> i64 {
        self.recovery_hold_ms.min(self.recovery_grace_for(base_ms))
    }
}

/// One exponential-moving-average step of a round-trip measurement, capped at
/// [`RTT_EMA_MAX_MS`]; `prev` is `None` before the first sample, which is taken as is.
#[must_use]
pub fn rtt_ema_step(prev: Option<f64>, sample_ms: f64) -> f64 {
    let s = if sample_ms.is_nan() { 0.0 } else { sample_ms.clamp(0.0, RTT_EMA_MAX_MS) };
    match prev {
        Some(p) if p.is_finite() => (p + RTT_EMA_ALPHA * (s - p)).min(RTT_EMA_MAX_MS),
        _ => s,
    }
}

/// The clock accounting of one move ([`GameClock::check`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockCheck {
    /// Time since the turn started (never negative).
    pub elapsed: i64,
    /// The client's thinking time, clamped to `elapsed`.
    pub think: i64,
    /// `elapsed - think`.
    pub lag: i64,
    /// Compensation granted.
    pub comp: i64,
    /// Time charged (`elapsed - comp`).
    pub charged: i64,
    /// The mover's flag fell.
    pub flagged: bool,
    /// `think_ms` exceeds `elapsed` by more than [`IMPLAUSIBLE_MARGIN_MS`].
    pub implausible: bool,
    /// The mover's remaining time after the move, increment included (0 when flagged).
    pub clock_after: i64,
    /// The mover's lag quota after the move.
    pub quota_after: i64,
}

/// Both players' clocks, lag quotas and round-trip averages. The side to move and the ply come
/// from the owner (the room): the clock does not know the chess rules.
#[derive(Clone, Debug)]
pub struct GameClock {
    inc_ms: i64,
    policy: ClockPolicy,
    /// Remaining time at `turn_start` (side to move) or now (the other side).
    ms: [i64; 2],
    quota: [i64; 2],
    /// Raw moving averages (not journaled).
    rtt_f: [Option<f64>; 2],
    /// Integer view of the averages used by the arithmetic.
    rtt: [i64; 2],
    turn_start: i64,
}

impl GameClock {
    /// A clock of `base_ms` + `inc_ms` whose first turn (White's first move) starts at `start_at`.
    #[must_use]
    pub fn new(base_ms: i64, inc_ms: i64, policy: ClockPolicy, start_at: i64) -> Self {
        GameClock {
            inc_ms,
            policy,
            ms: [base_ms; 2],
            quota: [policy.quota_initial_ms; 2],
            rtt_f: [None; 2],
            rtt: [INITIAL_RTT_MS; 2],
            turn_start: start_at,
        }
    }

    /// The clock policy.
    #[must_use]
    pub fn policy(&self) -> &ClockPolicy {
        &self.policy
    }

    /// Remaining time of `side` at the turn start (side to move) or now (the other side).
    #[must_use]
    pub fn ms(&self, side: Side) -> i64 {
        self.ms[side.index()]
    }

    /// Lag quota of `side`.
    #[must_use]
    pub fn quota(&self, side: Side) -> i64 {
        self.quota[side.index()]
    }

    /// Integer round-trip average of `side`.
    #[must_use]
    pub fn rtt(&self, side: Side) -> i64 {
        self.rtt[side.index()]
    }

    /// Start of the current turn (in the future while a restored game's clock is held).
    #[must_use]
    pub fn turn_start(&self) -> i64 {
        self.turn_start
    }

    /// Records a round-trip measurement of `side`.
    pub fn set_rtt(&mut self, side: Side, sample_ms: f64) {
        let i = side.index();
        let f = rtt_ema_step(self.rtt_f[i], sample_ms);
        self.rtt_f[i] = Some(f);
        // Rounds half up like the reference (the values are never negative).
        self.rtt[i] = (f + 0.5).floor() as i64;
    }

    /// Largest compensation `side` could still get on its next move.
    #[must_use]
    pub fn comp_cap(&self, side: Side) -> i64 {
        let i = side.index();
        self.quota[i].min(self.rtt[i] + RTT_EXTRA_MS).min(self.policy.lag_comp_max_ms)
    }

    /// Deadline of a first move (plies 0 and 1) of `side`: the first-move timeout plus its cap.
    #[must_use]
    pub fn first_move_deadline(&self, side: Side) -> i64 {
        self.turn_start + self.policy.first_move_ms + self.comp_cap(side)
    }

    /// Flag deadline of `side` while its clock runs.
    #[must_use]
    pub fn flag_deadline(&self, side: Side) -> i64 {
        self.turn_start + self.ms[side.index()] + self.comp_cap(side)
    }

    /// The deadline of the move expected at `ply` (first-move timer or flag).
    #[must_use]
    pub fn deadline(&self, ply: usize) -> i64 {
        let side = Side::to_move(ply);
        if ply < 2 { self.first_move_deadline(side) } else { self.flag_deadline(side) }
    }

    /// First-move time left at `now` for the move of `ply` (0 once the clocks run; no margin).
    #[must_use]
    pub fn first_move_left(&self, ply: usize, now: i64) -> i64 {
        if ply < 2 { (self.turn_start + self.policy.first_move_ms - now).max(0) } else { 0 }
    }

    /// Remaining time of `side` at `now` after `ply` plies (no compensation; never negative).
    #[must_use]
    pub fn remaining_at(&self, side: Side, ply: usize, now: i64) -> i64 {
        let ms = self.ms[side.index()];
        if ply >= 2 && Side::to_move(ply) == side { (ms - (now - self.turn_start).max(0)).max(0) } else { ms }
    }

    /// Clock accounting of a move of `side` at `ply` received at `recv`, without changing
    /// anything.
    #[must_use]
    pub fn check(&self, side: Side, ply: usize, recv: i64, think_ms: i64) -> ClockCheck {
        let i = side.index();
        let elapsed = (recv - self.turn_start).max(0);
        let tm = think_ms.max(0);
        let think = tm.min(elapsed);
        let mut r = ClockCheck {
            elapsed,
            think,
            lag: elapsed - think,
            implausible: tm > elapsed + IMPLAUSIBLE_MARGIN_MS,
            ..ClockCheck::default()
        };
        if ply < 2 {
            // No clock: the first-move timeout is a deadline of its own (the room checks it
            // before validating the move, like every other deadline).
            r.clock_after = self.ms[i];
            r.quota_after = self.quota[i];
            return r;
        }
        r.comp = r.lag.min(self.comp_cap(side));
        r.charged = elapsed - r.comp;
        let left = self.ms[i] - r.charged;
        r.flagged = left <= 0;
        r.clock_after = if r.flagged { 0 } else { left + self.inc_ms };
        r.quota_after = (self.quota[i] - r.comp + self.policy.quota_gain_ms).min(self.policy.quota_max_ms);
        r
    }

    /// Applies an accepted move (live or replayed): the opponent's turn starts at `at`.
    pub fn apply(&mut self, side: Side, clock_after: i64, quota_after: i64, at: i64) {
        self.ms[side.index()] = clock_after;
        self.quota[side.index()] = quota_after;
        self.turn_start = at;
    }

    /// Server restart: the running clock restarts from its journaled value at `at`. A turn start
    /// in the future holds the clock until then: nothing is charged before it, and the deadlines
    /// move with it.
    pub fn restart(&mut self, at: i64) {
        self.turn_start = at;
    }

    /// Restores the quota of `side` (journal checkpoint).
    pub fn set_quota(&mut self, side: Side, quota: i64) {
        self.quota[side.index()] = quota;
    }

    /// Whether the clock of the side to move is held at `now` (its turn starts later).
    #[must_use]
    pub fn held_at(&self, now: i64) -> bool {
        self.turn_start > now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;

    const W: Side = Side::White;
    const B: Side = Side::Black;

    fn policy() -> ClockPolicy {
        ClockPolicy::from_config(&Config::for_tests())
    }

    fn graces(overrides: &[(&str, &str)]) -> GracePolicy {
        GracePolicy::from_config(&test_config(overrides).expect("valid test configuration"))
    }

    #[test]
    fn grace_is_base_over_10_clamped_to_the_configured_bounds() {
        let g = graces(&[]);
        assert_eq!(g.grace_for(60000), 15000);
        assert_eq!(g.grace_for(180000), 18000);
        assert_eq!(g.grace_for(5400000), 60000);
        assert_eq!(graces(&[("RECONNECT_GRACE_MIN_MS", "20000")]).grace_for(180000), 20000);
        assert_eq!(GracePolicy::default(), g);
        assert_eq!(g.grace_for(180009), 18000, "floored");
    }

    #[test]
    fn recovery_grace_and_hold_follow_the_configuration() {
        let g = GracePolicy::default();
        assert_eq!(g.recovery_grace_for(180000), 90000);
        assert_eq!(g.recovery_hold_for(180000), 20000);
        // A long time control's normal grace is longer than RECOVERY_GRACE_MS.
        let short = GracePolicy { recovery_ms: 20000, ..g };
        assert_eq!(short.recovery_grace_for(5400000), 60000);
        let tiny = GracePolicy { recovery_ms: 0, min_ms: 1000, max_ms: 1000, recovery_hold_ms: 20000 };
        assert_eq!(tiny.recovery_hold_for(180000), 1000, "the hold never exceeds the grace");
    }

    #[test]
    fn clock_policy_reads_the_games_section_of_the_configuration() {
        let c = test_config(&[
            ("FIRST_MOVE_TIMEOUT_MS", "10000"),
            ("LAG_COMP_MAX_MS", "500"),
            ("LAG_QUOTA_MAX_MS", "4000"),
        ])
        .expect("valid test configuration");
        assert_eq!(
            ClockPolicy::from_config(&c),
            ClockPolicy {
                first_move_ms: 10000,
                lag_comp_max_ms: 500,
                quota_initial_ms: 2000,
                quota_gain_ms: 100,
                quota_max_ms: 4000
            }
        );
        assert_eq!(policy(), ClockPolicy::default());
    }

    #[test]
    fn round_trip_average_first_sample_exponential_average_cap() {
        assert_eq!(rtt_ema_step(None, 80.0), 80.0);
        assert_eq!(rtt_ema_step(Some(80.0), 160.0), 100.0);
        assert_eq!(rtt_ema_step(Some(100.0), 99999.0), 100.0 + 0.25 * (RTT_EMA_MAX_MS - 100.0));
        assert_eq!(rtt_ema_step(None, -5.0), 0.0);
        let mut c = GameClock::new(60000, 0, policy(), 0);
        assert_eq!(c.rtt(W), INITIAL_RTT_MS);
        c.set_rtt(W, 42.4);
        assert_eq!(c.rtt(W), 42);
        c.set_rtt(B, 42.5);
        assert_eq!(c.rtt(B), 43, "half rounds up");
    }

    #[test]
    fn check_is_pure_and_the_flag_deadline_is_exactly_where_a_zero_think_move_flags() {
        let mut c = GameClock::new(60000, 1000, policy(), 1000);
        let r0 = c.check(W, 0, 50000, 0);
        assert_eq!((r0.charged, r0.clock_after, r0.quota_after, r0.flagged), (0, 60000, 2000, false));
        c.apply(W, 60000, 2000, 2000);
        c.apply(B, 60000, 2000, 3000); // White's clock starts at 3000
        let d = c.flag_deadline(W);
        assert_eq!(d, 3000 + 60000 + 150);
        assert!(!c.check(W, 2, d - 1, 0).flagged);
        assert!(c.check(W, 2, d, 0).flagged);
        assert_eq!(c.ms(W), 60000, "check() changes nothing");
        let r = c.check(W, 2, 13000, 9990); // 10 ms of lag
        assert_eq!(
            (r.elapsed, r.comp, r.charged, r.clock_after, r.quota_after, r.implausible),
            (10000, 10, 9990, 60000 - 9990 + 1000, 2000 - 10 + 100, false)
        );
        assert!(c.check(W, 2, 13000, 10101).implausible);
        assert_eq!(c.remaining_at(W, 2, 13000), 50000);
        assert_eq!(c.remaining_at(B, 2, 13000), 60000);
        assert_eq!(c.remaining_at(W, 2, 99999999), 0);
    }

    #[test]
    fn a_first_move_has_the_margin_of_a_flag_in_its_deadline_not_in_the_countdown_shown() {
        let mut c = GameClock::new(60000, 1000, policy(), 1000);
        assert_eq!(c.first_move_deadline(W), 1000 + 30000 + 150);
        assert_eq!(c.deadline(0), c.first_move_deadline(W));
        assert_eq!(c.first_move_left(0, 11000), 20000);
        c.set_rtt(B, 700.0); // cap min(quota 2000, 700 + 50, LAG_COMP_MAX_MS 1000)
        c.apply(W, 60000, 2000, 5000);
        assert_eq!(c.deadline(1), 5000 + 30000 + 750);
        assert_eq!(c.first_move_left(1, 35000), 0);
        assert_eq!(c.first_move_left(2, 5000), 0);
    }

    #[test]
    fn a_held_clock_charges_nothing_before_its_turn_start() {
        let mut c = GameClock::new(60000, 0, policy(), 0);
        c.apply(W, 60000, 2000, 10);
        c.apply(B, 60000, 2000, 20);
        c.restart(5000);
        assert!(c.held_at(4999));
        assert!(!c.held_at(5000));
        assert_eq!(c.remaining_at(W, 2, 3000), 60000);
        assert_eq!(c.check(W, 2, 3000, 0).charged, 0);
        assert_eq!(c.flag_deadline(W), 5000 + 60000 + 150);
    }
}
