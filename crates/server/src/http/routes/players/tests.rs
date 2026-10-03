//! The player routes, ported from the Node suite `store.routes` (registration, profiles, recent
//! games) through the real pipeline and an in-memory store.

use std::sync::Arc;

use serde_json::{Value, json};

use super::super::games::{self, GamesDeps};
use super::super::leaderboard::{self, LeaderboardDeps};
use super::super::read_support::*;
use super::*;
use crate::http::AuthMode;
use crate::store::{GameRecord, status};

/// 2026-09-28T12:00:00Z.
const STARTED: i64 = 1_790_596_800_000;

/// The five public read routes of the Node `players.js`.
fn register_all(router: &mut Router, config: &Arc<Config>, store: &Store) {
    let log = test_logger("players-test");
    register(router, PlayersDeps { config: config.clone(), store: store.clone(), log: log.clone() });
    let desk = Arc::new(StoreDesk { store: store.clone(), reports_per_day: config.reports_per_day });
    games::register(
        router,
        GamesDeps { config: config.clone(), store: store.clone(), reports: desk, log: log.clone() },
    );
    leaderboard::register(router, LeaderboardDeps { config: config.clone(), store: store.clone(), log });
}

async fn setup() -> Server {
    let config = config(&[
        ("PROVISIONAL_GAMES", "2"),
        ("SERVER_NAME", "Test Server"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
        ("HTTP_RATE_PER_IP", "100000"),
    ]);
    let store = memory_store(&config).await;
    let s = store.clone();
    Server::start(config, store, STARTED + 3_600_000, move |router, config| register_all(router, config, &s))
}

/// The game of the Node suite: 1. e4 e5 2. Nf3 Nc6, White wins by resignation.
fn game(id: u64, white: &Player, black: &Player) -> GameRecord {
    let mut g = record(id, white, black, vec![12 | (28 << 6), 52 | (36 << 6), 6 | (21 << 6), 57 | (42 << 6)]);
    g.started_at = Some(STARTED);
    g.ended_at = Some(STARTED + 600_000);
    g.spent_ms = Some(vec![0, 0, 1500, 2100]);
    g.clock_ms = Some(vec![180_000, 180_000, 180_500, 179_900]);
    g
}

#[tokio::test]
async fn registers_the_public_routes_with_an_optional_session_and_one_shared_limit() {
    let config = Arc::new(Config::for_tests());
    let store = memory_store(&config).await;
    let mut router = Router::new();
    register_all(&mut router, &config, &store);
    let routes: Vec<String> =
        router.routes().iter().map(|r| format!("{} {:?}", r.label(), r.opts().auth)).collect();
    assert_eq!(
        routes,
        [
            "GET /api/v1/players/:username Optional",
            "GET /api/v1/players/:username/games Optional",
            "GET /api/v1/games/:id Optional",
            "GET /api/v1/games/:id/pgn Optional",
            "GET /api/v1/leaderboard None",
        ]
    );
    let rates: Vec<_> = router.routes().iter().flat_map(|r| r.opts().rates.clone()).collect();
    assert_eq!(rates.len(), 4, "the leaderboard has no limit of its own");
    for rate in rates {
        assert_eq!(
            (rate.key.as_ref(), rate.limit, rate.window_ms, rate.by_user),
            ("public_read", 60.0, 60_000, true)
        );
        assert!(!rate.shared);
    }
    assert_eq!(router.routes()[4].opts().auth, AuthMode::None);
}

#[tokio::test]
async fn profile_public_data_only_ratings_in_config_order_counts_validation_and_404s() {
    let s = setup().await;
    let alice = s.user("Alice").await;
    let bob = s.user("Bob").await;
    let mut draw = game(2_000_000_000_002, &bob, &alice);
    draw.status = status::DRAW;
    let mut five = game(2_000_000_000_003, &alice, &bob);
    (five.category, five.base_ms, five.inc_ms) = ("5+0".into(), 300_000, 0);
    let mut casual = game(2_000_000_000_004, &alice, &bob);
    casual.rated = false;
    s.commit(vec![game(2_000_000_000_001, &alice, &bob), draw, five, casual]).await;

    let res = s.t.get("/api/v1/players/alice").send().await;
    assert_eq!(res.status, 200);
    let body = res.json();
    assert_eq!(body["username"], "Alice");
    assert_eq!(body["createdAt"], 1000);
    let ratings: Vec<(Value, Value, Value)> = body["ratings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["category"].clone(), r["games"].clone(), r["provisional"].clone()))
        .collect();
    assert_eq!(ratings, [(json!("3+2"), json!(2), json!(false)), (json!("5+0"), json!(1), json!(true))]);
    assert_eq!(body["ratings"][0]["rating"], 1510, "won 10, drew 0");
    let keys: Vec<&str> = body["ratings"][0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["category", "rating", "provisional", "games", "wins", "draws", "losses", "peak"]);
    assert_eq!(body["games"], json!({ "total": 4, "rated": 3, "wins": 2, "draws": 1, "losses": 0 }));
    let top: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(top, ["username", "createdAt", "ratings", "games"]);
    for secret in ["email", "example.org", "password", "hash", "integrity", "sanction"] {
        assert!(!res.text().contains(secret), "no {secret} in the profile");
    }

    let get = |name: &str| s.t.get(&format!("/api/v1/players/{name}")).send();
    let r = get("nobody").await;
    assert_eq!((r.status, r.json()), (404, json!({ "error": "not_found", "message": "No such player." })));
    let r = get("a").await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({ "error": "invalid_username", "message": "Invalid username." }))
    );
    assert_eq!(get(&"x".repeat(25)).await.status, 400);
    assert_eq!(get("bob'%3B%20DROP%20TABLE%20users%3B--").await.status, 400);
    assert_eq!(get("Al%2569ce").await.json()["username"], "Alice", "percent-decoded once more");
    assert_eq!(get("Al%69ce").await.json()["username"], "Alice");
    assert_eq!(get("%25E0%25A4%25A").await.status, 400, "a malformed escape");
    s.store.users().anonymize(bob.id, 5_000).await.unwrap();
    assert_eq!(get("bob").await.status, 404, "a deleted account has no profile");
    assert_eq!(get(&format!("deleted%23{}", bob.id)).await.status, 400);
    // A session changes nothing.
    let signed = s.t.get("/api/v1/players/alice").bearer(&alice.token).send().await;
    assert_eq!(signed.json(), body);
}

