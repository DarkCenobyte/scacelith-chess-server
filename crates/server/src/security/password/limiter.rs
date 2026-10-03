//! Bounded FIFO semaphore for the password hashes of the server (DESIGN.md section 8, "Password
//! hash cap").
//!
//! At most `concurrency` hashes run at once; the others wait in arrival order. A hash that finds
//! `queue_max` hashes already waiting is refused at once. So is a hash whose `source` (a client
//! address or network prefix) already has `per_source_max` hashes waiting, but only under
//! contention, once at least half of `queue_max` wait: one source (a classroom behind one IPv4
//! address) may use an idle queue, and, as long as `per_source_max` is at most half of
//! `queue_max`, it cannot take more than half of it. A waiting hash gives up after
//! `queue_timeout`, or after its own shorter wait (it leaves the queue). A finished hash hands its
//! slot straight to the oldest waiter, so a newcomer never overtakes a waiter.
//!
//! Every refusal happens before any work, with a [`PasswordBusy`] error whose [`BusyReason`] the
//! auth service maps to its answer ([`BusyReason::answer`]).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::config::Config;
use crate::metrics::{self, CounterVec, Gauge, Histogram};

static IN_FLIGHT: LazyLock<Gauge> = LazyLock::new(|| {
    metrics::gauge(
        "scacelith_password_hash_in_flight",
        "Password hashes and verifications running (blocking threads)",
    )
});
static QUEUED: LazyLock<Gauge> = LazyLock::new(|| {
    metrics::gauge("scacelith_password_hash_queued", "Password hashes and verifications waiting for a slot")
});
static WAIT_MS: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram(
        "scacelith_password_hash_wait_ms",
        "Time a password hash waited for its slot (granted ones)",
        &[1.0, 10.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 20000.0],
    )
});
static REJECTED: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec(
        "scacelith_password_hash_rejected_total",
        "Password hashes refused (queue_full, timeout: 503 server_busy; source_limit: 429 rate_limited)",
        &["reason"],
    )
});

/// Why the limiter refused a hash (nothing was hashed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusyReason {
    /// `queue_max` hashes already wait.
    QueueFull,
    /// The wait for a slot expired (the queue timeout, or the call's shorter budget).
    Timeout,
    /// `per_source_max` hashes of the same client source already wait, with the queue at least
    /// half full.
    SourceLimit,
    /// No slot was free and the call would not wait. This is no refusal of a request: the caller
    /// skips optional work (the rehash of an outdated hash), and no metric counts it.
    NoWait,
}

/// What the auth service answers for a refused hash (its error mapping, DESIGN.md section 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyAnswer {
    /// Skip the optional work silently ([`BusyReason::NoWait`]).
    Skip,
    /// 429 `rate_limited` with a random 5-15 s `Retry-After`, and the request's rate tokens given
    /// back ([`BusyReason::SourceLimit`]).
    RateLimited,
    /// 503 `server_busy` with a random 5-15 s `Retry-After` ([`BusyReason::QueueFull`],
    /// [`BusyReason::Timeout`]).
    ServerBusy,
}

impl BusyReason {
    /// The label of the reason (metrics, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            BusyReason::QueueFull => "queue_full",
            BusyReason::Timeout => "timeout",
            BusyReason::SourceLimit => "source_limit",
            BusyReason::NoWait => "no_wait",
        }
    }

    /// The answer the auth service gives for this refusal.
    pub fn answer(self) -> BusyAnswer {
        match self {
            BusyReason::NoWait => BusyAnswer::Skip,
            BusyReason::SourceLimit => BusyAnswer::RateLimited,
            BusyReason::QueueFull | BusyReason::Timeout => BusyAnswer::ServerBusy,
        }
    }

    fn message(self) -> &'static str {
        match self {
            BusyReason::QueueFull => "password hashing: the queue is full",
            BusyReason::Timeout => "password hashing: the wait for a slot expired",
            BusyReason::SourceLimit => {
                "password hashing: this client already has as many hashes waiting as it may"
            }
            BusyReason::NoWait => "password hashing: no slot is free and the call would not wait",
        }
    }
}

/// A password hash the limiter refused (nothing was hashed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasswordBusy {
    /// Why.
    pub reason: BusyReason,
}

impl fmt::Display for PasswordBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason.message())
    }
}

impl std::error::Error for PasswordBusy {}

