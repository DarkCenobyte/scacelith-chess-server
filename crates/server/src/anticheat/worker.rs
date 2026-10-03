//! The analysis pool: `ANALYSIS_WORKERS` loops, each driving one UCI engine (Stockfish at nice 19,
//! one thread), claim finished rated games from the store's analysis queue, analyse them
//! ([`analyse_game`]), store the features, update both players' integrity level and, for the games
//! of the ordinary random sample only, the population statistics.
//!
//! Everything statistical is per analysis profile: a game joins, and its players are judged
//! against, the population of the profile it was analysed with. A loop never claims a job without
//! a working engine (a wrong `ANALYSIS_ENGINE_PATH` must not mark the whole queue failed); an
//! engine that cannot start, or dies, is started again after a delay of 1 s doubling up to 60 s,
//! back to 1 s once it ran for 60 s. Every engine start is logged with where the engine keeps its
//! network: Stockfish 19 and later share one copy between the engines, and one that falls back to
//! a copy of its own while several engines run is a warning.
//!
//! The engines run as child processes; the loops are tokio tasks that only wait on them and on
//! the store (the scoring runs in store read jobs, on the reader threads).

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::analysis::analyzer::{AnalysisEngine, Depths, GameRecord, SideContext, analyse_game};
use super::analysis::engine::{EngineError, EngineOptions, NetworkMemory, SHARED_NETWORK_HINT, UciEngine};
use super::players::{add_game_to_population, apply_player_score, score_player_games};
use super::scoring::Population;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::metrics::{self, Counter, Gauge, Histogram};
use crate::store::{ClaimedJob, Priority, Store, StoreError};
use crate::util::js;
use crate::{log_error, log_info, log_security, log_warn};

/// How often a job being analysed is renewed: well within the 10 minutes after which the store
/// gives the job of a vanished worker to another one.
pub const HEARTBEAT: Duration = Duration::from_secs(60);
/// First delay before an engine is started again.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// Longest delay before an engine is started again.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// An engine that ran this long starts again after [`MIN_BACKOFF`].
pub const STABLE_UPTIME: Duration = Duration::from_secs(60);
/// Longest stored error of a failed job, in UTF-16 units.
const ERROR_MAX: usize = 500;

/// What the pool needs from an engine on top of [`AnalysisEngine`] (implemented by
/// [`UciEngine`]; tests use scripted engines).
pub trait PoolEngine: AnalysisEngine + Send + 'static {
    /// Starts the engine if it is not running (a no-op when it is).
    fn start(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;
    /// Stops the engine for good.
    fn close(&mut self) -> impl Future<Output = ()> + Send;
    /// Whether the engine runs and is usable.
    fn is_alive(&mut self) -> bool;
    /// Starts so far (a new value means a new process to report).
    fn starts(&self) -> u64;
    /// Where the engine keeps its network.
    fn net_memory(&self) -> Option<NetworkMemory>;
    /// Why the network is not shared, as the engine says.
    fn net_memory_error(&self) -> Option<&str>;
    /// The engine's process id.
    fn pid(&self) -> Option<u32>;
}

impl PoolEngine for UciEngine {
    fn start(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        UciEngine::start(self)
    }

    fn close(&mut self) -> impl Future<Output = ()> + Send {
        UciEngine::close(self)
    }

    fn is_alive(&mut self) -> bool {
        UciEngine::is_alive(self)
    }

    fn starts(&self) -> u64 {
        UciEngine::starts(self)
    }

    fn net_memory(&self) -> Option<NetworkMemory> {
        UciEngine::net_memory(self)
    }

    fn net_memory_error(&self) -> Option<&str> {
        UciEngine::net_memory_error(self)
    }

    fn pid(&self) -> Option<u32> {
        UciEngine::pid(self)
    }
}

/// The settings of the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolSettings {
    /// Engines (and loops), at least 1.
    pub workers: usize,
    pub depths: Depths,
    /// Wait between two looks at an empty queue (`ANALYSIS_POLL_MS`).
    pub poll: Duration,
    pub heartbeat: Duration,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    pub stable_uptime: Duration,
    /// Claim owner (`<hostname>:<pid>`).
    pub worker_id: String,
}

