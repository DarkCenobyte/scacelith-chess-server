//! Port of store.games.test.js: the game commit (ratings, idempotence, atomicity, analysis queue
//! policy), the provisional rule, the leaderboard, the history queries and the analysis queue.
//! The FIDE rating tests of the Node suite exercise the Elo module (owned by matching); the store's
//! part of them (unrated-phase sums, counted games, the leaderboard filters) is tested here with a
//! scripted rating function.

use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use serde_json::json;

use super::support::*;
use crate::ids::UserId;
use crate::log::Logger;
use crate::store::{
    ANALYSED_FOR_USER_SQL, Color, CommitEntry, CommitRatings, ErrorKind, GameOutcome, GameRecord,
    IntegrityLevel, IntegrityUpdate, JobStatus, QueueStats, RatingChange, RatingFn, RatingRecord,
    SideOutcome, Store, StoreOptions, status,
};

const ID_BASE: u64 = 1_000_000_000_000;

/// Minimal Elo (K 40 while provisional, else 20), every game rated and counted.
fn elo(provisional_games: i64) -> RatingFn {
    Arc::new(move |w: &RatingRecord, b: &RatingRecord, score: f64| {
        let e = 1.0 / (1.0 + 10f64.powf((b.rating - w.rating) as f64 / 400.0));
        let side = |r: &RatingRecord, s: f64, exp: f64| {
            let k = if r.games < provisional_games { 40.0 } else { 20.0 };
            // JavaScript's Math.round (halves up), as the Node suite's expectations.
            let after = 100.max(r.rating + (k * (s - exp) + 0.5).floor() as i64);
            let mut rec = *r;
            rec.rating = after;
            rec.games += 1;
            rec.wins += i64::from(s == 1.0);
            rec.draws += i64::from(s == 0.5);
            rec.losses += i64::from(s == 0.0);
            rec.peak = rec.peak.max(after);
            rec.reached_senior |= after >= 2400;
            rec.rated = true;
            rec.counted_games += 1;
            SideOutcome { before: r.rating, after, k: Some(k as i64), record: rec }
        };
        GameOutcome { white: side(w, score, e), black: side(b, 1.0 - score, 1.0 - e) }
    })
}

/// Game records with increasing ids.
struct Gen(u64);

impl Gen {
    fn new() -> Gen {
        Gen(ID_BASE)
    }

    fn game(&mut self, white: UserId, black: UserId) -> GameRecord {
        self.with(white, black, |_| {})
    }

    fn with(&mut self, white: UserId, black: UserId, edit: impl FnOnce(&mut GameRecord)) -> GameRecord {
        self.0 += 1;
        let mut r = record(self.0, white, black);
        r.reason = 1;
        r.started_at = Some(1_800_000_000_000);
        r.ended_at = Some(1_800_000_600_000);
        r.flags = 0;
        set_plies(&mut r, 40);
        edit(&mut r);
        r
    }
}

fn set_plies(r: &mut GameRecord, plies: u16) {
    r.moves = (0..plies).map(|i| (i * 7) & 0x7fff).collect();
    r.spent_ms = Some((0..u32::from(plies)).map(|i| 1000 + i).collect());
    r.clock_ms = Some((0..u32::from(plies)).map(|i| 180_000 - i * 10).collect());
}

fn plies(n: u16) -> impl FnOnce(&mut GameRecord) {
    move |r| set_plies(r, n)
}

async fn setup_with(config: crate::config::Config, rating: Option<RatingFn>) -> (Store, [UserId; 4]) {
    let pg = config.provisional_games;
    let rating = rating.unwrap_or_else(|| elo(pg));
    let store = store_with(&config, StoreOptions { rating: Some(rating), ..StoreOptions::default() }).await;
    let mut ids = [0; 4];
    for (i, name) in ["Ann", "Ben", "Cid", "Dee"].iter().enumerate() {
        ids[i] = store.users().create(new_user(name, Some(&format!("{name}@example.org")))).await.unwrap();
    }
    (store, ids)
}

async fn setup() -> (Store, [UserId; 4]) {
    setup_with(config(), None).await
}

fn change(before: i64, after: i64, games: i64, provisional: bool) -> RatingChange {
    RatingChange { before, after, games, provisional }
}

fn ratings(e: &CommitEntry) -> CommitRatings {
    e.ratings.expect("a rated game")
}

