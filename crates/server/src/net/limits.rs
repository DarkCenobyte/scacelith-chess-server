//! Rate limiters and single-use keys (DESIGN 5.7 and 8).
//!
//! * [`TokenBucketLimiter`]: per-key token buckets with an LRU bound (the per-address request
//!   and connection budgets of the IP guard, the per-account budget and the per-route rates of
//!   the API). An evicted bucket counts as full again, which only makes the limit more lenient.
//! * [`SlidingWindowLimiter`]: sliding-window counters (the previous fixed window weighted by how
//!   much of it still overlaps the sliding window, plus the current window), accurate to a few
//!   percent, O(1) below capacity. Keys expire two windows after their last use; beyond `max_keys`
//!   the expired keys go first (the first 64 in insertion order), then the 16 oldest insertions.
//!   The expired keys are found through a min-heap on a lower bound of their expiry.
//! * [`OnceStore`]: keys remembered until their time to live (proof-of-work challenges, TOTP
//!   steps); bounded, the oldest insertions evicted first.
//! * [`SharedLimits`]: the process-wide sliding windows and single-use keys behind a lock, what the
//!   Node primary served over IPC (`ratelimit.take`, `ratelimit.refund`, `once.consume`). With one
//!   process the counts are exact: there is no per-worker share any more.
//!
//! Times are monotonic milliseconds (fractional), passed in by the caller so that tests drive
//! them; expiry is lazy, no timer is involved.

use std::borrow::Borrow;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::hash::Hash;

use parking_lot::Mutex;

use super::linked::LinkedMap;
use crate::clock::SharedClock;

/// The outcome of a token-bucket take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketGrant {
    /// Whether the tokens were taken.
    pub allowed: bool,
    /// When refused, the milliseconds until `cost` tokens are back (rounded up).
    pub retry_after_ms: f64,
    /// Whole tokens left after the take.
    pub remaining: f64,
}

/// One token bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    /// Tokens held (fractional).
    pub tokens: f64,
    /// When the tokens were last computed.
    pub at: f64,
}

/// Per-key token buckets: `limit` tokens refilled evenly over `window_ms`. A bucket that holds a
/// burst `b` and refills `r` tokens per minute is `take(key, b, b * 60000 / r, ..)`.
#[derive(Debug)]
pub struct TokenBucketLimiter<K> {
    buckets: LinkedMap<K, Bucket>,
    max_keys: usize,
}

impl<K: Hash + Eq + Clone> TokenBucketLimiter<K> {
    /// A limiter keeping at most `max_keys` buckets (least recently used evicted first).
    pub fn new(max_keys: usize) -> TokenBucketLimiter<K> {
        TokenBucketLimiter { buckets: LinkedMap::new(), max_keys: max_keys.max(1) }
    }

    /// Takes `cost` tokens from the bucket of `key` at time `now`.
    pub fn take<Q>(&mut self, key: &Q, limit: f64, window_ms: f64, cost: f64, now: f64) -> BucketGrant
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        let rate = limit / window_ms;
        let b = if self.buckets.move_to_back(key) {
            let b = self.buckets.get_mut(key).expect("present: just moved");
            b.tokens = limit.min(b.tokens + (now - b.at).max(0.0) * rate);
            b.at = now;
            b
        } else {
            if self.buckets.len() >= self.max_keys {
                self.buckets.pop_front();
            }
            self.buckets.insert(key.to_owned(), Bucket { tokens: limit, at: now });
            self.buckets.get_mut(key).expect("present: just inserted")
        };
        if b.tokens >= cost {
            b.tokens -= cost;
            return BucketGrant { allowed: true, retry_after_ms: 0.0, remaining: b.tokens.floor() };
        }
        BucketGrant { allowed: false, retry_after_ms: ((cost - b.tokens) / rate).ceil(), remaining: 0.0 }
    }

    /// Gives back `cost` tokens that [`take`](Self::take) granted to `key` (a request that did
    /// nothing), never beyond `limit`. A bucket evicted meanwhile is full already. The bucket's
    /// place in the LRU order does not change.
    pub fn give<Q>(&mut self, key: &Q, limit: f64, window_ms: f64, cost: f64, now: f64)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        if let Some(b) = self.buckets.get_mut(key) {
            b.tokens = limit.min(b.tokens + (now - b.at).max(0.0) * (limit / window_ms) + cost);
            b.at = now;
        }
    }

    /// Forgets the bucket of `key` (full again).
    pub fn reset<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        self.buckets.remove(key);
    }

    /// The bucket of `key`, without refreshing it.
    pub fn peek<Q>(&self, key: &Q) -> Option<&Bucket>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        self.buckets.get(key)
    }

    /// Number of buckets kept.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no bucket is kept.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// The outcome of a sliding-window take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowGrant {
    /// Whether the units were counted.
    pub allowed: bool,
    /// When refused, the milliseconds until `cost` more units fit.
    pub retry_after_ms: f64,
    /// The estimate after the take (allowed) or before it (refused), rounded up.
    pub count: f64,
}

