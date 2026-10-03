//! Escalating temporary blocks of client addresses (DESIGN 5.7 and 8).
//!
//! The IP guard counts what it refuses to each address (rate-limit 429s, connections refused
//! before TLS, malformed requests, failed TLS handshakes) and reports the sums once a second at
//! most. The tracker adds them up over a sliding minute and, when an address keeps going after
//! being refused, blocks it; the guard then closes the address's new connections with an RST
//! before any TLS work and answers its requests 429.
//!
//! Decision:
//! * an IPv4 address or IPv6 /64 is blocked when its weighted refusals of the last minute reach
//!   `ABUSE_BLOCK_REFUSALS_PER_MIN` (T); an IPv6 /48 when its /64s together reach 4 T, or when
//!   [`ABUSE_PREFIX_BLOCK_AFTER`] of its /64s are blocked at the same time;
//! * level: the key's previous level + 1, or 1 when its last block began more than
//!   [`ABUSE_BLOCK_FORGET_MS`] ago; duration `min(ABUSE_BLOCK_MAX_SEC, ABUSE_BLOCK_BASE_SEC *
//!   4^(level - 1))`: 1 min, 4 min, 16 min, then 1 h with the defaults;
//! * reports for a key that is blocked (or whose /48 is) are ignored, and the refusals that led to
//!   a block are forgotten with it: after the block the key counts from zero;
//! * at most [`ABUSE_MAX_BLOCKS`] running blocks, the oldest ending first;
//! * T = 0 turns blocking off (the per-address budgets still apply).
//!
//! Times are monotonic milliseconds passed by the caller.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::ip::AddrKey;
use super::limits::SlidingWindowLimiter;
use super::linked::LinkedMap;
use crate::config::Config;
use crate::log::{Level, Logger};
use crate::metrics;

/// A key whose last block began longer ago than this starts again at level 1.
pub const ABUSE_BLOCK_FORGET_MS: f64 = 6.0 * 3_600_000.0;
/// /64 networks of one /48 blocked at the same time that block the /48 itself.
pub const ABUSE_PREFIX_BLOCK_AFTER: u32 = 4;
/// Running blocks kept.
pub const ABUSE_MAX_BLOCKS: usize = 20_000;
/// Each new block of a key within [`ABUSE_BLOCK_FORGET_MS`] lasts this many times longer.
pub const ABUSE_BLOCK_FACTOR: f64 = 4.0;
/// The /48 thresholds and caps, in /64 ones.
pub const ABUSE_PREFIX_FACTOR: u32 = 4;
const WINDOW_MS: u64 = 60_000;
/// The largest weight of one report entry.
const MAX_WEIGHT: f64 = 1e6;
/// Keys of the sliding window.
const WINDOW_KEYS: usize = 100_000;

/// Refusals of one address over a report interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefusalEntry {
    /// The IPv4 address or IPv6 /64.
    pub k64: AddrKey,
    /// The IPv6 /48 (`None` for IPv4).
    pub k48: Option<AddrKey>,
    /// The summed weights.
    pub weight: f64,
}

/// A block: the key, its duration and its level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockOrder {
    /// The blocked key (an address, a /64 or a /48).
    pub key: AddrKey,
    /// Milliseconds the block lasts.
    pub ttl_ms: f64,
    /// 1 for a first block, more for each further block within 6 hours.
    pub level: u32,
}

impl BlockOrder {
    /// A block order (tests and callers building blocks by hand).
    pub fn new(key: AddrKey, ttl_ms: f64, level: u32) -> BlockOrder {
        BlockOrder { key, ttl_ms, level }
    }
}

/// The scope of a block: one address or /64, or a whole /48.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockScope {
    /// An IPv4 address or an IPv6 /64.
    Ip,
    /// An IPv6 /48.
    Prefix,
}

