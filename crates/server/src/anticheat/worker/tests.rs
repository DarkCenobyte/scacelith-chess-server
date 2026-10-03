//! Tests of the analysis pool, ported from anticheat.worker: scripted engines (the played move is
//! always the best of three lines) on a real in-memory store, and one `/bin/sh` UCI engine run
//! end to end by [`AnalysisPool::start`].

use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use super::*;
use crate::anticheat::analysis::engine::{EngineErrorKind, PvLine, SearchOptions, SearchResult};
use crate::anticheat::analysis::moves::move_to_uci;
use crate::anticheat::scoring::model::suspected;
use crate::anticheat::testing::*;
use crate::clock::ManualClock;
use crate::store::status::WHITE_WINS;
use crate::store::tests::support::{LogCapture, TempDir};
use crate::store::{IntegrityLevel as StoredLevel, IntegrityUpdate, JobStatus};

mod real_engine;

/// Serialises the tests that run pools: the engine gauges are process-wide. Taken before any log
/// capture.
static POOLS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const NET: &str = "nn-1a298aa575a0.nnue";

// ---- a scripted engine ---------------------------------------------------------------------------

/// What a fake engine shares with its test.
#[derive(Default)]
struct Probe {
    starts: AtomicU64,
    attempts: Mutex<Vec<Instant>>,
    closed: AtomicBool,
    /// Set by the test: the process dies (seen at the next liveness check).
    kill: AtomicBool,
}

/// An engine answering every position with three lines, the played move the best one.
struct Fake {
    name: String,
    net: Option<String>,
    /// The played moves (UCI).
    best: Vec<String>,
    probe: Arc<Probe>,
    alive: bool,
    start_error: Option<EngineError>,
    analyse_error: Option<EngineError>,
    memory: Option<NetworkMemory>,
    memory_error: Option<String>,
    /// The first search waits for it.
    gate: Option<Arc<Notify>>,
}

impl Fake {
    fn new(best: &[String]) -> Fake {
        Fake {
            name: "fake".into(),
            net: None,
            best: best.to_vec(),
            probe: Arc::default(),
            alive: false,
            start_error: None,
            analyse_error: None,
            memory: None,
            memory_error: None,
            gate: None,
        }
    }

    fn answer(&self, ply: usize, multi_pv: u32) -> SearchResult {
        let best = self.best.get(ply).cloned();
        let line = |multipv, cp, mv: Option<String>| PvLine {
            multipv,
            depth: 8,
            cp: Some(cp),
            mate: None,
            bound: None,
            pv: mv.iter().cloned().collect(),
            mv,
        };
        let mut lines = vec![line(1, 20, best.clone())];
        if multi_pv > 1 {
            lines.push(line(2, -40, Some("a1a1".into())));
            lines.push(line(3, -90, Some("b1b1".into())));
        }
        SearchResult { lines, bestmove: best, node_limited: false }
    }
}

impl AnalysisEngine for Fake {
    fn name(&self) -> &str {
        &self.name
    }

    fn net(&self) -> Option<String> {
        self.net.clone()
    }

    fn hash_mb(&self) -> Option<u32> {
        None
    }

    fn new_game(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        ready(Ok(()))
    }

    fn clear_hash(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        ready(Ok(()))
    }

    fn analyse(
        &mut self,
        moves: &[String],
        opts: &SearchOptions,
    ) -> impl Future<Output = Result<SearchResult, EngineError>> + Send {
        let gate = self.gate.take();
        let result = match &self.analyse_error {
            Some(e) => Err(e.clone()),
            None => Ok(self.answer(moves.len(), opts.multi_pv)),
        };
        async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            result
        }
    }
}

impl PoolEngine for Fake {
    fn start(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        self.probe.attempts.lock().push(Instant::now());
        if let Some(e) = &self.start_error {
            return ready(Err(e.clone()));
        }
        if !self.alive {
            self.alive = true;
            self.probe.starts.fetch_add(1, Ordering::SeqCst);
        }
        ready(Ok(()))
    }

    fn close(&mut self) -> impl Future<Output = ()> + Send {
        self.alive = false;
        self.probe.closed.store(true, Ordering::SeqCst);
        ready(())
    }

    fn is_alive(&mut self) -> bool {
        if self.probe.kill.swap(false, Ordering::SeqCst) {
            self.alive = false;
        }
        self.alive
    }

    fn starts(&self) -> u64 {
        self.probe.starts.load(Ordering::SeqCst)
    }

