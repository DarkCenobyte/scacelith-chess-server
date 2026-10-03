//! The in-process "control requests" of the auth service: `ratelimit.take`, `ratelimit.refund`
//! and `once.consume`, which the former server sent to its primary process (DESIGN.md 5.7). In
//! one process they are plain calls on whole-server state, with the formulas of the former
//! primary (`cluster/limits.js`), its `Retry-After` included.
//!
//! Keys the auth service uses: `auth:login-failures` (the global failure-rate detector),
//! `mfa:u<id>` (second-factor codes per account), `pow:<signature>` (single use of a proof of
//! work) and the mail throttles `mail:<kind>:<hash>`.

use parking_lot::Mutex;

use super::lru::LruMap;
use crate::clock::SharedClock;

/// The answer of [`LocalControl::take`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Take {
    /// The units were counted.
    pub allowed: bool,
    /// When refused, the time until `cost` more units fit, in ms (0 when allowed).
    pub retry_after_ms: i64,
    /// The estimate after the take (allowed), or the current one (refused), rounded up.
    pub count: i64,
}

#[derive(Clone, Copy, Debug)]
struct Window {
    start: i64,
    cur: f64,
    prev: f64,
    window_ms: i64,
    last: i64,
}

impl Window {
    fn new(now: i64, window_ms: i64) -> Window {
        Window { start: now - now.rem_euclid(window_ms), cur: 0.0, prev: 0.0, window_ms, last: now }
    }

    fn advance(&mut self, now: i64) {
        let w = self.window_ms;
        if now < self.start + w {
            return;
        }
        let start = now - now.rem_euclid(w);
        self.prev = if start - self.start == w { self.cur } else { 0.0 };
        self.cur = 0.0;
        self.start = start;
    }

    fn estimate(&self, now: i64) -> f64 {
        self.prev * (1.0 - (now - self.start) as f64 / self.window_ms as f64) + self.cur
    }

    fn expired(&self, now: i64) -> bool {
        now - self.last > 2 * self.window_ms
    }

    /// Time until `cost` more units fit: the previous window's weight decays linearly, and the
    /// current count only leaves when the window rolls over.
    fn retry_after(&self, now: i64, limit: f64, cost: f64) -> i64 {
        let w = self.window_ms as f64;
        let start = self.start as f64;
        let to_next = start + w - now as f64;
        // Within the current window: prev * (1 - (t - start)/w) + cur + cost <= limit.
        if self.cur + cost <= limit && self.prev > 0.0 {
            let t = start + w * (1.0 - (limit - cost - self.cur) / self.prev);
            if t > now as f64 && t <= start + w {
                return (t - now as f64).ceil() as i64;
            }
        }
        // After the rollover: cur becomes prev and decays over the next window.
        if cost > limit {
            return (to_next + w) as i64;
        }
        if self.cur == 0.0 {
            return to_next as i64;
        }
        let frac = 1.0 - (limit - cost) / self.cur;
        (to_next + frac.max(0.0) * w).ceil() as i64
    }
}

/// Sliding-window counters and single-use keys, bounded in memory. Windows run on the monotonic
/// clock; single-use keys live on the wall clock (their TTLs come from wall-clock expiries).
pub struct LocalControl {
    clock: SharedClock,
    windows: Mutex<LruMap<Window>>,
    once: Mutex<LruMap<i64>>,
}

impl std::fmt::Debug for LocalControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalControl")
            .field("windows", &self.windows.lock().len())
            .field("once", &self.once.lock().len())
            .finish()
    }
}

impl LocalControl {
    /// The default bound of each map.
    pub const DEFAULT_MAX_KEYS: usize = 200_000;

    /// A control of at most [`Self::DEFAULT_MAX_KEYS`] windows and as many single-use keys.
    pub fn new(clock: SharedClock) -> LocalControl {
        LocalControl::with_max_keys(Self::DEFAULT_MAX_KEYS, clock)
    }

    /// A control of at most `max_keys` windows and as many single-use keys.
    pub fn with_max_keys(max_keys: usize, clock: SharedClock) -> LocalControl {
        LocalControl {
            clock,
            windows: Mutex::new(LruMap::new(max_keys)),
            once: Mutex::new(LruMap::new(max_keys)),
        }
    }