impl BlockScope {
    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            BlockScope::Ip => "ip",
            BlockScope::Prefix => "prefix",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Block {
    until: f64,
    level: u32,
    scope: BlockScope,
    key48: Option<AddrKey>,
}

#[derive(Debug, Clone, Copy)]
struct History {
    level: u32,
    at: f64,
}

struct TrackerMetrics {
    blocks_total: metrics::CounterVec,
    blocked: metrics::GaugeVec,
    evicted: metrics::Counter,
}

fn tracker_metrics() -> &'static TrackerMetrics {
    static M: std::sync::LazyLock<TrackerMetrics> = std::sync::LazyLock::new(|| TrackerMetrics {
        blocks_total: metrics::counter_vec(
            "scacelith_abuse_blocks_total",
            "Addresses blocked before TLS, by scope (ip: IPv4 address or IPv6 /64; prefix: IPv6 /48) and level (4: the fourth block within 6 h or later)",
            &["scope", "level"],
        ),
        blocked: metrics::gauge_vec(
            "scacelith_abuse_blocked",
            "Running blocks of addresses, by scope",
            &["scope"],
        ),
        evicted: metrics::counter(
            "scacelith_abuse_blocks_evicted_total",
            "Blocks ended early because ABUSE_MAX_BLOCKS were running",
        ),
    });
    &M
}

/// The tracker of refusals and blocks (see the module documentation).
pub struct AbuseTracker {
    threshold: f64,
    base_ms: f64,
    max_ms: f64,
    max_blocks: usize,
    window: SlidingWindowLimiter<AddrKey>,
    blocks: LinkedMap<AddrKey, Block>,
    history: LinkedMap<AddrKey, History>,
    blocked_per_prefix: HashMap<AddrKey, u32>,
    count_ip: u64,
    count_prefix: u64,
    evicted: u64,
    log: Logger,
}

impl std::fmt::Debug for AbuseTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbuseTracker").field("blocks", &self.blocks.len()).finish()
    }
}

impl AbuseTracker {
    /// A tracker with `ABUSE_BLOCK_REFUSALS_PER_MIN`, `ABUSE_BLOCK_BASE_SEC` and
    /// `ABUSE_BLOCK_MAX_SEC` of `config`.
    pub fn new(config: &Config, log: Logger) -> AbuseTracker {
        AbuseTracker::with_max_blocks(config, log, ABUSE_MAX_BLOCKS)
    }

    /// A tracker keeping at most `max_blocks` running blocks.
    pub fn with_max_blocks(config: &Config, log: Logger, max_blocks: usize) -> AbuseTracker {
        let base_ms = 1000.0 * config.abuse_block_base_sec.max(1) as f64;
        AbuseTracker {
            threshold: config.abuse_block_refusals_per_min.max(0) as f64,
            base_ms,
            max_ms: base_ms.max(1000.0 * config.abuse_block_max_sec as f64),
            max_blocks: max_blocks.max(1),
            window: SlidingWindowLimiter::new(WINDOW_KEYS),
            blocks: LinkedMap::new(),
            history: LinkedMap::new(),
            blocked_per_prefix: HashMap::new(),
            count_ip: 0,
            count_prefix: 0,
            evicted: 0,
            log,
        }
    }

    /// Whether blocking is on (`ABUSE_BLOCK_REFUSALS_PER_MIN` > 0).
    pub fn enabled(&self) -> bool {
        self.threshold > 0.0
    }

    /// Running blocks (expired ones may linger until the next sweep).
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether no block is running.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Running blocks of a scope (the `scacelith_abuse_blocked` gauge).
    pub fn blocked_count(&self, scope: BlockScope) -> u64 {
        match scope {
            BlockScope::Ip => self.count_ip,
            BlockScope::Prefix => self.count_prefix,
        }
    }

    /// Blocks ended early because the cap was reached.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Adds the refusals of one interval and blocks the keys that went over. Returns the new
    /// blocks.
    pub fn report(&mut self, entries: &[RefusalEntry], now: f64) -> Vec<BlockOrder> {
        let mut fresh = Vec::new();
        if !self.enabled() {
            return fresh;
        }
        let t = self.threshold;
        for e in entries {
            // NaN and non-positive weights are ignored.
            if e.weight.is_nan() || e.weight <= 0.0 {
                continue;
            }
            let w = e.weight.min(MAX_WEIGHT);
            if self.is_blocked_at(e.k64, now) || e.k48.is_some_and(|k| self.is_blocked_at(k, now)) {
                continue;
            }
            // Limit T - 1: the take that brings the sum to T is the one refused, so T refusals block.
            let r = self.window.take(&e.k64, t - 1.0, WINDOW_MS, w, now);
            if !r.allowed {
                self.block(e.k64, BlockScope::Ip, e.k48, now, r.count + w, &mut fresh);
                if let Some(k48) = e.k48
                    && self.blocked_per_prefix.get(&k48).copied().unwrap_or(0) >= ABUSE_PREFIX_BLOCK_AFTER
                {
                    self.block(k48, BlockScope::Prefix, None, now, 0.0, &mut fresh);
                }
                continue;
            }
            if let Some(k48) = e.k48 {
                let limit = f64::from(ABUSE_PREFIX_FACTOR) * t - 1.0;
                let p = self.window.take(&k48, limit, WINDOW_MS, w, now);
                if !p.allowed {
                    self.block(k48, BlockScope::Prefix, None, now, p.count + w, &mut fresh);
                }
            }
        }
        fresh
    }

