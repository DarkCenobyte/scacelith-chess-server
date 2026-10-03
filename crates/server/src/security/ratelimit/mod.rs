//! The rate-limiting building blocks of the auth service, all memory-bounded and driven by an
//! injected clock (no timers: expiry is lazy):
//!
//! * [`address`]: the rate-limit keys of client addresses (IPv4, IPv6 /64 and /48);
//! * [`LruMap`]: a bounded map that evicts the least recently used entry;
//! * [`TokenBucketLimiter`]: per-key token buckets;
//! * [`FailureCounter`]: per-key failure counts with an exponential delay (login brute force);
//! * [`SlidingWindowCounter`]: an approximate count of the events of the last window;
//! * [`LocalControl`]: the whole-server `ratelimit.take`, `ratelimit.refund` and `once.consume`
//!   of the former primary process;
//! * [`LoginPowDetector`]: the global failed-login rate that turns the login proof of work on;
//! * [`random_retry_after`]: a `Retry-After` drawn at random, so refused clients spread out.
//!
//! The generic per-address sliding-window limiter of the network edge and the abuse tracker live
//! in the `net` module.

pub mod address;
pub mod control;
pub mod counters;
pub mod login_pow;
pub mod lru;

pub use address::{ip_key, normalize_address, normalize_ip, prefix_key};
pub use control::{LocalControl, Take};
pub use counters::{
    BucketTake, Failure, FailureCounter, FailureCounterConfig, SlidingWindowCounter, TokenBucketLimiter,
};
pub use login_pow::{LOGIN_FAILURES_KEY, LoginPowDetector, POW_LOGIN_HOLD_MS, PowSource};
pub use lru::LruMap;

/// A `Retry-After` in seconds drawn uniformly at random in `[min, max]` (`min` when `max` is not
/// above it), so that the clients refused during one burst do not all come back together.
pub fn random_retry_after(min: u64, max: u64) -> u64 {
    if max <= min {
        return min;
    }
    let draw = || u64::from_le_bytes(crate::security::encoding::random_bytes::<8>());
    let Some(span) = (max - min).checked_add(1) else {
        return draw();
    };
    // Rejection sampling keeps the draw unbiased: accept only below the largest multiple of
    // `span` that fits in 64 bits.
    let zone = u64::MAX - (u64::MAX - span + 1) % span;
    loop {
        let v = draw();
        if v <= zone {
            return min + v % span;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_retry_after_stays_in_its_range() {
        let mut seen = [false; 11];
        for _ in 0..2000 {
            let s = random_retry_after(5, 15);
            assert!((5..=15).contains(&s));
            seen[(s - 5) as usize] = true;
        }
        assert!(seen.iter().all(|&s| s), "every value of the range is drawn");
        assert_eq!(random_retry_after(7, 7), 7);
        assert_eq!(random_retry_after(9, 3), 9);
        // The full range does not overflow.
        let _ = random_retry_after(0, u64::MAX);
        assert!(random_retry_after(u64::MAX - 1, u64::MAX) >= u64::MAX - 1);
    }
}
