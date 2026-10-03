//! Protection per address: the layer every request and every connection meets first, before
//! routing, before authentication and, with native TLS, before any TLS work (DESIGN 8,
//! docs/SIZING.md "Protection per address"). Quotas per signed-in account come later in the
//! pipeline; this layer only stops one network address from saturating the server, and is loose
//! enough for the players a school or a mobile operator (carrier-grade NAT) puts behind one
//! address.
//!
//! An address is an IPv4 address or an IPv6 /64 (`k64`); an IPv6 address also counts toward its
//! /48 (`k48`, one customer's site) with 4 times the limit, so that rotating over the /64s of a
//! /48 does not multiply it. One process enforces the whole-server limits exactly.
//!
//! * [`IpGuard::request`]: every HTTP request (health checks and WebSocket upgrades included):
//!   blocked? then one token of `HTTP_RATE_PER_IP` (k64) and `HTTP_RATE_PER_PREFIX` (k48), burst
//!   half a minute. A refusal by the /48 gives the /64 token back.
//! * [`IpGuard::enter_slot`]: requests in progress, `IP_MAX_INFLIGHT` per k64 (4x per k48).
//! * [`IpGuard::connection`]: a new TCP connection (the TLS gate): blocked? then `IP_CONN_RATE`
//!   new connections per second (burst 4 s, 4x per /48) and `IP_MAX_CONNECTIONS` open ones (4x per
//!   /48), counted open until the returned [`OpenConnection`] is dropped.
//! * [`IpGuard::note_refusal`]: a refusal counted toward a block (the per-address refusals above,
//!   the TLS gate's per-address refusals, malformed HTTP, the auth family with weight 5). Not
//!   counted: per-account limits, server-wide capacity refusals, requests refused because the
//!   address is already blocked.
//!
//! Blocks: the refusals of each key are summed per interval and handed to the
//! [`AbuseTracker`] once per [`REPORT_INTERVAL`] (at most [`REPORT_MAX_ENTRIES`] keys, the largest
//! first; at most [`REFUSAL_KEYS_PER_INTERVAL`] keys counted per interval), never per request.
//! Local fast path: a key that reaches `ABUSE_BLOCK_REFUSALS_PER_MIN` within one interval is
//! blocked at once for `ABUSE_BLOCK_BASE_SEC`, and still reported. A blocked address gets an RST
//! before TLS for its new connections, 429 with the time left (and `Connection: close` unless
//! behind a proxy) for its requests and upgrades. WebSockets already open are never closed by a
//! block.
//!
//! `ABUSE_EXEMPT` (addresses and subnets) skips all of this: no budget, no caps, never counted,
//! never blocked.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;

use super::abuse::{ABUSE_MAX_BLOCKS, ABUSE_PREFIX_FACTOR, AbuseTracker, BlockOrder, RefusalEntry};
use super::ip::{AddrKey, IpListError, IpMatcher, canonical};
use super::limits::TokenBucketLimiter;
use super::linked::LinkedMap;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::log::Logger;
use crate::metrics::gauge_fn;
use crate::metrics::{self, Counter, CounterVec};

/// Interval of the refusal reports.
pub const REPORT_INTERVAL: Duration = Duration::from_millis(1000);
/// Keys in one report (the largest weights).
pub const REPORT_MAX_ENTRIES: usize = 512;
/// Keys counted in one interval (new keys beyond it are dropped, counted).
pub const REFUSAL_KEYS_PER_INTERVAL: usize = 10_000;
/// Refusal weight of the login, registration, password reset and MFA family.
pub const AUTH_REFUSAL_WEIGHT: f64 = 5.0;
/// Request budget burst: this much of the per-minute rate.
const REQUEST_BURST_MS: f64 = 30_000.0;
/// New-connection burst: this much of the per-second rate.
const CONN_BURST_MS: f64 = 4_000.0;
/// Longest block applied.
const MAX_TTL_MS: f64 = 7.0 * 86_400_000.0;
/// Buckets kept by each limiter.
const LIMITER_KEYS: usize = 50_000;
/// How often the tracker forgets expired blocks and old history.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// The keys of a client address, with `ABUSE_EXEMPT` applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressKeys {
    /// The client address (canonical: IPv4-mapped IPv6 is IPv4).
    pub ip: IpAddr,
    /// The IPv4 address or IPv6 /64.
    pub k64: AddrKey,
    /// The IPv6 /48 (`None` for IPv4).
    pub k48: Option<AddrKey>,
    /// Whether the address is in `ABUSE_EXEMPT`.
    pub exempt: bool,
}

impl AddressKeys {
    /// The keys of `ip`, not exempt.
    pub fn of(ip: IpAddr) -> AddressKeys {
        let ip = canonical(ip);
        AddressKeys { ip, k64: AddrKey::of(ip), k48: AddrKey::prefix48_of(ip), exempt: false }
    }

    /// The address group of the TLS gate: IPv4 itself or the IPv6 /48.
    pub fn site(&self) -> AddrKey {
        self.k48.unwrap_or(self.k64)
    }
}

/// Why a request was refused by the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRefusalReason {
    /// The address (or its /48) is blocked.
    Blocked,
    /// The address's budget (`HTTP_RATE_PER_IP`) is spent.
    Ip,
    /// The /48's budget (`HTTP_RATE_PER_PREFIX`) is spent.
    Ip48,
}

/// A request refused by [`IpGuard::request`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestRefusal {
    /// Why.
    pub reason: RequestRefusalReason,
    /// When to try again, in milliseconds.
    pub retry_after_ms: f64,
}

/// Why a new connection was refused by [`IpGuard::connection`] (closed with an RST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnRefusal {
    /// The address (or its /48) is blocked.
    Blocked,
    /// More than `IP_CONN_RATE` new connections per second.
    ConnRate,
    /// More than `IP_MAX_CONNECTIONS` open connections.
    ConnOpen,
}