    fn net_memory(&self) -> Option<NetworkMemory> {
        self.memory
    }

    fn net_memory_error(&self) -> Option<&str> {
        self.memory_error.as_deref()
    }

    fn pid(&self) -> Option<u32> {
        self.alive.then_some(4242)
    }
}

// ---- the world -----------------------------------------------------------------------------------

struct World {
    config: Config,
    store: Store,
    clock: Arc<ManualClock>,
}

/// A game of the queue: its players and its moves (UCI).
struct Queued {
    id: GameId,
    white: UserId,
    black: UserId,
    moves: Vec<String>,
}

async fn world() -> World {
    let config =
        config(&[("ANALYSIS_DEPTH_FAST", "4"), ("ANALYSIS_DEPTH_DEEP", "8"), ("ANALYSIS_POLL_MS", "100")]);
    let clock = ManualClock::new(0.0, NOW);
    let store = store(&config, &clock).await;
    World { config, store, clock }
}

impl World {
    /// Settings of a pool of `workers` engines claiming as `worker_id`, polling every 10 ms, its
    /// engines restarted after 10 ms doubling to 40 ms.
    fn settings(&self, workers: usize, worker_id: &str) -> PoolSettings {
        PoolSettings {
            workers,
            poll: Duration::from_millis(10),
            min_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            worker_id: worker_id.into(),
            ..PoolSettings::from_config(&self.config)
        }
    }

    /// The pool's shared state without its loops, to drive `process_job` directly.
    fn worker(&self, worker_id: &str) -> (Arc<Shared>, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        let shared = Shared::new(self.settings(1, worker_id), self.store.clone(), self.clock.clone(), rx);
        (Arc::new(shared), tx)
    }

    /// A finished rated 5+0 game of `plies` plies between new players (or `players`), queued at
    /// `priority`. Its moves are not legal chess: the analysis compares them with the engine's.
    async fn add_game(&self, plies: usize, priority: Priority, players: Option<(UserId, UserId)>) -> Queued {
        static N: AtomicU64 = AtomicU64::new(0);
        let (white, black) = match players {
            Some(p) => p,
            None => {
                let n = N.fetch_add(1, Ordering::Relaxed);
                let ids = users(&self.store, &[&format!("w{n}"), &format!("b{n}")]).await;
                (ids[0], ids[1])
            }
        };
        let raw: Vec<u16> = (0..plies).map(|i| ((i % 64) | (((i + 9) % 64) << 6)) as u16).collect();
        let mut g = game(white, black, WHITE_WINS, NOW - HOUR);
        g.category = "5+0".into();
        g.base_ms = 300_000;
        g.inc_ms = 0;
        g.spent_ms = Some((0..plies).map(|i| 1000 + (i as u32 * 37) % 900).collect());
        g.clock_ms = Some(vec![0; plies]);
        g.moves = raw.clone();
        let id = g.id;
        self.store.finish_batch(vec![g]).await.expect("game stored");
        // The queue policy gives a game of a flagged player the signal priority: set it here.
        self.store
            .write(move |db| {
                db.connection()
                    .execute(
                        "UPDATE analysis_jobs SET priority = ?1 WHERE game_id = ?2",
                        rusqlite::params![priority as i64, id as i64],
                    )
                    .map_err(StoreError::from)
            })
            .await
            .expect("job queued");
        Queued { id, white, black, moves: raw.iter().map(|&m| move_to_uci(m)).collect() }
    }

    async fn claim(&self, worker_id: &str) -> ClaimedJob {
        let mut jobs = self.store.analysis().next(1, Some(worker_id.into()), NOW).await.unwrap();
        assert_eq!(jobs.len(), 1, "a job waits");
        jobs.remove(0)
    }

    async fn job(&self, id: GameId) -> crate::store::Job {
        self.store.analysis().job(id).await.unwrap().expect("a job")
    }

    async fn statistics(&self, user: UserId) -> Value {
        let integrity = self.store.integrity().get(user).await.unwrap();
        integrity.evidence.unwrap_or(Value::Null)["statistics"].clone()
    }

    /// Values in the population of `profile` for the accuracy of 5+0 players rated 1500.
    async fn accuracy_n(&self, profile: &str) -> i64 {
        let stats = self.store.integrity().population_stats(profile.to_string()).await.unwrap();
        stats.get("5+0|1500|accuracy").map_or(0, |s| s.n)
    }

