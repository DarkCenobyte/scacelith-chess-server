//! Memory-bounded counters of the auth service, on the injected monotonic clock (integer
//! milliseconds): per-key token buckets, per-key failure counters with an exponential delay
//! (login brute force) and an approximate sliding-window count (the failure-rate detector).
//! Every operation is O(1); expiry is lazy (no timers).

use parking_lot::Mutex;

use super::lru::LruMap;
use crate::clock::SharedClock;

/// The answer of [`TokenBucketLimiter::take`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketTake {
    /// The tokens were taken.
    pub allowed: bool,
    /// When refused, the time until enough tokens are back, in ms (0 when allowed).
    pub retry_after_ms: i64,
    /// Whole tokens left (0 when refused).
    pub remaining: i64,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    at: i64,
}

/// Per-key token buckets: `limit` tokens refilled evenly over `window_ms`. A bucket that holds a
/// burst `b` and refills at `r` tokens per minute is `take(key, b, b * 60000 / r, 1)`. At most
/// `max_keys` buckets are kept (an evicted bucket comes back full: only more lenient).
pub struct TokenBucketLimiter {
    clock: SharedClock,
    buckets: Mutex<LruMap<Bucket>>,
}

impl std::fmt::Debug for TokenBucketLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBucketLimiter").field("buckets", &self.len()).finish()
    }
}

impl TokenBucketLimiter {
    /// The default bound on the number of buckets.
    pub const DEFAULT_MAX_KEYS: usize = 100_000;

    /// A limiter of at most `max_keys` buckets.
    pub fn new(max_keys: usize, clock: SharedClock) -> TokenBucketLimiter {
        TokenBucketLimiter { clock, buckets: Mutex::new(LruMap::new(max_keys)) }
    }

    /// Takes `cost` tokens from the bucket of `key`.
    pub fn take(&self, key: &str, limit: f64, window_ms: f64, cost: f64) -> BucketTake {
        let t = self.clock.now_ms();
        let rate = limit / window_ms;
        let mut buckets = self.buckets.lock();
        let b = match buckets.get(key) {
            Some(b) => {
                b.tokens = limit.min(b.tokens + (t - b.at).max(0) as f64 * rate);
                b.at = t;
                b
            }
            None => {
                buckets.insert(key, Bucket { tokens: limit, at: t });
                buckets.peek_mut(key).expect("just inserted")
            }
        };
        if b.tokens >= cost {
            b.tokens -= cost;
            return BucketTake { allowed: true, retry_after_ms: 0, remaining: b.tokens.floor() as i64 };
        }
        BucketTake { allowed: false, retry_after_ms: ((cost - b.tokens) / rate).ceil() as i64, remaining: 0 }
    }

    /// Gives back `cost` tokens that [`take`](Self::take) granted to `key` (a request that did
    /// nothing), never beyond `limit`. A bucket evicted meanwhile is full already.
    pub fn give(&self, key: &str, limit: f64, window_ms: f64, cost: f64) {
        let t = self.clock.now_ms();
        let mut buckets = self.buckets.lock();
        if let Some(b) = buckets.peek_mut(key) {
            b.tokens = limit.min(b.tokens + (t - b.at).max(0) as f64 * (limit / window_ms) + cost);
            b.at = t;
        }
    }

    /// Forgets the bucket of `key` (it comes back full).
    pub fn reset(&self, key: &str) {
        self.buckets.lock().remove(key);
    }

    /// The number of buckets kept.
    pub fn len(&self) -> usize {
        self.buckets.lock().len()
    }

    /// True when no bucket is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The settings of a [`FailureCounter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureCounterConfig {
    /// From this many failures on, each attempt must wait (at least 1).
    pub threshold: u32,
    /// The delay at the threshold, doubled at each further failure.
    pub base_delay_ms: i64,
    /// The longest delay.
    pub max_delay_ms: i64,
    /// A key is forgotten this long after its last failure (or after its delay and a minute,
    /// when that is longer).
    pub forget_ms: i64,
    /// At most this many keys are kept (the least recently failed go first).
    pub max_keys: usize,
}

