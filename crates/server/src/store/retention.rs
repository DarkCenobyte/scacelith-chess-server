//! Retention purge (DESIGN 7): expired and revoked sessions, expired tokens and signups, old
//! security events, anomalies, conduct events and failed analysis jobs are deleted, and stored IP
//! addresses erased, in short chunked writer jobs; and the scheduler that runs it every
//! `RETENTION_INTERVAL_MS`.
//!
//! Each chunk is one statement in its own writer job, so that the purge never holds the write lock
//! for long and other jobs interleave. The chunk size adapts so that one statement takes about half
//! of a time slice (`slice_ms`, 10 ms), between 50 and 1000 rows, and the purge pauses for a slice
//! whenever a slice of work is done. IP addresses are erased before old rows are deleted: a
//! deletion makes SQLite rebalance pages, which copies the neighbouring rows and can leave stale
//! copies in unused page space where `secure_delete` does not reach.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::params;
use tokio::sync::{mpsc, oneshot};

use super::Store;
use super::error::{ErrorKind, StoreError};
use crate::config::Config;
use crate::log::Logger;
use crate::metrics::{self, Counter, CounterVec, Histogram};

const DAY: i64 = 86_400_000;

/// Rows per statement of [`RetentionApi::run`] (and the largest adaptive chunk).
const CHUNK: i64 = 1000;
/// First chunk of each step of [`RetentionApi::run_async`].
const CHUNK_START: i64 = 200;
/// Smallest adaptive chunk: every statement pays a fixed cost (the commit's fsync) that fewer rows
/// do not reduce.
const CHUNK_MIN: i64 = 50;

const REVOKED_SESSION_TTL_MS: i64 = DAY;
const CONDUCT_EVENT_TTL_MS: i64 = 30 * DAY;
const ANALYSIS_FAILED_TTL_MS: i64 = 30 * DAY;

/// How long personal data is kept (`RETENTION_SECURITY_DAYS`, `RETENTION_IP_DAYS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Security events and non-certain anomalies are deleted after this many days.
    pub security_days: i64,
    /// Stored IP addresses are erased after this many days.
    pub ip_days: i64,
}

impl RetentionPolicy {
    /// The policy of the configuration.
    pub fn from_config(config: &Config) -> RetentionPolicy {
        RetentionPolicy { security_days: config.retention_security_days, ip_days: config.retention_ip_days }
    }
}

/// What a purge removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeCounts {
    /// Expired, idle-expired and revoked sessions.
    pub sessions: u64,
    /// Expired single-use tokens and pending signups.
    pub tokens: u64,
    pub security_events: u64,
    pub anomalies: u64,
    pub conduct_events: u64,
    /// Failed analysis jobs.
    pub analysis_jobs: u64,
    /// IP addresses erased (sessions and security events).
    pub ip_erased: u64,
}

/// A failed purge: the error and what was removed before it.
#[derive(Debug)]
pub struct RetentionError {
    pub error: StoreError,
    pub counts: PurgeCounts,
}

impl std::fmt::Display for RetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for RetentionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// What [`RetentionApi::purge_security`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecurityPurge {
    pub deleted: u64,
    pub ip_erased: u64,
}

/// Asks a purge to stop before its next statement. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct AbortFlag(Arc<AtomicBool>);

impl AbortFlag {
    /// A flag not raised yet.
    pub fn new() -> AbortFlag {
        AbortFlag::default()
    }