    /// `ratelimit.take`: counts `cost` units against the key's allowance of `limit` per
    /// `window_ms` over a sliding window (the previous fixed window weighted by how much of it
    /// still overlaps, plus the current one). A refused take is not counted.
    pub fn take(&self, key: &str, limit: u32, window_ms: i64, cost: u32) -> Take {
        let window_ms = window_ms.max(1);
        let (limit, cost) = (f64::from(limit), f64::from(cost));
        let now = self.clock.now_ms();
        let mut windows = self.windows.lock();
        match windows.peek_mut(key) {
            // A new window length starts the key again (it keeps its place).
            Some(e) if e.window_ms != window_ms => *e = Window::new(now, window_ms),
            Some(_) => {}
            None => {
                if windows.len() >= windows.capacity() {
                    evict_windows(&mut windows, now);
                }
                windows.insert(key, Window::new(now, window_ms));
            }
        }
        let e = windows.peek_mut(key).expect("present or just inserted");
        e.advance(now);
        e.last = now;
        let estimate = e.estimate(now);
        if estimate + cost <= limit {
            e.cur += cost;
            return Take { allowed: true, retry_after_ms: 0, count: (estimate + cost).ceil() as i64 };
        }
        Take {
            allowed: false,
            retry_after_ms: e.retry_after(now, limit, cost),
            count: estimate.ceil() as i64,
        }
    }

    /// `ratelimit.refund`: takes back `cost` units that [`take`](Self::take) counted `age_ms`
    /// ago, from the fixed window that counted them (the current one or the one before; an older
    /// one no longer counts anyway). True when something was taken back.
    pub fn refund(&self, key: &str, window_ms: i64, cost: u32, age_ms: i64) -> bool {
        let window_ms = window_ms.max(1);
        let now = self.clock.now_ms();
        let mut windows = self.windows.lock();
        let Some(e) = windows.peek_mut(key).filter(|e| e.window_ms == window_ms) else {
            return false;
        };
        e.advance(now);
        let at = now - age_ms.max(0);
        let start = at - at.rem_euclid(window_ms);
        let cost = f64::from(cost);
        if start == e.start {
            e.cur = (e.cur - cost).max(0.0);
        } else if start == e.start - window_ms {
            e.prev = (e.prev - cost).max(0.0);
        } else {
            return false;
        }
        true
    }

    /// The current estimate of a key (0 when unknown), without counting anything.
    pub fn peek(&self, key: &str) -> f64 {
        let now = self.clock.now_ms();
        let mut windows = self.windows.lock();
        windows.peek_mut(key).map_or(0.0, |e| {
            e.advance(now);
            e.estimate(now)
        })
    }

    /// Forgets a key's counts: its next take starts from zero.
    pub fn forget(&self, key: &str) {
        self.windows.lock().remove(key);
    }

    /// `once.consume`: marks `key` as used for `ttl_ms` (at least 1 ms). True the first time, and
    /// again once the TTL has passed.
    pub fn consume_once(&self, key: &str, ttl_ms: i64) -> bool {
        let now = self.clock.wall_ms();
        let mut once = self.once.lock();
        match once.peek(key) {
            Some(&exp) if exp > now => return false,
            Some(_) => {
                once.remove(key);
            }
            None if once.len() >= once.capacity() => {
                // At capacity, the 64 oldest insertions go.
                for _ in 0..64 {
                    if once.pop_oldest().is_none() {
                        break;
                    }
                }
            }
            None => {}
        }
        once.insert(key, now + ttl_ms.max(1));
        true
    }

    /// Removes the windows unused for two window lengths and the expired single-use keys; returns
    /// how many entries went. Optional (expiry is otherwise lazy); call it periodically to give
    /// the memory back after a burst.
    pub fn sweep(&self) -> usize {
        let (mono, wall) = (self.clock.now_ms(), self.clock.wall_ms());
        let w = self.windows.lock().remove_where(|e| e.expired(mono));
        let o = self.once.lock().remove_where(|&exp| exp <= wall);
        w + o
    }
}

