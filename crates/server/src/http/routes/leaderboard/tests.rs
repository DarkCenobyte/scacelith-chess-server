//! The leaderboard, ported from the Node suite `store.routes`: official categories only,
//! established players, both spellings of "3+2", the 10 s cache; and one read per category for a
//! burst of requests on an empty or an old board.

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::super::read_support::*;
use super::*;
use crate::http::testing::TestResponse;
use crate::store::{ErrorKind, GameRecord, LeaderboardRow, StoreError};

const NOW: i64 = 1_790_856_000_000;

async fn setup() -> Server {
    let config = config(&[("PROVISIONAL_GAMES", "2"), ("RATED_CATEGORIES", "3+2,5+0")]);
    let store = memory_store(&config).await;
    let s = store.clone();
    Server::start(config, store, NOW, move |router, config| {
        register(
            router,
            LeaderboardDeps { config: config.clone(), store: s, log: test_logger("leaderboard-test") },
        );
    })
}

fn game(id: u64, white: &Player, black: &Player) -> GameRecord {
    record(id, white, black, moves_of("e2e4 e7e5"))
}

fn ranks(body: &Value) -> Vec<(i64, String, i64)> {
    body["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["rank"].as_i64().unwrap(),
                p["username"].as_str().unwrap().to_string(),
                p["rating"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn official_categories_only_established_players_plus_spellings_and_the_cache() {
    let s = setup().await;
    let [gil, hal, ivy, jon] =
        [s.user("Gil").await, s.user("Hal").await, s.user("Ivy").await, s.user("Jon").await];
    s.commit(vec![
        game(1, &gil, &hal),
        game(2, &gil, &hal),
        game(3, &gil, &ivy),
        game(4, &ivy, &hal),
        game(5, &jon, &gil),
    ])
    .await;
    // Gil 1500+10+10+10-10 = 1520 (4 games), Hal 1470 (3), Ivy 1500 (2), Jon 1510 (1 game: not yet).
    let get = |q: &str| s.t.get(&format!("/api/v1/leaderboard{q}")).send();
    let r = get("?category=3+2").await;
    assert_eq!(r.status, 200);
    let body = r.json();
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["category", "minGames", "updatedAt", "players"]);
    assert_eq!((body["category"].clone(), body["minGames"].clone()), (json!("3+2"), json!(2)));
    assert_eq!(body["updatedAt"], NOW);
    assert_eq!(ranks(&body), [(1, "Gil".into(), 1520), (2, "Ivy".into(), 1500), (3, "Hal".into(), 1470)]);
    assert_eq!(
        body["players"][0],
        json!({ "rank": 1, "username": "Gil", "rating": 1520, "games": 4, "wins": 3, "draws": 0, "losses": 1, "peak": 1530 })
    );
    let r = get("?category=3%2B2&limit=1").await.json();
    assert_eq!(r["players"].as_array().unwrap().len(), 1);
    assert_eq!(get("?category=%203%202%20").await.json()["category"], "3+2", "trimmed");
    let r = get("?category=4%2B2").await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({ "error": "invalid_category", "message": "category must be one of: 3+2, 5+0." }))
    );
    assert_eq!(get("?category=custom").await.status, 400);
    assert_eq!(get("").await.status, 400);
    assert_eq!(get("?category=").await.status, 400);
    let r = get("?category=3%2B2&limit=0").await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({ "error": "invalid_limit", "message": "limit must be 1 to 100." }))
    );
    assert_eq!(get("?category=3%2B2&limit=x").await.status, 400);
    assert_eq!(
        get("?category=3%2B2&limit=500").await.json()["players"].as_array().unwrap().len(),
        3,
        "capped at 100"
    );
    assert_eq!(get("?category=5%2B0").await.json()["players"], json!([]));

    // Cached for 10 seconds: new results do not show at once.
    s.commit(vec![game(6, &jon, &hal), game(7, &jon, &hal)]).await;
    s.advance(10_000);
    let r = get("?category=3%2B2").await.json();
    assert_eq!(ranks(&r).len(), 3);
    assert_eq!(r["updatedAt"], NOW, "the cached board");
    s.advance(1);
    let r = get("?category=3%2B2").await.json();
    assert_eq!(ranks(&r).len(), 4, "read again after 10 s");
    assert_eq!(r["updatedAt"], NOW + 10_001);
    assert_eq!(r["players"][0]["username"], "Jon");
    // A session is not read (the route takes none).
    assert_eq!(s.t.get("/api/v1/leaderboard?category=3%2B2").bearer("sct_nobody").send().await.status, 200);
}