    /// Raises the flag.
    pub fn abort(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the flag is raised.
    pub fn is_aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A boxed future of the purge's hooks.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The pause between two slices (default: a timer of `slice_ms`).
pub type PauseFn = Arc<dyn Fn() -> BoxFuture<()> + Send + Sync>;

/// A milliseconds clock for the slice accounting (default: monotonic time).
pub type MsClock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Options of [`RetentionApi::run_async`].
#[derive(Clone)]
pub struct PurgeOptions {
    /// Work done before a pause of the same length; a statement aims at half of it.
    pub slice_ms: f64,
    /// Stops the purge before its next statement.
    pub abort: Option<AbortFlag>,
    /// Replaces the pause (tests).
    pub pause: Option<PauseFn>,
    /// A fixed number of rows per statement instead of the adaptive one.
    pub chunk: Option<i64>,
    /// Replaces the clock of the slice accounting (tests). Called on the writer thread right
    /// after each statement's commit, and on the caller's task.
    pub clock: Option<MsClock>,
}

impl Default for PurgeOptions {
    fn default() -> PurgeOptions {
        PurgeOptions { slice_ms: 10.0, abort: None, pause: None, chunk: None, clock: None }
    }
}

impl std::fmt::Debug for PurgeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PurgeOptions")
            .field("slice_ms", &self.slice_ms)
            .field("abort", &self.abort)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

/// The count a step adds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    Sessions,
    Tokens,
    SecurityEvents,
    Anomalies,
    ConductEvents,
    AnalysisJobs,
    IpErased,
}

impl PurgeCounts {
    fn add(&mut self, count: Count, n: u64) {
        let slot = match count {
            Count::Sessions => &mut self.sessions,
            Count::Tokens => &mut self.tokens,
            Count::SecurityEvents => &mut self.security_events,
            Count::Anomalies => &mut self.anomalies,
            Count::ConductEvents => &mut self.conduct_events,
            Count::AnalysisJobs => &mut self.analysis_jobs,
            Count::IpErased => &mut self.ip_erased,
        };
        *slot += n;
    }
}

/// One chunked statement: `?1` its cutoff, `?2` its LIMIT.
#[derive(Debug, Clone, Copy)]
struct Step {
    count: Count,
    cutoff: i64,
    sql: &'static str,
}

const SECURITY_IP_SQL: &str =
    "UPDATE security_events SET ip = NULL WHERE id IN (SELECT id FROM security_events
    WHERE ip IS NOT NULL AND at < ?1 LIMIT ?2)";
const SECURITY_DELETE_SQL: &str =
    "DELETE FROM security_events WHERE id IN (SELECT id FROM security_events WHERE at < ?1 LIMIT ?2)";

/// The purge's statements, in order.
fn steps(t: i64, policy: RetentionPolicy) -> [Step; 10] {
    let ip_before = t - policy.ip_days * DAY;
    let security_before = t - policy.security_days * DAY;
    [
        Step {
            count: Count::IpErased,
            cutoff: ip_before,
            sql: "UPDATE sessions SET ip = NULL WHERE id IN (SELECT id FROM sessions
                  WHERE ip IS NOT NULL AND created_at < ?1 LIMIT ?2)",
        },
        Step { count: Count::IpErased, cutoff: ip_before, sql: SECURITY_IP_SQL },
        Step {
            count: Count::Sessions,
            cutoff: t,
            sql: "DELETE FROM sessions WHERE id IN (SELECT id FROM sessions
                  WHERE min(expires_at, idle_expires_at) <= ?1 LIMIT ?2)",
        },
        Step {
            count: Count::Sessions,
            cutoff: t - REVOKED_SESSION_TTL_MS,
            sql: "DELETE FROM sessions WHERE id IN (SELECT id FROM sessions
                  WHERE revoked_at IS NOT NULL AND revoked_at <= ?1 LIMIT ?2)",
        },
        Step {
            count: Count::Tokens,
            cutoff: t,
            sql: "DELETE FROM tokens WHERE id IN (SELECT id FROM tokens WHERE expires_at <= ?1 LIMIT ?2)",
        },
        // Expired pending signups count with the tokens (their link expired with them).
        Step {
            count: Count::Tokens,
            cutoff: t,
            sql: "DELETE FROM pending_signups WHERE id IN (SELECT id FROM pending_signups
                  WHERE expires_at <= ?1 LIMIT ?2)",
        },
        Step { count: Count::SecurityEvents, cutoff: security_before, sql: SECURITY_DELETE_SQL },
        Step {
            count: Count::Anomalies,
            cutoff: security_before,
            sql: "DELETE FROM anomalies WHERE id IN (SELECT id FROM anomalies
                  WHERE severity <> 'certain' AND at < ?1 LIMIT ?2)",
        },
        Step {
            count: Count::ConductEvents,
            cutoff: t - CONDUCT_EVENT_TTL_MS,
            sql: "DELETE FROM conduct_events WHERE id IN (SELECT id FROM conduct_events WHERE at < ?1 LIMIT ?2)",
        },
        // Done jobs are kept: their features are the players' analysed history.
        Step {
            count: Count::AnalysisJobs,
            cutoff: t - ANALYSIS_FAILED_TTL_MS,
            sql: "DELETE FROM analysis_jobs WHERE game_id IN (SELECT game_id FROM analysis_jobs
                  WHERE status = 'failed' AND finished_at < ?1 LIMIT ?2)",
        },
    ]
}

/// The LIMIT of the next statement of a step, from the one just run (`rows` touched in `took`
/// ms): shrunk in proportion when it took longer than `target`, doubled (up to 1000) when a full
/// chunk took less than half of it.
pub fn next_chunk(limit: i64, rows: i64, took: f64, target: f64) -> i64 {
    if took > target {
        return CHUNK_MIN.max(limit.min((limit as f64 * target / took).floor() as i64));
    }
    if rows >= limit && took < target / 2.0 {
        return CHUNK.min(limit * 2);
    }
    limit
}

