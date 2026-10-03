//! The leaderboard, ported from the Node suite `store.routes`: official categories only,
//! established players, both spellings of "3+2", the 10 s cache.

use serde_json::{Value, json};

use super::super::read_support::*;
use super::*;
use crate::store::GameRecord;

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