/// The board reads of a test: each is counted and waits for the gate of the moment to open (a
/// slow database), then answers one player whose rating is 1500 + the number of reads so far, or
/// fails with `fail`.
struct SlowReads {
    reads: Mutex<Vec<String>>,
    /// Open once closed: a closed semaphore lets every acquire through at once.
    gate: Mutex<Arc<Semaphore>>,
    fail: Mutex<Option<ErrorKind>>,
}

impl SlowReads {
    fn new() -> Arc<SlowReads> {
        Arc::new(SlowReads {
            reads: Mutex::default(),
            gate: Mutex::new(Arc::new(Semaphore::new(0))),
            fail: Mutex::default(),
        })
    }

    /// Lets the waiting reads, and the next ones, through.
    fn open(&self) {
        self.gate.lock().close();
    }

    /// Makes the next reads wait again.
    fn shut(&self) {
        *self.gate.lock() = Arc::new(Semaphore::new(0));
    }

    /// The reads of `category` so far.
    fn count(&self, category: &str) -> usize {
        self.reads.lock().iter().filter(|c| *c == category).count()
    }

    fn reader(self: &Arc<Self>) -> ReadRows {
        let reads = self.clone();
        Arc::new(move |category: String| {
            let reads = reads.clone();
            Box::pin(async move {
                let n = {
                    let mut list = reads.reads.lock();
                    list.push(category);
                    list.len()
                };
                let gate = reads.gate.lock().clone();
                let _ = gate.acquire().await;
                if let Some(kind) = *reads.fail.lock() {
                    return Err(StoreError::new(kind, "the test's failure"));
                }
                Ok(vec![LeaderboardRow {
                    user_id: 1,
                    username: "Gil".into(),
                    rating: 1500 + n as i64,
                    games: 4,
                    wins: 3,
                    draws: 0,
                    losses: 1,
                    peak: 1530,
                }])
            })
        })
    }
}

fn slow_server(reads: &Arc<SlowReads>, store: Store) -> Server {
    let config = config(&[("PROVISIONAL_GAMES", "2"), ("RATED_CATEGORIES", "3+2,5+0")]);
    let (s, read_rows) = (store.clone(), reads.reader());
    Server::start(config, store, NOW, move |router, config| {
        let deps = LeaderboardDeps { config: config.clone(), store: s, log: test_logger("leaderboard-test") };
        register_with(router, deps, read_rows);
    })
}

/// `n` requests of each category at once, each in a task of its own; the abort handles of the
/// first one of each category (the request that starts its read).
fn burst(s: &Server, n: usize) -> (JoinSet<TestResponse>, Vec<tokio::task::AbortHandle>) {
    let (mut set, mut first) = (JoinSet::new(), Vec::new());
    for category in ["3%2B2", "5%2B0"] {
        for i in 0..n {
            let (t, path) = (s.t.clone(), format!("/api/v1/leaderboard?category={category}"));
            let handle = set.spawn(async move { t.get(&path).send().await });
            if i == 0 {
                first.push(handle);
            }
        }
    }
    (set, first)
}

/// Lets the spawned requests run until each one answered or waits (one thread: they run in turn).
async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

/// `(category, updatedAt, rating)` of an answer.
fn seen(r: &TestResponse) -> (String, i64, i64) {
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    (
        b["category"].as_str().unwrap().to_owned(),
        b["updatedAt"].as_i64().unwrap(),
        b["players"][0]["rating"].as_i64().unwrap(),
    )
}