impl ConnRefusal {
    /// The label of `scacelith_tls_refused_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnRefusal::Blocked => "blocked",
            ConnRefusal::ConnRate => "conn_rate",
            ConnRefusal::ConnOpen => "conn_open",
        }
    }
}

/// Counts of what the guard refused, since it started (mirrors the metrics, per guard).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuardStats {
    /// Requests refused by the address budget.
    pub rate_limited_ip: u64,
    /// Requests refused by the /48 budget.
    pub rate_limited_ip48: u64,
    /// Requests refused by the in-progress cap.
    pub rate_limited_inflight: u64,
    /// Requests refused because the address is blocked.
    pub rate_limited_blocked: u64,
    /// Blocks set by the local fast path.
    pub local_blocks: u64,
    /// Refusal keys left out of a report.
    pub report_entries_dropped: u64,
}

/// Receives each report of refusals instead of the guard's own tracker (tests).
pub type ReportSink = Box<dyn Fn(&[RefusalEntry]) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
struct GuardBlock {
    until: f64,
    level: u32,
}

#[derive(Debug, Clone, Copy)]
struct PendingRefusals {
    k48: Option<AddrKey>,
    weight: f64,
    local: bool,
}

struct GuardState {
    reqs: TokenBucketLimiter<AddrKey>,
    reqs48: TokenBucketLimiter<AddrKey>,
    conns: TokenBucketLimiter<AddrKey>,
    conns48: TokenBucketLimiter<AddrKey>,
    open: HashMap<AddrKey, u32>,
    open48: HashMap<AddrKey, u32>,
    open_total: u64,
    inflight: HashMap<AddrKey, u32>,
    inflight48: HashMap<AddrKey, u32>,
    inflight_total: u64,
    blocks: LinkedMap<AddrKey, GuardBlock>,
    refusals: LinkedMap<AddrKey, PendingRefusals>,
    report_armed: bool,
    stats: GuardStats,
}

/// The budgets of the guard, from the configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardLimits {
    /// Request burst per address.
    pub req_burst: f64,
    /// Window over which the request burst refills.
    pub req_window: f64,
    /// Request burst per /48.
    pub req_burst48: f64,
    /// Window over which the /48 burst refills.
    pub req_window48: f64,
    /// New-connection burst per address (over 4 s).
    pub conn_burst: f64,
    /// New-connection burst per /48.
    pub conn_burst48: f64,
    /// Open connections per address.
    pub max_open: u32,
    /// Open connections per /48.
    pub max_open48: u32,
    /// Requests in progress per address.
    pub max_inflight: u32,
    /// Requests in progress per /48.
    pub max_inflight48: u32,
    /// Refusals per interval that block at once (0: blocking off).
    pub threshold: f64,
    /// Duration of a first block, in ms.
    pub base_ms: f64,
}

impl GuardLimits {
    /// The budgets of `config`.
    pub fn from_config(config: &Config) -> GuardLimits {
        let rate = config.http_rate_per_ip.max(1) as f64;
        let rate48 =
            if config.http_rate_per_prefix > 0 { config.http_rate_per_prefix as f64 } else { 4.0 * rate };
        let req_burst = (rate * REQUEST_BURST_MS / 60_000.0).round().max(1.0);
        let req_burst48 = (rate48 * REQUEST_BURST_MS / 60_000.0).round().max(1.0);
        let conn = config.ip_conn_rate.max(1) as f64;
        let conn_burst = (conn * CONN_BURST_MS / 1000.0).round().max(1.0);
        let max_open = config.ip_max_connections.max(1) as u32;
        let max_inflight = config.ip_max_inflight.max(1) as u32;
        GuardLimits {
            req_burst,
            req_window: req_burst * 60_000.0 / rate,
            req_burst48,
            req_window48: req_burst48 * 60_000.0 / rate48,
            conn_burst,
            conn_burst48: f64::from(ABUSE_PREFIX_FACTOR) * conn_burst,
            max_open,
            max_open48: ABUSE_PREFIX_FACTOR * max_open,
            max_inflight,
            max_inflight48: ABUSE_PREFIX_FACTOR * max_inflight,
            threshold: config.abuse_block_refusals_per_min.max(0) as f64,
            base_ms: 1000.0 * config.abuse_block_base_sec.max(1) as f64,
        }
    }
}

struct GuardMetrics {
    rl_ip: Counter,
    rl_ip48: Counter,
    rl_inflight: Counter,
    rl_blocked: Counter,
    local_blocks: Counter,
    dropped: Counter,
}

/// `scacelith_http_rate_limited_total{limit}`: API requests refused by a rate limit (the guard's
/// `ip`, `ip48`, `inflight`, `blocked`, and the API's `user` and route keys).
pub fn http_rate_limited() -> &'static CounterVec {
    static C: LazyLock<CounterVec> = LazyLock::new(|| {
        metrics::counter_vec(
            "scacelith_http_rate_limited_total",
            "API requests refused by a rate limit",
            &["limit"],
        )
    });
    &C
}

fn guard_metrics() -> &'static GuardMetrics {
    static M: LazyLock<GuardMetrics> = LazyLock::new(|| {
        let rl = http_rate_limited();
        GuardMetrics {
            rl_ip: rl.with(&["ip"]),
            rl_ip48: rl.with(&["ip48"]),
            rl_inflight: rl.with(&["inflight"]),
            rl_blocked: rl.with(&["blocked"]),
            local_blocks: metrics::counter(
                "scacelith_abuse_local_blocks_total",
                "Addresses blocked at once by the local fast path (a flood within one report interval)",
            ),
            dropped: metrics::counter(
                "scacelith_abuse_report_entries_dropped_total",
                "Refusal keys left out of a report to the abuse tracker",
            ),
        }
    });
    &M
}

/// The protection per address (see the module documentation).
pub struct IpGuard {
    clock: SharedClock,
    exempt: IpMatcher,
    limits: GuardLimits,
    state: Mutex<GuardState>,
    tracker: Mutex<AbuseTracker>,
    sink: Option<ReportSink>,
    report_interval: Duration,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for IpGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpGuard").field("limits", &self.limits).finish()
    }
}