/// The options of one hash: its wait budget and its client source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HashOpts {
    /// The longest wait of this call: the queue timeout when `None`, never more. `Some(ZERO)`
    /// runs the hash only when a slot is free at once and is otherwise refused with
    /// [`BusyReason::NoWait`].
    pub max_wait: Option<Duration>,
    /// The client source that `per_source_max` counts (`None`: not counted).
    pub source: Option<String>,
}

/// Limits of a [`HashLimiter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashLimiterConfig {
    /// Hashes running at once (at least 1).
    pub concurrency: usize,
    /// Hashes waiting at most (0: none waits).
    pub queue_max: usize,
    /// The longest wait for a slot.
    pub queue_timeout: Duration,
    /// Hashes of one source waiting at most once the queue is half full (`None`: no cap; at
    /// least 1).
    pub per_source_max: Option<usize>,
}

impl Default for HashLimiterConfig {
    fn default() -> Self {
        HashLimiterConfig {
            concurrency: 1,
            queue_max: 32,
            queue_timeout: Duration::from_secs(10),
            per_source_max: None,
        }
    }
}

impl HashLimiterConfig {
    /// The limits of `PASSWORD_HASH_CONCURRENCY`, `PASSWORD_HASH_QUEUE_MAX`,
    /// `PASSWORD_HASH_QUEUE_TIMEOUT_MS` and `PASSWORD_HASH_WAITERS_PER_SOURCE`, as whole-server
    /// values (the former server applied them per worker process).
    pub fn from_config(config: &Config) -> HashLimiterConfig {
        HashLimiterConfig {
            concurrency: usize::try_from(config.password_hash_concurrency).unwrap_or(0),
            queue_max: usize::try_from(config.password_hash_queue_max).unwrap_or(0),
            queue_timeout: Duration::from_millis(
                u64::try_from(config.password_hash_queue_timeout_ms).unwrap_or(0),
            ),
            per_source_max: Some(usize::try_from(config.password_hash_waiters_per_source).unwrap_or(0)),
        }
    }
}

/// Invalid limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimiterConfigError(&'static str);

impl fmt::Display for LimiterConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for LimiterConfigError {}

/// The state of a limiter, for logs and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimiterStats {
    /// Hashes running.
    pub active: usize,
    /// Hashes waiting.
    pub waiting: usize,
    /// The concurrency limit.
    pub concurrency: usize,
    /// The queue limit.
    pub queue_max: usize,
    /// The queue timeout in milliseconds.
    pub queue_timeout_ms: u64,
}

impl LimiterStats {
    /// The stats as the JSON fields of a log record (`active, waiting, concurrency, queueMax,
    /// queueTimeoutMs`).
    pub fn to_json(&self) -> Value {
        json!({
            "active": self.active,
            "waiting": self.waiting,
            "concurrency": self.concurrency,
            "queueMax": self.queue_max,
            "queueTimeoutMs": self.queue_timeout_ms,
        })
    }
}

struct Waiter {
    id: u64,
    since: Instant,
    source: Option<String>,
    grant: oneshot::Sender<()>,
}

#[derive(Default)]
struct State {
    active: usize,
    /// Oldest first. Invariant: empty whenever `active < concurrency` (a released slot is handed
    /// over instead of freed while someone waits).
    waiting: VecDeque<Waiter>,
    /// Source -> number of its hashes waiting (sources with at least one).
    by_source: HashMap<String, usize>,
    next_id: u64,
}

impl State {
    /// Bookkeeping of a waiter that leaves the queue (granted, expired or cancelled).
    fn leave(&mut self, w: &Waiter) {
        QUEUED.dec();
        if let Some(s) = &w.source
            && let Some(n) = self.by_source.get_mut(s)
        {
            *n -= 1;
            if *n == 0 {
                self.by_source.remove(s);
            }
        }
    }

    /// Frees one slot: hands it to the oldest waiter, or decrements `active`.
    fn release(&mut self) {
        if let Some(w) = self.waiting.pop_front() {
            self.leave(&w);
            WAIT_MS.observe(w.since.elapsed().as_secs_f64() * 1000.0);
            // The slot passes to the waiter: `active` is unchanged. Should the waiter be gone
            // already (its future dropped between our pop and its own clean-up), its guard sees
            // that it is no longer queued and releases the slot again.
            let _ = w.grant.send(());
            return;
        }
        self.active -= 1;
        IN_FLIGHT.dec();
    }