#[tokio::test]
async fn a_burst_on_an_empty_then_an_old_board_reads_each_category_once() {
    let reads = SlowReads::new();
    let config = config(&[]);
    let s = slow_server(&reads, memory_store(&config).await);

    // No board yet: everyone waits for the one read of their category.
    let (mut set, first) = burst(&s, 20);
    settle().await;
    assert_eq!((reads.count("3+2"), reads.count("5+0")), (1, 1));
    assert!(set.try_join_next().is_none(), "every request waits for the read");
    // The requests that started the reads go away: the reads go on for the others.
    for handle in first {
        handle.abort();
    }
    reads.open();
    let mut answers = Vec::new();
    while let Some(r) = set.join_next().await {
        match r {
            Ok(r) => answers.push(seen(&r)),
            Err(e) => assert!(e.is_cancelled()),
        }
    }
    assert_eq!(answers.len(), 38);
    let first_read = |c: &str| answers.iter().find(|a| a.0 == c).unwrap().2;
    let (r32, r50) = (first_read("3+2"), first_read("5+0"));
    assert!(answers.iter().all(|a| a.1 == NOW && a.2 == if a.0 == "3+2" { r32 } else { r50 }), "{answers:?}");
    assert_eq!((reads.count("3+2"), reads.count("5+0")), (1, 1));

    // An old board: one request per category reads it again and waits for it, the others get
    // the old board meanwhile.
    reads.shut();
    s.advance(BOARD_CACHE_MS + 1);
    let (mut set, _) = burst(&s, 20);
    settle().await;
    assert_eq!((reads.count("3+2"), reads.count("5+0")), (2, 2));
    let mut old = Vec::new();
    while let Some(r) = set.try_join_next() {
        old.push(seen(&r.unwrap()));
    }
    assert_eq!(old.len(), 38, "all but the two that read");
    assert!(old.iter().all(|a| a.1 == NOW && a.2 == if a.0 == "3+2" { r32 } else { r50 }), "{old:?}");
    reads.open();
    let mut new = Vec::new();
    while let Some(r) = set.join_next().await {
        new.push(seen(&r.unwrap()));
    }
    new.sort();
    assert_eq!(
        new.iter().map(|a| (a.0.as_str(), a.1)).collect::<Vec<_>>(),
        [("3+2", NOW + 10_001), ("5+0", NOW + 10_001)]
    );
    assert!(new.iter().all(|a| a.2 > 1502), "the new reads: {new:?}");

    // The new board is fresh: no read.
    let r = s.t.get("/api/v1/leaderboard?category=3%2B2").send().await;
    assert_eq!(seen(&r), new[0]);
    assert_eq!((reads.count("3+2"), reads.count("5+0")), (2, 2));
}

#[tokio::test]
async fn a_failed_read_answers_the_requests_that_waited_for_it_and_the_next_request_reads_again() {
    let reads = SlowReads::new();
    *reads.fail.lock() = Some(ErrorKind::Busy);
    let config = config(&[]);
    let s = slow_server(&reads, memory_store(&config).await);
    let (mut set, _) = burst(&s, 5);
    settle().await;
    reads.open();
    while let Some(r) = set.join_next().await {
        let r = r.unwrap();
        assert_eq!((r.status, r.json()["error"].clone()), (503, json!("busy")));
    }
    assert_eq!((reads.count("3+2"), reads.count("5+0")), (1, 1));
    let get = || s.t.get("/api/v1/leaderboard?category=3%2B2").send();
    *reads.fail.lock() = Some(ErrorKind::Sqlite);
    assert_eq!(get().await.status, 500);
    *reads.fail.lock() = None;
    let r = get().await;
    assert_eq!(seen(&r), ("3+2".into(), NOW, 1504), "the 4th read");
    assert_eq!(reads.count("3+2"), 3);
}