impl FailureCounterConfig {
    /// The settings of the login counters: delays 2 s, 4 s, ... up to 15 min from `threshold`
    /// failures, forgotten after an hour without failures, 100,000 keys.
    pub fn with_threshold(threshold: u32) -> FailureCounterConfig {
        FailureCounterConfig {
            threshold,
            base_delay_ms: 2000,
            max_delay_ms: 15 * 60_000,
            forget_ms: 60 * 60_000,
            max_keys: 100_000,
        }
    }
}

/// The answer of [`FailureCounter::fail`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Failures of the key, this one included.
    pub failures: u32,
    /// The delay the key must now wait before its next attempt, in ms.
    pub retry_after_ms: i64,
}

#[derive(Clone, Copy, Debug)]
struct FailureEntry {
    failures: u32,
    last: i64,
}

/// Failures per key; from the `threshold`-th failure on, each further attempt must wait
/// `base_delay_ms * 2^(failures - threshold)` (at most `max_delay_ms`) after the last failure.
pub struct FailureCounter {
    cfg: FailureCounterConfig,
    clock: SharedClock,
    entries: Mutex<LruMap<FailureEntry>>,
}

impl std::fmt::Debug for FailureCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailureCounter").field("cfg", &self.cfg).finish()
    }
}

impl FailureCounter {
    /// A counter with these settings (a threshold of 0 counts as 1).
    pub fn new(cfg: FailureCounterConfig, clock: SharedClock) -> FailureCounter {
        let cfg = FailureCounterConfig { threshold: cfg.threshold.max(1), ..cfg };
        FailureCounter { cfg, clock, entries: Mutex::new(LruMap::new(cfg.max_keys)) }
    }

    /// The delay imposed after `failures` failures, in ms.
    pub fn delay_for(&self, failures: u32) -> i64 {
        if failures < self.cfg.threshold {
            return 0;
        }
        let exp = (failures - self.cfg.threshold).min(30);
        self.cfg.max_delay_ms.min(self.cfg.base_delay_ms.saturating_mul(1i64 << exp))
    }

    /// The live entry of `key` at `t` (an expired one is forgotten).
    fn entry(&self, entries: &mut LruMap<FailureEntry>, key: &str, t: i64) -> Option<FailureEntry> {
        let e = *entries.peek(key)?;
        if t - e.last > self.cfg.forget_ms.max(self.delay_for(e.failures).saturating_add(60_000)) {
            entries.remove(key);
            return None;
        }
        Some(e)
    }

    /// Milliseconds before `key` may try again (0: now).
    pub fn retry_after(&self, key: &str) -> i64 {
        let t = self.clock.now_ms();
        let mut entries = self.entries.lock();
        self.entry(&mut entries, key, t).map_or(0, |e| (e.last + self.delay_for(e.failures) - t).max(0))
    }

    /// Records a failure; returns the new count and the delay it imposes.
    pub fn fail(&self, key: &str) -> Failure {
        let t = self.clock.now_ms();
        let mut entries = self.entries.lock();
        let mut e = self.entry(&mut entries, key, t).unwrap_or(FailureEntry { failures: 0, last: t });
        e.failures = e.failures.saturating_add(1);
        e.last = t;
        entries.insert(key, e);
        Failure { failures: e.failures, retry_after_ms: self.delay_for(e.failures) }
    }

    /// The failures counted for `key`.
    pub fn failures(&self, key: &str) -> u32 {
        let t = self.clock.now_ms();
        let mut entries = self.entries.lock();
        self.entry(&mut entries, key, t).map_or(0, |e| e.failures)
    }

    /// Forgets the failures of `key` (a successful attempt).
    pub fn reset(&self, key: &str) {
        self.entries.lock().remove(key);
    }
}

#[derive(Debug, Default)]
struct WindowState {
    start: i64,
    cur: f64,
    prev: f64,
}

/// Approximate count of the events of the last `window_ms`: two fixed windows, the previous one
/// weighted by how much of it still overlaps the sliding window.
pub struct SlidingWindowCounter {
    window_ms: i64,
    clock: SharedClock,
    state: Mutex<WindowState>,
}

impl std::fmt::Debug for SlidingWindowCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlidingWindowCounter").field("window_ms", &self.window_ms).finish()
    }
}