    /// The running blocks with the time each has left.
    pub fn snapshot(&self, now: f64) -> Vec<BlockOrder> {
        self.blocks
            .iter()
            .filter(|(_, b)| b.until > now)
            .map(|(k, b)| BlockOrder { key: *k, ttl_ms: (b.until - now).ceil(), level: b.level })
            .collect()
    }

    /// Whether `key` is blocked at `now`.
    pub fn is_blocked(&mut self, key: AddrKey, now: f64) -> bool {
        self.is_blocked_at(key, now)
    }

    /// Ends the expired blocks and forgets old history (every 10 s).
    pub fn sweep(&mut self, now: f64) {
        let expired: Vec<AddrKey> =
            self.blocks.iter().filter(|(_, b)| b.until <= now).map(|(k, _)| *k).collect();
        for key in expired {
            self.unblock(key);
        }
        self.history.retain(|_, h| now - h.at <= ABUSE_BLOCK_FORGET_MS);
        self.window.sweep(now);
    }

    fn is_blocked_at(&mut self, key: AddrKey, now: f64) -> bool {
        match self.blocks.get(&key) {
            None => false,
            Some(b) if b.until > now => true,
            Some(_) => {
                self.unblock(key);
                false
            }
        }
    }

    fn block(
        &mut self,
        key: AddrKey,
        scope: BlockScope,
        key48: Option<AddrKey>,
        now: f64,
        refusals: f64,
        fresh: &mut Vec<BlockOrder>,
    ) {
        if self.is_blocked_at(key, now) {
            return;
        }
        let level = match self.history.get(&key) {
            Some(h) if now - h.at <= ABUSE_BLOCK_FORGET_MS => h.level + 1,
            _ => 1,
        };
        let ttl_ms = self.max_ms.min(self.base_ms * ABUSE_BLOCK_FACTOR.powi(level.min(31) as i32 - 1));
        let m = tracker_metrics();
        while self.blocks.len() >= self.max_blocks {
            let Some((&old, _)) = self.blocks.front() else { break };
            self.unblock(old);
            self.evicted += 1;
            m.evicted.inc();
        }
        self.blocks.insert(key, Block { until: now + ttl_ms, level, scope, key48 });
        self.window.forget(&key);
        // Re-inserted: the newest history entry is the last.
        self.history.remove(&key);
        self.history.insert(key, History { level, at: now });
        if self.history.len() > 4 * self.max_blocks {
            self.history.pop_front();
        }
        match scope {
            BlockScope::Prefix => self.count_prefix += 1,
            BlockScope::Ip => self.count_ip += 1,
        }
        self.publish_gauge(scope);
        if let Some(k48) = key48 {
            *self.blocked_per_prefix.entry(k48).or_insert(0) += 1;
        }
        m.blocks_total.with(&[scope.as_str(), &level.min(4).to_string()]).inc();
        fresh.push(BlockOrder { key, ttl_ms, level });
        self.log.emit(Level::Warn, "ip blocked", Some(block_log_fields(key, scope, level, ttl_ms, refusals)));
    }

    fn unblock(&mut self, key: AddrKey) {
        let Some(b) = self.blocks.remove(&key) else { return };
        match b.scope {
            BlockScope::Prefix => self.count_prefix -= 1,
            BlockScope::Ip => self.count_ip -= 1,
        }
        self.publish_gauge(b.scope);
        if let Some(k48) = b.key48 {
            let n = self.blocked_per_prefix.get(&k48).copied().unwrap_or(0).saturating_sub(1);
            if n > 0 {
                self.blocked_per_prefix.insert(k48, n);
            } else {
                self.blocked_per_prefix.remove(&k48);
            }
        }
    }

    fn publish_gauge(&self, scope: BlockScope) {
        let n = self.blocked_count(scope);
        tracker_metrics().blocked.with(&[scope.as_str()]).set(n as f64);
    }
}