    /// Statistics stored, every profile.
    async fn population_size(&self) -> i64 {
        self.store
            .read(|db| {
                let n =
                    db.connection().query_row("SELECT count(*) FROM population_stats", [], |r| r.get(0))?;
                Ok::<_, StoreError>(n)
            })
            .await
            .unwrap()
    }

    /// Overwrites a rating record (`None`: no record).
    async fn set_rating(&self, user: UserId, record: Option<(i64, bool, i64)>) {
        self.store
            .write(move |db| {
                let c = db.connection();
                c.execute("DELETE FROM ratings WHERE user_id = ?1 AND category = '5+0'", [user])?;
                if let Some((games, rated, counted)) = record {
                    c.execute(
                        "INSERT INTO ratings (user_id, category, rating, games, peak, rated, counted_games, updated_at)
                         VALUES (?1, '5+0', 1500, ?2, 1500, ?3, ?4, 0)",
                        rusqlite::params![user, games, rated, counted],
                    )?;
                }
                Ok::<_, StoreError>(())
            })
            .await
            .expect("rating set");
    }
}

fn analysed(shared: &Shared) -> (u64, u64) {
    (shared.counters.analysed.load(Ordering::SeqCst), shared.counters.failed.load(Ordering::SeqCst))
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---- process_job ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_job_stores_its_features_scores_both_players_and_feeds_the_population() {
    let w = world().await;
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let (worker, _stop) = w.worker("t");
    let mut engine = Fake::new(&g.moves);
    let job = w.claim("t").await;
    let f = worker.process_job(&mut engine, &job).await.expect("analysed");
    assert_eq!(f["gameId"], json!(g.id));
    assert_eq!(f["white"]["n"], 12, "plies 16..39, white's half");
    assert_eq!(f["white"]["t1Deep"], 1);
    assert_eq!(f["white"]["ratingGames"], 0);
    assert_eq!(f["profile"], "fake; depth 4/8; analysis 2");
    assert_eq!(w.job(g.id).await.status, JobStatus::Done);
    let stored = w.store.analysis().for_user(g.white, 10, true).await.unwrap();
    assert_eq!(stored[0].features.as_ref(), Some(&f), "the features are stored");
    assert_eq!(w.store.integrity().get(g.white).await.unwrap().level, StoredLevel::None);
    let st = w.statistics(g.black).await;
    assert_eq!(st["games"], 1);
    assert_eq!(st["profile"], f["profile"]);
    assert_eq!(w.accuracy_n(f["profile"].as_str().unwrap()).await, 2);
    assert_eq!(analysed(&worker), (1, 0));
}

#[tokio::test]
async fn the_rating_evidence_of_a_side_is_its_counted_games_once_rated_and_0_while_unrated() {
    let w = world().await;
    let (worker, _stop) = w.worker("t");
    let cases = [
        (Some((60, false, 3)), 0),  // many games, never rated
        (Some((50, true, 12)), 12), // the unrated phase's losses did not count
        (Some((45, true, 45)), 45),
        (None, 0), // no record: the initial one, unrated
    ];
    for (record, expected) in cases {
        let g = w.add_game(40, Priority::Ordinary, None).await;
        w.set_rating(g.white, record).await;
        w.set_rating(g.black, Some((60, true, 60))).await;
        let mut engine = Fake::new(&g.moves);
        let job = w.claim("t").await;
        let f = worker.process_job(&mut engine, &job).await.expect("analysed");
        assert_eq!(
            [&f["white"]["ratingGames"], &f["black"]["ratingGames"]],
            [&json!(expected), &json!(60)],
            "{record:?}"
        );
    }
}

#[tokio::test]
async fn only_games_claimed_at_ordinary_priority_feed_the_population() {
    let w = world().await;
    let (worker, _stop) = w.worker("t");
    // Flagged, reported and requested games are analysed first and in full: counted in the
    // population, they would shift the baseline towards the suspects it judges.
    let mut flagged = Vec::new();
    for p in [Priority::Signal, Priority::Report, Priority::Manual] {
        flagged.push(w.add_game(40, p, None).await);
    }
    let mut engine = Fake::new(&flagged[0].moves);
    for _ in 0..3 {
        let job = w.claim("t").await;
        assert_ne!(job.priority, Priority::Ordinary);
        assert!(worker.process_job(&mut engine, &job).await.is_some());
    }
    assert_eq!(w.population_size().await, 0, "no population update from prioritised games");
    for g in &flagged {
        assert_eq!(w.statistics(g.black).await["games"], 1, "the players are scored all the same");
    }
    // An ordinary job does.
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let job = w.claim("t").await;
    assert_eq!((job.game_id, job.priority), (g.id, Priority::Ordinary));
    let f = worker.process_job(&mut engine, &job).await.expect("analysed");
    assert_eq!(w.accuracy_n(f["profile"].as_str().unwrap()).await, 2);
}

#[tokio::test]
async fn a_failed_population_write_leaves_the_job_done_without_the_sample() {
    let w = world().await;
    let (worker, _stop) = w.worker("t");
    let g = w.add_game(40, Priority::Ordinary, None).await;
    exec(
        &w.store,
        "CREATE TRIGGER population_busy BEFORE INSERT ON population_stats
         BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
    )
    .await;
    let mut engine = Fake::new(&g.moves);
    let logs = LogCapture::start();
    let job = w.claim("t").await;
    let f = worker.process_job(&mut engine, &job).await.expect("analysed");
    assert_eq!(w.job(g.id).await.status, JobStatus::Done, "not sent back to the queue");
    assert_eq!(analysed(&worker), (1, 0));
    let errors: Vec<Value> = logs
        .records("analysis")
        .into_iter()
        .filter(|r| r["level"] == "error" && (r["gameId"] == json!(g.id) || r["userId"] == g.white))
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["msg"], "population update failed");
    drop(logs);
    assert_eq!(w.population_size().await, 0);
    // The next sample joins as usual.
    exec(&w.store, "DROP TRIGGER population_busy;").await;
    let next = w.add_game(40, Priority::Ordinary, None).await;
    let job = w.claim("t").await;
    assert_eq!(job.game_id, next.id);
    worker.process_job(&mut engine, &job).await.expect("analysed");
    assert_eq!(w.accuracy_n(f["profile"].as_str().unwrap()).await, 2);
}