fn dec(map: &mut HashMap<AddrKey, u32>, key: &AddrKey) {
    if let Some(n) = map.get_mut(key) {
        if *n <= 1 {
            map.remove(key);
        } else {
            *n -= 1;
        }
    }
}

impl IpGuard {
    /// A guard with the budgets of `config`, on `clock`. Fails when `ABUSE_EXEMPT` has an invalid
    /// entry.
    pub fn new(config: &Config, clock: SharedClock, log: Logger) -> Result<IpGuard, IpListError> {
        let exempt = IpMatcher::new(&config.abuse_exempt)?;
        Ok(IpGuard {
            clock,
            exempt,
            limits: GuardLimits::from_config(config),
            state: Mutex::new(GuardState {
                reqs: TokenBucketLimiter::new(LIMITER_KEYS),
                reqs48: TokenBucketLimiter::new(LIMITER_KEYS),
                conns: TokenBucketLimiter::new(LIMITER_KEYS),
                conns48: TokenBucketLimiter::new(LIMITER_KEYS),
                open: HashMap::new(),
                open48: HashMap::new(),
                open_total: 0,
                inflight: HashMap::new(),
                inflight48: HashMap::new(),
                inflight_total: 0,
                blocks: LinkedMap::new(),
                refusals: LinkedMap::new(),
                report_armed: false,
                stats: GuardStats::default(),
            }),
            tracker: Mutex::new(AbuseTracker::new(config, log)),
            sink: None,
            report_interval: REPORT_INTERVAL,
            notify: Arc::new(Notify::new()),
        })
    }

    /// Sends the reports to `sink` instead of the guard's own tracker (tests).
    pub fn with_report_sink(mut self, sink: ReportSink) -> IpGuard {
        self.sink = Some(sink);
        self
    }

    /// Changes the report interval (tests shorten it).
    pub fn with_report_interval(mut self, interval: Duration) -> IpGuard {
        self.report_interval = interval;
        self
    }

    /// The budgets in force.
    pub fn limits(&self) -> &GuardLimits {
        &self.limits
    }

    /// The keys of an address, with `ABUSE_EXEMPT` applied.
    pub fn keys(&self, ip: IpAddr) -> AddressKeys {
        let mut k = AddressKeys::of(ip);
        k.exempt = !self.exempt.is_empty() && self.exempt.matches(k.ip);
        k
    }

    /// Whether an address is in `ABUSE_EXEMPT`.
    pub fn is_exempt(&self, ip: IpAddr) -> bool {
        self.keys(ip).exempt
    }

    /// One request: the block, then the budget of its address.
    pub fn request(&self, k: &AddressKeys) -> Result<(), RequestRefusal> {
        if k.exempt {
            return Ok(());
        }
        let now = self.clock.mono_ms();
        let m = guard_metrics();
        let l = &self.limits;
        let mut st = self.state.lock();
        if !st.blocks.is_empty() {
            let left = blocked_for(&mut st, k, now);
            if left > 0.0 {
                st.stats.rate_limited_blocked += 1;
                m.rl_blocked.inc();
                return Err(RequestRefusal { reason: RequestRefusalReason::Blocked, retry_after_ms: left });
            }
        }
        let a = st.reqs.take(&k.k64, l.req_burst, l.req_window, 1.0, now);
        if !a.allowed {
            st.stats.rate_limited_ip += 1;
            m.rl_ip.inc();
            self.note_refusal_locked(&mut st, k, 1.0, now);
            return Err(RequestRefusal {
                reason: RequestRefusalReason::Ip,
                retry_after_ms: a.retry_after_ms,
            });
        }
        if let Some(k48) = k.k48 {
            let b = st.reqs48.take(&k48, l.req_burst48, l.req_window48, 1.0, now);
            if !b.allowed {
                st.reqs.give(&k.k64, l.req_burst, l.req_window, 1.0, now);
                st.stats.rate_limited_ip48 += 1;
                m.rl_ip48.inc();
                self.note_refusal_locked(&mut st, k, 1.0, now);
                return Err(RequestRefusal {
                    reason: RequestRefusalReason::Ip48,
                    retry_after_ms: b.retry_after_ms,
                });
            }
        }
        Ok(())
    }

    /// Counts a request in progress; false (counted as a refusal) when its address already has
    /// `IP_MAX_INFLIGHT` of them (4x for its /48). Exempt addresses are not counted. Every true
    /// must be followed by exactly one [`leave`](Self::leave); prefer [`enter_slot`](Self::enter_slot).
    pub fn enter(&self, k: &AddressKeys) -> bool {
        if k.exempt {
            return true;
        }
        let l = &self.limits;
        let mut st = self.state.lock();
        let n = st.inflight.get(&k.k64).copied().unwrap_or(0);
        let refused = n >= l.max_inflight
            || k.k48.is_some_and(|k48| st.inflight48.get(&k48).copied().unwrap_or(0) >= l.max_inflight48);
        if refused {
            st.stats.rate_limited_inflight += 1;
            guard_metrics().rl_inflight.inc();
            let now = self.clock.mono_ms();
            self.note_refusal_locked(&mut st, k, 1.0, now);
            return false;
        }
        if let Some(k48) = k.k48 {
            *st.inflight48.entry(k48).or_insert(0) += 1;
        }
        *st.inflight.entry(k.k64).or_insert(0) += 1;
        st.inflight_total += 1;
        true
    }

    /// Ends a request counted by [`enter`](Self::enter).
    pub fn leave(&self, k: &AddressKeys) {
        if k.exempt {
            return;
        }
        let mut st = self.state.lock();
        dec(&mut st.inflight, &k.k64);
        if let Some(k48) = k.k48 {
            dec(&mut st.inflight48, &k48);
        }
        st.inflight_total = st.inflight_total.saturating_sub(1);
    }