#[tokio::test]
async fn finish_batch_writes_game_rows_and_ratings_read_and_written_in_the_transaction() {
    let (store, [a, b, c, _]) = setup().await;
    let mut mk = Gen::new();
    let g1 = mk.game(a, b);
    let g2 = mk.with(c, a, |g| (g.status, g.reason) = (status::DRAW, 2));
    let res = store.finish_batch(vec![g1.clone(), g2.clone()]).await.unwrap();
    assert_eq!(res.len(), 2);
    assert_eq!(
        res[0],
        CommitEntry {
            game_id: g1.id,
            duplicate: false,
            ratings: Some(CommitRatings {
                white: change(1500, 1520, 1, true),
                black: change(1500, 1480, 1, true)
            }),
            analysis_skipped: None,
            analysis_displaced: vec![],
        }
    );
    // The second game uses a's rating as updated by the first one, inside the same transaction.
    let r2 = ratings(&res[1]);
    assert_eq!(r2.black.before, 1520);
    assert_eq!(r2.white.before, 1500);
    assert_eq!(r2.black.games, 2);
    assert_eq!(r2.white.after - 1500, 1520 - r2.black.after, "zero-sum with equal K");

    let rec = |rating, games, wins, draws, losses, peak, counted| RatingRecord {
        rating,
        games,
        wins,
        draws,
        losses,
        peak,
        reached_senior: false,
        rated: true,
        counted_games: counted,
        unrated_games: 0,
        unrated_opponents: 0,
        unrated_half_points: 0,
    };
    let r = store.ratings();
    assert_eq!(r.get(a, "3+2".into()).await.unwrap(), rec(r2.black.after, 2, 1, 1, 0, 1520, 2));
    assert_eq!(r.get(b, "3+2".into()).await.unwrap(), rec(1480, 1, 0, 0, 1, 1500, 1));
    // No record yet: unrated, at INITIAL_RATING.
    assert_eq!(r.get(a, "5+0".into()).await.unwrap(), RatingRecord::initial(1500));
    let fu = r.for_user(a).await.unwrap();
    assert_eq!(fu.len(), 1);
    assert_eq!(fu[0].category, "3+2");
    assert!(fu[0].provisional);

    let g = store.games().by_id(g1.id).await.unwrap().unwrap();
    let s = &g.summary;
    assert_eq!((s.white_id, s.black_id), (a, b));
    assert_eq!(s.status, status::WHITE_WINS);
    assert_eq!(s.reason, 1);
    assert_eq!(s.ply_count, 40);
    assert!(s.rated);
    assert_eq!(s.rematch_of, None);
    assert_eq!((s.started_at, s.ended_at), (1_800_000_000_000, 1_800_000_600_000));
    let rc = s.rating_changes.unwrap();
    assert_eq!((rc.white.before, rc.white.after, rc.black.before, rc.black.after), (1500, 1520, 1500, 1480));
    assert_eq!(g.moves, g1.moves);
    assert_eq!(Some(g.spent_ms), g1.spent_ms);
    assert_eq!(Some(g.clock_ms), g1.clock_ms);
    assert_eq!(store.games().by_id(12345).await.unwrap(), None);

    // Missing times default to the commit time (the store clock); missing arrays read empty.
    let g3 = mk.with(a, b, |g| {
        (g.started_at, g.ended_at, g.spent_ms, g.clock_ms, g.rated) = (None, None, None, None, false)
    });
    store.finish_batch(vec![g3.clone()]).await.unwrap();
    let g = store.games().by_id(g3.id).await.unwrap().unwrap();
    assert!(g.summary.ended_at > 1_700_000_000_000 && g.summary.started_at == g.summary.ended_at);
    assert!(g.spent_ms.is_empty() && g.clock_ms.is_empty());
    store.close().await;
}