#[tokio::test]
async fn another_engine_restarts_the_statistics_and_levels_stand_until_judged_on_its_games() {
    let w = world().await;
    let (worker, _stop) = w.worker("t");
    let first = w.add_game(40, Priority::Ordinary, None).await;
    let players = Some((first.white, first.black));
    let mut before = Fake::new(&first.moves);
    let mut after = Fake { name: "fake 2".into(), ..Fake::new(&first.moves) };
    let job = w.claim("t").await;
    let old = worker.process_job(&mut before, &job).await.expect("analysed");
    let old_profile = old["profile"].as_str().unwrap().to_string();
    // A level reached with the earlier engine (the model would not flag these few games).
    let suspected_level = IntegrityUpdate {
        level: Some(StoredLevel::Suspected),
        score: Some(3.6),
        evidence: Some(Some(json!({}))),
        ..IntegrityUpdate::default()
    };
    w.store.integrity().set(first.white, suspected_level).await.unwrap();
    for i in 1..=suspected::MIN_GAMES {
        w.add_game(40, Priority::Ordinary, players).await;
        let job = w.claim("t").await;
        let f = worker.process_job(&mut after, &job).await.expect("analysed");
        let profile = f["profile"].as_str().unwrap();
        assert_ne!(profile, old_profile);
        let st = w.statistics(first.white).await;
        assert_eq!(
            (st["profile"].as_str().unwrap(), &st["games"]),
            (profile, &json!(i)),
            "scored on the new profile only"
        );
        let level = w.store.integrity().get(first.white).await.unwrap().level;
        let expected = if i < suspected::MIN_GAMES { StoredLevel::Suspected } else { StoredLevel::None };
        assert_eq!(level, expected, "after {i} games of the new profile");
        assert_eq!(w.accuracy_n(profile).await, 2 * i as i64);
    }
    assert_eq!(w.accuracy_n(&old_profile).await, 2, "the earlier population is left as it was");
}