    /// [`enter`](Self::enter) with a slot that calls [`leave`](Self::leave) when dropped.
    pub fn enter_slot(self: &Arc<Self>, k: &AddressKeys) -> Option<InflightSlot> {
        if !self.enter(k) {
            return None;
        }
        Some(InflightSlot { guard: if k.exempt { None } else { Some((self.clone(), *k)) } })
    }

    /// Admission of a new TCP connection (the TLS gate, before anything else). Returns the open
    /// connection (counted until dropped), or the reason to close it with an RST.
    pub fn connection(self: &Arc<Self>, ip: IpAddr) -> Result<OpenConnection, ConnRefusal> {
        let k = self.keys(ip);
        if k.exempt {
            return Ok(OpenConnection { keys: k, guard: None });
        }
        let now = self.clock.mono_ms();
        let l = &self.limits;
        let mut st = self.state.lock();
        if !st.blocks.is_empty() && blocked_for(&mut st, &k, now) > 0.0 {
            return Err(ConnRefusal::Blocked);
        }
        if !st.conns.take(&k.k64, l.conn_burst, CONN_BURST_MS, 1.0, now).allowed {
            self.note_refusal_locked(&mut st, &k, 1.0, now);
            return Err(ConnRefusal::ConnRate);
        }
        if let Some(k48) = k.k48
            && !st.conns48.take(&k48, l.conn_burst48, CONN_BURST_MS, 1.0, now).allowed
        {
            st.conns.give(&k.k64, l.conn_burst, CONN_BURST_MS, 1.0, now);
            self.note_refusal_locked(&mut st, &k, 1.0, now);
            return Err(ConnRefusal::ConnRate);
        }
        let n = st.open.get(&k.k64).copied().unwrap_or(0);
        let full = n >= l.max_open
            || k.k48.is_some_and(|k48| st.open48.get(&k48).copied().unwrap_or(0) >= l.max_open48);
        if full {
            self.note_refusal_locked(&mut st, &k, 1.0, now);
            return Err(ConnRefusal::ConnOpen);
        }
        if let Some(k48) = k.k48 {
            *st.open48.entry(k48).or_insert(0) += 1;
        }
        *st.open.entry(k.k64).or_insert(0) += 1;
        st.open_total += 1;
        Ok(OpenConnection { keys: k, guard: Some(self.clone()) })
    }

    fn connection_closed(&self, k: &AddressKeys) {
        let mut st = self.state.lock();
        dec(&mut st.open, &k.k64);
        if let Some(k48) = k.k48 {
            dec(&mut st.open48, &k48);
        }
        st.open_total = st.open_total.saturating_sub(1);
    }

    /// Counts a refusal of an address toward a block (`weight` 1, or [`AUTH_REFUSAL_WEIGHT`]).
    /// The report leaves on a timer, at most once per interval.
    pub fn note_refusal(&self, k: &AddressKeys, weight: f64) {
        if self.limits.threshold <= 0.0 || k.exempt {
            return;
        }
        let now = self.clock.mono_ms();
        let mut st = self.state.lock();
        self.note_refusal_locked(&mut st, k, weight, now);
    }

    fn note_refusal_locked(&self, st: &mut GuardState, k: &AddressKeys, weight: f64, now: f64) {
        if self.limits.threshold <= 0.0 || k.exempt || weight.is_nan() || weight <= 0.0 {
            return;
        }
        if !st.blocks.is_empty() && blocked_for(st, k, now) > 0.0 {
            return;
        }
        if !st.refusals.contains_key(&k.k64) {
            if st.refusals.len() >= REFUSAL_KEYS_PER_INTERVAL {
                st.stats.report_entries_dropped += 1;
                guard_metrics().dropped.inc();
                return;
            }
            st.refusals.insert(k.k64, PendingRefusals { k48: k.k48, weight: 0.0, local: false });
        }
        let e = st.refusals.get_mut(&k.k64).expect("present: just ensured");
        e.weight += weight;
        if !e.local && e.weight >= self.limits.threshold {
            e.local = true;
            st.stats.local_blocks += 1;
            guard_metrics().local_blocks.inc();
            apply_blocks_locked(st, &[BlockOrder::new(k.k64, self.limits.base_ms, 1)], now);
        }
        if !st.report_armed {
            st.report_armed = true;
            self.notify.notify_one();
        }
    }

    /// Hands the refusals of the interval (largest first, at most [`REPORT_MAX_ENTRIES`]) to the
    /// tracker (or the sink) and applies the blocks it decides. Called by the reporter task; tests
    /// call it directly. Returns what was reported.
    pub fn flush_reports(&self) -> Vec<RefusalEntry> {
        let mut entries: Vec<RefusalEntry> = {
            let mut st = self.state.lock();
            st.report_armed = false;
            if st.refusals.is_empty() {
                return Vec::new();
            }
            let out = st
                .refusals
                .iter()
                .map(|(k, e)| RefusalEntry { k64: *k, k48: e.k48, weight: e.weight })
                .collect();
            st.refusals.clear();
            out
        };
        entries.sort_by(|a, b| b.weight.total_cmp(&a.weight));
        if entries.len() > REPORT_MAX_ENTRIES {
            let dropped = (entries.len() - REPORT_MAX_ENTRIES) as u64;
            self.state.lock().stats.report_entries_dropped += dropped;
            guard_metrics().dropped.add(dropped);
            entries.truncate(REPORT_MAX_ENTRIES);
        }
        match &self.sink {
            Some(sink) => sink(&entries),
            None => {
                let now = self.clock.mono_ms();
                let fresh = self.tracker.lock().report(&entries, now);
                if !fresh.is_empty() {
                    self.apply_blocks(&fresh);
                }
            }
        }
        entries
    }