/// Makes room for a new window: the expired keys among the 64 oldest insertions go first; when
/// none has expired, the 16 oldest go (which can only make the limit more lenient for them).
fn evict_windows(windows: &mut LruMap<Window>, now: i64) {
    if windows.remove_oldest_where(64, |e| e.expired(now)) > 0 {
        return;
    }
    for _ in 0..16 {
        if windows.pop_oldest().is_none() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn take_and_once_consume() {
        let now = ManualClock::new(0.0, 0);
        let ctl = LocalControl::new(now.clone());
        let take = || ctl.take("k", 2, 1000, 1);
        assert_eq!(take(), Take { allowed: true, retry_after_ms: 0, count: 1 });
        assert_eq!(take(), Take { allowed: true, retry_after_ms: 0, count: 2 });
        let r = take();
        assert!(!r.allowed);
        // The two takes still count in full when the window rolls over (at 1000 ms), then decay
        // over the next window: one more fits at 1500 ms.
        assert_eq!(r.retry_after_ms, 1500);
        now.advance((r.retry_after_ms - 1) as f64);
        assert!(!take().allowed);
        now.advance(1.0);
        assert!(take().allowed, "allowed when its Retry-After ends");
        // Limit 5 per minute, 6th take 2 s into the window: 70 s, not 58 s then 12 s more.
        let take5 = || ctl.take("k5", 5, 60000, 1);
        now.set_mono(120_000.0 + 2000.0);
        for _ in 0..5 {
            assert!(take5().allowed);
        }
        let r5 = take5();
        assert_eq!((r5.allowed, r5.retry_after_ms), (false, 70000));
        now.advance(70000.0);
        assert!(take5().allowed);
        assert!(ctl.consume_once("x", 100));
        assert!(!ctl.consume_once("x", 100));
        now.advance(101.0);
        assert!(ctl.consume_once("x", 100));
    }

    #[test]
    fn refund_takes_back_a_counted_take_in_its_own_window() {
        let now = ManualClock::new(0.0, 0);
        let ctl = LocalControl::new(now.clone());
        let take = || ctl.take("k", 2, 1000, 1).allowed;
        let refund = |age| ctl.refund("k", 1000, 1, age);
        assert!(take());
        assert!(take());
        assert!(!take());
        assert!(refund(0));
        assert!(take(), "the refunded unit is free again");
        assert!(!take());
        // A take of the previous window is taken back from that window.
        now.advance(1250.0); // window [1000, 2000): the previous window (2) weighs 0.75
        assert!(refund(1150)); // taken at 100, in window [0, 1000)
        assert!(take(), "prev 1 x 0.75 + 1 <= 2 (without the refund: 2 x 0.75 + 1 > 2)");
        assert!(!take());
        // Unknown keys and takes older than the previous window change nothing.
        assert!(!ctl.refund("nope", 1000, 1, 0));
        assert!(!refund(5000));
        assert!(!ctl.refund("k", 2000, 1, 0), "another window length is another counter");
    }

    #[test]
    fn retry_after_cases() {
        let now = ManualClock::new(0.0, 0);
        let ctl = LocalControl::new(now.clone());
        // A cost above the limit waits for the end of the next window.
        let r = ctl.take("big", 2, 1000, 3);
        assert_eq!((r.allowed, r.retry_after_ms, r.count), (false, 2000, 0));
        // Within the window, when the previous one's weight decays enough.
        for _ in 0..4 {
            ctl.take("w", 4, 1000, 1);
        }
        now.advance(1000.0); // prev 4, cur 0
        let r = ctl.take("w", 4, 1000, 1);
        assert_eq!((r.allowed, r.retry_after_ms, r.count), (false, 250, 4));
        now.advance(250.0);
        assert_eq!(ctl.take("w", 4, 1000, 1).count, 4);
        assert!((ctl.peek("w") - 4.0).abs() < 1e-9);
        assert_eq!(ctl.peek("unknown"), 0.0);
        ctl.forget("w");
        assert_eq!(ctl.peek("w"), 0.0);
        // A new window length starts the key again.
        ctl.take("len", 1, 1000, 1);
        assert!(!ctl.take("len", 1, 1000, 1).allowed);
        assert!(ctl.take("len", 1, 5000, 1).allowed);
    }

    #[test]
    fn memory_is_bounded() {
        let now = ManualClock::new(0.0, 0);
        let ctl = LocalControl::with_max_keys(100, now.clone());
        for i in 0..1000 {
            ctl.take(&format!("k{i}"), 5, 1000, 1);
            ctl.consume_once(&format!("o{i}"), 60000);
        }
        assert!(ctl.windows.lock().len() <= 100);
        assert!(ctl.once.lock().len() <= 100);
        // Expired windows go first.
        now.advance(10_000.0);
        ctl.take("fresh", 5, 1000, 1);
        assert!(ctl.windows.lock().len() <= 100);
        assert!(ctl.windows.lock().contains("fresh"));
        // A sweep gives the memory back.
        now.advance(120_000.0);
        assert!(ctl.sweep() > 0);
        assert_eq!((ctl.windows.lock().len(), ctl.once.lock().len()), (0, 0));
    }
}