#[derive(Debug, Clone)]
struct WindowEntry {
    start: f64,
    cur: f64,
    prev: f64,
    window_ms: f64,
    last: f64,
    /// A lower bound of the expiry (`last + 2 windows`), the heap's order.
    exp: f64,
    /// Generation of the entry: heap items of an older generation are stale.
    generation: u64,
}

#[derive(Debug)]
struct HeapItem<K> {
    exp: f64,
    generation: u64,
    key: K,
}

impl<K> PartialEq for HeapItem<K> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<K> Eq for HeapItem<K> {}

impl<K> PartialOrd for HeapItem<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for HeapItem<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.exp.total_cmp(&other.exp).then(self.generation.cmp(&other.generation))
    }
}

/// Sliding-window counters keyed by `K` (see the module documentation).
#[derive(Debug)]
pub struct SlidingWindowLimiter<K> {
    entries: LinkedMap<K, WindowEntry>,
    heap: BinaryHeap<Reverse<HeapItem<K>>>,
    max_keys: usize,
    next_generation: u64,
    evicted: u64,
}

/// Expired keys taken from the heap in one eviction: more than 64 means a walk in insertion order
/// picks the 64 to drop.
const EXPIRED_BATCH: usize = 64;
/// Fresh keys dropped in one eviction when none has expired.
const LRU_BATCH: usize = 16;

impl<K: Hash + Eq + Clone> SlidingWindowLimiter<K> {
    /// A limiter keeping at most `max_keys` keys.
    pub fn new(max_keys: usize) -> SlidingWindowLimiter<K> {
        SlidingWindowLimiter {
            entries: LinkedMap::new(),
            heap: BinaryHeap::new(),
            max_keys: max_keys.max(1),
            next_generation: 0,
            evicted: 0,
        }
    }