#[tokio::test]
async fn unrated_aborted_and_custom_games_change_no_rating_and_the_analysis_queue_rules() {
    let mut cfg = config();
    cfg.analysis_min_plies = 20;
    let (store, [a, b, ..]) = setup_with(cfg, None).await;
    let mut mk = Gen::new();
    let casual = mk.with(a, b, |g| g.rated = false);
    let aborted = mk.with(a, b, |g| {
        (g.status, g.reason) = (status::ABORTED, 9);
        set_plies(g, 1);
    });
    let custom = mk.with(a, b, |g| (g.category, g.base_ms) = ("custom".into(), 420_000));
    let short = mk.with(a, b, plies(19));
    let long = mk.with(b, a, |g| {
        (g.status, g.reason) = (status::BLACK_WINS, 4);
        set_plies(g, 20);
    });
    let res =
        store.finish_batch(vec![casual, aborted.clone(), custom.clone(), short, long.clone()]).await.unwrap();
    assert!(res[..3].iter().all(|r| r.ratings.is_none()));
    assert!(res[3].ratings.is_some() && res[4].ratings.is_some());
    let r = store.ratings();
    assert_eq!(r.get(a, "3+2".into()).await.unwrap().games, 2);
    assert_eq!(r.get(a, "3+2".into()).await.unwrap().wins, 2);
    assert_eq!(r.get(b, "3+2".into()).await.unwrap().losses, 2);
    assert_eq!(r.get(a, "custom".into()).await.unwrap().games, 0);
    assert_eq!(store.games().by_id(aborted.id).await.unwrap().unwrap().summary.ply_count, 1);
    assert_eq!(store.games().by_id(custom.id).await.unwrap().unwrap().summary.rating_changes, None);
    // Only the rated, played game with at least ANALYSIS_MIN_PLIES plies is queued.
    let jobs = store.analysis().next(10, Some("w".into()), 1).await.unwrap();
    assert_eq!(jobs.iter().map(|j| j.game_id).collect::<Vec<_>>(), vec![long.id]);
    store.close().await;
}

#[tokio::test]
async fn finish_batch_is_idempotent_and_reports_the_stored_change_of_a_recommitted_game() {
    let (store, [a, b, ..]) = setup().await;
    let mut mk = Gen::new();
    let g = mk.game(a, b);
    let first = store.finish_batch(vec![g.clone()]).await.unwrap();
    let again = store.finish_batch(vec![g.clone(), mk.game(b, a)]).await.unwrap();
    assert!(again[0].duplicate);
    // The duplicate is evaluated before the batch's new game: the player's current count is 1.
    assert_eq!(ratings(&again[0]).white, change(1500, 1520, 1, true));
    assert!(!again[1].duplicate);
    assert_eq!(ratings(&first[0]).white.after, 1520);
    assert_eq!(store.ratings().get(a, "3+2".into()).await.unwrap().games, 2, "only the new game was rated");
    assert_eq!(store.games().recent_for_user(a, 10, None).await.unwrap().len(), 2);
    assert_eq!(store.analysis().stats().await.unwrap().queued, 2);
    store.close().await;
}

#[tokio::test]
async fn last_id_and_a_game_id_stored_for_other_players_stays_a_duplicate_with_a_warning() {
    let logs = LogCapture::start();
    let logger = Logger::root().child("store-games-duplicate-test");
    let opts =
        StoreOptions { rating: Some(elo(30)), logger: Some(logger.clone()), ..StoreOptions::default() };
    let store = store_with(&config(), opts).await;
    let mut ids = Vec::new();
    for name in ["Ann", "Ben", "Cid"] {
        ids.push(store.users().create(new_user(name, Some(&format!("{name}@example.org")))).await.unwrap());
    }
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    assert_eq!(store.games().last_id().await.unwrap(), 0);
    let mut mk = Gen::new();
    let g = mk.game(a, b);
    let g2 = mk.game(b, a);
    store.finish_batch(vec![g.clone(), g2.clone()]).await.unwrap();
    assert_eq!(store.games().last_id().await.unwrap(), g2.id);
    store.finish_batch(vec![g.clone()]).await.unwrap(); // a crash recovery's re-commit
    let warned = |logs: &LogCapture| -> Vec<serde_json::Value> {
        logs.records(logger.component())
            .iter()
            .filter(|r| r["level"] == "warn")
            .map(|r| r["gameId"].clone())
            .collect()
    };
    assert!(warned(&logs).is_empty());
    let mut other = mk.game(a, c);
    other.id = g.id;
    let res = store.finish_batch(vec![other]).await.unwrap();
    assert!(res[0].duplicate);
    assert_eq!(warned(&logs), vec![json!(g.id)]);
    let stored = store.games().by_id(g.id).await.unwrap().unwrap();
    assert_eq!((stored.summary.white_id, stored.summary.black_id), (a, b), "the stored game is kept");
    store.close().await;
}