fn default_clock() -> MsClock {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    Arc::new(|| START.elapsed().as_secs_f64() * 1000.0)
}

/// The retention purge of a store (see the module documentation).
#[derive(Clone)]
pub struct RetentionApi {
    pub(crate) store: Store,
}

impl std::fmt::Debug for RetentionApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetentionApi").finish_non_exhaustive()
    }
}

impl RetentionApi {
    /// The whole purge in chunks of 1000 rows, without pauses.
    pub async fn run(&self, now: i64, policy: RetentionPolicy) -> Result<PurgeCounts, RetentionError> {
        let opts = PurgeOptions { slice_ms: f64::INFINITY, chunk: Some(CHUNK), ..PurgeOptions::default() };
        self.run_steps(&steps(now, policy), opts).await
    }

    /// The purge in adaptive chunks and slices (module documentation). Stops before the next
    /// statement once `opts.abort` is raised or the store is closed (what was deleted stays
    /// deleted) and returns the counts so far. A failure returns the error with the counts done
    /// before it.
    pub async fn run_async(
        &self,
        now: i64,
        policy: RetentionPolicy,
        opts: PurgeOptions,
    ) -> Result<PurgeCounts, RetentionError> {
        self.run_steps(&steps(now, policy), opts).await
    }

    /// Erases the IP addresses of the security events older than `ip_days`, then deletes those
    /// older than `security_days` (chunks of 1000 rows).
    pub async fn purge_security(
        &self,
        now: i64,
        policy: RetentionPolicy,
    ) -> Result<SecurityPurge, RetentionError> {
        let all = steps(now, policy);
        let opts = PurgeOptions { slice_ms: f64::INFINITY, chunk: Some(CHUNK), ..PurgeOptions::default() };
        let counts = self.run_steps(&[all[1], all[6]], opts).await?;
        Ok(SecurityPurge { deleted: counts.security_events, ip_erased: counts.ip_erased })
    }

    async fn run_steps(&self, steps: &[Step], opts: PurgeOptions) -> Result<PurgeCounts, RetentionError> {
        let mut counts = PurgeCounts::default();
        let clock = opts.clock.clone().unwrap_or_else(default_clock);
        let fixed = opts.chunk.map(|c| c.max(1));
        let slice_ms = opts.slice_ms;
        let mut slice_start = clock();
        for step in steps {
            let mut limit = fixed.unwrap_or(CHUNK_START);
            loop {
                if self.store.is_closed() || opts.abort.as_ref().is_some_and(AbortFlag::is_aborted) {
                    return Ok(counts);
                }
                let used = limit;
                let (n, t0, t1) = match self.chunk(*step, used, clock.clone()).await {
                    Ok(r) => r,
                    Err(error) => return Err(RetentionError { error, counts }),
                };
                counts.add(step.count, n as u64);
                if fixed.is_none() {
                    limit = next_chunk(used, n, t1 - t0, slice_ms / 2.0);
                }
                if t1 - slice_start >= slice_ms {
                    match &opts.pause {
                        Some(pause) => pause().await,
                        None => tokio::time::sleep(Duration::from_secs_f64(slice_ms.max(0.0) / 1000.0)).await,
                    }
                    slice_start = clock();
                }
                if n < used {
                    break;
                }
            }
        }
        Ok(counts)
    }

    /// One statement in its own writer job: rows touched, and the clock before the statement and
    /// after its commit.
    async fn chunk(&self, step: Step, limit: i64, clock: MsClock) -> Result<(i64, f64, f64), StoreError> {
        let Some(writer) = self.store.writer() else {
            return Err(StoreError::new(ErrorKind::ReadOnly, "read-only store"));
        };
        writer
            .submit(move |w| {
                let t0 = clock();
                let (n, _) = w.transact(|db| db.exec(step.sql, params![step.cutoff, limit]));
                let t1 = clock();
                Ok((n? as i64, t0, t1))
            })
            .await
    }
}

static PURGED: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec("scacelith_retention_purged_total", "Rows deleted by the retention purge", &["kind"])
});
static IP_ERASED: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter(
        "scacelith_retention_ip_erased_total",
        "Stored IP addresses erased by the retention purge",
    )
});
static RUNS: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec("scacelith_retention_runs_total", "Retention purge runs, by result", &["result"])
});
static RUN_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram(
        "scacelith_retention_run_seconds",
        "Duration of one retention purge (pauses included)",
        &[0.1, 1.0, 10.0, 60.0, 600.0, 3600.0],
    )
});