    /// Number of keys kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no key is kept.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fresh keys dropped at capacity so far (expired keys are not counted).
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// The keys, in insertion order (tests).
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    /// Takes `cost` units from the allowance of `limit` per `window_ms` of `key`.
    pub fn take<Q>(&mut self, key: &Q, limit: f64, window_ms: u64, cost: f64, now: f64) -> WindowGrant
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        let w = window_ms.clamp(1, i32::MAX as u64) as f64;
        let limit = if limit.is_nan() { 0.0 } else { limit.max(0.0) };
        let cost = if cost.is_nan() { 0.0 } else { cost.max(0.0) };
        let same_window = self.entries.get(key).map(|e| e.window_ms == w);
        if same_window != Some(true) {
            if same_window.is_none() && self.entries.len() >= self.max_keys {
                self.evict(now);
            }
            let generation = self.bump_generation();
            let entry = WindowEntry {
                start: now - now % w,
                cur: 0.0,
                prev: 0.0,
                window_ms: w,
                last: now,
                exp: now + 2.0 * w,
                generation,
            };
            self.heap.push(Reverse(HeapItem { exp: entry.exp, generation, key: key.to_owned() }));
            self.entries.insert(key.to_owned(), entry);
        }
        let mut lowered = None;
        let e = self.entries.get_mut(key).expect("present: just ensured");
        advance(e, now);
        e.last = now;
        // A later expiry waits for the eviction to find it; an earlier one (the clock stepped back)
        // is filed again now.
        let exp = now + 2.0 * w;
        if exp < e.exp {
            e.exp = exp;
            lowered = Some(e.generation);
        }
        let estimate = e.prev * (1.0 - (now - e.start) / w) + e.cur;
        let grant = if estimate + cost <= limit {
            e.cur += cost;
            WindowGrant { allowed: true, retry_after_ms: 0.0, count: (estimate + cost).ceil() }
        } else {
            WindowGrant {
                allowed: false,
                retry_after_ms: retry_after(e, now, limit, cost),
                count: estimate.ceil(),
            }
        };
        if lowered.is_some() {
            let generation = self.bump_generation();
            let e = self.entries.get_mut(key).expect("present");
            e.generation = generation;
            self.heap.push(Reverse(HeapItem { exp, generation, key: key.to_owned() }));
            self.compact_heap();
        }
        grant
    }

    /// Gives back `cost` units that [`take`](Self::take) granted `age_ms` ago, from the fixed
    /// window that counted them (the current one or the one before; an older window no longer
    /// counts). Returns whether anything was given back.
    pub fn refund<Q>(&mut self, key: &Q, window_ms: u64, cost: f64, age_ms: f64, now: f64) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        let w = window_ms.clamp(1, i32::MAX as u64) as f64;
        let cost = if cost.is_nan() { 0.0 } else { cost.max(0.0) };
        let Some(e) = self.entries.get_mut(key) else { return false };
        if e.window_ms != w {
            return false;
        }
        advance(e, now);
        let at = now - if age_ms.is_nan() { 0.0 } else { age_ms.max(0.0) };
        let start = at - at % w;
        if start == e.start {
            e.cur = (e.cur - cost).max(0.0);
        } else if start == e.start - w {
            e.prev = (e.prev - cost).max(0.0);
        } else {
            return false;
        }
        true
    }

    /// The current estimate of `key` (0 when unknown), without taking anything.
    pub fn peek<Q>(&mut self, key: &Q, now: f64) -> f64
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        let Some(e) = self.entries.get_mut(key) else { return 0.0 };
        advance(e, now);
        e.prev * (1.0 - (now - e.start) / e.window_ms) + e.cur
    }

    /// Removes the keys unused for two windows. Returns how many.
    pub fn sweep(&mut self, now: f64) -> usize {
        let n = self.entries.retain(|_, e| now - e.last <= 2.0 * e.window_ms);
        self.compact_heap();
        n
    }

    /// Forgets the counts of `key`: its next take starts from zero.
    pub fn forget<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        if self.entries.remove(key).is_some() {
            self.compact_heap();
        }
    }

    fn bump_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Makes room for one key: the expired keys first (the first 64 in insertion order), else
    /// the 16 oldest insertions.
    fn evict(&mut self, now: f64) {
        let expired = self.take_expired(now, EXPIRED_BATCH + 1);
        if expired.len() > EXPIRED_BATCH {
            for item in expired {
                self.heap.push(Reverse(item));
            }
            let mut removed = 0;
            self.entries.retain_while(|_, e| {
                if removed >= EXPIRED_BATCH {
                    return None;
                }
                let keep = now - e.last <= 2.0 * e.window_ms;
                if !keep {
                    removed += 1;
                }
                Some(keep)
            });
            self.compact_heap();
            return;
        }
        if !expired.is_empty() {
            for item in expired {
                self.entries.remove(&item.key);
            }
            return;
        }
        for _ in 0..LRU_BATCH {
            if self.entries.pop_front().is_none() {
                break;
            }
            self.evicted += 1;
        }
        self.compact_heap();
    }

    /// Takes up to `max` expired keys out of the heap. Keys whose bound has passed but which were
    /// used since are filed again at their expiry; stale items are dropped.
    fn take_expired(&mut self, now: f64, max: usize) -> Vec<HeapItem<K>> {
        let mut out = Vec::new();
        while out.len() < max {
            let Some(Reverse(top)) = self.heap.peek() else { break };
            if top.exp > now {
                break;
            }
            let Reverse(mut item) = self.heap.pop().expect("peeked");
            let Some(e) = self.entries.get_mut(&item.key) else { continue };
            if e.generation != item.generation {
                continue;
            }
            if now - e.last > 2.0 * e.window_ms {
                out.push(item);
            } else {
                e.exp = e.last + 2.0 * e.window_ms;
                item.exp = e.exp;
                self.heap.push(Reverse(item));
            }
        }
        out
    }

    /// Rebuilds the heap when stale items outnumber the live ones.
    fn compact_heap(&mut self) {
        if self.heap.len() <= 2 * self.entries.len() + 64 {
            return;
        }
        let items: Vec<Reverse<HeapItem<K>>> = self
            .entries
            .iter()
            .map(|(k, e)| Reverse(HeapItem { exp: e.exp, generation: e.generation, key: k.clone() }))
            .collect();
        self.heap = BinaryHeap::from(items);
    }
}