impl PoolSettings {
    /// The settings of a configuration (`ANALYSIS_WORKERS`, depths, `ANALYSIS_POLL_MS`).
    pub fn from_config(config: &Config) -> PoolSettings {
        let positive = |v: i64, default: u64| if v > 0 { v as u64 } else { default };
        PoolSettings {
            workers: config.analysis_workers.max(1) as usize,
            depths: Depths {
                fast: config.analysis_depth_fast.clamp(1, i64::from(u32::MAX)) as u32,
                deep: config.analysis_depth_deep.clamp(1, i64::from(u32::MAX)) as u32,
            },
            poll: Duration::from_millis(positive(config.analysis_poll_ms, 5000)),
            heartbeat: HEARTBEAT,
            min_backoff: MIN_BACKOFF,
            max_backoff: MAX_BACKOFF,
            stable_uptime: STABLE_UPTIME,
            worker_id: format!("{}:{}", crate::sys::hostname(), std::process::id()),
        }
    }
}

/// The engine options of a configuration: `ANALYSIS_ENGINE_PATH`, one thread,
/// `ANALYSIS_HASH_MB`, `ANALYSIS_POSITION_TIMEOUT_MS` per search, nice 19.
pub fn engine_options(config: &Config, logger: &Logger) -> EngineOptions {
    let positive = |v: i64, default: u64| if v > 0 { v as u64 } else { default };
    EngineOptions {
        threads: 1,
        hash_mb: positive(config.analysis_hash_mb, 32).min(u64::from(u32::MAX)) as u32,
        timeout: Duration::from_millis(positive(config.analysis_position_timeout_ms, 60_000)),
        low_priority: true,
        log: Some(logger.clone()),
        ..EngineOptions::new(&config.analysis_engine_path)
    }
}

struct Metrics {
    ok: Counter,
    failed: Counter,
    seconds: Histogram,
    engines: Gauge,
    shared: Gauge,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let games = metrics::counter_vec(
        "scacelith_anticheat_analysis_games_total",
        "Games analysed by the engine",
        &["result"],
    );
    Metrics {
        ok: games.with(&["ok"]),
        failed: games.with(&["failed"]),
        seconds: metrics::histogram(
            "scacelith_anticheat_analysis_seconds",
            "Engine analysis time per game",
            &[5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0],
        ),
        engines: metrics::gauge("scacelith_anticheat_analysis_engines", "Analysis engine processes running"),
        shared: metrics::gauge(
            "scacelith_anticheat_analysis_engines_shared",
            "Analysis engines whose network is in memory shared with the other engines (Stockfish 19 and later)",
        ),
    }
});

/// One loop's share of the engine gauges.
#[derive(Default)]
struct EngineGauge {
    alive: bool,
    shared: bool,
}

impl EngineGauge {
    fn set(&mut self, alive: bool, shared: bool) {
        let shared = alive && shared;
        if alive != self.alive {
            METRICS.engines.add(if alive { 1.0 } else { -1.0 });
            self.alive = alive;
        }
        if shared != self.shared {
            METRICS.shared.add(if shared { 1.0 } else { -1.0 });
            self.shared = shared;
        }
    }

    fn refresh<E: PoolEngine>(&mut self, engine: &mut E) {
        let alive = engine.is_alive();
        self.set(alive, engine.net_memory() == Some(NetworkMemory::Shared));
    }
}

impl Drop for EngineGauge {
    fn drop(&mut self) {
        self.set(false, false);
    }
}

/// Counters of a pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub analysed: u64,
    pub failed: u64,
    /// Jobs being analysed now.
    pub running: u64,
}

#[derive(Default)]
struct Counters {
    analysed: AtomicU64,
    failed: AtomicU64,
    running: AtomicU64,
}

/// What the loops share.
struct Shared {
    store: Store,
    clock: SharedClock,
    logger: Logger,
    settings: PoolSettings,
    /// Analysis profile -> its population.
    populations: Mutex<HashMap<String, Arc<Population>>>,
    counters: Counters,
    stop: watch::Receiver<bool>,
}