#[tokio::test]
async fn recent_games_newest_first_before_cursor_limit_cap_player_colour() {
    let s = setup().await;
    let carol = s.user("Carol").await;
    let dave = s.user("Dave").await;
    let list: Vec<GameRecord> = (0..60u64)
        .map(|i| {
            let id = 2_100_000_000_000 + i;
            let mut g = if i % 2 == 1 { game(id, &carol, &dave) } else { game(id, &dave, &carol) };
            g.rated = false;
            g
        })
        .collect();
    let ids: Vec<u64> = list.iter().map(|g| g.id).collect();
    s.commit(list).await;
    let get = |q: &str| s.t.get(&format!("/api/v1/players/carol/games{q}")).send();
    let r = get("").await;
    assert_eq!(r.status, 200);
    let body = r.json();
    assert_eq!(body["username"], "Carol");
    let games = body["games"].as_array().unwrap();
    assert_eq!(games.len(), 20);
    assert_eq!(games[0]["id"], ids[59]);
    assert_eq!(games[0]["color"], "white");
    assert_eq!(games[1]["color"], "black");
    assert_eq!(games[0]["result"], "1-0");
    assert_eq!(games[0]["termination"], "Resignation");
    assert_eq!(games[0]["timeControl"], "180+2");
    let keys: Vec<&str> = games[0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "id",
            "category",
            "rated",
            "timeControl",
            "white",
            "black",
            "color",
            "status",
            "reason",
            "result",
            "termination",
            "plies",
            "startedAt",
            "endedAt"
        ]
    );
    assert_eq!(
        games[0]["white"],
        json!({ "name": "Carol", "rating": 1500, "ratingAfter": null, "ratingDiff": null })
    );
    assert_eq!(body["next"], ids[40]);

    let r = get(&format!("?before={}&limit=50", ids[40])).await.json();
    assert_eq!(r["games"].as_array().unwrap().len(), 40);
    assert_eq!(r["games"][0]["id"], ids[39]);
    assert_eq!(r["next"], Value::Null);
    assert_eq!(get("?limit=500").await.json()["games"].as_array().unwrap().len(), 50, "capped at 50");
    assert_eq!(get("?limit=&before=").await.json()["games"].as_array().unwrap().len(), 20, "empty values");
    let r = get("?before=abc").await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({ "error": "invalid_cursor", "message": "before must be a game id." }))
    );
    assert_eq!(get("?before=-5").await.status, 400);
    assert_eq!(get("?before=99999999999999999999").await.status, 400);
    let r = get("?limit=0").await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({ "error": "invalid_limit", "message": "limit must be 1 to 50." }))
    );
    assert_eq!(s.t.get("/api/v1/players/zed/games").send().await.status, 404);
    assert_eq!(s.t.get("/api/v1/players/zed/games?limit=0").send().await.status, 404, "the player first");
    assert_eq!(s.t.get("/api/v1/players/a/games").send().await.status, 400);
}

#[tokio::test]
async fn an_invalid_token_is_refused_and_the_limit_is_shared_by_the_four_routes() {
    let s = setup().await;
    let alice = s.user("Alice").await;
    let bob = s.user("Bob").await;
    s.commit(vec![game(2_200_000_000_001, &alice, &bob)]).await;
    let r = s.t.get("/api/v1/players/alice").bearer(&format!("sct_{}", "z".repeat(43))).send().await;
    assert_eq!(r.status, 401);
    let paths = [
        "/api/v1/players/alice",
        "/api/v1/players/alice/games",
        "/api/v1/games/2200000000001",
        "/api/v1/games/2200000000001/pgn",
    ];
    for i in 0..60 {
        let r = s.t.get(paths[i % 4]).send().await;
        assert_eq!(r.status, 200, "request {i}");
    }
    let r = s.t.get(paths[0]).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
    // Per account when signed in.
    assert_eq!(s.t.get(paths[1]).bearer(&alice.token).send().await.status, 200);
    // The leaderboard has no limit of its own.
    assert_eq!(s.t.get("/api/v1/leaderboard?category=3%2B2").send().await.status, 200);
}