fn advance(e: &mut WindowEntry, now: f64) {
    let w = e.window_ms;
    if now < e.start + w {
        return;
    }
    let start = now - now % w;
    e.prev = if start - e.start == w { e.cur } else { 0.0 };
    e.cur = 0.0;
    e.start = start;
}

/// Time until `cost` more units fit: the previous window's weight decays linearly, and the current
/// count only leaves when the window rolls over.
fn retry_after(e: &WindowEntry, now: f64, limit: f64, cost: f64) -> f64 {
    let w = e.window_ms;
    let to_next = e.start + w - now;
    // Within the current window: prev * (1 - (t - start) / w) + cur + cost <= limit.
    if e.cur + cost <= limit && e.prev > 0.0 {
        let t = e.start + w * (1.0 - (limit - cost - e.cur) / e.prev);
        if t > now && t <= e.start + w {
            return (t - now).ceil();
        }
    }
    // After the rollover: cur becomes prev and decays over the next window.
    if cost > limit {
        return to_next + w;
    }
    if e.cur == 0.0 {
        return to_next;
    }
    let frac = 1.0 - (limit - cost) / e.cur;
    (to_next + frac.max(0.0) * w).ceil()
}

/// Single-use keys with a time to live (see the module documentation).
#[derive(Debug)]
pub struct OnceStore<K> {
    keys: LinkedMap<K, f64>,
    max_keys: usize,
    evicted: u64,
}

impl<K: Hash + Eq + Clone> OnceStore<K> {
    /// A store keeping at most `max_keys` keys.
    pub fn new(max_keys: usize) -> OnceStore<K> {
        OnceStore { keys: LinkedMap::new(), max_keys: max_keys.max(1), evicted: 0 }
    }

    /// Marks `key` as used for `ttl_ms`. Returns true the first time (and again after its TTL).
    pub fn consume<Q>(&mut self, key: &Q, ttl_ms: u64, now: f64) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        match self.keys.get(key) {
            Some(&exp) if exp > now => return false,
            Some(_) => {
                self.keys.remove(key);
            }
            None if self.keys.len() >= self.max_keys => self.evict(now),
            None => {}
        }
        self.keys.insert(key.to_owned(), now + ttl_ms.clamp(1, i32::MAX as u64) as f64);
        true
    }

    fn evict(&mut self, now: f64) {
        for _ in 0..64 {
            let Some((_, exp)) = self.keys.pop_front() else { break };
            if exp > now {
                self.evicted += 1;
            }
        }
    }

    /// Removes the expired keys. Returns how many.
    pub fn sweep(&mut self, now: f64) -> usize {
        self.keys.retain(|_, exp| *exp > now)
    }

    /// Number of keys kept.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is kept.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Live keys dropped at capacity so far.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

/// Keys kept by [`SharedLimits`] in each of its stores.
pub const SHARED_LIMITS_MAX_KEYS: usize = 200_000;

/// The process-wide sliding windows and single-use keys, on the monotonic clock (a step of the
/// wall clock locks no key out). Shared rates of the API (`RateSpec::shared`) and the
/// authentication limits count here.
pub struct SharedLimits {
    clock: SharedClock,
    windows: Mutex<SlidingWindowLimiter<String>>,
    once: Mutex<OnceStore<String>>,
}

impl std::fmt::Debug for SharedLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedLimits").field("windows", &self.windows.lock().len()).finish()
    }
}

