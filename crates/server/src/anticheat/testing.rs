//! Helpers of the store-side anti-cheat tests: stores with the real rating rules and a manual
//! clock, players with seeded ratings, finished games, a recorder of the sanction events and a
//! writer barrier.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use crate::clock::ManualClock;
use crate::config::{Config, test_config};
use crate::events::{SanctionApplied, SanctionEvents};
use crate::ids::{GameId, UserId};
use crate::matching::elo::{self, EloSettings, Record};
use crate::store::tests::support::new_user;
use crate::store::{
    GameOutcome, GameRecord, RatingFn, RatingRecord, SideOutcome, Store, StoreError, StoreOptions,
};

/// 2026-09-01T12:00:00Z, the time of most scenarios.
pub const NOW: i64 = 1_788_264_000_000;
/// Milliseconds in a day.
pub const DAY: i64 = 86_400_000;
/// Milliseconds in an hour.
pub const HOUR: i64 = 3_600_000;

/// A test configuration with overrides.
pub fn config(overrides: &[(&str, &str)]) -> Config {
    test_config(overrides).expect("valid test configuration")
}

/// The server's rating rules (the Elo of `matching`) as the store's rating function.
pub fn elo_rating(settings: EloSettings) -> RatingFn {
    fn to_record(r: &RatingRecord) -> Record {
        Record {
            rating: r.rating,
            games: r.games,
            wins: r.wins,
            draws: r.draws,
            losses: r.losses,
            peak: r.peak,
            reached_senior: r.reached_senior,
            rated: r.rated,
            counted_games: r.counted_games,
            unrated_games: r.unrated_games,
            unrated_opponents: r.unrated_opponents,
            unrated_half_points: r.unrated_half_points,
        }
    }
    fn to_rating(r: &Record) -> RatingRecord {
        RatingRecord {
            rating: r.rating,
            games: r.games,
            wins: r.wins,
            draws: r.draws,
            losses: r.losses,
            peak: r.peak,
            reached_senior: r.reached_senior,
            rated: r.rated,
            counted_games: r.counted_games,
            unrated_games: r.unrated_games,
            unrated_opponents: r.unrated_opponents,
            unrated_half_points: r.unrated_half_points,
        }
    }
    Arc::new(move |w: &RatingRecord, b: &RatingRecord, score: f64| {
        let change = elo::apply_game(&to_record(w), &to_record(b), score, &settings).expect("valid score");
        let side = |c: &elo::SideChange| SideOutcome {
            before: c.before,
            after: c.after,
            k: Some(c.k),
            record: to_rating(&c.record),
        };
        GameOutcome { white: side(&change.white), black: side(&change.black) }
    })
}

/// A migrated in-memory store with the real rating rules, the clock, every game sampled for the
/// analysis.
pub async fn store(config: &Config, clock: &Arc<ManualClock>) -> Store {
    let opts = StoreOptions {
        path: Some(":memory:".into()),
        rating: Some(elo_rating(EloSettings::from_config(config))),
        random: Some(Arc::new(|| 0.0)),
        clock: Some(clock.clone()),
        ..StoreOptions::default()
    };
    let store = Store::open(config, opts).await.expect("store opens");
    store.migrate().await.expect("store migrates");
    store
}

/// Creates accounts; returns their ids in order.
pub async fn users(store: &Store, names: &[&str]) -> Vec<UserId> {
    let mut ids = Vec::new();
    for n in names {
        let email = format!("{n}@example.org");
        ids.push(store.users().create(new_user(n, Some(&email))).await.expect("user created"));
    }
    ids
}

/// Seeds a rated record (`games` counted games, half won) in a category.
pub async fn seed_rating(store: &Store, user: UserId, category: &str, rating: i64, games: i64) {
    let category = category.to_string();
    store
        .write(move |db| {
            db.connection()
                .execute(
                    "INSERT INTO ratings (user_id, category, rating, games, wins, losses, peak, rated, counted_games,
                     updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?3, 1, ?4, 0)",
                    rusqlite::params![user, category, rating, games, games / 2, games - games / 2],
                )
                .map_err(StoreError::from)?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("rating seeded");
}

/// A fresh game id (valid, time ordered).
pub fn next_game_id() -> GameId {
    static NEXT: AtomicU64 = AtomicU64::new(7_000_000_000_000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A finished rated 3+2 game of 40 plies.
pub fn game(white: UserId, black: UserId, status: u8, ended_at: i64) -> GameRecord {
    let plies = 40;
    GameRecord {
        id: next_game_id(),
        category: "3+2".into(),
        rated: true,
        base_ms: 180_000,
        inc_ms: 2_000,
        white_id: white,
        black_id: black,
        white_name: "W".into(),
        black_name: "B".into(),
        white_rating: Some(1500),
        black_rating: Some(1500),
        started_at: Some(ended_at - 600_000),
        ended_at: Some(ended_at),
        status,
        reason: if status == crate::store::status::DRAW { 2 } else { 1 },
        rematch_of: None,
        flags: 1,
        moves: vec![0; plies],
        spent_ms: Some(vec![0; plies]),
        clock_ms: Some(vec![0; plies]),
    }
}

/// Waits until every write job queued before the call has run.
pub async fn barrier(store: &Store) {
    let _ = store.write(|_| Ok::<_, StoreError>(())).await;
}

/// Polls `cond` until it holds (5 s at most).
pub async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

/// Runs SQL on the writer connection (failure injection with triggers).
pub async fn exec(store: &Store, sql: &'static str) {
    store
        .write(move |db| db.connection().execute_batch(sql).map_err(StoreError::from))
        .await
        .expect("SQL runs");
}

/// Holds the writer thread until the returned guard is dropped.
pub fn hold_writer(store: &Store) -> impl Drop {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    // Queued now; the job runs whether or not its future is awaited.
    drop(store.write(move |_| {
        let _ = rx.recv();
        Ok::<_, StoreError>(())
    }));
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    Release(Some(tx))
}

/// Records the sanction events.
#[derive(Default)]
pub struct Recorder {
    pub applied: Mutex<Vec<SanctionApplied>>,
    pub refunds_pending: AtomicUsize,
}

impl Recorder {
    pub fn new() -> Arc<Recorder> {
        Arc::new(Recorder::default())
    }

    pub fn applied(&self) -> Vec<SanctionApplied> {
        self.applied.lock().clone()
    }

    pub fn refunds_pending(&self) -> usize {
        self.refunds_pending.load(Ordering::SeqCst)
    }
}

impl SanctionEvents for Recorder {
    fn sanction_applied(&self, sanction: SanctionApplied) {
        self.applied.lock().push(sanction);
    }

    fn refunds_pending(&self) {
        self.refunds_pending.fetch_add(1, Ordering::SeqCst);
    }
}