/// Why a job was not analysed.
enum JobError {
    /// The pool is stopping: the job stays claimed and is analysed again later.
    Interrupted,
    Engine(EngineError),
    Store(StoreError),
    Missing,
}

impl JobError {
    fn message(&self) -> String {
        match self {
            JobError::Interrupted => "interrupted".into(),
            JobError::Engine(e) => format!("engine {}: {}", e.code(), e.message()),
            JobError::Store(e) => e.to_string(),
            JobError::Missing => "game not found".into(),
        }
    }
}

impl From<StoreError> for JobError {
    fn from(e: StoreError) -> JobError {
        JobError::Store(e)
    }
}

impl Shared {
    fn new(settings: PoolSettings, store: Store, clock: SharedClock, stop: watch::Receiver<bool>) -> Shared {
        Shared {
            store,
            clock,
            logger: Logger::root().child("analysis"),
            settings,
            populations: Mutex::new(HashMap::new()),
            counters: Counters::default(),
            stop,
        }
    }

    fn stopping(&self) -> bool {
        *self.stop.borrow()
    }

    /// Waits `d`, or less when the pool stops.
    async fn sleep(&self, d: Duration) {
        let mut stop = self.stop.clone();
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = stop.wait_for(|s| *s) => {}
        }
    }

    fn population(&self, profile: &str) -> Arc<Population> {
        let mut populations = self.populations.lock();
        if let Some(p) = populations.get(profile) {
            return p.clone();
        }
        log_info!(self.logger, "analysis profile", { "profile": profile });
        let p = Arc::new(Population::new(Some(profile.to_string()), self.clock.clone()));
        populations.insert(profile.to_string(), p.clone());
        p
    }

    /// The evidence behind the player's rating in the category, which sets the width of the
    /// rating band of the scoring: 0 while unrated, the counted games once rated (`None` when the
    /// record cannot be read).
    async fn rating_games(&self, user: UserId, category: &str) -> Option<i64> {
        let r = self.store.ratings().get(user, category.to_string()).await.ok()?;
        Some(if r.rated { r.counted_games } else { 0 })
    }

    /// Recomputes a player's integrity level after a new analysed game and stores it: scored on
    /// a reader, the memory rules applied to the record as it is at the write.
    async fn update_player(&self, user: UserId, population: &Arc<Population>) -> Result<(), StoreError> {
        let pop = population.clone();
        let (games, result) =
            self.store.read(move |db| Ok::<_, StoreError>(score_player_games(db, user, &pop))).await?;
        let now = self.clock.wall_ms();
        let pop = population.clone();
        let groups = result.groups.to_json();
        let score = result.score;
        let update =
            self.store.write(move |db| apply_player_score(db, user, &pop, &games, &result, now)).await?;
        if update.changed() {
            log_security!(self.logger, "integrity.level", { "userId": user, "from": update.previous.as_str(),
                "to": update.level.as_str(), "score": crate::anticheat::num::json_num(score), "groups": groups });
        }
        Ok(())
    }

    /// Analyses one claimed job end to end; returns the stored features, `None` when the job
    /// failed or was interrupted.
    async fn process_job<E: PoolEngine>(&self, engine: &mut E, job: &ClaimedJob) -> Option<Value> {
        let game_id = job.game_id;
        let started = self.clock.wall_ms();
        let heartbeat = self.heartbeat(game_id);
        let result = self.analyse_job(engine, job).await;
        heartbeat.abort();
        let now = self.clock.wall_ms();
        match result {
            Ok((features, plies, white_n, black_n)) => {
                self.counters.analysed.fetch_add(1, Ordering::Relaxed);
                METRICS.ok.inc();
                METRICS.seconds.observe((now - started) as f64 / 1000.0);
                log_info!(self.logger, "game analysed", { "gameId": game_id, "plies": plies, "white": white_n,
                    "black": black_n, "ms": now - started });
                Some(features)
            }
            Err(JobError::Interrupted) => {
                log_info!(self.logger, "analysis interrupted by shutdown", { "gameId": game_id });
                None
            }
            Err(e) => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                METRICS.failed.inc();
                let msg = e.message();
                log_warn!(self.logger, "analysis failed", { "gameId": game_id, "err": msg });
                let error = js::truncate_utf16(&msg, ERROR_MAX).to_string();
                if let Err(e2) = self.store.analysis().fail(game_id, Some(error), now).await {
                    log_error!(self.logger, "cannot mark the job failed", { "err": crate::log::error(&e2), "gameId": game_id });
                }
                None
            }
        }
    }

    /// Renews the job's claim every heartbeat while it is analysed.
    fn heartbeat(&self, game_id: GameId) -> tokio::task::AbortHandle {
        let (store, clock, logger) = (self.store.clone(), self.clock.clone(), self.logger.clone());
        let (every, worker) = (self.settings.heartbeat, self.settings.worker_id.clone());
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(Instant::now() + every, every);
            loop {
                interval.tick().await;
                if let Err(e) = store.analysis().touch(game_id, Some(worker.clone()), clock.wall_ms()).await {
                    log_warn!(logger, "analysis heartbeat not stored", { "err": crate::log::error(&e), "gameId": game_id });
                }
            }
        })
        .abort_handle()
    }

    async fn analyse_job<E: PoolEngine>(
        &self,
        engine: &mut E,
        job: &ClaimedJob,
    ) -> Result<(Value, usize, u32, u32), JobError> {
        let game_id = job.game_id;
        let game = self.store.games().by_id(game_id).await?.ok_or(JobError::Missing)?;
        let s = &game.summary;
        let record = GameRecord {
            id: s.id,
            category: s.category.clone(),
            rated: s.rated,
            base_ms: s.base_ms,
            inc_ms: s.inc_ms,
            white_id: s.white_id,
            black_id: s.black_id,
            white_rating: s.white_rating,
            black_rating: s.black_rating,
            ended_at: Some(s.ended_at),
            moves: game.moves,
            spent_ms: game.spent_ms,
        };
        let context = [
            SideContext { rating_games: self.rating_games(record.white_id, &record.category).await },
            SideContext { rating_games: self.rating_games(record.black_id, &record.category).await },
        ];
        let mut stop = self.stop.clone();
        let analysed = tokio::select! {
            r = analyse_game(engine, &record, self.settings.depths, context, &*self.clock) => r,
            _ = stop.wait_for(|s| *s) => return Err(JobError::Interrupted),
        };
        let features = analysed.map_err(JobError::Engine)?;
        let json = features.to_json();
        let now = self.clock.wall_ms();
        self.store.analysis().complete(game_id, Some(json.clone()), now).await?;

        // Score the players first (their new game is judged against the population as it was),
        // then let the game join the population, but only a game claimed at ordinary priority:
        // those are the random sample of the rated games. Flagged, reported and requested games
        // are analysed first and in full, so counting them would shift the baseline towards the
        // suspects it is meant to judge.
        let population = self.population(&features.profile);
        for user in [record.white_id, record.black_id] {
            if user == 0 {
                continue;
            }
            if let Err(e) = self.update_player(user, &population).await {
                log_error!(self.logger, "integrity update failed", { "err": crate::log::error(&e), "userId": user });
            }
        }
        if job.priority == Priority::Ordinary {
            // The job is done: a failed write loses this sample, it does not send the game back
            // to the queue.
            let (pop, features_json) = (population.clone(), json.clone());
            let now = self.clock.wall_ms();
            let added =
                self.store.write(move |db| add_game_to_population(db, &pop, &features_json, now)).await;
            if let Err(e) = added {
                population.invalidate();
                log_error!(self.logger, "population update failed", { "err": crate::log::error(&e), "gameId": game_id });
            }
        }
        Ok((json, features.plies, features.white.n, features.black.n))
    }

    /// One line per engine process: which engine, and whether its network is shared (a warning
    /// when it is not while other engines run: each holds its own copy).
    fn report_start<E: PoolEngine>(&self, i: usize, engine: &E) {
        let memory = engine.net_memory();
        let mut fields = json!({
            "engine": i,
            "name": engine.name(),
            "net": engine.net(),
            "pid": engine.pid(),
            "network": NetworkMemory::label(memory),
        });
        if memory != Some(NetworkMemory::Local) {
            self.logger.emit(crate::log::Level::Info, "analysis engine started", Some(fields));
            return;
        }
        fields["why"] = json!(engine.net_memory_error());
        if self.settings.workers > 1 {
            fields["engines"] = json!(self.settings.workers);
            fields["hint"] = json!(SHARED_NETWORK_HINT);
            self.logger.emit(
                crate::log::Level::Warn,
                "analysis engine started with its own copy of the network",
                Some(fields),
            );
        } else {
            self.logger.emit(crate::log::Level::Info, "analysis engine started", Some(fields));
        }
    }

    /// One analysis loop: (re)starts its engine, claims a job, analyses it, until the pool stops.
    async fn run_loop<E: PoolEngine>(self: Arc<Self>, i: usize, mut engine: E) {
        let mut gauge = EngineGauge::default();
        let mut backoff = Backoff::new(&self.settings);
        let mut first = true;
        let mut reported = 0;
        while !self.stopping() {
            // Never claim a job without a working engine: a wrong ANALYSIS_ENGINE_PATH must not
            // mark the whole queue failed.
            if !engine.is_alive() {
                gauge.set(false, false);
                if !first {
                    self.sleep(backoff.delay(Instant::now())).await;
                    if self.stopping() {
                        break;
                    }
                }
                first = false;
                let mut stop = self.stop.clone();
                let started = tokio::select! {
                    r = engine.start() => r,
                    _ = stop.wait_for(|s| *s) => break,
                };
                match started {
                    Ok(()) => backoff.started(Instant::now()),
                    Err(e) => {
                        backoff.failed();
                        log_error!(self.logger, "analysis engine unavailable", { "err": crate::log::error(&e),
                            "retryInMs": backoff.upcoming().as_millis() as u64 });
                        continue;
                    }
                }
            }
            // A restart during a game's analysis is reported here, before the next job.
            if engine.starts() != reported {
                reported = engine.starts();
                self.report_start(i, &engine);
            }
            gauge.refresh(&mut engine);
            let claimed = self
                .store
                .analysis()
                .next(1, Some(self.settings.worker_id.clone()), self.clock.wall_ms())
                .await;
            let job = match claimed {
                Ok(mut jobs) if !jobs.is_empty() => jobs.swap_remove(0),
                Ok(_) => {
                    self.sleep(self.settings.poll).await;
                    continue;
                }
                Err(e) => {
                    log_error!(self.logger, "analysis queue unavailable", { "err": crate::log::error(&e) });
                    self.sleep(self.settings.poll).await;
                    continue;
                }
            };
            self.counters.running.fetch_add(1, Ordering::Relaxed);
            self.process_job(&mut engine, &job).await;
            self.counters.running.fetch_sub(1, Ordering::Relaxed);
        }
        engine.close().await;
        gauge.set(false, false);
    }
}