#[tokio::test]
async fn finish_batch_is_atomic_a_failing_record_rolls_the_whole_batch_back() {
    let (store, [a, b, ..]) = setup().await;
    let mut mk = Gen::new();
    let ok1 = mk.game(a, b);
    let bad = mk.game(a, 999_999); // unknown user: foreign key violation
    let invalid = mk.with(a, b, |g| g.status = 0);
    let e = store.finish_batch(vec![ok1.clone(), bad.clone()]).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ForeignKey);
    assert_eq!(e.code(), "foreign_key");
    assert_eq!(e.game_id(), Some(bad.id), "the record at fault");
    assert_eq!(store.games().by_id(ok1.id).await.unwrap(), None);
    assert_eq!(store.ratings().get(a, "3+2".into()).await.unwrap().games, 0);
    assert_eq!(store.analysis().stats().await.unwrap().queued, 0);
    let e = store.finish_batch(vec![ok1.clone(), invalid.clone()]).await.unwrap_err();
    assert_eq!((e.kind(), e.game_id()), (ErrorKind::InvalidRecord, Some(invalid.id)));
    let no_category = mk.with(a, b, |g| g.category.clear());
    let e = store.finish_batch(vec![no_category.clone()]).await.unwrap_err();
    assert_eq!((e.kind(), e.game_id()), (ErrorKind::InvalidRecord, Some(no_category.id)));
    let big_id = GameRecord { id: 1 << 53, ..ok1.clone() };
    assert_eq!(store.finish_batch(vec![big_id]).await.unwrap_err().kind(), ErrorKind::InvalidRecord);
    assert_eq!(store.games().by_id(ok1.id).await.unwrap(), None);

    // A panicking rating function also rolls back.
    let exploding: RatingFn = Arc::new(|_: &RatingRecord, _: &RatingRecord, _: f64| panic!("elo exploded"));
    let (s2, [x, y, ..]) = setup_with(config(), Some(exploding)).await;
    let casual = mk.with(x, y, |g| g.rated = false);
    let e = s2.finish_batch(vec![casual.clone(), mk.game(x, y)]).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(s2.games().by_id(casual.id).await.unwrap(), None);
    // The writer survives the panic.
    assert_eq!(s2.finish_batch(vec![casual.clone()]).await.unwrap()[0].game_id, casual.id);
    s2.close().await;

    // Without a rating function, rated games cannot be committed (unrated ones can).
    let s3 = store_with(&config(), StoreOptions::default()).await;
    let p = s3.users().create(new_user("P", Some("p@e.org"))).await.unwrap();
    let q = s3.users().create(new_user("Q", Some("q@e.org"))).await.unwrap();
    let e = s3.finish_batch(vec![mk.game(p, q)]).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NoRatingFunction);
    assert_eq!(e.code(), "no_rating_function");
    let unrated = mk.with(p, q, |g| g.rated = false);
    assert_eq!(s3.finish_batch(vec![unrated]).await.unwrap()[0].ratings, None);
    s3.close().await;

    // The store is still usable after a rollback.
    assert_eq!(store.finish_batch(vec![ok1.clone()]).await.unwrap()[0].game_id, ok1.id);
    assert!(store.finish_batch(vec![]).await.unwrap().is_empty());
    store.close().await;
}

#[tokio::test]
async fn ratings_over_many_games_provisional_flag_peak_and_leaderboard_filters() {
    let mut cfg = config();
    cfg.provisional_games = 3;
    let (store, [a, b, c, d]) = setup_with(cfg, None).await;
    let mut mk = Gen::new();
    let mut batch = Vec::new();
    for _ in 0..4 {
        batch.push(mk.game(a, b));
    }
    for _ in 0..3 {
        batch.push(mk.with(c, b, |g| g.status = status::DRAW));
    }
    batch.push(mk.with(d, c, |g| g.status = status::BLACK_WINS));
    let res = store.finish_batch(batch).await.unwrap();
    assert!(!ratings(&res[2]).white.provisional, "3 games played: established");
    assert!(ratings(&res[1]).white.provisional);
    let ra = store.ratings().get(a, "3+2".into()).await.unwrap();
    assert_eq!(ra.games, 4);
    assert_eq!(ra.peak, ra.rating);
    let rb = store.ratings().get(b, "3+2".into()).await.unwrap();
    assert_eq!((rb.peak, rb.games), (1500, 7));

    let board = |limit, min| store.ratings().leaderboard("3+2".into(), limit, Some(min));
    let users = |rows: Vec<crate::store::LeaderboardRow>| rows.iter().map(|r| r.user_id).collect::<Vec<_>>();
    let rows = board(100, 3).await.unwrap();
    assert_eq!(rows[0].username, "Ann");
    assert_eq!(users(rows), vec![a, c, b], "d has 1 game only; sorted by rating");
    assert_eq!(users(board(1, 3).await.unwrap()), vec![a]);
    assert_eq!(users(store.ratings().leaderboard("3+2".into(), 100, None).await.unwrap()), vec![a, c, b]);
    assert!(store.ratings().leaderboard("5+0".into(), 100, Some(0)).await.unwrap().is_empty());
    let confirmed =
        IntegrityUpdate { level: Some(IntegrityLevel::Confirmed), score: Some(9.0), ..Default::default() };
    store.integrity().set(a, confirmed).await.unwrap();
    store.users().anonymize(c, 1).await.unwrap();
    assert_eq!(
        users(board(100, 3).await.unwrap()),
        vec![b],
        "confirmed cheaters and deleted accounts are hidden"
    );
    store.close().await;
}