impl SlidingWindowCounter {
    /// A counter over `window_ms` (at least 1).
    pub fn new(window_ms: i64, clock: SharedClock) -> SlidingWindowCounter {
        SlidingWindowCounter { window_ms: window_ms.max(1), clock, state: Mutex::new(WindowState::default()) }
    }

    fn roll(&self, st: &mut WindowState, t: i64) {
        let w = t.div_euclid(self.window_ms) * self.window_ms;
        if w == st.start {
            return;
        }
        st.prev = if w - st.start == self.window_ms { st.cur } else { 0.0 };
        st.cur = 0.0;
        st.start = w;
    }

    fn count_at(&self, st: &mut WindowState, t: i64) -> f64 {
        self.roll(st, t);
        let frac = (t - st.start) as f64 / self.window_ms as f64;
        st.prev * (1.0 - frac) + st.cur
    }

    /// Counts `n` events; returns the new estimate.
    pub fn add(&self, n: f64) -> f64 {
        let t = self.clock.now_ms();
        let mut st = self.state.lock();
        self.roll(&mut st, t);
        st.cur += n;
        self.count_at(&mut st, t)
    }

    /// The estimate of the events of the last window (not rounded).
    pub fn count(&self) -> f64 {
        let t = self.clock.now_ms();
        let mut st = self.state.lock();
        self.count_at(&mut st, t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    /// The clock of the former tests: 2026-09-28T12:00:00Z.
    fn clock() -> std::sync::Arc<ManualClock> {
        ManualClock::new(1_790_596_800_000.0, 1_790_596_800_000)
    }

    #[test]
    fn token_bucket_burst_refusal_with_retry_time_refill() {
        let now = clock();
        let l = TokenBucketLimiter::new(TokenBucketLimiter::DEFAULT_MAX_KEYS, now.clone());
        for _ in 0..3 {
            assert!(l.take("k", 3.0, 60000.0, 1.0).allowed);
        }
        let r = l.take("k", 3.0, 60000.0, 1.0);
        assert!(!r.allowed);
        assert_eq!(r.retry_after_ms, 20000);
        now.advance(20000.0);
        assert!(l.take("k", 3.0, 60000.0, 1.0).allowed);
        assert!(l.take("other", 3.0, 60000.0, 1.0).allowed, "keys are independent");
    }

    #[test]
    fn token_bucket_give_returns_granted_tokens_never_beyond_the_limit() {
        let now = clock();
        let l = TokenBucketLimiter::new(TokenBucketLimiter::DEFAULT_MAX_KEYS, now.clone());
        let take = || l.take("k", 3.0, 60000.0, 1.0).allowed;
        for _ in 0..3 {
            assert!(take());
        }
        assert!(!take());
        l.give("k", 3.0, 60000.0, 1.0);
        assert!(take(), "the given-back token is taken again");
        assert!(!take());
        // The refill up to now counts first, then the token; the bucket never holds more than the
        // limit.
        now.advance(20000.0);
        for _ in 0..3 {
            l.give("k", 3.0, 60000.0, 1.0);
        }
        for _ in 0..3 {
            assert!(take());
        }
        assert!(!take());
        l.give("unknown", 3.0, 60000.0, 1.0); // an evicted or unknown bucket is full already
        assert_eq!(l.len(), 1);
        l.reset("k");
        assert!(l.is_empty());
    }

    #[test]
    fn token_bucket_memory_is_bounded() {
        let l = TokenBucketLimiter::new(100, clock());
        for i in 0..1000 {
            l.take(&format!("k{i}"), 1.0, 1000.0, 1.0);
        }
        assert_eq!(l.len(), 100);
    }

    #[test]
    fn token_bucket_burst_and_rate_as_limit_and_window() {
        // 600 per minute with a burst of half a minute: take(key, 300, 300 * 60000 / 600 = 30000).
        let now = clock();
        let l = TokenBucketLimiter::new(TokenBucketLimiter::DEFAULT_MAX_KEYS, now.clone());
        for _ in 0..300 {
            assert!(l.take("k", 300.0, 30000.0, 1.0).allowed);
        }
        let r = l.take("k", 300.0, 30000.0, 1.0);
        assert!(!r.allowed);
        assert_eq!(r.retry_after_ms, 100, "one token per 100 ms: 600 per minute");
        now.advance(60000.0);
        let mut n = 0;
        while l.take("k", 300.0, 30000.0, 1.0).allowed {
            n += 1;
        }
        assert_eq!(n, 300, "a minute refills the burst, not more");
        assert_eq!(
            l.take("fresh", 5.0, 1000.0, 2.0),
            BucketTake { allowed: true, retry_after_ms: 0, remaining: 3 }
        );
    }

    #[test]
    fn failure_counter_exponential_delay_capped_reset_forgotten() {
        let now = clock();
        let f = FailureCounter::new(
            FailureCounterConfig {
                threshold: 3,
                base_delay_ms: 1000,
                max_delay_ms: 8000,
                forget_ms: 3_600_000,
                max_keys: 100,
            },
            now.clone(),
        );
        f.fail("a");
        f.fail("a");
        assert_eq!(f.retry_after("a"), 0);
        assert_eq!(f.fail("a").retry_after_ms, 1000);
        assert_eq!(f.retry_after("a"), 1000);
        now.advance(1000.0);
        assert_eq!(f.retry_after("a"), 0);
        assert_eq!(f.fail("a").retry_after_ms, 2000);
        assert_eq!(f.fail("a").retry_after_ms, 4000);
        assert_eq!(f.fail("a").retry_after_ms, 8000);
        assert_eq!(f.fail("a"), Failure { failures: 7, retry_after_ms: 8000 }, "capped");
        f.reset("a");
        assert_eq!(f.retry_after("a"), 0);
        f.fail("b");
        f.fail("b");
        f.fail("b");
        now.advance(3_600_000.0 + 60_001.0);
        assert_eq!(f.failures("b"), 0, "forgotten after forgetMs");
    }

    #[test]
    fn failure_counter_login_settings() {
        let now = clock();
        let f = FailureCounter::new(FailureCounterConfig::with_threshold(5), now.clone());
        let delays: Vec<i64> = (0..8).map(|_| f.fail("l:alice").retry_after_ms).collect();
        assert_eq!(delays, [0, 0, 0, 0, 2000, 4000, 8000, 16000]);
        assert_eq!(f.delay_for(100), 15 * 60_000);
        assert_eq!(f.retry_after("l:alice"), 16000);
        // A delay longer than forgetMs keeps the key until the delay and a minute have passed.
        let short = FailureCounter::new(
            FailureCounterConfig { forget_ms: 60_000, ..FailureCounterConfig::with_threshold(5) },
            now.clone(),
        );
        for _ in 0..20 {
            short.fail("l:bob");
        }
        now.advance(900_000.0 + 60_000.0);
        assert_eq!(short.failures("l:bob"), 20);
        now.advance(1.0);
        assert_eq!(short.failures("l:bob"), 0);
        let zero = FailureCounter::new(FailureCounterConfig::with_threshold(0), now);
        assert_eq!(zero.fail("x").retry_after_ms, 2000, "a threshold of 0 counts as 1");
    }

    #[test]
    fn failure_counter_memory_is_bounded() {
        let f = FailureCounter::new(
            FailureCounterConfig { max_keys: 10, ..FailureCounterConfig::with_threshold(5) },
            clock(),
        );
        for i in 0..100 {
            f.fail(&format!("k{i}"));
        }
        assert_eq!(f.entries.lock().len(), 10);
        assert_eq!(f.failures("k99"), 1);
        assert_eq!(f.failures("k0"), 0);
    }

    #[test]
    fn sliding_window_counter_decays_over_the_next_window() {
        let now = ManualClock::new(0.0, 0);
        let c = SlidingWindowCounter::new(60000, now.clone());
        for _ in 0..10 {
            c.add(1.0);
        }
        assert_eq!(c.count(), 10.0);
        now.advance(60000.0 + 30000.0);
        assert_eq!(c.count(), 5.0);
        now.advance(60000.0);
        assert_eq!(c.count(), 0.0);
        assert_eq!(c.add(2.0), 2.0);
    }
}