/// The delays between the starts of an engine: [`PoolSettings::min_backoff`] doubling up to
/// [`PoolSettings::max_backoff`], back to the minimum once the engine ran
/// [`PoolSettings::stable_uptime`].
#[derive(Debug)]
struct Backoff {
    min: Duration,
    max: Duration,
    stable: Duration,
    next: Duration,
    /// When the engine last started, `None` after a failed start.
    up_since: Option<Instant>,
}

impl Backoff {
    fn new(settings: &PoolSettings) -> Backoff {
        Backoff {
            min: settings.min_backoff,
            max: settings.max_backoff.max(settings.min_backoff),
            stable: settings.stable_uptime,
            next: settings.min_backoff,
            up_since: None,
        }
    }

    fn started(&mut self, at: Instant) {
        self.up_since = Some(at);
    }

    fn failed(&mut self) {
        self.up_since = None;
    }

    /// The wait before the next start, without consuming it.
    fn upcoming(&self) -> Duration {
        self.next
    }

    /// The wait before a start at `now` (an engine found dead after a stable run waits the
    /// minimum); the next one doubles.
    fn delay(&mut self, now: Instant) -> Duration {
        if self.up_since.is_some_and(|t| now.saturating_duration_since(t) >= self.stable) {
            self.next = self.min;
        }
        let delay = self.next;
        self.next = delay.saturating_mul(2).min(self.max);
        delay
    }
}

