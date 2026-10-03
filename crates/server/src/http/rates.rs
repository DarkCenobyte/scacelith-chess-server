//! Rate limits of the API (docs/API.md 1.5): the budget of a signed-in account across every
//! endpoint (`USER_RATE_PER_MIN`), and the per-route rates.
//!
//! A [`RateSpec`] takes from the bucket of the account (`<key>:u<id>`, with `by_user` and a
//! session) or of the client's address (`<key>:<ipKey>`: an IPv4 address or an IPv6 /64); with a
//! `prefix_limit`, an IPv6 client also takes from its /48 (`<key>/48:<prefixKey>`). Local token
//! buckets (LRU-bounded) come first; `shared` rates then also count in the process-wide sliding
//! windows ([`SharedLimits`]) that the authentication limits use. The rates of a request are taken
//! in order, all or none, and a request that did none of the work they protect gives them back
//! (`refund_rate`). A refusal keyed by an address counts toward a block of that address with the
//! rate's `abuse_weight` (or 1); one keyed by an account never does.

use std::borrow::Cow;
use std::sync::Arc;

use parking_lot::Mutex;

use super::answer::ApiError;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::ids::UserId;
use crate::net::guard::{AddressKeys, http_rate_limited};
use crate::net::limits::{SharedLimits, TokenBucketLimiter};

/// Buckets kept by the route limiter and by the account budget.
const LIMITER_KEYS: usize = 100_000;

/// A route rate: `limit` per `window_ms` for each client (or account).
#[derive(Debug, Clone, PartialEq)]
pub struct RateSpec {
    /// The bucket name, also the metric label (`scacelith_http_rate_limited_total{limit}`).
    pub key: Cow<'static, str>,
    /// Requests allowed per window (the burst).
    pub limit: f64,
    /// The window over which the burst refills.
    pub window_ms: u64,
    /// Also count in the process-wide sliding window (exact across the whole window).
    pub shared: bool,
    /// Count per account when the request carries a session (else per address).
    pub by_user: bool,
    /// The limit of an IPv6 /48 as a whole (address-keyed rates only).
    pub prefix_limit: Option<f64>,
    /// Weight of a refusal toward a block of the address (0: the default weight 1).
    pub abuse_weight: f64,
}

impl RateSpec {
    /// `limit` requests per `window_ms` per address.
    pub fn new(key: impl Into<Cow<'static, str>>, limit: f64, window_ms: u64) -> RateSpec {
        RateSpec {
            key: key.into(),
            limit,
            window_ms,
            shared: false,
            by_user: false,
            prefix_limit: None,
            abuse_weight: 0.0,
        }
    }

    /// Also counts in the process-wide sliding window.
    pub fn shared(mut self) -> RateSpec {
        self.shared = true;
        self
    }

    /// Counts per account when signed in.
    pub fn by_user(mut self) -> RateSpec {
        self.by_user = true;
        self
    }

    /// Limits an IPv6 /48 as a whole.
    pub fn prefix_limit(mut self, limit: f64) -> RateSpec {
        self.prefix_limit = Some(limit);
        self
    }

    /// Weight of a refusal toward a block of the address ([`crate::net::guard::AUTH_REFUSAL_WEIGHT`]
    /// for the login family).
    pub fn abuse_weight(mut self, weight: f64) -> RateSpec {
        self.abuse_weight = weight;
        self
    }
}

/// One token a request took (given back on `refund_rate`).
#[derive(Debug, Clone, PartialEq)]
pub struct Taken {
    /// The bucket key.
    pub key: String,
    /// Its limit.
    pub limit: f64,
    /// Its window.
    pub window_ms: u64,
    /// When the shared window counted it (monotonic ms), `None` for a local rate.
    pub shared_at: Option<f64>,
}

/// The route limiter: local token buckets plus the shared windows.
pub struct RateLimits {
    clock: SharedClock,
    buckets: Mutex<TokenBucketLimiter<String>>,
    shared: Arc<SharedLimits>,
}

impl std::fmt::Debug for RateLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimits").field("buckets", &self.buckets.lock().len()).finish()
    }
}

impl RateLimits {
    /// A limiter on `clock` whose shared rates count in `shared`.
    pub fn new(clock: SharedClock, shared: Arc<SharedLimits>) -> RateLimits {
        RateLimits { clock, buckets: Mutex::new(TokenBucketLimiter::new(LIMITER_KEYS)), shared }
    }

    /// The shared windows.
    pub fn shared(&self) -> &Arc<SharedLimits> {
        &self.shared
    }