    fn remove_waiter(&mut self, id: u64) -> bool {
        match self.waiting.iter().position(|w| w.id == id) {
            Some(i) => {
                let w = self.waiting.remove(i).expect("the position was just found");
                self.leave(&w);
                true
            }
            None => false,
        }
    }
}

struct Shared {
    cfg: HashLimiterConfig,
    contended: usize,
    state: Mutex<State>,
}

/// The bounded FIFO hash semaphore. Cheap to clone (shared state).
#[derive(Clone)]
pub struct HashLimiter {
    shared: Arc<Shared>,
}

impl fmt::Debug for HashLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashLimiter").field("stats", &self.stats()).finish()
    }
}

/// A granted slot. Dropping it frees the slot (hands it to the oldest waiter).
pub struct HashPermit {
    shared: Arc<Shared>,
}

impl fmt::Debug for HashPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashPermit")
    }
}

impl Drop for HashPermit {
    fn drop(&mut self) {
        self.shared.state.lock().release();
    }
}

/// A waiter's place in the queue while its future is pending. Dropped without being settled (the
/// request was cancelled), it leaves the queue, or gives back the slot it was handed meanwhile.
struct QueuePlace {
    shared: Arc<Shared>,
    id: u64,
    settled: bool,
}

impl Drop for QueuePlace {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut st = self.shared.state.lock();
        if !st.remove_waiter(self.id) {
            st.release();
        }
    }
}