/// The fields of the `ip blocked` log line: `refusals` is left out when unknown (a /48 blocked
/// because of its blocked /64s).
fn block_log_fields(key: AddrKey, scope: BlockScope, level: u32, ttl_ms: f64, refusals: f64) -> Value {
    let mut f = Map::new();
    f.insert("ip".into(), json!(key.for_log()));
    f.insert("scope".into(), json!(scope.as_str()));
    f.insert("blockLevel".into(), json!(level));
    f.insert("ttlSec".into(), json!((ttl_ms / 1000.0).round() as i64));
    let refusals = refusals.round() as i64;
    if refusals != 0 {
        f.insert("refusals".into(), json!(refusals));
    }
    Value::Object(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_with(edit: impl FnOnce(&mut Config)) -> AbuseTracker {
        let mut c = Config::for_tests();
        c.abuse_block_refusals_per_min = 100;
        edit(&mut c);
        AbuseTracker::new(&c, Logger::root().child("abuse"))
    }

    fn key(s: &str) -> AddrKey {
        s.parse().expect("a key")
    }

    fn e(k64: &str, k48: Option<&str>, weight: f64) -> RefusalEntry {
        RefusalEntry { k64: key(k64), k48: k48.map(key), weight }
    }

    fn b(k: &str, ttl_ms: f64, level: u32) -> BlockOrder {
        BlockOrder::new(key(k), ttl_ms, level)
    }

    const T0: f64 = 1_000_000.0;

    #[test]
    fn sums_reports_and_blocks_at_the_threshold() {
        let mut t = tracker_with(|_| {});
        assert_eq!(t.report(&[e("198.51.100.1", None, 60.0)], T0), []);
        assert_eq!(t.report(&[e("198.51.100.1", None, 39.0)], T0), []);
        assert_eq!(
            t.report(&[e("198.51.100.1", None, 1.0), e("198.51.100.2", None, 5.0)], T0),
            [b("198.51.100.1", 60000.0, 1)]
        );
        assert!(t.is_blocked(key("198.51.100.1"), T0));
        assert!(!t.is_blocked(key("198.51.100.2"), T0));
        assert_eq!(t.blocked_count(BlockScope::Ip), 1);
    }

    #[test]
    fn the_log_line_names_the_truncated_address() {
        let f = block_log_fields(key("198.51.100.1"), BlockScope::Ip, 1, 60000.0, 100.0);
        assert_eq!(
            f,
            json!({"ip": "198.51.100.0/24", "scope": "ip", "blockLevel": 1, "ttlSec": 60, "refusals": 100})
        );
        let g = block_log_fields(key("2001:db8:2::/48"), BlockScope::Prefix, 2, 240000.0, 0.0);
        assert_eq!(g, json!({"ip": "2001:db8:2::/48", "scope": "prefix", "blockLevel": 2, "ttlSec": 240}));
    }

    #[test]
    fn counts_over_a_sliding_minute() {
        let mut t = tracker_with(|_| {});
        let mut now = T0;
        t.report(&[e("192.0.2.1", None, 90.0)], now);
        now += 120000.0;
        assert_eq!(t.report(&[e("192.0.2.1", None, 90.0)], now), [], "the old refusals left the window");
        now += 30000.0;
        assert_eq!(
            t.report(&[e("192.0.2.1", None, 60.0)], now).len(),
            1,
            "half of the previous minute counts"
        );
    }

    #[test]
    fn ladder_then_reset_after_six_hours() {
        let mut t = tracker_with(|_| {});
        let mut now = T0;
        let mut ttls = Vec::new();
        for _ in 0..5 {
            let fresh = t.report(&[e("192.0.2.9", None, 1000.0)], now);
            let BlockOrder { ttl_ms, level, .. } = fresh[0];
            ttls.push((ttl_ms / 1000.0, level));
            assert_eq!(
                t.report(&[e("192.0.2.9", None, 1000.0)], now),
                [],
                "reports while blocked are ignored"
            );
            now += ttl_ms;
        }
        assert_eq!(ttls, [(60.0, 1), (240.0, 2), (960.0, 3), (3600.0, 4), (3600.0, 5)]);
        now += ABUSE_BLOCK_FORGET_MS + 1.0;
        t.sweep(now);
        assert_eq!(t.report(&[e("192.0.2.9", None, 1000.0)], now), [b("192.0.2.9", 60000.0, 1)]);
    }

    #[test]
    fn after_a_block_the_key_counts_from_zero() {
        let mut t = tracker_with(|_| {});
        let mut now = 1_020_000.0 + 1.0;
        assert_eq!(t.report(&[e("192.0.2.7", Some("2001:db8::/48"), 99.0)], now), []);
        assert_eq!(
            t.report(&[e("192.0.2.7", Some("2001:db8::/48"), 1.0)], now),
            [b("192.0.2.7", 60000.0, 1)]
        );
        now += 60000.0;
        assert!(!t.is_blocked(key("192.0.2.7"), now));
        assert_eq!(t.report(&[e("192.0.2.7", None, 99.0)], now), [], "T - 1 refusals after the block");
        assert_eq!(t.report(&[e("192.0.2.7", None, 1.0)], now), [b("192.0.2.7", 240000.0, 2)]);
    }

    #[test]
    fn base_and_max_shape_the_ladder() {
        let mut t = tracker_with(|c| {
            c.abuse_block_base_sec = 10;
            c.abuse_block_max_sec = 100;
        });
        let mut now = T0;
        let mut ttls = Vec::new();
        for _ in 0..4 {
            let ttl = t.report(&[e("192.0.2.9", None, 1000.0)], now)[0].ttl_ms;
            ttls.push(ttl);
            now += ttl;
        }
        assert_eq!(ttls, [10000.0, 40000.0, 100000.0, 100000.0]);
    }

    #[test]
    fn blocks_a_48_by_its_sum_or_by_its_blocked_64s() {
        let mut t = tracker_with(|_| {});
        let mut fresh = Vec::new();
        for i in 0..50 {
            fresh.extend(t.report(&[e(&format!("2001:db8:1:{i:x}::/64"), Some("2001:db8:1::/48"), 9.0)], T0));
        }
        assert_eq!(fresh, [b("2001:db8:1::/48", 60000.0, 1)]);
        assert_eq!(t.blocked_count(BlockScope::Prefix), 1);
        assert_eq!(
            t.report(&[e("2001:db8:1:99::/64", Some("2001:db8:1::/48"), 1000.0)], T0),
            [],
            "a /64 of a blocked /48 is ignored"
        );

        let mut u = tracker_with(|_| {});
        let mut out = Vec::new();
        for i in 0..4 {
            out.extend(u.report(&[e(&format!("2001:db8:2:{i}::/64"), Some("2001:db8:2::/48"), 100.0)], T0));
        }
        let keys: Vec<String> = out.iter().map(|o| o.key.to_string()).collect();
        assert_eq!(
            keys,
            [
                "2001:db8:2:0::/64",
                "2001:db8:2:1::/64",
                "2001:db8:2:2::/64",
                "2001:db8:2:3::/64",
                "2001:db8:2::/48"
            ]
        );
    }

    #[test]
    fn snapshot_and_sweep() {
        let mut t = tracker_with(|_| {});
        let mut now = T0;
        t.report(&[e("192.0.2.1", None, 100.0)], now);
        now += 10000.0;
        t.report(&[e("192.0.2.2", None, 100.0)], now);
        assert_eq!(t.snapshot(now), [b("192.0.2.1", 50000.0, 1), b("192.0.2.2", 60000.0, 1)]);
        now += 50000.0;
        assert_eq!(t.snapshot(now), [b("192.0.2.2", 10000.0, 1)]);
        t.sweep(now);
        assert_eq!(t.len(), 1);
        assert_eq!(t.blocked_count(BlockScope::Ip), 1);
    }

    #[test]
    fn keeps_at_most_max_blocks() {
        let mut c = Config::for_tests();
        c.abuse_block_refusals_per_min = 100;
        let mut t = AbuseTracker::with_max_blocks(&c, Logger::root(), 3);
        for i in 1..=4 {
            t.report(&[e(&format!("192.0.2.{i}"), None, 100.0)], T0);
        }
        assert_eq!(t.len(), 3);
        assert!(!t.is_blocked(key("192.0.2.1"), T0) && t.is_blocked(key("192.0.2.4"), T0));
        assert_eq!(t.evicted(), 1);
    }

    #[test]
    fn threshold_zero_never_blocks_and_bad_weights_are_ignored() {
        let mut t = tracker_with(|c| c.abuse_block_refusals_per_min = 0);
        assert!(!t.enabled());
        assert_eq!(t.report(&[e("192.0.2.1", None, 1e9)], T0), []);
        let mut u = tracker_with(|_| {});
        assert_eq!(u.report(&[e("192.0.2.3", None, -5.0), e("192.0.2.3", None, f64::NAN)], T0), []);
    }
}