    /// Takes one token of each rate, in order, all or none. On a refusal the tokens taken so far
    /// are given back and the 429 is returned (with its abuse weight).
    pub fn check(
        &self,
        rates: &[RateSpec],
        client: &AddressKeys,
        user: Option<UserId>,
    ) -> Result<Vec<Taken>, ApiError> {
        let mut taken = Vec::new();
        for rate in rates {
            let by_user = rate.by_user && user.is_some();
            let key = match (by_user, user) {
                (true, Some(id)) => format!("{}:u{id}", rate.key),
                _ => format!("{}:{}", rate.key, client.k64),
            };
            match self.take(rate, key, rate.limit, &rate.key, by_user) {
                Ok(t) => taken.push(t),
                Err(e) => {
                    self.give_back(&taken);
                    return Err(e);
                }
            }
            if let (Some(prefix_limit), false, Some(k48)) = (rate.prefix_limit, by_user, client.k48) {
                let label = format!("{}/48", rate.key);
                match self.take(rate, format!("{label}:{k48}"), prefix_limit, &label, false) {
                    Ok(t) => taken.push(t),
                    Err(e) => {
                        self.give_back(&taken);
                        return Err(e);
                    }
                }
            }
        }
        Ok(taken)
    }

    fn take(
        &self,
        rate: &RateSpec,
        key: String,
        limit: f64,
        label: &str,
        by_user: bool,
    ) -> Result<Taken, ApiError> {
        let now = self.clock.mono_ms();
        let local = self.buckets.lock().take(&key, limit, rate.window_ms as f64, 1.0, now);
        if !local.allowed {
            return Err(refusal(rate, local.retry_after_ms, label, by_user));
        }
        let mut taken = Taken { key, limit, window_ms: rate.window_ms, shared_at: None };
        if rate.shared {
            let r = self.shared.take(&taken.key, limit, rate.window_ms, 1.0);
            if !r.allowed {
                // The local token stays spent, as in the Node server.
                let ms = if r.retry_after_ms > 0.0 { r.retry_after_ms } else { 1000.0 };
                return Err(refusal(rate, ms, label, by_user));
            }
            taken.shared_at = Some(now);
        }
        Ok(taken)
    }

    /// Gives back tokens (local buckets, and the shared windows that counted them).
    pub fn give_back(&self, taken: &[Taken]) {
        if taken.is_empty() {
            return;
        }
        let now = self.clock.mono_ms();
        let mut buckets = self.buckets.lock();
        for t in taken {
            buckets.give(&t.key, t.limit, t.window_ms as f64, 1.0, now);
        }
        drop(buckets);
        for t in taken {
            if let Some(at) = t.shared_at {
                self.shared.refund(&t.key, t.window_ms, 1.0, (now - at).max(0.0));
            }
        }
    }
}

/// The 429 of a route rate: counted under `label`; toward a block of the address unless keyed by
/// an account.
fn refusal(rate: &RateSpec, retry_after_ms: f64, label: &str, by_user: bool) -> ApiError {
    http_rate_limited().with(&[label]).inc();
    let weight = if by_user {
        0.0
    } else if rate.abuse_weight > 0.0 {
        rate.abuse_weight
    } else {
        1.0
    };
    ApiError::rate_limited(retry_after_ms).abuse_weight(weight)
}

/// The budget of one signed-in account across every endpoint (`USER_RATE_PER_MIN`): a token
/// bucket holding half a minute of the rate. A refusal never counts toward an address block.
pub struct UserBudget {
    clock: SharedClock,
    burst: f64,
    window_ms: f64,
    buckets: Mutex<TokenBucketLimiter<UserId>>,
}

impl std::fmt::Debug for UserBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserBudget").field("burst", &self.burst).field("window_ms", &self.window_ms).finish()
    }
}

impl UserBudget {
    /// The budget of `config` (120 per minute when unset).
    pub fn new(config: &Config, clock: SharedClock) -> UserBudget {
        let per_min = if config.user_rate_per_min > 0 { config.user_rate_per_min as f64 } else { 120.0 };
        let burst = (per_min / 2.0).ceil().max(1.0);
        let window_ms = (burst * 60_000.0 / per_min).round().max(1.0);
        UserBudget { clock, burst, window_ms, buckets: Mutex::new(TokenBucketLimiter::new(LIMITER_KEYS)) }
    }

    /// The burst (tokens of a full bucket).
    pub fn burst(&self) -> f64 {
        self.burst
    }

