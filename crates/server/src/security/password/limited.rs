//! The hasher behind the limiter: every hash and verification of the server (the dummy one and the
//! warm-up included) waits for a slot of the [`HashLimiter`] and runs on a blocking thread; the
//! failed checks of a login are padded to the [`CheckFloor`].

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use zeroize::Zeroizing;

use super::floor::CheckFloor;
use super::hasher::{HashFailure, PasswordHasher, Verified};
use super::limiter::{BusyReason, HashLimiter, HashOpts, PasswordBusy};
use crate::log::Logger;
use crate::log_warn;
use crate::security::ratelimit::prefix_key;

/// How often, at most, the saturation of the hash queue is logged.
const SATURATION_LOG_EVERY: Duration = Duration::from_secs(60);

/// A hash that did not give a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HashError {
    /// The limiter refused it: nothing was hashed, nothing may change (see
    /// [`BusyReason::answer`]).
    Busy(BusyReason),
    /// The key derivation failed (an internal error).
    Failed(HashFailure),
}

impl HashError {
    /// The busy reason, when the limiter refused the hash.
    pub fn busy(&self) -> Option<BusyReason> {
        match self {
            HashError::Busy(r) => Some(*r),
            HashError::Failed(_) => None,
        }
    }
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashError::Busy(r) => PasswordBusy { reason: *r }.fmt(f),
            HashError::Failed(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for HashError {}

impl From<PasswordBusy> for HashError {
    fn from(e: PasswordBusy) -> Self {
        HashError::Busy(e.reason)
    }
}

impl From<HashFailure> for HashError {
    fn from(e: HashFailure) -> Self {
        HashError::Failed(e)
    }
}

/// The wait budget of one request: all its hashes together wait at most the queue timeout (a
/// password change checks the current password and hashes the new one within it), and they count
/// toward the request's client source (an IPv4 address, or the /48 of an IPv6 address).
#[derive(Clone, Debug)]
pub struct HashBudget {
    deadline: Instant,
    source: Option<String>,
}

impl HashBudget {
    /// A budget of `queue_timeout` from now for a request from `ip` (no source when unknown).
    pub fn new(queue_timeout: Duration, ip: Option<&str>) -> HashBudget {
        HashBudget { deadline: Instant::now() + queue_timeout, source: ip.map(prefix_key) }
    }

    /// The client source of the request.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The options of the request's next hash: what is left of the budget (at least 1 ms, so a
    /// spent budget ends as a quick timeout, not as an optional hash).
    pub fn next(&self) -> HashOpts {
        let left = self.deadline.saturating_duration_since(Instant::now());
        let ms = left.as_secs_f64() * 1000.0;
        HashOpts {
            max_wait: Some(Duration::from_millis((ms.ceil() as u64).max(1))),
            source: self.source.clone(),
        }
    }

    /// The options of an optional hash (the rehash of an outdated hash): only when a slot is free
    /// at once.
    pub fn no_wait(&self) -> HashOpts {
        HashOpts { max_wait: Some(Duration::ZERO), source: self.source.clone() }
    }
}

/// The server's password hasher behind its limiter and padding floor. Cheap to share (`Arc`).
pub struct LimitedHasher {
    hasher: Arc<dyn PasswordHasher>,
    limiter: HashLimiter,
    floor: Arc<CheckFloor>,
    log: Logger,
    last_saturation_log: Mutex<Option<Instant>>,
}

impl fmt::Debug for LimitedHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LimitedHasher")
            .field("algorithm", &self.hasher.algorithm())
            .field("limiter", &self.limiter)
            .finish()
    }
}

impl LimitedHasher {
    /// Puts `hasher` behind `limiter` and `floor`. `log` receives the saturation warning (the auth
    /// service's logger).
    pub fn new(
        hasher: Arc<dyn PasswordHasher>,
        limiter: HashLimiter,
        floor: Arc<CheckFloor>,
        log: Logger,
    ) -> LimitedHasher {
        LimitedHasher { hasher, limiter, floor, log, last_saturation_log: Mutex::new(None) }
    }