#[tokio::test]
async fn provisional_is_unrated_or_fewer_counted_games_than_provisional_games_at_commit_recommit_and_for_user()
 {
    for pg in [0i64, 1, 10, 30, 100] {
        // The rating function hands back the record under test for both sides.
        let next = Arc::new(Mutex::new(RatingRecord::initial(1500)));
        let n = next.clone();
        let fixed: RatingFn = Arc::new(move |w: &RatingRecord, b: &RatingRecord, _| {
            let rec = *n.lock();
            GameOutcome {
                white: SideOutcome { before: w.rating, after: 1500, k: None, record: rec },
                black: SideOutcome { before: b.rating, after: 1500, k: None, record: rec },
            }
        });
        let mut cfg = config();
        cfg.provisional_games = pg;
        let (store, [a, b, ..]) = setup_with(cfg, Some(fixed)).await;
        let mut mk = Gen::new();
        let mut counts = vec![0, pg - 1, pg, pg + 1];
        counts.retain(|&n| n >= 0);
        counts.dedup();
        for counted in counts {
            for rated in [false, true] {
                *next.lock() = RatingRecord {
                    games: counted + 3,
                    rated,
                    counted_games: counted,
                    ..RatingRecord::initial(1500)
                };
                let expected = !rated || counted < pg;
                let why = format!("PROVISIONAL_GAMES {pg}, {counted} counted, rated {rated}");
                let g = mk.game(a, b);
                let res = store.finish_batch(vec![g.clone()]).await.unwrap();
                assert_eq!(ratings(&res[0]).white.provisional, expected, "{why}");
                assert_eq!(ratings(&res[0]).black.provisional, expected, "{why}");
                let again = store.finish_batch(vec![g]).await.unwrap();
                assert_eq!(ratings(&again[0]).white.provisional, expected, "{why}, re-commit");
                assert_eq!(
                    store.ratings().for_user(a).await.unwrap()[0].provisional,
                    expected,
                    "{why}, for_user"
                );
            }
        }
        store.close().await;
    }
}