fn count_metrics(c: &PurgeCounts) {
    let kinds = [
        ("sessions", c.sessions),
        ("tokens", c.tokens),
        ("security_events", c.security_events),
        ("anomalies", c.anomalies),
        ("conduct_events", c.conduct_events),
        ("analysis_jobs", c.analysis_jobs),
    ];
    for (kind, n) in kinds {
        if n > 0 {
            PURGED.with(&[kind]).add(n);
        }
    }
    if c.ip_erased > 0 {
        IP_ERASED.add(c.ip_erased);
    }
}

/// Log fields of a run: its counts, `tokens` renamed (the logger redacts any field whose name looks
/// like a credential, and a count of deleted single-use tokens is not one).
fn log_fields(c: &PurgeCounts, ms: Option<u64>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "sessions": c.sessions,
        "securityEvents": c.security_events,
        "anomalies": c.anomalies,
        "conductEvents": c.conduct_events,
        "analysisJobs": c.analysis_jobs,
        "ipErased": c.ip_erased,
        "singleUse": c.tokens,
    });
    if let (Some(ms), Some(map)) = (ms, v.as_object_mut()) {
        map.insert("ms".into(), ms.into());
    }
    v
}

/// One run of the purge, as the scheduler calls it: `(now, abort)` to its outcome.
pub type PurgeFn =
    Arc<dyn Fn(i64, AbortFlag) -> BoxFuture<Result<PurgeCounts, RetentionError>> + Send + Sync>;

/// Options of [`RetentionScheduler::start`].
#[derive(Clone)]
pub struct SchedulerOptions {
    /// Delay of the first run (60 s).
    pub first_delay: Duration,
    /// Delay between the end of a run and the start of the next (`RETENTION_INTERVAL_MS`).
    pub interval: Duration,
    /// Work done before a pause of the same length (10 ms).
    pub slice_ms: f64,
    /// The wall clock giving each run's `now`.
    pub clock: crate::clock::SharedClock,
    pub logger: Logger,
}

impl SchedulerOptions {
    /// The options of the configuration.
    pub fn from_config(config: &Config) -> SchedulerOptions {
        SchedulerOptions {
            first_delay: Duration::from_secs(60),
            interval: Duration::from_millis(config.retention_interval_ms.clamp(0, 2_147_483_647) as u64),
            slice_ms: 10.0,
            clock: crate::clock::system(),
            logger: Logger::root().child("retention"),
        }
    }
}

enum Request {
    RunNow(oneshot::Sender<Option<PurgeCounts>>),
}

struct SchedulerShared {
    running: AtomicBool,
    stopped: AtomicBool,
    abort: Mutex<Option<AbortFlag>>,
}

/// Runs the purge every interval (the first time after `first_delay`), one run at a time: the next
/// run is scheduled when the previous one ends. Each run is logged with its counts only (no
/// personal data) and counted in the metrics; a failure is logged (warn when the database was
/// busy) and retried at the next interval.
pub struct RetentionScheduler {
    shared: Arc<SchedulerShared>,
    tx: Mutex<Option<mpsc::UnboundedSender<Request>>>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for RetentionScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetentionScheduler").field("running", &self.running()).finish_non_exhaustive()
    }
}

impl RetentionScheduler {
    /// Schedules the purge of `store` with the policy of `config`.
    pub fn for_store(store: &Store, config: &Config) -> RetentionScheduler {
        let api = store.retention();
        let policy = RetentionPolicy::from_config(config);
        let opts = SchedulerOptions::from_config(config);
        let slice_ms = opts.slice_ms;
        let purge: PurgeFn = Arc::new(move |now, abort| {
            let api = api.clone();
            Box::pin(async move {
                api.run_async(
                    now,
                    policy,
                    PurgeOptions { slice_ms, abort: Some(abort), ..PurgeOptions::default() },
                )
                .await
            })
        });
        RetentionScheduler::start(purge, opts)
    }