    /// Takes one token of `user`; a refusal is the 429 labelled `user`.
    pub fn take(&self, user: UserId) -> Result<(), ApiError> {
        let now = self.clock.mono_ms();
        let r = self.buckets.lock().take(&user, self.burst, self.window_ms, 1.0, now);
        if r.allowed {
            return Ok(());
        }
        http_rate_limited().with(&["user"]).inc();
        Err(ApiError::rate_limited(r.retry_after_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, ManualClock};
    use crate::net::ip::normalize_ip;

    fn limits() -> (RateLimits, Arc<ManualClock>) {
        let clock = ManualClock::new(1_000_000.0, 0);
        let c: SharedClock = clock.clone() as Arc<dyn Clock>;
        (RateLimits::new(c.clone(), Arc::new(SharedLimits::new(c))), clock)
    }

    fn keys(ip: &str) -> AddressKeys {
        AddressKeys::of(normalize_ip(ip).expect("an address"))
    }

    #[test]
    fn keys_name_the_account_or_the_address() {
        let (l, _) = limits();
        let r = RateSpec::new("pr", 2.0, 60000).by_user();
        assert_eq!(
            l.check(std::slice::from_ref(&r), &keys("192.0.2.77"), None).expect("ok")[0].key,
            "pr:192.0.2.77"
        );
        assert_eq!(
            l.check(std::slice::from_ref(&r), &keys("192.0.2.77"), Some(7)).expect("ok")[0].key,
            "pr:u7"
        );
        let v6 = RateSpec::new("shr", 100.0, 60000).shared().prefix_limit(300.0);
        let t = l.check(&[v6], &keys("2001:db8::1"), Some(7)).expect("ok");
        let names: Vec<&str> = t.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(names, ["shr:2001:db8:0:0::/64", "shr/48:2001:db8:0::/48"]);
        assert!(t.iter().all(|t| t.shared_at.is_some()));
    }

    #[test]
    fn a_later_refusal_gives_back_the_earlier_tokens() {
        let (l, clock) = limits();
        let rates = [RateSpec::new("slow", 2.0, 3_600_000), RateSpec::new("fast", 1.0, 60000)];
        let k = keys("198.51.100.1");
        assert!(l.check(&rates, &k, None).is_ok());
        for _ in 0..4 {
            let e = l.check(&rates, &k, None).expect_err("refused by fast");
            assert_eq!((e.status, e.abuse_weight), (429, 1.0));
        }
        clock.advance(60000.0);
        assert!(l.check(&rates, &k, None).is_ok(), "slow kept its token");
        clock.advance(60000.0);
        assert!(l.check(&rates, &k, None).is_err(), "now slow is spent");
    }

    #[test]
    fn refusal_weights() {
        let (l, _) = limits();
        let k = keys("198.51.100.2");
        let auth = [RateSpec::new("auth", 1.0, 600000).shared().prefix_limit(5.0).abuse_weight(5.0)];
        l.check(&auth, &k, None).expect("first");
        assert_eq!(l.check(&auth, &k, None).unwrap_err().abuse_weight, 5.0);
        let mine = [RateSpec::new("pub", 1.0, 60000).by_user()];
        l.check(&mine, &k, Some(9)).expect("first");
        assert_eq!(
            l.check(&mine, &k, Some(9)).unwrap_err().abuse_weight,
            0.0,
            "an account's limit never counts"
        );
        l.check(&mine, &k, None).expect("anonymous: the address");
        assert_eq!(l.check(&mine, &k, None).unwrap_err().abuse_weight, 1.0);
    }

    #[test]
    fn refunds_reach_the_shared_window() {
        let (l, _) = limits();
        let k = keys("203.0.113.1");
        let rates = [RateSpec::new("extra", 1.0, 3_600_000).shared()];
        let t = l.check(&rates, &k, None).expect("ok");
        assert!(l.check(&rates, &k, None).is_err());
        l.give_back(&t);
        assert_eq!(l.shared().peek("extra:203.0.113.1"), 0.0);
        assert!(l.check(&rates, &k, None).is_ok(), "both stages gave the token back");
    }

    #[test]
    fn user_budget_eight_per_minute() {
        let clock = ManualClock::new(0.0, 0);
        let mut c = Config::for_tests();
        c.user_rate_per_min = 8;
        let b = UserBudget::new(&c, clock.clone() as Arc<dyn Clock>);
        assert_eq!(b.burst(), 4.0);
        for _ in 0..4 {
            assert!(b.take(7).is_ok());
        }
        let e = b.take(7).unwrap_err();
        assert_eq!((e.status, e.abuse_weight), (429, 0.0));
        assert!(b.take(8).is_ok(), "another account");
        clock.advance(7500.0);
        assert!(b.take(7).is_ok(), "one token every 7.5 s");
        assert!(b.take(7).is_err());
    }
}