impl HashLimiter {
    /// A limiter with these limits.
    pub fn new(cfg: HashLimiterConfig) -> Result<HashLimiter, LimiterConfigError> {
        if cfg.concurrency < 1 {
            return Err(LimiterConfigError("concurrency must be an integer >= 1"));
        }
        if cfg.per_source_max == Some(0) {
            return Err(LimiterConfigError("perSourceMax must be an integer >= 1"));
        }
        LazyLock::force(&IN_FLIGHT);
        LazyLock::force(&QUEUED);
        Ok(HashLimiter {
            shared: Arc::new(Shared {
                cfg,
                contended: cfg.queue_max / 2,
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// The limits.
    pub fn config(&self) -> HashLimiterConfig {
        self.shared.cfg
    }

    /// The per-source cap (`None`: none).
    pub fn per_source_max(&self) -> Option<usize> {
        self.shared.cfg.per_source_max
    }

    /// The current state.
    pub fn stats(&self) -> LimiterStats {
        let st = self.shared.state.lock();
        let cfg = self.shared.cfg;
        LimiterStats {
            active: st.active,
            waiting: st.waiting.len(),
            concurrency: cfg.concurrency,
            queue_max: cfg.queue_max,
            queue_timeout_ms: cfg.queue_timeout.as_millis() as u64,
        }
    }

    /// Number of hashes of `source` waiting now.
    pub fn waiting_from(&self, source: &str) -> usize {
        self.shared.state.lock().by_source.get(source).copied().unwrap_or(0)
    }

    /// Waits for a slot, in arrival order, at most the call's budget (see [`HashOpts`]). The
    /// checks are, in this order: a free slot is granted at once; a call that would not wait is
    /// refused (`no_wait`); a source at its cap under contention is refused (`source_limit`); a
    /// full queue refuses (`queue_full`); else the call waits (`timeout` when its budget ends).
    pub async fn acquire(&self, opts: &HashOpts) -> Result<HashPermit, PasswordBusy> {
        let cfg = self.shared.cfg;
        let wait = opts.max_wait.map_or(cfg.queue_timeout, |w| w.min(cfg.queue_timeout));
        let no_wait = opts.max_wait.is_some_and(|w| w.is_zero());
        let (id, rx) = {
            let mut st = self.shared.state.lock();
            if st.active < cfg.concurrency {
                st.active += 1;
                IN_FLIGHT.inc();
                WAIT_MS.observe(0.0);
                return Ok(self.permit());
            }
            if no_wait {
                return Err(PasswordBusy { reason: BusyReason::NoWait });
            }
            if let (Some(source), Some(cap)) = (&opts.source, cfg.per_source_max)
                && st.waiting.len() >= self.shared.contended
                && st.by_source.get(source).copied().unwrap_or(0) >= cap
            {
                REJECTED.with(&["source_limit"]).inc();
                return Err(PasswordBusy { reason: BusyReason::SourceLimit });
            }
            if st.waiting.len() >= cfg.queue_max {
                REJECTED.with(&["queue_full"]).inc();
                return Err(PasswordBusy { reason: BusyReason::QueueFull });
            }
            let id = st.next_id;
            st.next_id += 1;
            let (tx, rx) = oneshot::channel();
            if let Some(s) = &opts.source {
                *st.by_source.entry(s.clone()).or_insert(0) += 1;
            }
            st.waiting.push_back(Waiter {
                id,
                since: Instant::now(),
                source: opts.source.clone(),
                grant: tx,
            });
            QUEUED.inc();
            (id, rx)
        };
        let mut place = QueuePlace { shared: self.shared.clone(), id, settled: false };
        let granted = tokio::time::timeout(wait, rx).await;
        let mut st = self.shared.state.lock();
        place.settled = true;
        match granted {
            Ok(Ok(())) => Ok(self.permit()),
            // The budget ended: leave the queue, unless the slot was handed over in the meantime
            // (then the call owns it and runs).
            _ if st.remove_waiter(id) => {
                REJECTED.with(&["timeout"]).inc();
                Err(PasswordBusy { reason: BusyReason::Timeout })
            }
            _ => Ok(self.permit()),
        }
    }

    fn permit(&self) -> HashPermit {
        HashPermit { shared: self.shared.clone() }
    }

    /// Runs the blocking function `f` on a blocking thread once a slot is granted, and returns its
    /// result. The slot is held until `f` returns, even when the caller stops waiting: the cap
    /// bounds the work actually running. A panic in `f` is resumed in the caller.
    pub async fn run_blocking<T, F>(&self, opts: &HashOpts, f: F) -> Result<T, PasswordBusy>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = self.acquire(opts).await?;
        let job = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f()
        });
        match job.await {
            Ok(v) => Ok(v),
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => panic!("password hashing task cancelled: {e}"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The hash metrics are process-wide: the tests that look at them, and every test that moves
    /// them, run one at a time.
    pub(crate) static METRICS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn limiter(
        concurrency: usize,
        queue_max: usize,
        timeout_ms: u64,
        per_source: Option<usize>,
    ) -> HashLimiter {
        HashLimiter::new(HashLimiterConfig {
            concurrency,
            queue_max,
            queue_timeout: Duration::from_millis(timeout_ms),
            per_source_max: per_source,
        })
        .unwrap()
    }

    fn src(s: &str) -> HashOpts {
        HashOpts { max_wait: None, source: Some(s.to_string()) }
    }

    fn rejected(reason: &str) -> u64 {
        REJECTED.with(&[reason]).get()
    }

    /// Lets the spawned tasks run up to their next await.
    async fn tick() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    /// Spawns a task that acquires a slot, records its name in `order`, and keeps the slot until
    /// `hold` resolves.
    fn spawn_holder(
        lim: &HashLimiter,
        opts: HashOpts,
        name: &'static str,
        order: &Arc<Mutex<Vec<&'static str>>>,
    ) -> (oneshot::Sender<()>, tokio::task::JoinHandle<Result<(), BusyReason>>) {
        let (tx, hold) = oneshot::channel::<()>();
        let lim = lim.clone();
        let order = order.clone();
        let task = tokio::spawn(async move {
            let permit = lim.acquire(&opts).await.map_err(|e| e.reason)?;
            order.lock().push(name);
            let _ = hold.await;
            drop(permit);
            Ok(())
        });
        (tx, task)
    }

    /// Spawns a task that acquires a slot, records its name and frees the slot at once.
    fn spawn_quick(
        lim: &HashLimiter,
        opts: HashOpts,
        name: &'static str,
        order: &Arc<Mutex<Vec<&'static str>>>,
    ) -> tokio::task::JoinHandle<Result<(), BusyReason>> {
        let lim = lim.clone();
        let order = order.clone();
        tokio::spawn(async move {
            let _permit = lim.acquire(&opts).await.map_err(|e| e.reason)?;
            order.lock().push(name);
            tokio::task::yield_now().await;
            Ok(())
        })
    }

    #[tokio::test]
    async fn waiters_run_in_arrival_order_one_at_a_time() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 10, 5000, None);
        let order = Arc::new(Mutex::new(Vec::new()));
        let (release_a, a) = spawn_holder(&lim, HashOpts::default(), "a", &order);
        tick().await;
        let mut all = vec![];
        for name in ["b", "c", "d", "e"] {
            all.push(spawn_quick(&lim, HashOpts::default(), name, &order));
            tick().await;
        }
        assert_eq!(*order.lock(), ["a"]);
        assert_eq!(
            lim.stats(),
            LimiterStats { active: 1, waiting: 4, concurrency: 1, queue_max: 10, queue_timeout_ms: 5000 }
        );
        release_a.send(()).unwrap();
        // A newcomer arriving while the queue drains does not overtake the waiters.
        tokio::task::yield_now().await;
        all.push(spawn_quick(&lim, HashOpts::default(), "late", &order));
        a.await.unwrap().unwrap();
        for t in all {
            t.await.unwrap().unwrap();
        }
        assert_eq!(*order.lock(), ["a", "b", "c", "d", "e", "late"]);
        assert_eq!((lim.stats().active, lim.stats().waiting), (0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn never_more_than_concurrency_and_every_task_returns_its_value() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(3, 100, 5000, None);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = vec![];
        for i in 0..20u64 {
            let (lim, running, peak) = (lim.clone(), running.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                lim.run_blocking(&HashOpts::default(), move || {
                    let n = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1 + i % 3));
                    running.fetch_sub(1, Ordering::SeqCst);
                    i * 2
                })
                .await
            }));
        }
        let mut results = vec![];
        for t in tasks {
            results.push(t.await.unwrap().unwrap());
        }
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(results, (0..20).map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(lim.stats().active, 0);
    }

    #[tokio::test]
    async fn a_full_queue_refuses_at_once() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 2, 5000, None);
        let order = Arc::new(Mutex::new(Vec::new()));
        let before = rejected("queue_full");
        let (release, running) = spawn_holder(&lim, HashOpts::default(), "run", &order);
        tick().await;
        let w1 = spawn_quick(&lim, HashOpts::default(), "w1", &order);
        let w2 = spawn_quick(&lim, HashOpts::default(), "w2", &order);
        tick().await;
        let t0 = Instant::now();
        let err = lim.acquire(&HashOpts::default()).await.unwrap_err();
        assert_eq!(err.reason, BusyReason::QueueFull);
        assert!(t0.elapsed() < Duration::from_secs(1), "refused at once, not after the timeout");
        assert_eq!(rejected("queue_full"), before + 1);
        assert_eq!(lim.stats().waiting, 2, "the waiters keep their place");
        release.send(()).unwrap();
        for t in [running, w1, w2] {
            t.await.unwrap().unwrap();
        }
        assert_eq!(*order.lock(), ["run", "w1", "w2"]);

        // queue_max 0: no waiting at all.
        let none = limiter(1, 0, 5000, None);
        let p = none.acquire(&HashOpts::default()).await.unwrap();
        assert_eq!(none.acquire(&HashOpts::default()).await.unwrap_err().reason, BusyReason::QueueFull);
        drop(p);
        assert_eq!(none.run_blocking(&HashOpts::default(), || "free again").await.unwrap(), "free again");
    }