#[tokio::test]
async fn unrated_phase_sums_are_stored_until_rated_and_unrated_records_stay_off_the_leaderboard() {
    // A scripted rating function: unrated for the first three games of a player (each summing the
    // opponent's rating and the score), rated from the fourth on; only games against rated
    // opponents count.
    let scripted: RatingFn = Arc::new(|w: &RatingRecord, b: &RatingRecord, score: f64| {
        let side = |me: &RatingRecord, opp: &RatingRecord, s: f64| {
            let mut rec = *me;
            rec.games += 1;
            rec.unrated_games += 1;
            rec.unrated_opponents += opp.rating;
            rec.unrated_half_points += (s * 2.0) as i64;
            if rec.games >= 4 {
                rec.rated = true;
                rec.rating += 100;
            }
            if opp.rated {
                rec.counted_games += 1;
            }
            SideOutcome { before: me.rating, after: rec.rating, k: Some(0), record: rec }
        };
        GameOutcome { white: side(w, b, score), black: side(b, w, 1.0 - score) }
    });
    let mut cfg = config();
    cfg.provisional_games = 1;
    let (store, [a, b, ..]) = setup_with(cfg, Some(scripted)).await;
    let mut mk = Gen::new();
    store
        .finish_batch(vec![mk.with(a, b, |g| g.status = status::DRAW), mk.game(b, a), mk.game(a, b)])
        .await
        .unwrap();
    let ra = store.ratings().get(a, "3+2".into()).await.unwrap();
    assert_eq!(
        (ra.rated, ra.games, ra.unrated_games, ra.unrated_opponents, ra.unrated_half_points),
        (false, 3, 3, 4500, 3)
    );
    assert!(store.ratings().leaderboard("3+2".into(), 100, Some(0)).await.unwrap().is_empty(), "unrated");
    store.finish_batch(vec![mk.game(a, b)]).await.unwrap();
    let ra = store.ratings().get(a, "3+2".into()).await.unwrap();
    assert_eq!(
        (ra.rated, ra.rating, ra.unrated_games, ra.unrated_opponents, ra.unrated_half_points),
        (true, 1600, 0, 0, 0)
    );
    // Rated, but no game counted yet (the opponent was unrated): off a board that needs one.
    assert_eq!(ra.counted_games, 0);
    assert!(store.ratings().leaderboard("3+2".into(), 100, None).await.unwrap().is_empty());
    let all = store.ratings().leaderboard("3+2".into(), 100, Some(0)).await.unwrap();
    assert_eq!(all.iter().map(|r| r.user_id).collect::<Vec<_>>(), vec![a, b]);
    store.finish_batch(vec![mk.game(a, b)]).await.unwrap();
    let one = store.ratings().leaderboard("3+2".into(), 100, None).await.unwrap();
    assert_eq!(one.len(), 2, "both counted a game against a rated opponent");
    assert!(!store.ratings().for_user(a).await.unwrap()[0].provisional);
    store.close().await;
}

#[tokio::test]
async fn recent_for_user_pagination_count_between_count_for_user() {
    let (store, [a, b, c, _]) = setup().await;
    let mut mk = Gen::new();
    let batch: Vec<GameRecord> = (0..25)
        .map(|i| {
            let (w, bl) = if i % 2 == 1 { (a, b) } else { (c, a) };
            mk.with(w, bl, |g| (g.rated, g.ended_at) = (false, Some(1000 + i)))
        })
        .collect();
    store.finish_batch(batch.clone()).await.unwrap();
    let games = store.games();
    let page1 = games.recent_for_user(a, 10, None).await.unwrap();
    let expected: Vec<u64> = batch[15..].iter().rev().map(|g| g.id).collect();
    assert_eq!(page1.iter().map(|g| g.id).collect::<Vec<_>>(), expected);
    let page2 = games.recent_for_user(a, 10, Some(page1[9].id)).await.unwrap();
    let page3 = games.recent_for_user(a, 10, Some(page2[9].id)).await.unwrap();
    assert_eq!(page3.len(), 5);
    assert_eq!(page3[4].id, batch[0].id);
    assert_eq!(games.recent_for_user(b, 50, None).await.unwrap().len(), 12);
    assert_eq!(games.count_between(a, b, 0, false).await.unwrap(), 12);
    assert_eq!(games.count_between(b, a, 0, false).await.unwrap(), 12);
    assert_eq!(games.count_between(a, c, 1020, false).await.unwrap(), 3);
    assert_eq!(games.count_between(a, c, 0, true).await.unwrap(), 0);
    assert_eq!(games.count_for_user(a, None).await.unwrap(), 25);
    assert_eq!(games.count_for_user(99, None).await.unwrap(), 0);
    store.close().await;
}

fn w(name: &str) -> Option<String> {
    Some(name.into())
}

fn stats(queued: i64, running: i64, done: i64, failed: i64) -> QueueStats {
    QueueStats { queued, running, done, failed }
}