#[tokio::test]
async fn a_job_is_renewed_while_the_engine_works_on_it_and_no_longer_after() {
    let w = world().await;
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let (tx, rx) = watch::channel(false);
    let settings = PoolSettings { heartbeat: Duration::from_millis(10), ..w.settings(1, "w7") };
    let worker = Shared::new(settings, w.store.clone(), w.clock.clone(), rx);
    let job = w.claim("w7").await;
    // Every renewal of the claim, as the store saw it.
    exec(
        &w.store,
        "CREATE TABLE touches (game_id INTEGER, at INTEGER);
         CREATE TRIGGER touched AFTER UPDATE OF started_at ON analysis_jobs
         BEGIN INSERT INTO touches VALUES (NEW.game_id, NEW.started_at); END;",
    )
    .await;
    w.clock.advance(1234.0);
    let touches = || async {
        w.store
            .read(|db| {
                let mut stmt = db.connection().prepare("SELECT game_id, at FROM touches ORDER BY rowid")?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
                Ok::<_, StoreError>(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap()
    };
    // A long analysis: the first position waits for two renewals.
    let gate = Arc::new(Notify::new());
    let mut engine = Fake { gate: Some(gate.clone()), ..Fake::new(&g.moves) };
    let watcher = async {
        let end = Instant::now() + Duration::from_secs(10);
        while touches().await.len() < 2 {
            assert!(Instant::now() < end, "no renewal");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        gate.notify_one();
    };
    let (f, ()) = tokio::join!(worker.process_job(&mut engine, &job), watcher);
    assert!(f.is_some());
    let seen = touches().await;
    assert_eq!(seen[..2], [(g.id as i64, NOW + 1234), (g.id as i64, NOW + 1234)]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(touches().await.len(), seen.len(), "stopped with the job");
    drop(tx);
}

#[tokio::test]
async fn a_job_whose_game_is_missing_or_whose_engine_crashes_is_marked_failed() {
    let w = world().await;
    let (worker, _stop) = w.worker("t");
    let logs = LogCapture::start();
    // A job whose game is gone (claimed, then the game was deleted).
    let missing = ClaimedJob {
        game_id: next_game_id(),
        priority: Priority::Ordinary,
        attempts: 1,
        queued_at: NOW,
        started_at: Some(NOW),
        worker: Some("t".into()),
    };
    let mut engine = Fake::new(&[]);
    assert_eq!(worker.process_job(&mut engine, &missing).await, None);
    let warned = logs.records("analysis").into_iter().any(|r| {
        r["msg"] == "analysis failed" && r["gameId"] == json!(missing.game_id) && r["err"] == "game not found"
    });
    assert!(warned);
    drop(logs);
    // Re-queued until it was claimed three times, then failed.
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let mut crashing = Fake {
        analyse_error: Some(EngineError::new(EngineErrorKind::Crashed, "engine exited")),
        ..Fake::new(&g.moves)
    };
    for i in 1..=3 {
        let job = w.claim("t").await;
        assert_eq!(worker.process_job(&mut crashing, &job).await, None);
        let expected = if i < 3 { JobStatus::Queued } else { JobStatus::Failed };
        assert_eq!(w.job(g.id).await.status, expected, "after {i} failures");
    }
    let error = w.job(g.id).await.error.unwrap();
    assert_eq!(error, "engine crashed: engine exited");
    assert_eq!(analysed(&worker), (0, 4));
}

#[tokio::test]
async fn a_long_error_is_stored_cut_to_500_units() {
    let w = world().await;
    let (worker, _stop) = w.worker("t");
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let long = "é".repeat(600);
    let mut crashing =
        Fake { analyse_error: Some(EngineError::new(EngineErrorKind::Crashed, long)), ..Fake::new(&g.moves) };
    let job = w.claim("t").await;
    assert_eq!(worker.process_job(&mut crashing, &job).await, None);
    let error = w.job(g.id).await.error.unwrap();
    assert_eq!(js::utf16_len(&error), 500);
    assert!(error.starts_with("engine crashed: éé"));
}

// ---- the loops -----------------------------------------------------------------------------------

#[test]
fn the_restart_delay_doubles_to_its_maximum_and_resets_after_a_stable_run() {
    let settings = PoolSettings { workers: 1, ..PoolSettings::from_config(&config(&[])) };
    assert_eq!(
        (settings.min_backoff, settings.max_backoff, settings.stable_uptime),
        (MIN_BACKOFF, MAX_BACKOFF, STABLE_UPTIME)
    );
    let mut b = Backoff::new(&settings);
    let t0 = Instant::now();
    let secs = |d: Duration| d.as_secs();
    // Failed starts: 1, 2, 4 ... 60 s.
    let delays: Vec<u64> = (0..8).map(|_| secs(b.delay(t0))).collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
    // An engine that dies 59 s after its start keeps the delay; one that ran 60 s starts after 1 s.
    b.started(t0);
    assert_eq!(secs(b.delay(t0 + Duration::from_secs(59))), 60);
    b.started(t0);
    assert_eq!(b.upcoming(), MAX_BACKOFF);
    assert_eq!(secs(b.delay(t0 + Duration::from_secs(60))), 1);
    assert_eq!(secs(b.upcoming()), 2);
    b.failed();
    assert_eq!(secs(b.delay(t0 + Duration::from_secs(600))), 2, "a failed start never resets");
}

#[test]
fn the_settings_and_the_engine_options_come_from_the_configuration() {
    let config = config(&[
        ("ANALYSIS_ENGINE_PATH", "/opt/stockfish/sf"),
        ("ANALYSIS_WORKERS", "3"),
        ("ANALYSIS_DEPTH_FAST", "7"),
        ("ANALYSIS_DEPTH_DEEP", "13"),
        ("ANALYSIS_POLL_MS", "250"),
        ("ANALYSIS_HASH_MB", "64"),
        ("ANALYSIS_POSITION_TIMEOUT_MS", "30000"),
    ]);
    let s = PoolSettings::from_config(&config);
    assert_eq!((s.workers, s.depths, s.poll), (3, Depths { fast: 7, deep: 13 }, Duration::from_millis(250)));
    assert_eq!(s.heartbeat, HEARTBEAT);
    assert_eq!(s.worker_id, format!("{}:{}", crate::sys::hostname(), std::process::id()));
    let o = engine_options(&config, &Logger::root().child("analysis"));
    assert_eq!(o.path, std::path::PathBuf::from("/opt/stockfish/sf"));
    assert_eq!((o.threads, o.hash_mb, o.timeout), (1, 64, Duration::from_secs(30)));
    assert!(o.low_priority, "nice 19");
    assert!(o.log.is_some());
}

#[tokio::test]
async fn the_pool_is_disabled_without_an_engine_path_or_without_workers() {
    let w = world().await;
    let logs = LogCapture::start();
    let mut pool = AnalysisPool::start(&config(&[]), w.store.clone(), w.clock.clone());
    assert!(!pool.enabled());
    assert_eq!(pool.stats(), PoolStats::default());
    pool.stop().await;
    let config = config(&[("ANALYSIS_ENGINE_PATH", "/bin/sh"), ("ANALYSIS_WORKERS", "0")]);
    let pool = AnalysisPool::start(&config, w.store.clone(), w.clock.clone());
    assert!(!pool.enabled());
    let reasons: Vec<Value> = logs
        .records("anticheat")
        .into_iter()
        .filter(|r| r["msg"] == "engine analysis disabled")
        .map(|r| r["reason"].clone())
        .collect();
    assert_eq!(reasons, ["ANALYSIS_ENGINE_PATH empty", "ANALYSIS_WORKERS=0"]);
}

#[tokio::test]
async fn an_engine_that_cannot_start_never_claims_a_job_and_is_retried_with_backoff() {
    let _pools = POOLS.lock().await;
    let w = world().await;
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let logs = LogCapture::start();
    let mut broken = Fake::new(&[]);
    let tag = format!("ENOENT {}", g.id);
    broken.start_error = Some(EngineError::new(EngineErrorKind::Spawn, tag.clone()));
    let probe = broken.probe.clone();
    let mut engine = Some(broken);
    let mut pool =
        AnalysisPool::start_with(w.settings(1, "broken"), w.store.clone(), w.clock.clone(), |_| {
            engine.take().expect("one engine")
        });
    wait_for("five starts", || probe.attempts.lock().len() >= 5).await;
    pool.stop().await;
    assert_eq!(w.job(g.id).await.status, JobStatus::Queued, "the job stays in the queue");
    assert!(probe.closed.load(Ordering::SeqCst), "the engine is closed");
    // 10 ms doubling to 40 ms between the starts.
    let attempts = probe.attempts.lock().clone();
    let gaps: Vec<Duration> = attempts.windows(2).map(|w| w[1] - w[0]).collect();
    for (gap, min) in gaps.iter().zip([10, 20, 40, 40]) {
        assert!(*gap >= Duration::from_millis(min), "{gaps:?}");
    }
    let retries: Vec<Value> = logs
        .records("analysis")
        .into_iter()
        .filter(|r| {
            r["msg"] == "analysis engine unavailable"
                && r["err"]["message"].as_str().is_some_and(|m| m.contains(&tag))
        })
        .map(|r| r["retryInMs"].clone())
        .collect();
    assert_eq!(retries[..4], [json!(10), json!(20), json!(40), json!(40)]);
    assert_eq!(pool.stats(), PoolStats::default());
}

#[tokio::test]
async fn the_loops_analyse_the_queued_games_restart_a_dead_engine_and_close_it_at_stop() {
    let _pools = POOLS.lock().await;
    let w = world().await;
    let g1 = w.add_game(40, Priority::Ordinary, None).await;
    w.add_game(40, Priority::Ordinary, None).await;
    let fake = Fake::new(&g1.moves);
    let probe = fake.probe.clone();
    let mut engine = Some(fake);
    let mut pool = AnalysisPool::start_with(w.settings(1, "loop"), w.store.clone(), w.clock.clone(), |_| {
        engine.take().expect("one engine")
    });
    assert!(pool.enabled());
    wait_for("two games", || pool.stats().analysed == 2).await;
    assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
    // The engine dies while the queue is empty: started again, the next game analysed.
    probe.kill.store(true, Ordering::SeqCst);
    let g3 = w.add_game(40, Priority::Ordinary, None).await;
    wait_for("the third game", || pool.stats().analysed == 3).await;
    assert_eq!(probe.starts.load(Ordering::SeqCst), 2);
    pool.stop().await;
    pool.stop().await;
    assert!(probe.closed.load(Ordering::SeqCst));
    assert_eq!(pool.stats(), PoolStats { analysed: 3, failed: 0, running: 0 });
    assert_eq!(w.job(g3.id).await.status, JobStatus::Done);
}

#[tokio::test]
async fn stop_interrupts_an_analysis_and_leaves_its_job_claimed() {
    let _pools = POOLS.lock().await;
    let w = world().await;
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let logs = LogCapture::start();
    // A search that never ends.
    let fake = Fake { gate: Some(Arc::new(Notify::new())), ..Fake::new(&g.moves) };
    let probe = fake.probe.clone();
    let mut engine = Some(fake);
    let mut pool = AnalysisPool::start_with(w.settings(1, "stop"), w.store.clone(), w.clock.clone(), |_| {
        engine.take().expect("one engine")
    });
    wait_for("the analysis", || pool.stats().running == 1).await;
    pool.stop().await;
    assert!(probe.closed.load(Ordering::SeqCst));
    assert_eq!(pool.stats(), PoolStats::default(), "neither analysed nor failed");
    assert_eq!(w.job(g.id).await.status, JobStatus::Running, "analysed again once its claim is stale");
    let interrupted = logs
        .records("analysis")
        .into_iter()
        .any(|r| r["msg"] == "analysis interrupted by shutdown" && r["gameId"] == json!(g.id));
    assert!(interrupted);
}

/// Runs a pool of engines reporting `memory` until each logged its start, restarts the first one,
/// stops; returns the start logs and the gauges seen while the engines ran.
async fn start_logs(
    workers: usize,
    memory: &[(Option<NetworkMemory>, Option<&str>)],
) -> (Vec<Value>, [f64; 2]) {
    static RUN: AtomicU64 = AtomicU64::new(0);
    let w = world().await;
    let name = format!("Stockfish 19 #{}", RUN.fetch_add(1, Ordering::Relaxed));
    let engines: Vec<Fake> = memory
        .iter()
        .map(|(m, why)| Fake {
            name: name.clone(),
            net: Some(NET.into()),
            memory: *m,
            memory_error: why.map(str::to_string),
            ..Fake::new(&[])
        })
        .collect();
    let first = engines[0].probe.clone();
    let mut engines = engines.into_iter();
    let logs = LogCapture::start();
    let mut pool =
        AnalysisPool::start_with(w.settings(workers, "logs"), w.store.clone(), w.clock.clone(), |_| {
            engines.next().expect("an engine per loop")
        });
    let starts =
        || logs.records("analysis").into_iter().filter(|r| r["name"] == name.as_str()).collect::<Vec<_>>();
    wait_for("every start", || starts().len() >= workers).await;
    let gauges = [METRICS.engines.get(), METRICS.shared.get()];
    first.starts.fetch_add(1, Ordering::SeqCst); // restarted during a game: reported once more
    wait_for("the restart", || starts().len() > workers).await;
    pool.stop().await;
    (starts(), gauges)
}

fn summary(logs: &[Value], keys: &[&str]) -> Vec<Vec<Value>> {
    logs.iter().map(|r| keys.iter().map(|k| r[*k].clone()).collect()).collect()
}

#[tokio::test]
async fn every_engine_start_is_logged_with_where_its_network_lives_and_the_gauges_count_them() {
    let _pools = POOLS.lock().await;
    use NetworkMemory::{Local, Shared};

    let (logs, gauges) = start_logs(2, &[(Some(Shared), None), (Some(Shared), None)]).await;
    assert_eq!(gauges, [2.0, 2.0]);
    let info = vec![json!("info"), json!("analysis engine started"), json!("shared memory")];
    assert_eq!(summary(&logs, &["level", "msg", "network"]), vec![info; 3]);
    let mut engines: Vec<i64> = logs.iter().map(|r| r["engine"].as_i64().unwrap()).collect();
    engines.sort();
    assert_eq!(engines, [0, 0, 1], "once per start of each engine");
    assert_eq!((&logs[0]["net"], &logs[0]["pid"]), (&json!(NET), &json!(4242)));

    // Without sharing while two engines run: a warning with the engine's reason.
    let why = "Shared memory is not serving to other processes";
    let (logs, gauges) = start_logs(2, &[(Some(Local), Some(why)), (Some(Shared), None)]).await;
    assert_eq!(gauges, [2.0, 1.0]);
    let warn = logs.iter().find(|r| r["level"] == "warn").expect("a warning");
    assert_eq!(warn["msg"], "analysis engine started with its own copy of the network");
    assert_eq!(
        summary(std::slice::from_ref(warn), &["network", "why", "engines"]),
        [[json!("local memory"), json!(why), json!(2)]]
    );
    assert_eq!(warn["hint"], SHARED_NETWORK_HINT);

    // Alone, a copy of its own costs nothing more; an engine that says nothing (Stockfish 16).
    let (logs, _) = start_logs(1, &[(Some(Local), Some("why"))]).await;
    assert_eq!(summary(&logs, &["level", "network"]), vec![vec![json!("info"), json!("local memory")]; 2]);
    let (logs, gauges) = start_logs(1, &[(None, None)]).await;
    assert_eq!(gauges, [1.0, 0.0]);
    assert_eq!(summary(&logs, &["level", "network"]), vec![vec![json!("info"), json!("not reported")]; 2]);
    assert_eq!([METRICS.engines.get(), METRICS.shared.get()], [0.0, 0.0], "stopped pools count no engine");
}

// ---- end to end ----------------------------------------------------------------------------------

/// A UCI engine in a shell script: answers `uci` and `isready`, every `go` with one line whose
/// best move is e2e4, quits on `quit`.
const UCI_SCRIPT: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "${line%% *}" in
    uci) printf 'id name Fake Engine 1\nuciok\n' ;;
    isready) printf 'readyok\n' ;;
    go) printf 'info depth 30 seldepth 30 multipv 1 score cp 20 nodes 20 pv e2e4\nbestmove e2e4\n' ;;
    quit) exit 0 ;;
  esac
done
"#;

#[tokio::test]
async fn a_pool_started_from_the_configuration_analyses_a_game_with_a_uci_engine_process() {
    use std::os::unix::fs::PermissionsExt;
    let _pools = POOLS.lock().await;
    let dir = TempDir::new("analysis-pool");
    let path = dir.file("fake-engine.sh");
    std::fs::write(&path, UCI_SCRIPT).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = config(&[
        ("ANALYSIS_ENGINE_PATH", path.as_str()),
        ("ANALYSIS_WORKERS", "1"),
        ("ANALYSIS_DEPTH_FAST", "4"),
        ("ANALYSIS_DEPTH_DEEP", "8"),
        ("ANALYSIS_POLL_MS", "100"),
    ]);
    let clock = ManualClock::new(0.0, NOW);
    let w = World { store: store(&config, &clock).await, config, clock };
    let g = w.add_game(40, Priority::Ordinary, None).await;
    let mut pool = AnalysisPool::start(&w.config, w.store.clone(), w.clock.clone());
    assert!(pool.enabled());
    // A spawn refused while the script was still open elsewhere is retried after a second.
    wait_for("the analysis", || pool.stats().analysed == 1).await;
    assert_eq!(METRICS.engines.get(), 1.0);
    pool.stop().await;
    assert_eq!(METRICS.engines.get(), 0.0);
    let job = w.job(g.id).await;
    assert_eq!(job.status, JobStatus::Done);
    let stored = w.store.analysis().for_user(g.white, 1, true).await.unwrap();
    let profile = stored[0].features.as_ref().unwrap()["profile"].as_str().unwrap().to_string();
    assert_eq!(profile, "Fake Engine 1; depth 4/8; hash 32; analysis 2");
    // One line per position: every move is forced, the player is scored on no move.
    assert_eq!(w.statistics(g.white).await["profile"], json!(profile));
}