    /// Applies blocks: a key already blocked for longer keeps its deadline. Returns the number of
    /// blocks set or extended.
    pub fn apply_blocks(&self, blocks: &[BlockOrder]) -> usize {
        let now = self.clock.mono_ms();
        apply_blocks_locked(&mut self.state.lock(), blocks, now)
    }

    /// Milliseconds left in the block of an address (its k64 or its k48), 0 when not blocked.
    pub fn blocked_for(&self, k: &AddressKeys) -> f64 {
        if k.exempt {
            return 0.0;
        }
        let now = self.clock.mono_ms();
        let mut st = self.state.lock();
        if st.blocks.is_empty() { 0.0 } else { blocked_for(&mut st, k, now) }
    }

    /// Running blocks (expired ones are dropped).
    pub fn blocked_count(&self) -> usize {
        let now = self.clock.mono_ms();
        let mut st = self.state.lock();
        st.blocks.retain(|_, b| b.until > now);
        st.blocks.len()
    }

    /// Connections counted open.
    pub fn open_total(&self) -> u64 {
        self.state.lock().open_total
    }

    /// Requests counted in progress.
    pub fn inflight_total(&self) -> u64 {
        self.state.lock().inflight_total
    }

    /// The refusal counts of this guard.
    pub fn stats(&self) -> GuardStats {
        self.state.lock().stats
    }

    /// The tokens left in the request bucket of a key (tests).
    pub fn request_tokens(&self, k: AddrKey) -> Option<f64> {
        self.state.lock().reqs.peek(&k).map(|b| b.tokens)
    }

    /// Running blocks of the guard's own tracker.
    pub fn tracker_len(&self) -> usize {
        self.tracker.lock().len()
    }

    /// Ends expired blocks and forgets old history in the tracker.
    pub fn sweep(&self) {
        let now = self.clock.mono_ms();
        self.tracker.lock().sweep(now);
        self.state.lock().blocks.retain(|_, b| b.until > now);
    }

    /// Drops the refusals not reported yet.
    pub fn close(&self) {
        let mut st = self.state.lock();
        st.refusals.clear();
        st.report_armed = false;
    }

    /// Starts the task that sends the reports (one interval after the first refusal) and sweeps
    /// the tracker. It ends when the guard is dropped.
    pub fn spawn_reporter(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak: Weak<IpGuard> = Arc::downgrade(self);
        let notify = self.notify.clone();
        let interval = self.report_interval;
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = notify.notified() => {
                        tokio::time::sleep(interval).await;
                        let Some(guard) = weak.upgrade() else { return };
                        guard.flush_reports();
                    }
                    _ = sweep.tick() => {
                        let Some(guard) = weak.upgrade() else { return };
                        guard.sweep();
                    }
                }
            }
        })
    }

    /// Registers the gauges read from this guard (`scacelith_tls_connections_open`,
    /// `scacelith_http_inflight`, `scacelith_abuse_blocked_keys`). The first guard registered
    /// keeps them: call it once, for the server's guard.
    pub fn register_gauges(self: &Arc<Self>) {
        let w = Arc::downgrade(self);
        gauge_fn(
            "scacelith_tls_connections_open",
            "Connections admitted by the TLS gate and still open (TLS_MODE=native)",
            move || w.upgrade().map_or(0.0, |g| g.open_total() as f64),
        );
        let w = Arc::downgrade(self);
        gauge_fn(
            "scacelith_http_inflight",
            "HTTP requests in progress (exempt addresses not counted)",
            move || w.upgrade().map_or(0.0, |g| g.inflight_total() as f64),
        );
        let w = Arc::downgrade(self);
        gauge_fn("scacelith_abuse_blocked_keys", "Blocked addresses the server knows", move || {
            w.upgrade().map_or(0.0, |g| g.blocked_count() as f64)
        });
    }
}

fn blocked_for(st: &mut GuardState, k: &AddressKeys, now: f64) -> f64 {
    for key in std::iter::once(k.k64).chain(k.k48) {
        if let Some(b) = st.blocks.get(&key) {
            if b.until > now {
                return b.until - now;
            }
            st.blocks.remove(&key);
        }
    }
    0.0
}

fn apply_blocks_locked(st: &mut GuardState, blocks: &[BlockOrder], now: f64) -> usize {
    let mut n = 0;
    for b in blocks {
        if b.ttl_ms.is_nan() || b.ttl_ms <= 0.0 {
            continue;
        }
        let level = b.level.max(1);
        let until = now + b.ttl_ms.min(MAX_TTL_MS);
        if let Some(cur) = st.blocks.get_mut(&b.key) {
            cur.level = cur.level.max(level);
            if cur.until >= until {
                continue;
            }
            cur.until = until;
            n += 1;
            continue;
        }
        while st.blocks.len() >= ABUSE_MAX_BLOCKS {
            st.blocks.pop_front();
        }
        st.blocks.insert(b.key, GuardBlock { until, level });
        n += 1;
    }
    n
}

/// A connection counted open by [`IpGuard::connection`]; the count is given back when dropped.
pub struct OpenConnection {
    keys: AddressKeys,
    guard: Option<Arc<IpGuard>>,
}

impl OpenConnection {
    /// The keys of the connection's address.
    pub fn keys(&self) -> &AddressKeys {
        &self.keys
    }

    /// An uncounted connection (no guard: plain modes, tests).
    pub fn detached(ip: IpAddr) -> OpenConnection {
        OpenConnection { keys: AddressKeys::of(ip), guard: None }
    }
}

impl std::fmt::Debug for OpenConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenConnection")
            .field("keys", &self.keys)
            .field("counted", &self.guard.is_some())
            .finish()
    }
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        if let Some(g) = self.guard.take() {
            g.connection_closed(&self.keys);
        }
    }
}

/// A request counted in progress by [`IpGuard::enter_slot`]; given back when dropped.
pub struct InflightSlot {
    guard: Option<(Arc<IpGuard>, AddressKeys)>,
}

impl std::fmt::Debug for InflightSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InflightSlot").field("counted", &self.guard.is_some()).finish()
    }
}