impl SharedLimits {
    /// Stores of [`SHARED_LIMITS_MAX_KEYS`] keys each.
    pub fn new(clock: SharedClock) -> SharedLimits {
        SharedLimits::with_max_keys(clock, SHARED_LIMITS_MAX_KEYS)
    }

    /// Stores of `max_keys` keys each.
    pub fn with_max_keys(clock: SharedClock, max_keys: usize) -> SharedLimits {
        SharedLimits {
            clock,
            windows: Mutex::new(SlidingWindowLimiter::new(max_keys)),
            once: Mutex::new(OnceStore::new(max_keys)),
        }
    }

    /// Takes `cost` units of `limit` per `window_ms` for `key` (`ratelimit.take`).
    pub fn take(&self, key: &str, limit: f64, window_ms: u64, cost: f64) -> WindowGrant {
        let now = self.clock.mono_ms();
        self.windows.lock().take(key, limit, window_ms, cost, now)
    }

    /// Gives back units granted `age_ms` ago (`ratelimit.refund`). Returns whether it did.
    pub fn refund(&self, key: &str, window_ms: u64, cost: f64, age_ms: f64) -> bool {
        let now = self.clock.mono_ms();
        self.windows.lock().refund(key, window_ms, cost, age_ms, now)
    }

    /// The current estimate of `key`, without taking anything.
    pub fn peek(&self, key: &str) -> f64 {
        let now = self.clock.mono_ms();
        self.windows.lock().peek(key, now)
    }

    /// Marks a single-use key as used for `ttl_ms` (`once.consume`). True the first time.
    pub fn consume(&self, key: &str, ttl_ms: u64) -> bool {
        let now = self.clock.mono_ms();
        self.once.lock().consume(key, ttl_ms, now)
    }

    /// Removes the expired keys of both stores (every 10 s, as the Node primary).
    pub fn sweep(&self) {
        let now = self.clock.mono_ms();
        self.windows.lock().sweep(now);
        self.once.lock().sweep(now);
    }

    /// The clock the limits run on.
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use std::sync::Arc;