#[tokio::test]
async fn analysis_queue_claim_complete_fail_with_attempt_cap_stale_requeue_for_user_enqueue() {
    let (store, [a, b, ..]) = setup().await;
    let mut mk = Gen::new();
    let gs = vec![mk.game(a, b), mk.game(b, a), mk.game(a, b)];
    store.finish_batch(gs.clone()).await.unwrap();
    let q = store.analysis();
    let t = 1_900_000_000_000;
    let j1 = q.next(2, w("w1"), t).await.unwrap();
    assert_eq!(j1.iter().map(|j| j.game_id).collect::<Vec<_>>(), vec![gs[0].id, gs[1].id]);
    assert_eq!(j1[0].attempts, 1);
    assert_eq!(j1[0].worker.as_deref(), Some("w1"));
    assert_eq!(j1[0].started_at, Some(t));
    let j2 = q.next(5, w("w2"), t).await.unwrap();
    assert_eq!(j2.iter().map(|j| j.game_id).collect::<Vec<_>>(), vec![gs[2].id]);
    assert!(q.next(5, w("w2"), t).await.unwrap().is_empty());
    assert!(q.complete(gs[0].id, Some(json!({"acpl": 23.5, "moves": 20})), t).await.unwrap());
    assert_eq!(q.fail(gs[1].id, Some("engine crashed".into()), t).await.unwrap(), Some(JobStatus::Queued));
    assert_eq!(q.stats().await.unwrap(), stats(1, 1, 1, 0));
    // Retried until the cap.
    assert_eq!(q.next(1, w("w1"), t).await.unwrap()[0].attempts, 2);
    assert_eq!(q.fail(gs[1].id, Some("again".into()), t).await.unwrap(), Some(JobStatus::Queued));
    assert_eq!(q.next(1, w("w1"), t).await.unwrap()[0].attempts, 3);
    assert_eq!(q.fail(gs[1].id, Some("third".into()), t).await.unwrap(), Some(JobStatus::Failed));
    let job = q.job(gs[1].id).await.unwrap().unwrap();
    assert_eq!(
        (job.status, job.error.as_deref(), job.finished_at),
        (JobStatus::Failed, Some("third"), Some(t))
    );
    assert!(q.next(5, w("w1"), t).await.unwrap().is_empty());
    assert_eq!(q.fail(424_242, Some("x".into()), t).await.unwrap(), None);
    // gs[2] is running for w2 since t: 10 minutes later it goes back to the queue.
    assert!(q.next(5, w("w3"), t + 5 * 60_000).await.unwrap().is_empty());
    let again = q.next(5, w("w3"), t + 11 * 60_000).await.unwrap();
    let got: Vec<_> = again.iter().map(|j| (j.game_id, j.attempts, j.worker.clone())).collect();
    assert_eq!(got, vec![(gs[2].id, 2, w("w3"))]);
    let mine = q.for_user(a, 10, false).await.unwrap();
    assert_eq!(mine.len(), 3);
    assert_eq!(mine[0].game_id, gs[2].id);
    let done = mine.iter().find(|j| j.game_id == gs[0].id).unwrap();
    assert_eq!(done.features, Some(json!({"acpl": 23.5, "moves": 20})));
    assert_eq!(done.color, Color::White);
    assert_eq!(done.category, "3+2");
    assert_eq!(done.ply_count, 40);
    assert_eq!(mine.iter().find(|j| j.game_id == gs[1].id).unwrap().color, Color::Black);
    q.enqueue(gs[1].id, t).await.unwrap();
    assert_eq!(q.next(5, w("w9"), t + 11 * 60_000).await.unwrap()[0].attempts, 1);
    assert_eq!(q.enqueue(55_555, t).await.unwrap_err().kind(), ErrorKind::ForeignKey);
    // The error text is bounded.
    q.enqueue(gs[0].id, t).await.unwrap();
    q.next(5, w("w9"), t + 11 * 60_000).await.unwrap();
    q.fail(gs[0].id, Some("e".repeat(5000)), t).await.unwrap();
    assert_eq!(q.job(gs[0].id).await.unwrap().unwrap().error.unwrap().len(), 2000);
    store.close().await;
}

#[tokio::test]
async fn analysis_touch_a_job_its_worker_keeps_renewing_is_not_taken_for_stale() {
    let (store, [a, b, ..]) = setup().await;
    let mut mk = Gen::new();
    let gs = vec![mk.game(a, b), mk.game(b, a)];
    store.finish_batch(gs.clone()).await.unwrap();
    let q = store.analysis();
    let t = 1_900_000_000_000;
    let claimed = q.next(2, w("w1"), t).await.unwrap();
    assert_eq!(claimed.iter().map(|j| j.game_id).collect::<Vec<_>>(), vec![gs[0].id, gs[1].id]);
    // gs[0] is renewed 9 minutes after the claim, gs[1] is not (and only its worker renews a job).
    assert!(q.touch(gs[0].id, w("w1"), t + 9 * 60_000).await.unwrap());
    assert!(!q.touch(gs[1].id, w("w2"), t + 9 * 60_000).await.unwrap());
    let again = q.next(5, w("w2"), t + 11 * 60_000).await.unwrap();
    let got: Vec<_> = again.iter().map(|j| (j.game_id, j.attempts, j.worker.clone())).collect();
    assert_eq!(got, vec![(gs[1].id, 2, w("w2"))]);
    assert!(!q.touch(gs[1].id, w("w1"), t + 11 * 60_000).await.unwrap(), "claimed by another worker since");
    assert!(q.complete(gs[0].id, Some(json!({"n": 1})), t + 12 * 60_000).await.unwrap());
    assert!(!q.touch(gs[0].id, w("w1"), t + 12 * 60_000).await.unwrap(), "done");
    store.close().await;
}