    /// Starts the schedule of `purge` (must be called within a tokio runtime).
    pub fn start(purge: PurgeFn, opts: SchedulerOptions) -> RetentionScheduler {
        let shared = Arc::new(SchedulerShared {
            running: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            abort: Mutex::new(None),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(schedule(purge, opts, shared.clone(), rx));
        RetentionScheduler { shared, tx: Mutex::new(Some(tx)), task: tokio::sync::Mutex::new(Some(task)) }
    }

    /// Whether a run is in progress.
    pub fn running(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst)
    }

    /// Runs a purge now, after the one in progress if any; its counts, `None` after a failure or
    /// once stopped.
    pub async fn run_now(&self) -> Option<PurgeCounts> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.lock().as_ref().is_some_and(|tx| tx.send(Request::RunNow(reply)).is_ok());
        if !sent {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Cancels the next run and aborts the current one between two statements; returns once it
    /// has ended (the store can be closed right after).
    pub async fn stop(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(abort) = self.shared.abort.lock().as_ref() {
            abort.abort();
        }
        drop(self.tx.lock().take());
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
}

async fn schedule(
    purge: PurgeFn,
    opts: SchedulerOptions,
    shared: Arc<SchedulerShared>,
    mut rx: mpsc::UnboundedReceiver<Request>,
) {
    let mut next = tokio::time::Instant::now() + opts.first_delay;
    loop {
        let reply = tokio::select! {
            _ = tokio::time::sleep_until(next) => None,
            req = rx.recv() => match req {
                Some(Request::RunNow(reply)) => Some(reply),
                None => return,
            },
        };
        if shared.stopped.load(Ordering::SeqCst) {
            if let Some(reply) = reply {
                let _ = reply.send(None);
            }
            return;
        }
        let counts = run_once(&purge, &opts, &shared).await;
        if let Some(reply) = reply {
            let _ = reply.send(counts);
        }
        // The timer runs again `interval` after this run ended; a timer that fired during a run
        // started by run_now does not start another.
        let now = tokio::time::Instant::now();
        if next <= now {
            next = now + opts.interval;
        }
    }
}

/// One run; never fails (a failure is logged, and the next run happens at the next interval).
async fn run_once(purge: &PurgeFn, opts: &SchedulerOptions, shared: &SchedulerShared) -> Option<PurgeCounts> {
    let abort = AbortFlag::new();
    *shared.abort.lock() = Some(abort.clone());
    shared.running.store(true, Ordering::SeqCst);
    let t0 = Instant::now();
    let out = purge(opts.clock.wall_ms(), abort.clone()).await;
    let elapsed = t0.elapsed();
    let ms = elapsed.as_millis() as u64;
    let logger = &opts.logger;
    let result = match out {
        Ok(counts) => {
            count_metrics(&counts);
            if abort.is_aborted() {
                RUNS.with(&["aborted"]).inc();
                logger.emit(
                    crate::log::Level::Info,
                    "retention purge interrupted by the shutdown",
                    Some(log_fields(&counts, Some(ms))),
                );
            } else {
                RUNS.with(&["ok"]).inc();
                if logger.enabled(crate::log::Level::Info) {
                    logger.emit(
                        crate::log::Level::Info,
                        "retention purge done",
                        Some(log_fields(&counts, Some(ms))),
                    );
                }
            }
            Some(counts)
        }
        Err(e) => {
            count_metrics(&e.counts);
            RUNS.with(&["failed"]).inc();
            let mut fields = log_fields(&e.counts, None);
            if let Some(map) = fields.as_object_mut() {
                map.insert("err".into(), e.error.to_string().into());
            }
            let level = if e.error.kind() == ErrorKind::Busy {
                crate::log::Level::Warn
            } else {
                crate::log::Level::Error
            };
            logger.emit(level, "retention purge failed; retried at the next interval", Some(fields));
            None
        }
    };
    RUN_SECONDS.observe(elapsed.as_secs_f64());
    shared.running.store(false, Ordering::SeqCst);
    *shared.abort.lock() = None;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_adaptation() {
        assert_eq!(next_chunk(200, 200, 20.0, 5.0), 50);
        assert_eq!(next_chunk(200, 200, 6.0, 5.0), 166);
        assert_eq!(next_chunk(200, 200, 1.0, 5.0), 400);
        assert_eq!(next_chunk(800, 800, 1.0, 5.0), 1000);
        assert_eq!(next_chunk(200, 150, 1.0, 5.0), 200);
        assert_eq!(next_chunk(200, 200, 3.0, 5.0), 200);
        assert_eq!(next_chunk(60, 60, 1000.0, 5.0), 50);
    }

    #[test]
    fn log_fields_rename_tokens() {
        let c = PurgeCounts { tokens: 2, sessions: 3, ..PurgeCounts::default() };
        let v = log_fields(&c, Some(7));
        assert_eq!(v["singleUse"], 2);
        assert_eq!(v["sessions"], 3);
        assert_eq!(v["ms"], 7);
        assert!(v.get("tokens").is_none());
    }
}