/// The analysis pool (module documentation).
pub struct AnalysisPool {
    shared: Option<Arc<Shared>>,
    stop: watch::Sender<bool>,
    loops: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for AnalysisPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalysisPool")
            .field("enabled", &self.enabled())
            .field("loops", &self.loops.len())
            .finish()
    }
}

impl AnalysisPool {
    /// Starts the pool of a server: `ANALYSIS_WORKERS` [`UciEngine`]s on `ANALYSIS_ENGINE_PATH`.
    /// Disabled (no task, an info line) when the path is empty or `ANALYSIS_WORKERS` is 0. Must
    /// be called inside a tokio runtime.
    pub fn start(config: &Config, store: Store, clock: SharedClock) -> AnalysisPool {
        if config.analysis_engine_path.is_empty() || config.analysis_workers <= 0 {
            let reason = if config.analysis_engine_path.is_empty() {
                "ANALYSIS_ENGINE_PATH empty"
            } else {
                "ANALYSIS_WORKERS=0"
            };
            log_info!(Logger::root().child("anticheat"), "engine analysis disabled", { "reason": reason });
            return AnalysisPool::disabled();
        }
        let logger = Logger::root().child("analysis");
        let options = engine_options(config, &logger);
        AnalysisPool::start_with(PoolSettings::from_config(config), store, clock, |_| {
            UciEngine::new(options.clone())
        })
    }