impl Drop for InflightSlot {
    fn drop(&mut self) {
        if let Some((g, k)) = self.guard.take() {
            g.leave(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, ManualClock};
    use crate::net::ip::normalize_ip;

    struct Fixture {
        guard: Arc<IpGuard>,
        clock: Arc<ManualClock>,
        reports: Arc<Mutex<Vec<Vec<RefusalEntry>>>>,
    }

    fn ip(s: &str) -> IpAddr {
        normalize_ip(s).expect("an address")
    }

    fn key(s: &str) -> AddrKey {
        s.parse().expect("a key")
    }

    fn fixture(edit: impl FnOnce(&mut Config), local: bool) -> Fixture {
        let mut c = Config::for_tests();
        c.http_rate_per_prefix = 0;
        edit(&mut c);
        let clock = ManualClock::new(0.0, 0);
        let reports = Arc::new(Mutex::new(Vec::new()));
        let mut guard =
            IpGuard::new(&c, clock.clone() as Arc<dyn Clock>, Logger::root()).expect("valid config");
        if !local {
            let r = reports.clone();
            guard = guard.with_report_sink(Box::new(move |e: &[RefusalEntry]| r.lock().push(e.to_vec())));
        }
        Fixture { guard: Arc::new(guard), clock, reports }
    }

    fn req(g: &IpGuard, a: &str) -> Option<RequestRefusal> {
        g.request(&g.keys(ip(a))).err()
    }

    #[test]
    fn keys_follow_net_ip() {
        let g = fixture(|_| {}, false).guard;
        assert_eq!(g.keys(ip("::ffff:192.0.2.7")).k64, key("192.0.2.7"));
        assert_eq!(g.keys(ip("192.0.2.7")).k48, None);
        let a = g.keys(ip("2001:DB8:1:2::5"));
        let b = g.keys(ip("2001:0db8:0001:0002:aaaa:0:0:9"));
        assert_eq!(a.k64, b.k64);
        assert_eq!(a.k64.to_string(), "2001:db8:1:2::/64");
        assert_eq!(a.k48, Some(key("2001:db8:1::/48")));
        assert_eq!(g.keys(ip("2001:db8:1:3::5")).k48, a.k48);
        assert_ne!(g.keys(ip("2001:db8:1:3::5")).k64, a.k64);
        let d = g.keys(ip("2001:db8:1:2:3:4:5.6.7.8"));
        assert_eq!(
            (d.k64.to_string(), d.k48.map(|k| k.to_string())),
            ("2001:db8:1:2::/64".into(), Some("2001:db8:1::/48".into()))
        );
    }

    #[test]
    fn request_budget_burst_then_rate() {
        let f = fixture(|c| c.http_rate_per_ip = 60, false);
        let k = f.guard.keys(ip("198.51.100.1"));
        for _ in 0..30 {
            assert_eq!(f.guard.request(&k), Ok(()));
        }
        assert_eq!(
            f.guard.request(&k),
            Err(RequestRefusal { reason: RequestRefusalReason::Ip, retry_after_ms: 1000.0 })
        );
        assert_eq!(req(&f.guard, "198.51.100.2"), None, "another address");
        f.clock.advance(1000.0);
        assert_eq!(f.guard.request(&k), Ok(()));
        assert_eq!(f.guard.request(&k).unwrap_err().reason, RequestRefusalReason::Ip);
        assert_eq!(f.guard.stats().rate_limited_ip, 2);
    }

    #[test]
    fn whole_server_limits_are_exact() {
        let g = fixture(|c| c.http_rate_per_ip = 600, false).guard;
        assert_eq!(g.limits().req_burst, 300.0);
        assert_eq!(g.limits().req_window, 30000.0);
        assert_eq!(
            fixture(|c| c.http_rate_per_ip = 60, false).guard.limits().req_burst48,
            120.0,
            "0 means 4x"
        );
    }

    #[test]
    fn a_rotating_48_meets_its_budget_and_the_64_token_is_given_back() {
        let f = fixture(
            |c| {
                c.http_rate_per_ip = 60;
                c.http_rate_per_prefix = 120;
            },
            false,
        );
        let (mut refused, mut first) = (0, None);
        for i in 0..100 {
            if let Some(r) = req(&f.guard, &format!("2001:db8:7:{i:x}::1")) {
                assert_eq!(r.reason, RequestRefusalReason::Ip48);
                refused += 1;
                first.get_or_insert(i);
            }
        }
        assert_eq!(first, Some(60));
        assert_eq!(refused, 40);
        assert_eq!(
            f.guard.request_tokens(key("2001:db8:7:50::/64")),
            Some(30.0),
            "the refused /64 keeps its burst"
        );
        assert_eq!(req(&f.guard, "2001:db8:8::1"), None, "another /48");
        assert_eq!(f.guard.stats().rate_limited_ip48, 40);
    }

    #[test]
    fn requests_in_progress_per_64_and_per_48() {
        let f = fixture(|c| c.ip_max_inflight = 2, false);
        let g = &f.guard;
        let a = g.keys(ip("2001:db8:1:1::1"));
        assert!(g.enter(&a) && g.enter(&a));
        assert!(!g.enter(&a));
        assert_eq!(g.stats().rate_limited_inflight, 1);
        g.leave(&a);
        assert!(g.enter(&a));
        let mut n = 2;
        let mut slots = Vec::new();
        for i in 2..10 {
            if let Some(s) = g.enter_slot(&g.keys(ip(&format!("2001:db8:1:{i}::1")))) {
                slots.push(s);
                n += 1;
            }
        }
        assert_eq!(n, 8);
        assert_eq!(g.inflight_total(), 8);
        drop(slots);
        assert_eq!(g.inflight_total(), 2, "slots give their place back when dropped");
    }

    #[test]
    fn connections_rate_and_open_cap() {
        let f = fixture(
            |c| {
                c.ip_conn_rate = 2;
                c.ip_max_connections = 5;
            },
            false,
        );
        let g = &f.guard;
        let mut open: Vec<OpenConnection> =
            (0..5).map(|_| g.connection(ip("192.0.2.1")).expect("admitted")).collect();
        assert_eq!(g.connection(ip("192.0.2.1")).unwrap_err(), ConnRefusal::ConnOpen);
        assert_eq!(g.open_total(), 5);
        drop(open.remove(0));
        assert_eq!(g.open_total(), 4);
        open.push(g.connection(ip("192.0.2.1")).expect("a freed place"));
        assert_eq!(g.connection(ip("192.0.2.1")).unwrap_err(), ConnRefusal::ConnOpen);
        // 8 tokens in the burst: 5 + 1 + 1 refused (conn_open spends one too) = 8.
        open.clear();
        assert_eq!(g.connection(ip("192.0.2.1")).unwrap_err(), ConnRefusal::ConnRate);
        f.clock.advance(500.0);
        let _a = g.connection(ip("192.0.2.1")).expect("one token per 500 ms");
        let _b = g.connection(ip("192.0.2.2")).expect("another address");
    }

    #[test]
    fn the_48_has_four_times_the_caps() {
        let f = fixture(
            |c| {
                c.ip_conn_rate = 100;
                c.ip_max_connections = 2;
            },
            false,
        );
        let kept: Vec<_> =
            (0..12).filter_map(|i| f.guard.connection(ip(&format!("2001:db8:5:{i}::1"))).ok()).collect();
        assert_eq!(kept.len(), 8);
    }

    #[test]
    fn exempt_addresses_are_left_alone() {
        let f = fixture(
            |c| {
                c.http_rate_per_ip = 2;
                c.ip_max_inflight = 1;
                c.ip_conn_rate = 1;
                c.ip_max_connections = 1;
                c.abuse_exempt = vec!["203.0.113.0/24".into(), "2001:db8:aa::/48".into()];
            },
            false,
        );
        let g = &f.guard;
        let k = g.keys(ip("203.0.113.5"));
        assert!(k.exempt);
        assert!(g.is_exempt(ip("::ffff:203.0.113.200")));
        for _ in 0..100 {
            assert_eq!(g.request(&k), Ok(()));
        }
        for _ in 0..10 {
            assert!(g.enter(&k));
        }
        assert_eq!(g.inflight_total(), 0, "not counted");
        let conns: Vec<_> = (0..10).map(|_| g.connection(ip("203.0.113.5")).expect("exempt")).collect();
        assert_eq!(g.open_total(), 0);
        drop(conns);
        g.apply_blocks(&[
            BlockOrder::new(key("203.0.113.5"), 60000.0, 1),
            BlockOrder::new(key("2001:db8:aa::/48"), 60000.0, 1),
        ]);
        assert_eq!(g.request(&k), Ok(()), "a block never applies to an exempt address");
        assert_eq!(req(g, "2001:db8:aa:1::1"), None);
        g.note_refusal(&k, 1000.0);
        assert_eq!(g.flush_reports(), [], "never reported");
        assert!(f.reports.lock().is_empty());
        assert_eq!(req(g, "198.51.100.1"), None, "2 per minute: a burst of 1");
        assert_eq!(req(g, "198.51.100.1").map(|r| r.reason), Some(RequestRefusalReason::Ip));
    }

    #[test]
    fn blocks_apply_cover_and_expire() {
        let f = fixture(|_| {}, false);
        let g = &f.guard;
        let n = g.apply_blocks(&[
            BlockOrder::new(key("198.51.100.9"), 60000.0, 1),
            BlockOrder::new(key("2001:db8:1::/48"), 240000.0, 2),
            BlockOrder::new(key("198.51.100.20"), -1.0, 1),
        ]);
        assert_eq!(n, 2);
        assert_eq!(
            req(g, "198.51.100.9"),
            Some(RequestRefusal { reason: RequestRefusalReason::Blocked, retry_after_ms: 60000.0 })
        );
        assert_eq!(req(g, "2001:db8:1:77::1").map(|r| r.reason), Some(RequestRefusalReason::Blocked));
        assert_eq!(g.connection(ip("2001:db8:1:1::1")).unwrap_err(), ConnRefusal::Blocked);
        assert_eq!(g.connection(ip("198.51.100.9")).unwrap_err(), ConnRefusal::Blocked);
        assert_eq!(g.open_total(), 0, "a refused connection is not counted open");
        assert_eq!(req(g, "198.51.100.10"), None);
        assert_eq!(g.blocked_count(), 2);
        assert_eq!(g.stats().rate_limited_blocked, 2);
        g.apply_blocks(&[BlockOrder::new(key("2001:db8:1::/48"), 1000.0, 1)]);
        f.clock.advance(60000.0);
        assert_eq!(req(g, "198.51.100.9"), None, "expired");
        assert_eq!(g.blocked_for(&g.keys(ip("2001:db8:1:77::1"))), 180000.0);
        f.clock.advance(180000.0);
        assert_eq!(req(g, "2001:db8:1:77::1"), None);
        assert_eq!(g.blocked_count(), 0);
    }

    #[test]
    fn requests_refused_for_a_block_are_not_counted_again() {
        let f = fixture(|_| {}, false);
        f.guard.apply_blocks(&[BlockOrder::new(key("198.51.100.9"), 60000.0, 1)]);
        for _ in 0..1000 {
            req(&f.guard, "198.51.100.9");
        }
        f.guard.note_refusal(&f.guard.keys(ip("198.51.100.9")), 5.0);
        f.guard.flush_reports();
        assert!(f.reports.lock().is_empty());
    }

    #[test]
    fn local_fast_path_blocks_at_once_and_still_reports() {
        let f = fixture(
            |c| {
                c.abuse_block_refusals_per_min = 20;
                c.abuse_block_base_sec = 30;
            },
            false,
        );
        let k = f.guard.keys(ip("192.0.2.50"));
        for _ in 0..3 {
            f.guard.note_refusal(&k, AUTH_REFUSAL_WEIGHT);
        }
        assert_eq!(f.guard.blocked_for(&k), 0.0, "15 < 20");
        f.guard.note_refusal(&k, AUTH_REFUSAL_WEIGHT);
        assert_eq!(f.guard.blocked_for(&k), 30000.0);
        assert_eq!(f.guard.stats().local_blocks, 1);
        f.guard.flush_reports();
        assert_eq!(
            *f.reports.lock(),
            [vec![RefusalEntry { k64: key("192.0.2.50"), k48: None, weight: 20.0 }]]
        );
    }

    #[test]
    fn the_local_tracker_sums_intervals_and_blocks() {
        let f = fixture(|c| c.abuse_block_refusals_per_min = 10, true);
        let k = f.guard.keys(ip("192.0.2.60"));
        for _ in 0..6 {
            f.guard.note_refusal(&k, 1.0);
        }
        f.guard.flush_reports();
        assert_eq!(f.guard.blocked_for(&k), 0.0, "6 in the minute");
        for _ in 0..4 {
            f.guard.note_refusal(&k, 1.0);
        }
        f.guard.flush_reports();
        assert_eq!(f.guard.blocked_for(&k), 60000.0, "10 in the minute: blocked for ABUSE_BLOCK_BASE_SEC");
        assert_eq!(f.guard.tracker_len(), 1);
    }

    #[test]
    fn threshold_zero_counts_no_refusal() {
        let f = fixture(
            |c| {
                c.abuse_block_refusals_per_min = 0;
                c.http_rate_per_ip = 2;
            },
            false,
        );
        let k = f.guard.keys(ip("192.0.2.70"));
        let _ = (f.guard.request(&k), f.guard.request(&k));
        for _ in 0..100 {
            assert_eq!(f.guard.request(&k).unwrap_err().reason, RequestRefusalReason::Ip);
        }
        assert_eq!(f.guard.flush_reports(), []);
        assert!(f.reports.lock().is_empty());
        assert_eq!(f.guard.blocked_for(&k), 0.0);
    }

    #[test]
    fn a_64_entry_carries_its_48() {
        let f = fixture(|_| {}, false);
        f.guard.note_refusal(&f.guard.keys(ip("2001:db8:9:1::1")), 2.0);
        f.guard.note_refusal(&f.guard.keys(ip("2001:db8:9:1::2")), 3.0);
        f.guard.flush_reports();
        assert_eq!(
            *f.reports.lock(),
            [vec![RefusalEntry {
                k64: key("2001:db8:9:1::/64"),
                k48: Some(key("2001:db8:9::/48")),
                weight: 5.0
            }]]
        );
    }

    #[tokio::test]
    async fn one_report_per_interval_on_a_timer() {
        let f = fixture(
            |c| {
                c.http_rate_per_ip = 1;
                c.abuse_block_refusals_per_min = 100_000;
            },
            false,
        );
        let mut guard = Arc::try_unwrap(f.guard).expect("one owner");
        guard = guard.with_report_interval(Duration::from_millis(30));
        let guard = Arc::new(guard);
        let task = guard.spawn_reporter();
        let k = guard.keys(ip("192.0.2.80"));
        assert_eq!(guard.request(&k), Ok(()));
        for _ in 0..50 {
            assert_eq!(guard.request(&k).unwrap_err().reason, RequestRefusalReason::Ip);
        }
        assert!(f.reports.lock().is_empty(), "no report on the request path");
        for i in 0..600u32 {
            guard
                .note_refusal(&guard.keys(ip(&format!("10.1.{}.{}", i >> 8, i & 255))), f64::from(1 + i % 7));
        }
        assert!(f.reports.lock().is_empty());
        tokio::time::sleep(Duration::from_millis(80)).await;
        {
            let reports = f.reports.lock();
            assert_eq!(reports.len(), 1, "one report for the interval");
            let entries = &reports[0];
            assert_eq!(entries.len(), REPORT_MAX_ENTRIES);
            assert_eq!(entries[0], RefusalEntry { k64: key("192.0.2.80"), k48: None, weight: 50.0 });
            assert!(entries.windows(2).all(|w| w[0].weight >= w[1].weight));
        }
        assert_eq!(guard.stats().report_entries_dropped, 601 - REPORT_MAX_ENTRIES as u64);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(f.reports.lock().len(), 1, "no report while nothing is refused");
        guard.close();
        task.abort();
    }

    #[test]
    fn the_checks_cost_about_a_microsecond() {
        let f = fixture(
            |c| {
                c.http_rate_per_ip = 1_000_000_000;
                c.ip_max_inflight = 100_000;
                c.ip_conn_rate = 100_000;
                c.ip_max_connections = 1_000_000;
            },
            false,
        );
        let g = &f.guard;
        let v4 = g.keys(ip("198.51.100.20"));
        let v6 = g.keys(ip("2001:db8:3:4::5"));
        let bench = |mut f: Box<dyn FnMut()>| {
            for _ in 0..2000 {
                f();
            }
            let t0 = std::time::Instant::now();
            for _ in 0..20000 {
                f();
            }
            t0.elapsed().as_nanos() as f64 / 20000.0
        };
        let req_v4 = bench(Box::new(|| {
            let _ = g.request(&v4);
            drop(g.enter_slot(&v4));
        }));
        let req_v6 = bench(Box::new(|| {
            let _ = g.request(&v6);
            drop(g.enter_slot(&v6));
        }));
        let conn = bench(Box::new(|| drop(g.connection(ip("198.51.100.22")))));
        // Generous bounds for a loaded, unoptimized test build.
        assert!(req_v4 < 50_000.0 && req_v6 < 50_000.0 && conn < 50_000.0, "{req_v4} {req_v6} {conn}");
        assert_eq!(g.open_total(), 0, "every benchmarked connection was released");
    }
}
