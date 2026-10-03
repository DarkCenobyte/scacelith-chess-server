//! Time sources.
//!
//! The server uses two notions of time and keeps them apart:
//!
//! * the *monotonic* clock, anchored to the Unix epoch when the process starts, in fractional
//!   milliseconds ([`Clock::mono_ms`]). Game clocks, timers, rate buckets, heartbeats, RTT and the
//!   `serverTime` fields of the realtime protocol use it; it never jumps backwards.
//! * the *wall* clock in integer milliseconds since the Unix epoch ([`Clock::wall_ms`]). Stored
//!   timestamps (accounts, sessions, games), expiries of single-use tokens, bans and retention use
//!   it.
//!
//! Components that need deterministic tests take an `Arc<dyn Clock>`; [`ManualClock`] lets a test
//! drive both clocks by hand.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A source of monotonic and wall time.
pub trait Clock: Send + Sync + 'static {
    /// Monotonic milliseconds anchored to the Unix epoch at process start (fractional).
    fn mono_ms(&self) -> f64;

    /// Wall-clock milliseconds since the Unix epoch.
    fn wall_ms(&self) -> i64;

    /// Monotonic milliseconds rounded down, the unit of game clocks and timers.
    fn now_ms(&self) -> i64 {
        self.mono_ms().floor() as i64
    }
}

/// Shared handle on a clock.
pub type SharedClock = Arc<dyn Clock>;

struct Anchor {
    instant: Instant,
    epoch_ms: f64,
}

fn anchor() -> &'static Anchor {
    static ANCHOR: OnceLock<Anchor> = OnceLock::new();
    ANCHOR.get_or_init(|| Anchor { instant: Instant::now(), epoch_ms: system_wall_ms() as f64 })
}

fn system_wall_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => 0,
    }
}

/// The process clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn mono_ms(&self) -> f64 {
        let a = anchor();
        a.epoch_ms + a.instant.elapsed().as_secs_f64() * 1000.0
    }

    fn wall_ms(&self) -> i64 {
        system_wall_ms()
    }
}

/// The process clock as a shared handle.
pub fn system() -> SharedClock {
    Arc::new(SystemClock)
}

/// Monotonic milliseconds of the process clock (see [`Clock::mono_ms`]).
pub fn mono_ms() -> f64 {
    SystemClock.mono_ms()
}

/// Wall-clock milliseconds of the process clock (see [`Clock::wall_ms`]).
pub fn wall_ms() -> i64 {
    system_wall_ms()
}

/// A clock driven by hand, for tests. Both clocks start at the given values and only move when
/// told to.
#[derive(Debug)]
pub struct ManualClock {
    mono_bits: AtomicU64,
    wall: AtomicI64,
}

impl ManualClock {
    pub fn new(mono_ms: f64, wall_ms: i64) -> Arc<ManualClock> {
        Arc::new(ManualClock { mono_bits: AtomicU64::new(mono_ms.to_bits()), wall: AtomicI64::new(wall_ms) })
    }

    /// Moves both clocks forward by `ms`.
    pub fn advance(&self, ms: f64) {
        self.set_mono(self.mono_ms() + ms);
        self.wall.fetch_add(ms as i64, Ordering::SeqCst);
    }

    pub fn set_mono(&self, ms: f64) {
        self.mono_bits.store(ms.to_bits(), Ordering::SeqCst);
    }

    pub fn set_wall(&self, ms: i64) {
        self.wall.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn mono_ms(&self) -> f64 {
        f64::from_bits(self.mono_bits.load(Ordering::SeqCst))
    }

    fn wall_ms(&self) -> i64 {
        self.wall.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_anchored_and_monotonic() {
        let c = SystemClock;
        let a = c.mono_ms();
        let b = c.mono_ms();
        assert!(b >= a);
        assert!((a - c.wall_ms() as f64).abs() < 5_000.0);
    }

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(1000.5, 2000);
        assert_eq!(c.now_ms(), 1000);
        c.advance(10.0);
        assert_eq!(c.mono_ms(), 1010.5);
        assert_eq!(c.wall_ms(), 2010);
    }
}