    /// The algorithm of new hashes.
    pub fn algorithm(&self) -> &'static str {
        self.hasher.algorithm()
    }

    /// The limiter.
    pub fn limiter(&self) -> &HashLimiter {
        &self.limiter
    }

    /// The padding floor.
    pub fn floor(&self) -> &CheckFloor {
        &self.floor
    }

    /// A new wait budget for one request from `ip`.
    pub fn budget(&self, ip: Option<&str>) -> HashBudget {
        HashBudget::new(self.limiter.config().queue_timeout, ip)
    }

    /// Logs, at most once a minute, that requests are refused because the queue is saturated.
    fn note_refusal(&self, reason: BusyReason) {
        if !matches!(reason, BusyReason::QueueFull | BusyReason::Timeout) {
            return;
        }
        {
            let mut last = self.last_saturation_log.lock();
            if last.is_some_and(|t| t.elapsed() < SATURATION_LOG_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let mut fields = serde_json::Map::new();
        fields.insert("reason".into(), reason.as_str().into());
        if let serde_json::Value::Object(stats) = self.limiter.stats().to_json() {
            fields.extend(stats);
        }
        self.log.emit(
            crate::log::Level::Warn,
            "password hashing saturated: requests refused with 503 server_busy",
            Some(serde_json::Value::Object(fields)),
        );
    }

    async fn run<T, F>(&self, opts: &HashOpts, f: F) -> Result<T, HashError>
    where
        T: Send + 'static,
        F: FnOnce(&dyn PasswordHasher) -> Result<T, HashFailure> + Send + 'static,
    {
        let hasher = self.hasher.clone();
        match self.limiter.run_blocking(opts, move || f(hasher.as_ref())).await {
            Ok(r) => Ok(r?),
            Err(busy) => {
                self.note_refusal(busy.reason);
                Err(busy.into())
            }
        }
    }

    /// A new hash of `password`.
    pub async fn hash(&self, password: &str, opts: &HashOpts) -> Result<String, HashError> {
        let pw = Zeroizing::new(password.to_owned());
        self.run(opts, move |h| h.hash(&pw)).await
    }

    /// Checks `password` against `stored` (no padding: re-authentication, compare-and-set
    /// re-checks).
    pub async fn verify(&self, stored: &str, password: &str, opts: &HashOpts) -> Result<Verified, HashError> {
        let (stored, pw) = (stored.to_owned(), Zeroizing::new(password.to_owned()));
        self.run(opts, move |h| h.verify(&stored, &pw)).await
    }

    /// The dummy verification of an unknown account (never matches).
    pub async fn verify_dummy(&self, password: &str, opts: &HashOpts) -> Result<(), HashError> {
        let pw = Zeroizing::new(password.to_owned());
        self.run(opts, move |h| h.verify_dummy(&pw)).await
    }

    /// The password check of a login: `stored` is the account's hash, or `None` (or empty) for an
    /// unknown account or one without a password, which gets the dummy check. The duration of the
    /// work is recorded in the floor (successes too); a failure resolves only once the check took
    /// the floor, waiting after the slot is released (the padding uses no hash capacity).
    pub async fn check_password(
        &self,
        stored: Option<&str>,
        password: &str,
        opts: &HashOpts,
    ) -> Result<Verified, HashError> {
        let stored = stored.filter(|s| !s.is_empty()).map(str::to_owned);
        let pw = Zeroizing::new(password.to_owned());
        let floor = self.floor.clone();
        let (result, work_ms) = self
            .run(opts, move |h| {
                let t0 = Instant::now();
                let r = match &stored {
                    Some(s) => h.verify(s, &pw),
                    None => h.verify_dummy(&pw).map(|()| Verified::default()),
                };
                let work_ms = t0.elapsed().as_secs_f64() * 1000.0;
                floor.record(work_ms);
                Ok((r, work_ms))
            })
            .await?;
        let verified = result?;
        if !verified.ok {
            let pad = self.floor.floor_ms() - work_ms;
            if pad >= 1.0 {
                tokio::time::sleep(Duration::from_secs_f64(pad / 1000.0)).await;
            }
        }
        Ok(verified)
    }

    /// Runs the hasher's warm-up in a slot (full queue timeout) and makes what it measured the
    /// floor's baseline. The server starts it in the background at start-up and logs a failure
    /// ("password hashing warm-up failed").
    pub async fn warm_up(&self) -> Result<(), HashError> {
        let ms = self.run(&HashOpts::default(), |h| h.warm_up()).await?;
        self.floor.set_baseline(ms);
        Ok(())
    }
}

/// Logs a failed warm-up with the former server's message.
pub fn log_warm_up_failure(log: &Logger, err: &HashError) {
    log_warn!(log, "password hashing warm-up failed", { "err": { "message": err.to_string() } });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::security::password::hasher::{Argon2Hasher, Argon2Params};
    use crate::security::password::limiter::HashLimiterConfig;
    use crate::security::password::limiter::tests::METRICS_LOCK;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A test double: every call sleeps (a slow kind of stored hash: `slow:`), `h:<pw>` hashes.
    struct Stub {
        calls: Mutex<Vec<&'static str>>,
        running: AtomicUsize,
        peak: AtomicUsize,
        warm_ms: f64,
    }

    impl Stub {
        fn new(warm_ms: f64) -> Arc<Stub> {
            Arc::new(Stub {
                calls: Mutex::new(vec![]),
                running: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                warm_ms,
            })
        }

        fn work(&self, name: &'static str, ms: u64) {
            self.calls.lock().push(name);
            let n = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(ms));
            self.running.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl PasswordHasher for Stub {
        fn algorithm(&self) -> &'static str {
            "stub"
        }
        fn hash(&self, password: &str) -> Result<String, HashFailure> {
            if password == "fail" {
                return Err(HashFailure("stub failed".into()));
            }
            self.work("hash", 2);
            Ok(format!("h:{password}"))
        }
        fn verify(&self, stored: &str, password: &str) -> Result<Verified, HashFailure> {
            self.work("verify", if stored.starts_with("slow") { 80 } else { 5 });
            Ok(Verified { ok: stored.ends_with(&format!(":{password}")), needs_rehash: false })
        }
        fn verify_dummy(&self, _password: &str) -> Result<(), HashFailure> {
            self.work("verifyDummy", 5);
            Ok(())
        }
        fn warm_up(&self) -> Result<f64, HashFailure> {
            self.work("warmUp", 5);
            Ok(self.warm_ms)
        }
    }

    fn capped(stub: Arc<Stub>, queue_max: usize) -> LimitedHasher {
        let limiter = HashLimiter::new(HashLimiterConfig {
            concurrency: 1,
            queue_max,
            queue_timeout: Duration::from_secs(5),
            per_source_max: None,
        })
        .unwrap();
        let floor = Arc::new(CheckFloor::new(crate::clock::system()));
        LimitedHasher::new(stub, limiter, floor, Logger::root().child("auth"))
    }

    fn zero() -> HashOpts {
        HashOpts { max_wait: Some(Duration::ZERO), source: None }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hash_verify_dummy_and_warm_up_share_the_cap() {
        let _m = METRICS_LOCK.lock().await;
        let stub = Stub::new(0.0);
        let h = Arc::new(capped(stub.clone(), 10));
        assert_eq!(h.algorithm(), "stub");
        let o = HashOpts::default();
        let (w, a, b, c, d) = tokio::join!(
            h.warm_up(),
            h.hash("pw", &o),
            h.verify("h:pw", "pw", &o),
            h.verify("h:pw", "no", &o),
            h.verify_dummy("x", &o)
        );
        assert_eq!(w, Ok(()));
        assert_eq!(a.unwrap(), "h:pw");
        assert_eq!(b.unwrap(), Verified { ok: true, needs_rehash: false });
        assert_eq!(c.unwrap(), Verified::default());
        assert_eq!(d, Ok(()));
        assert_eq!(*stub.calls.lock(), ["warmUp", "hash", "verify", "verify", "verifyDummy"]);
        assert_eq!(stub.peak.load(Ordering::SeqCst), 1);

        // A refusal comes back as Busy; an error of the hasher itself is not one.
        let full = Arc::new(capped(stub.clone(), 0));
        let f2 = full.clone();
        let first = tokio::spawn(async move { f2.verify_dummy("a", &HashOpts::default()).await });
        while full.limiter().stats().active == 0 {
            tokio::task::yield_now().await;
        }
        let refused = full.verify_dummy("b", &HashOpts::default()).await.unwrap_err();
        assert_eq!(refused, HashError::Busy(BusyReason::QueueFull));
        assert_eq!(refused.busy(), Some(BusyReason::QueueFull));
        first.await.unwrap().unwrap();
        let failed = h.hash("fail", &o).await.unwrap_err();
        assert_eq!(failed, HashError::Failed(HashFailure("stub failed".into())));
        assert_eq!(failed.busy(), None);
    }

    #[tokio::test]
    async fn warm_up_sets_the_baseline_in_a_slot() {
        let _m = METRICS_LOCK.lock().await;
        let stub = Stub::new(42.0);
        let h = Arc::new(capped(stub.clone(), 10));
        let busy = h.limiter().acquire(&HashOpts::default()).await.unwrap();
        let h2 = h.clone();
        let warm = tokio::spawn(async move { h2.warm_up().await });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!((stub.running.load(Ordering::SeqCst), h.limiter().stats().waiting), (0, 1));
        drop(busy);
        warm.await.unwrap().unwrap();
        assert_eq!(h.floor().baseline_ms(), 42.0);
        assert_eq!(h.floor().floor_ms(), 42.0);
        // The first failed check is padded to it.
        let t0 = Instant::now();
        assert_eq!(h.check_password(None, "x", &HashOpts::default()).await.unwrap(), Verified::default());
        assert!(t0.elapsed() >= Duration::from_millis(38), "{:?}", t0.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failure_is_padded_to_the_floor_after_the_slot_is_released() {
        let _m = METRICS_LOCK.lock().await;
        let h = Arc::new(capped(Stub::new(0.0), 10));
        let o = HashOpts::default();
        // A slow stored hash sets the floor; the fast dummy failure then takes as long.
        let t0 = Instant::now();
        assert_eq!(h.check_password(Some("slow:pw"), "nope", &o).await.unwrap(), Verified::default());
        let t1 = t0.elapsed();
        assert!(h.floor().floor_ms() >= 75.0, "floor {}", h.floor().floor_ms());
        let t0 = Instant::now();
        assert_eq!(h.check_password(None, "anything", &o).await.unwrap(), Verified::default());
        let t2 = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(
            t2 >= h.floor().floor_ms() - 5.0,
            "dummy failure {t2} ms, floor {} ms (slow {t1:?})",
            h.floor().floor_ms()
        );
        // Empty stored hashes are unknown accounts too.
        assert_eq!(h.check_password(Some(""), "x", &o).await.unwrap(), Verified::default());
        // A success is not padded.
        let t0 = Instant::now();
        assert_eq!(
            h.check_password(Some("fast:pw"), "pw", &o).await.unwrap(),
            Verified { ok: true, needs_rehash: false }
        );
        assert!(t0.elapsed() < Duration::from_millis(60), "success took {:?}", t0.elapsed());
        // The padding holds no slot: the next task runs while the failure still waits.
        let h2 = h.clone();
        let failing = tokio::spawn(async move { h2.check_password(None, "x", &HashOpts::default()).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let during = h.limiter().run_blocking(&HashOpts::default(), || ()).await;
        assert!(during.is_ok() && !failing.is_finished(), "the slot is free while the failure is padded");
        failing.await.unwrap().unwrap();
        // The options reach the limiter.
        let busy = h.limiter().acquire(&HashOpts::default()).await.unwrap();
        let no_wait = HashError::Busy(BusyReason::NoWait);
        assert_eq!(h.check_password(None, "x", &zero()).await.unwrap_err(), no_wait);
        assert_eq!(h.hash("x", &zero()).await.unwrap_err(), no_wait);
        assert_eq!(h.verify("fast:x", "x", &zero()).await.unwrap_err(), no_wait);
        assert_eq!(h.verify_dummy("x", &zero()).await.unwrap_err(), no_wait);
        drop(busy);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_check_takes_as_long_for_an_unknown_account_as_for_a_stored_hash() {
        let _m = METRICS_LOCK.lock().await;
        // The real hasher: the dummy and stored hashes use the same parameters; with the floor, an
        // unknown account is never faster than a wrong password.
        let hasher = Arc::new(Argon2Hasher::new(Argon2Params {
            memory_kib: 4096,
            passes: 1,
            lanes: 1,
            ..Argon2Params::DEFAULT
        }));
        let limiter = HashLimiter::new(HashLimiterConfig::default()).unwrap();
        let floor = Arc::new(CheckFloor::new(ManualClock::new(0.0, 0)));
        let h = LimitedHasher::new(hasher, limiter, floor, Logger::root().child("auth"));
        h.warm_up().await.unwrap();
        assert!(h.floor().baseline_ms() > 0.0);
        let stored = h.hash("the right one", &HashOpts::default()).await.unwrap();
        let o = HashOpts::default();
        assert!(h.check_password(Some(&stored), "the right one", &o).await.unwrap().ok);
        let t0 = Instant::now();
        assert!(!h.check_password(None, "a wrong one", &o).await.unwrap().ok);
        let unknown = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(
            unknown >= h.floor().floor_ms() - 2.0,
            "unknown {unknown} ms, floor {}",
            h.floor().floor_ms()
        );
    }

    #[tokio::test]
    async fn budgets_share_one_queue_timeout() {
        let _m = METRICS_LOCK.lock().await;
        let h = capped(Stub::new(0.0), 10);
        let b = h.budget(Some("2001:db8:aa:bb::1"));
        assert_eq!(b.source(), Some("2001:db8:aa::/48"));
        let next = b.next();
        assert!(
            next.max_wait.unwrap() <= Duration::from_secs(5)
                && next.max_wait.unwrap() > Duration::from_secs(4)
        );
        assert_eq!(next.source.as_deref(), Some("2001:db8:aa::/48"));
        assert_eq!(b.no_wait().max_wait, Some(Duration::ZERO));
        assert_eq!(h.budget(Some("::ffff:198.51.100.4")).source(), Some("198.51.100.4"));
        assert_eq!(h.budget(None).source(), None);
        let spent = HashBudget::new(Duration::ZERO, None);
        assert_eq!(spent.next().max_wait, Some(Duration::from_millis(1)), "a spent budget is a 1 ms wait");
    }

    #[tokio::test]
    async fn saturation_is_logged_at_most_once_a_minute() {
        let _m = METRICS_LOCK.lock().await;
        let h = capped(Stub::new(0.0), 0);
        let busy = h.limiter().acquire(&HashOpts::default()).await.unwrap();
        for _ in 0..3 {
            assert_eq!(
                h.hash("x", &HashOpts::default()).await.unwrap_err(),
                HashError::Busy(BusyReason::QueueFull)
            );
        }
        assert!(h.last_saturation_log.lock().is_some());
        let first = *h.last_saturation_log.lock();
        assert_eq!(h.hash("x", &HashOpts::default()).await.unwrap_err().busy(), Some(BusyReason::QueueFull));
        assert_eq!(*h.last_saturation_log.lock(), first, "not logged again within the minute");
        drop(busy);
        assert_eq!(
            HashError::Busy(BusyReason::Timeout).to_string(),
            "password hashing: the wait for a slot expired"
        );
    }
}