    #[tokio::test]
    async fn an_expired_wait_leaves_the_queue() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 5, 40, None);
        let before = rejected("timeout");
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let t0 = Instant::now();
        let err = lim.acquire(&HashOpts::default()).await.unwrap_err();
        assert_eq!(err.reason, BusyReason::Timeout);
        assert!(t0.elapsed() >= Duration::from_millis(35), "not before the timeout");
        assert_eq!(lim.stats().waiting, 0, "the expired waiter left the queue");
        assert_eq!(rejected("timeout"), before + 1);
        let order = Arc::new(Mutex::new(Vec::new()));
        let c = spawn_quick(&lim, HashOpts::default(), "c", &order);
        tick().await;
        drop(held);
        c.await.unwrap().unwrap();
        assert_eq!((lim.stats().active, lim.stats().waiting), (0, 0));
    }

    #[tokio::test]
    async fn a_task_that_panics_releases_its_slot() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 5, 5000, None);
        let l2 = lim.clone();
        let boom =
            tokio::spawn(async move { l2.run_blocking(&HashOpts::default(), || panic!("boom")).await });
        assert!(boom.await.unwrap_err().is_panic());
        assert_eq!(lim.run_blocking(&HashOpts::default(), || "still works").await.unwrap(), "still works");
        assert_eq!(lim.stats().active, 0);
    }

    #[tokio::test]
    async fn a_cancelled_waiter_leaves_the_queue_and_a_cancelled_grant_frees_its_slot() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 5, 5000, None);
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        // A request dropped while it waits leaves the queue.
        let l2 = lim.clone();
        let waiting = tokio::spawn(async move { l2.acquire(&src("A")).await.map(drop) });
        tick().await;
        assert_eq!((lim.stats().waiting, lim.waiting_from("A")), (1, 1));
        waiting.abort();
        let _ = waiting.await;
        assert_eq!((lim.stats().waiting, lim.waiting_from("A")), (0, 0));
        drop(held);
        assert_eq!(lim.stats().active, 0);
        // A slot handed to a waiter whose future is dropped before it runs is freed again.
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let opts = HashOpts::default();
        let mut fut = Box::pin(lim.acquire(&opts));
        assert!(futures_poll_once(fut.as_mut()).await.is_none());
        drop(held); // hands the slot to the pending future
        drop(fut); // which never runs
        assert_eq!((lim.stats().active, lim.stats().waiting), (0, 0));
        assert!(lim.acquire(&HashOpts { max_wait: Some(Duration::ZERO), source: None }).await.is_ok());
    }

    /// Polls a future once; `None` when it is pending.
    async fn futures_poll_once<F: std::future::Future + Unpin>(f: F) -> Option<F::Output> {
        let mut f = f;
        std::future::poll_fn(move |cx| match std::pin::Pin::new(&mut f).poll(cx) {
            std::task::Poll::Ready(v) => std::task::Poll::Ready(Some(v)),
            std::task::Poll::Pending => std::task::Poll::Ready(None),
        })
        .await
    }

    #[tokio::test]
    async fn metrics_in_flight_queued_wait_time_and_refusals() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 1, 5000, None);
        let (f0, q0, w0, r0) = (IN_FLIGHT.get(), QUEUED.get(), WAIT_MS.count(), rejected("queue_full"));
        let a = lim.acquire(&HashOpts::default()).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let b = spawn_quick(&lim, HashOpts::default(), "b", &order);
        tick().await;
        assert!(lim.acquire(&HashOpts::default()).await.is_err());
        assert_eq!(IN_FLIGHT.get(), f0 + 1.0);
        assert_eq!(QUEUED.get(), q0 + 1.0);
        assert_eq!(rejected("queue_full"), r0 + 1);
        drop(a);
        b.await.unwrap().unwrap();
        assert_eq!(IN_FLIGHT.get(), f0);
        assert_eq!(QUEUED.get(), q0);
        assert_eq!(WAIT_MS.count(), w0 + 2, "one wait observed per granted slot");
    }

    #[test]
    fn invalid_limits_are_refused() {
        let cfg = HashLimiterConfig::default();
        assert!(HashLimiter::new(HashLimiterConfig { concurrency: 0, ..cfg }).is_err());
        assert_eq!(
            HashLimiter::new(HashLimiterConfig { per_source_max: Some(0), ..cfg }).unwrap_err().to_string(),
            "perSourceMax must be an integer >= 1"
        );
        let from = HashLimiterConfig::from_config(&Config::for_tests());
        assert_eq!(
            from,
            HashLimiterConfig {
                concurrency: 1,
                queue_max: 32,
                queue_timeout: Duration::from_millis(10000),
                per_source_max: Some(2)
            }
        );
    }

    #[tokio::test]
    async fn max_wait_a_call_waits_at_most_its_own_budget() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 5, 5000, None);
        let before = rejected("timeout");
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let t0 = Instant::now();
        let err = lim
            .acquire(&HashOpts { max_wait: Some(Duration::from_millis(60)), source: None })
            .await
            .unwrap_err();
        let waited = t0.elapsed();
        assert_eq!(err.reason, BusyReason::Timeout);
        assert!(waited >= Duration::from_millis(55) && waited < Duration::from_secs(1), "{waited:?}");
        assert_eq!(rejected("timeout"), before + 1);
        assert_eq!(lim.stats().waiting, 0);
        // A budget above the queue timeout is capped by it.
        let short = limiter(1, 5, 40, None);
        let held2 = short.acquire(&HashOpts::default()).await.unwrap();
        let t1 = Instant::now();
        let err = short.acquire(&HashOpts { max_wait: Some(Duration::from_secs(60)), source: None }).await;
        assert_eq!(err.unwrap_err().reason, BusyReason::Timeout);
        assert!(t1.elapsed() < Duration::from_secs(1));
        drop((held, held2));
        // A granted call with a budget still runs normally.
        let opts = HashOpts { max_wait: Some(Duration::from_millis(60)), source: None };
        assert_eq!(lim.run_blocking(&opts, || "ran").await.unwrap(), "ran");
    }

    #[tokio::test]
    async fn max_wait_zero_runs_only_when_a_slot_is_free() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 5, 5000, None);
        let zero = HashOpts { max_wait: Some(Duration::ZERO), source: Some("x".into()) };
        assert_eq!(lim.run_blocking(&zero, || "free").await.unwrap(), "free");
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let counts = || ["queue_full", "timeout", "source_limit"].map(rejected);
        let (c0, q0) = (counts(), QUEUED.get());
        let t0 = Instant::now();
        let called = Arc::new(AtomicUsize::new(0));
        let c = called.clone();
        let err = lim.run_blocking(&zero, move || c.fetch_add(1, Ordering::SeqCst)).await.unwrap_err();
        assert_eq!(err.reason, BusyReason::NoWait);
        assert!(t0.elapsed() < Duration::from_secs(1), "refused at once");
        assert_eq!(called.load(Ordering::SeqCst), 0);
        assert_eq!(counts(), c0, "an optional task skipped is not a refused request");
        assert_eq!(QUEUED.get(), q0);
        assert_eq!((lim.stats().active, lim.stats().waiting, lim.waiting_from("x")), (1, 0, 0));
        drop(held);
    }

    #[tokio::test]
    async fn per_source_max_under_contention_keeps_fifo() {
        let _m = METRICS_LOCK.lock().await;
        // queue_max 6: the cap applies from 3 waiting tasks on (half the queue).
        let lim = limiter(1, 6, 5000, Some(2));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (s0, f0) = (rejected("source_limit"), rejected("queue_full"));
        // The running task does not count: only waiting ones do.
        let (release, running) = spawn_holder(&lim, src("A"), "run", &order);
        tick().await;
        let mut tasks = vec![];
        for (name, s) in [("a1", "A"), ("b1", "B"), ("a2", "A")] {
            tasks.push(spawn_quick(&lim, src(s), name, &order));
            tick().await;
        }
        assert_eq!(lim.acquire(&src("A")).await.unwrap_err().reason, BusyReason::SourceLimit);
        assert_eq!(rejected("source_limit"), s0 + 1);
        assert_eq!(rejected("queue_full"), f0);
        tasks.push(spawn_quick(&lim, HashOpts::default(), "n1", &order)); // no source: never limited
        tick().await;
        tasks.push(spawn_quick(&lim, HashOpts::default(), "n2", &order));
        tick().await;
        tasks.push(spawn_quick(&lim, src("B"), "b2", &order));
        tick().await;
        assert_eq!((lim.waiting_from("A"), lim.waiting_from("B"), lim.stats().waiting), (2, 2, 6));
        release.send(()).unwrap();
        running.await.unwrap().unwrap();
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(*order.lock(), ["run", "a1", "b1", "a2", "n1", "n2", "b2"]);
        assert_eq!(
            (lim.waiting_from("A"), lim.waiting_from("B")),
            (0, 0),
            "granted waiters are no longer counted"
        );

        // An expired waiter is no longer counted either.
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let budget = |s: &str| HashOpts { max_wait: Some(Duration::from_millis(20)), source: Some(s.into()) };
        let (c1, c2) = (budget("C"), budget("C"));
        let (e1, e2) = tokio::join!(lim.acquire(&c1), lim.acquire(&c2));
        assert_eq!(
            (e1.unwrap_err().reason, e2.unwrap_err().reason),
            (BusyReason::Timeout, BusyReason::Timeout)
        );
        assert_eq!(lim.waiting_from("C"), 0);
        let c3 = spawn_quick(&lim, src("C"), "c3", &order);
        tick().await;
        drop(held);
        c3.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn per_source_max_applies_only_from_half_full() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 8, 5000, Some(2));
        assert_eq!(lim.per_source_max(), Some(2));
        let order = Arc::new(Mutex::new(Vec::new()));
        let s0 = rejected("source_limit");
        let held = lim.acquire(&src("A")).await.unwrap();
        // An idle queue: one source (a classroom behind one address) queues 4 tasks, beyond its cap.
        let mut mine = vec![];
        for name in ["a1", "a2", "a3", "a4"] {
            mine.push(spawn_quick(&lim, src("A"), name, &order));
            tick().await;
        }
        assert_eq!((lim.waiting_from("A"), lim.stats().waiting), (4, 4));
        assert_eq!(rejected("source_limit"), s0);
        // Half full (4 of 8): the source is at its cap and refused, a new source is accepted.
        assert_eq!(lim.acquire(&src("A")).await.unwrap_err().reason, BusyReason::SourceLimit);
        assert_eq!(rejected("source_limit"), s0 + 1);
        mine.push(spawn_quick(&lim, src("B"), "b1", &order));
        tick().await;
        mine.push(spawn_quick(&lim, src("B"), "b2", &order));
        tick().await;
        assert_eq!(lim.acquire(&src("B")).await.unwrap_err().reason, BusyReason::SourceLimit);
        assert_eq!((lim.waiting_from("A"), lim.waiting_from("B"), lim.stats().waiting), (4, 2, 6));
        drop(held);
        for t in mine {
            t.await.unwrap().unwrap();
        }
        assert_eq!(*order.lock(), ["a1", "a2", "a3", "a4", "b1", "b2"]);

        // At exactly half full, a source with 2 waiting is refused while a new source is accepted.
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let mut fill = vec![];
        for s in ["X", "Y", "C", "C"] {
            fill.push(spawn_quick(&lim, src(s), "fill", &order));
            tick().await;
        }
        assert_eq!(lim.stats().waiting, 4);
        assert_eq!(lim.acquire(&src("C")).await.unwrap_err().reason, BusyReason::SourceLimit);
        fill.push(spawn_quick(&lim, src("D"), "d", &order));
        tick().await;
        assert_eq!(lim.stats().waiting, 5);
        // Just under half full (3 of 8), the same source would still have been accepted.
        let small = limiter(1, 8, 5000, Some(2));
        let held3 = small.acquire(&HashOpts::default()).await.unwrap();
        for s in ["X", "C", "C", "C"] {
            fill.push(spawn_quick(&small, src(s), "small", &order));
            tick().await;
        }
        assert_eq!(small.waiting_from("C"), 3);
        drop((held, held3));
        for t in fill {
            t.await.unwrap().unwrap();
        }
        // A queue of 0 or 1 is always contended: the cap never lets a task through that the queue
        // refuses.
        let one = limiter(1, 1, 5000, Some(1));
        let held4 = one.acquire(&HashOpts::default()).await.unwrap();
        let w4 = spawn_quick(&one, src("E"), "e", &order);
        tick().await;
        assert_eq!(one.acquire(&src("E")).await.unwrap_err().reason, BusyReason::SourceLimit);
        assert_eq!(one.acquire(&src("F")).await.unwrap_err().reason, BusyReason::QueueFull);
        drop(held4);
        w4.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn per_source_max_above_half_of_the_queue() {
        let _m = METRICS_LOCK.lock().await;
        let lim = limiter(1, 8, 5000, Some(6));
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = lim.acquire(&HashOpts::default()).await.unwrap();
        let mut tasks = vec![];
        for _ in 0..6 {
            tasks.push(spawn_quick(&lim, src("A"), "a", &order));
            tick().await;
        }
        assert_eq!(lim.waiting_from("A"), 6, "max(half of the queue, perSourceMax) = 6 of 8");
        assert_eq!(lim.acquire(&src("A")).await.unwrap_err().reason, BusyReason::SourceLimit);
        for s in ["B", "C"] {
            tasks.push(spawn_quick(&lim, src(s), "other", &order));
            tick().await;
        }
        assert_eq!(lim.acquire(&src("D")).await.unwrap_err().reason, BusyReason::QueueFull);
        drop(held);
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(order.lock().len(), 8);
    }

    #[test]
    fn busy_reasons_map_to_answers_and_messages() {
        assert_eq!(BusyReason::NoWait.answer(), BusyAnswer::Skip);
        assert_eq!(BusyReason::SourceLimit.answer(), BusyAnswer::RateLimited);
        assert_eq!(BusyReason::QueueFull.answer(), BusyAnswer::ServerBusy);
        assert_eq!(BusyReason::Timeout.answer(), BusyAnswer::ServerBusy);
        assert_eq!(
            PasswordBusy { reason: BusyReason::QueueFull }.to_string(),
            "password hashing: the queue is full"
        );
        assert_eq!(BusyReason::SourceLimit.as_str(), "source_limit");
        let s =
            LimiterStats { active: 1, waiting: 2, concurrency: 1, queue_max: 32, queue_timeout_ms: 10000 };
        assert_eq!(
            s.to_json().to_string(),
            r#"{"active":1,"waiting":2,"concurrency":1,"queueMax":32,"queueTimeoutMs":10000}"#
        );
    }
}