    fn k(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn lru_keeps_the_most_recent_buckets() {
        let mut l = TokenBucketLimiter::new(2);
        l.take(&k("a"), 1.0, 1000.0, 1.0, 0.0);
        l.take(&k("b"), 1.0, 1000.0, 1.0, 0.0);
        l.take(&k("a"), 1.0, 1000.0, 1.0, 0.0);
        l.take(&k("c"), 1.0, 1000.0, 1.0, 0.0);
        assert!(l.peek(&k("a")).is_some() && l.peek(&k("c")).is_some() && l.peek(&k("b")).is_none());
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn token_bucket_burst_refusal_and_refill() {
        let mut l = TokenBucketLimiter::new(100_000);
        let mut now = 1_000_000.0;
        for _ in 0..3 {
            assert!(l.take(&k("k"), 3.0, 60000.0, 1.0, now).allowed);
        }
        let r = l.take(&k("k"), 3.0, 60000.0, 1.0, now);
        assert!(!r.allowed);
        assert_eq!(r.retry_after_ms, 20000.0);
        now += 20000.0;
        assert!(l.take(&k("k"), 3.0, 60000.0, 1.0, now).allowed);
        assert!(l.take(&k("other"), 3.0, 60000.0, 1.0, now).allowed, "keys are independent");
    }

    #[test]
    fn token_bucket_give_never_exceeds_the_limit() {
        let mut l = TokenBucketLimiter::new(100_000);
        let mut now = 0.0;
        let take = |l: &mut TokenBucketLimiter<String>, now| l.take(&k("k"), 3.0, 60000.0, 1.0, now).allowed;
        for _ in 0..3 {
            assert!(take(&mut l, now));
        }
        assert!(!take(&mut l, now));
        l.give(&k("k"), 3.0, 60000.0, 1.0, now);
        assert!(take(&mut l, now), "the given-back token is taken again");
        assert!(!take(&mut l, now));
        now += 20000.0;
        for _ in 0..3 {
            l.give(&k("k"), 3.0, 60000.0, 1.0, now);
        }
        for _ in 0..3 {
            assert!(take(&mut l, now));
        }
        assert!(!take(&mut l, now));
        l.give(&k("unknown"), 3.0, 60000.0, 1.0, now);
        assert_eq!(l.len(), 1, "an unknown bucket is full already");
    }

    #[test]
    fn token_bucket_memory_is_bounded() {
        let mut l = TokenBucketLimiter::new(100);
        for i in 0..1000 {
            l.take(&format!("k{i}"), 1.0, 1000.0, 1.0, 0.0);
        }
        assert_eq!(l.len(), 100);
    }

    #[test]
    fn token_bucket_rate_as_limit_and_window() {
        // 600 per minute with a burst of half a minute: take(key, 300, 30000).
        let mut l = TokenBucketLimiter::new(100);
        let mut now = 0.0;
        for _ in 0..300 {
            assert!(l.take(&k("k"), 300.0, 30000.0, 1.0, now).allowed);
        }
        let r = l.take(&k("k"), 300.0, 30000.0, 1.0, now);
        assert_eq!((r.allowed, r.retry_after_ms), (false, 100.0), "one token per 100 ms");
        now += 60000.0;
        let mut n = 0;
        while l.take(&k("k"), 300.0, 30000.0, 1.0, now).allowed {
            n += 1;
        }
        assert_eq!(n, 300, "a minute refills the burst, not more");
    }

    #[test]
    fn window_allows_up_to_the_limit_then_refuses_with_a_retry_time() {
        let mut l = SlidingWindowLimiter::new(200_000);
        let mut t = 1_000_000.0;
        for _ in 0..5 {
            assert!(l.take(&k("k"), 5.0, 1000, 1.0, t).allowed);
        }
        let r = l.take(&k("k"), 5.0, 1000, 1.0, t);
        assert!(!r.allowed);
        assert!(r.retry_after_ms > 0.0 && r.retry_after_ms <= 2000.0);
        assert!(l.take(&k("other"), 5.0, 1000, 1.0, t).allowed);
        t += r.retry_after_ms;
        assert!(l.take(&k("k"), 5.0, 1000, 1.0, t).allowed);
    }

    #[test]
    fn window_slides() {
        let mut l = SlidingWindowLimiter::new(200_000);
        for _ in 0..10 {
            l.take(&k("k"), 10.0, 1000, 1.0, 0.0);
        }
        let mut ok = 0;
        while l.take(&k("k"), 10.0, 1000, 1.0, 1500.0).allowed {
            ok += 1;
        }
        assert_eq!(ok, 5, "half of the previous window still counts");
        assert_eq!(l.peek(&k("k"), 3000.0), 0.0);
    }

    #[test]
    fn window_refund_takes_back_from_the_window_that_counted() {
        let mut l = SlidingWindowLimiter::new(200_000);
        let mut t = 0.0;
        let take = |l: &mut SlidingWindowLimiter<String>, t| l.take(&k("k"), 2.0, 1000, 1.0, t).allowed;
        assert_eq!([take(&mut l, t), take(&mut l, t), take(&mut l, t)], [true, true, false]);
        assert!(l.refund(&k("k"), 1000, 1.0, 0.0, t));
        assert_eq!([take(&mut l, t), take(&mut l, t)], [true, false]);
        t = 1250.0;
        assert!(l.refund(&k("k"), 1000, 1.0, 1150.0, t));
        assert!(take(&mut l, t), "prev 1 x 0.75 + 1 <= 2");
        assert!(!take(&mut l, t));
        for _ in 0..5 {
            l.refund(&k("k"), 1000, 1.0, 0.0, t);
        }
        assert!(l.peek(&k("k"), t) >= 0.0);
        assert!(!l.refund(&k("nope"), 1000, 1.0, 0.0, t));
        assert!(!l.refund(&k("k"), 60000, 1.0, 0.0, t));
        assert!(!l.refund(&k("k"), 1000, 1.0, 5000.0, t));
    }

    #[test]
    fn window_honours_cost_and_stays_bounded() {
        let mut l = SlidingWindowLimiter::new(100);
        assert!(l.take(&k("c"), 10.0, 1000, 7.0, 0.0).allowed);
        assert!(!l.take(&k("c"), 10.0, 1000, 7.0, 0.0).allowed);
        for i in 0..1000 {
            l.take(&format!("ip{i}"), 1.0, 1000, 1.0, 0.0);
        }
        assert!(l.len() <= 100);
        l.sweep(10_000.0);
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn window_retry_after_matches_the_primary() {
        let mut l = SlidingWindowLimiter::new(100);
        let mut t = 0.0;
        assert_eq!(
            l.take(&k("k"), 2.0, 1000, 1.0, t),
            WindowGrant { allowed: true, retry_after_ms: 0.0, count: 1.0 }
        );
        assert_eq!(
            l.take(&k("k"), 2.0, 1000, 1.0, t),
            WindowGrant { allowed: true, retry_after_ms: 0.0, count: 2.0 }
        );
        let r = l.take(&k("k"), 2.0, 1000, 1.0, t);
        assert_eq!((r.allowed, r.retry_after_ms), (false, 1500.0));
        t += 1499.0;
        assert!(!l.take(&k("k"), 2.0, 1000, 1.0, t).allowed);
        t += 1.0;
        assert!(l.take(&k("k"), 2.0, 1000, 1.0, t).allowed, "allowed when its Retry-After ends");
        // Limit 5 per minute, the 6th take 2 s into the window: 70 s.
        t = 120_000.0 + 2000.0;
        for _ in 0..5 {
            assert!(l.take(&k("k5"), 5.0, 60000, 1.0, t).allowed);
        }
        let r5 = l.take(&k("k5"), 5.0, 60000, 1.0, t);
        assert_eq!((r5.allowed, r5.retry_after_ms), (false, 70000.0));
        t += 70000.0;
        assert!(l.take(&k("k5"), 5.0, 60000, 1.0, t).allowed);
    }

    #[test]
    fn window_drops_a_key_expired_by_a_rounding_hair() {
        let last = 1237.5581823846903_f64;
        let now = last + 2000.0;
        assert!(now - last > 2000.0);
        let mut l = SlidingWindowLimiter::new(2);
        for key in ["x", "y"] {
            l.take(&k(key), 1.0, 1000, 1.0, last);
        }
        l.take(&k("z"), 1.0, 1000, 1.0, now);
        assert_eq!(l.keys().cloned().collect::<Vec<_>>(), ["z"]);
        assert_eq!(l.evicted(), 0);
    }

    /// The Node limiter before its expiry heap: a walk of the map for the expired keys.
    struct Reference {
        entries: LinkedMap<String, (f64, f64, f64, f64, f64)>, // start, cur, prev, window, last
        max_keys: usize,
        evicted: u64,
    }

    impl Reference {
        fn take(&mut self, key: &str, limit: f64, window_ms: u64, cost: f64, now: f64) -> WindowGrant {
            let w = window_ms as f64;
            let key = key.to_string();
            let fresh = self.entries.get(&key).is_none_or(|e| e.3 != w);
            if fresh {
                if self.entries.get(&key).is_none() && self.entries.len() >= self.max_keys {
                    self.evict(now);
                }
                self.entries.insert(key.clone(), (now - now % w, 0.0, 0.0, w, now));
            }
            let e = self.entries.get_mut(&key).expect("present");
            let mut we = WindowEntry {
                start: e.0,
                cur: e.1,
                prev: e.2,
                window_ms: w,
                last: e.4,
                exp: 0.0,
                generation: 0,
            };
            advance(&mut we, now);
            we.last = now;
            let estimate = we.prev * (1.0 - (now - we.start) / w) + we.cur;
            let r = if estimate + cost <= limit {
                we.cur += cost;
                WindowGrant { allowed: true, retry_after_ms: 0.0, count: (estimate + cost).ceil() }
            } else {
                WindowGrant {
                    allowed: false,
                    retry_after_ms: retry_after(&we, now, limit, cost),
                    count: estimate.ceil(),
                }
            };
            *e = (we.start, we.cur, we.prev, w, we.last);
            r
        }

        fn evict(&mut self, now: f64) {
            let mut removed = 0;
            self.entries.retain_while(|_, e| {
                if removed >= 64 {
                    return None;
                }
                let keep = now - e.4 <= 2.0 * e.3;
                if !keep {
                    removed += 1;
                }
                Some(keep)
            });
            if removed > 0 {
                return;
            }
            for _ in 0..16 {
                if self.entries.pop_front().is_none() {
                    break;
                }
                self.evicted += 1;
            }
        }
    }

    #[test]
    fn window_decides_and_evicts_as_the_reference_at_capacity() {
        let mut seed: u64 = 20261003;
        let mut rnd = move || {
            seed = (seed * 1103515245 + 12345) % 2147483648;
            seed as f64 / 2147483648.0
        };
        let windows = [100u64, 1000, 7000, 60000];
        for run in 0..8 {
            let mut t = 1e6 + rnd() * 1000.0;
            let max_keys = 70 + (rnd() * 250.0) as usize;
            let mut a = SlidingWindowLimiter::new(max_keys);
            let mut b = Reference { entries: LinkedMap::new(), max_keys, evicted: 0 };
            let pace = [0.0, 0.5, 5.0, 40.0][run % 4];
            for i in 0..6000 {
                let step = rnd();
                if step < 0.005 {
                    t -= rnd() * 3000.0;
                } else if step < 0.008 {
                    t += 20000.0 + rnd() * 100000.0;
                } else {
                    t += rnd() * pace + if rnd() < 0.5 { 0.001 } else { 0.0 };
                }
                let key = format!("k{}", (rnd() * if rnd() < 0.7 { 1e6 } else { 300.0 }) as u64);
                let limit = 1.0 + (rnd() * 5.0).floor();
                let w = windows[(rnd() * windows.len() as f64) as usize];
                let cost = if rnd() < 0.9 { 1.0 } else { 2.0 };
                let op = rnd();
                if op < 0.9 {
                    assert_eq!(a.take(&key, limit, w, cost, t), b.take(&key, limit, w, cost, t), "op {i}");
                } else if op < 0.95 {
                    a.forget(&key);
                    b.entries.remove(&key);
                } else {
                    let n = a.sweep(t);
                    let m = b.entries.retain(|_, e| t - e.4 <= 2.0 * e.3);
                    assert_eq!(n, m);
                }
                assert_eq!(a.evicted(), b.evicted);
                assert_eq!(a.len(), b.entries.len());
                if i % 500 == 0 {
                    assert!(a.keys().eq(b.entries.keys()), "same keys in the same order");
                }
            }
        }
    }

    #[test]
    fn once_store_is_fresh_once_per_ttl_bounded_and_swept() {
        let mut o = OnceStore::new(50);
        assert!(o.consume(&k("a"), 100, 0.0));
        assert!(!o.consume(&k("a"), 100, 0.0));
        assert!(o.consume(&k("a"), 100, 150.0));
        for i in 0..500 {
            o.consume(&format!("k{i}"), 1000, 150.0);
        }
        assert!(o.len() <= 50);
        o.sweep(10_000.0);
        assert_eq!(o.len(), 0);
    }

    #[test]
    fn shared_limits_take_refund_and_consume() {
        let clock = ManualClock::new(0.0, 0);
        let ctl = SharedLimits::new(clock.clone() as Arc<dyn crate::clock::Clock>);
        assert!(ctl.take("k", 2.0, 1000, 1.0).allowed);
        assert!(ctl.take("k", 2.0, 1000, 1.0).allowed);
        assert!(!ctl.take("k", 2.0, 1000, 1.0).allowed);
        assert!(ctl.refund("k", 1000, 1.0, 0.0));
        assert!(ctl.take("k", 2.0, 1000, 1.0).allowed, "the refunded unit is free again");
        assert!(!ctl.take("k", 2.0, 1000, 1.0).allowed);
        clock.advance(1250.0);
        assert!(ctl.refund("k", 1000, 1.0, 1150.0));
        assert!(ctl.take("k", 2.0, 1000, 1.0).allowed);
        assert!(!ctl.take("k", 2.0, 1000, 1.0).allowed);
        assert!(!ctl.refund("nope", 1000, 1.0, 0.0));
        assert!(!ctl.refund("k", 1000, 1.0, 5000.0));
        assert!(ctl.consume("x", 100));
        assert!(!ctl.consume("x", 100));
        clock.advance(101.0);
        assert!(ctl.consume("x", 100));
    }
}