    /// Starts `settings.workers` loops on the engines `factory` makes (one per loop index).
    pub fn start_with<E: PoolEngine>(
        mut settings: PoolSettings,
        store: Store,
        clock: SharedClock,
        mut factory: impl FnMut(usize) -> E,
    ) -> AnalysisPool {
        let (stop, stop_rx) = watch::channel(false);
        let logger = Logger::root().child("analysis");
        let count = settings.workers.max(1);
        settings.workers = count;
        log_info!(logger, "analysis worker started", { "engines": count, "depthFast": settings.depths.fast,
            "depthDeep": settings.depths.deep, "workerId": settings.worker_id });
        let shared = Arc::new(Shared::new(settings, store, clock, stop_rx));
        let loops = (0..count).map(|i| tokio::spawn(shared.clone().run_loop(i, factory(i)))).collect();
        AnalysisPool { shared: Some(shared), stop, loops }
    }

    /// A pool that does nothing.
    pub fn disabled() -> AnalysisPool {
        AnalysisPool { shared: None, stop: watch::channel(false).0, loops: Vec::new() }
    }

    /// Whether engines run.
    pub fn enabled(&self) -> bool {
        self.shared.is_some()
    }

    /// Games analysed and failed so far, jobs in progress.
    pub fn stats(&self) -> PoolStats {
        let Some(s) = &self.shared else { return PoolStats::default() };
        PoolStats {
            analysed: s.counters.analysed.load(Ordering::Relaxed),
            failed: s.counters.failed.load(Ordering::Relaxed),
            running: s.counters.running.load(Ordering::Relaxed),
        }
    }

    /// Stops the loops: the analyses in progress are interrupted (their jobs stay claimed and are
    /// analysed again later), the engines are closed. Idempotent.
    pub async fn stop(&mut self) {
        let _ = self.stop.send(true);
        for handle in self.loops.drain(..) {
            if let Err(e) = handle.await
                && e.is_panic()
                && let Some(s) = &self.shared
            {
                log_error!(s.logger, "analysis loop failed", { "err": e.to_string() });
            }
        }
    }
}

impl Drop for AnalysisPool {
    fn drop(&mut self) {
        // The loops notice and close their engines; the engine processes are killed with them.
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod tests;