#[tokio::test]
async fn analysis_for_user_done_only_lists_the_latest_analysed_games_and_reads_the_players_games() {
    let (store, [a, b, ..]) = setup().await;
    let mut mk = Gen::new();
    let gs: Vec<GameRecord> =
        (0..40).map(|i| if i % 2 == 1 { mk.game(b, a) } else { mk.game(a, b) }).collect();
    store.finish_batch(gs.clone()).await.unwrap();
    let q = store.analysis();
    let t = 1_900_000_000_000;
    // The 25 oldest games are analysed, the next one failed for good, the 14 newest still wait.
    for (i, g) in gs[..26].iter().enumerate() {
        let job = q.next(1, w("w"), t).await.unwrap().remove(0);
        assert_eq!(job.game_id, g.id);
        if i == 25 {
            for k in 0..3 {
                q.fail(g.id, Some("engine crashed".into()), t).await.unwrap();
                if k < 2 {
                    q.next(1, w("w"), t).await.unwrap();
                }
            }
        } else {
            let features = json!({"gameId": g.id, "white": {"userId": g.white_id, "n": 20}});
            q.complete(g.id, Some(features), t).await.unwrap();
        }
    }
    assert_eq!(q.stats().await.unwrap(), stats(14, 0, 25, 1));
    assert_eq!(q.for_user(a, 30, false).await.unwrap().len(), 30, "every status");
    let done = q.for_user(a, 30, true).await.unwrap();
    let expected: Vec<u64> = gs[..25].iter().rev().map(|g| g.id).collect();
    assert_eq!(done.iter().map(|j| j.game_id).collect::<Vec<_>>(), expected);
    assert!(done.iter().all(|j| j.status == JobStatus::Done));
    assert_eq!(q.for_user(a, 10, true).await.unwrap().len(), 10);
    // The plan walks the player's games newest first, never every done job of the server.
    let plan = store
        .explain_query_plan(ANALYSED_FOR_USER_SQL, vec![SqlValue::Integer(a.into()), SqlValue::Integer(30)])
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(plan.contains("SEARCH a USING INTEGER PRIMARY KEY"), "{plan}");
    assert!(!plan.contains("analysis_jobs_queue") && !plan.contains("TEMP B-TREE FOR ORDER BY"), "{plan}");
    store.close().await;
}

/// Two stores (each with its own writer connection) on one file claim jobs concurrently: no job is
/// claimed twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analysis_next_two_workers_with_their_own_connections_never_claim_the_same_job() {
    let dir = TempDir::new("jobs");
    let opts = StoreOptions { rating: Some(elo(30)), ..options(Some(dir.file("jobs.db"))) };
    let store = file_store_with(&dir, &config(), opts).await;
    let a = store.users().create(new_user("Ja", Some("ja@e.org"))).await.unwrap();
    let b = store.users().create(new_user("Jb", Some("jb@e.org"))).await.unwrap();
    let mut mk = Gen::new();
    let batch: Vec<GameRecord> = (0..200)
        .map(|i| mk.with(a, b, |g| g.status = if i % 3 == 0 { status::DRAW } else { status::WHITE_WINS }))
        .collect();
    store.finish_batch(batch).await.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for name in ["w1", "w2"] {
        let worker = Store::open(&config(), options(Some(dir.file("jobs.db")))).await.unwrap();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut got = Vec::new();
            loop {
                let jobs = worker.analysis().next(3, w(name), 1).await.unwrap();
                if jobs.is_empty() {
                    break;
                }
                for j in jobs {
                    got.push(j.game_id);
                    worker.analysis().complete(j.game_id, Some(json!({"by": name})), 2).await.unwrap();
                }
            }
            worker.close().await;
            got
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    assert_eq!(all.len(), 200);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 200, "no job claimed twice");
    assert_eq!(store.analysis().stats().await.unwrap(), stats(0, 0, 200, 0));
    store.close().await;
}
