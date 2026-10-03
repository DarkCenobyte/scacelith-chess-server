//! `GET /account/games`, ported from the Node suite `http.games`: the player's own history with
//! filters, cursor, total and outcome; query errors, the limit cap, the per-player rate limit.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::super::read_support::*;
use super::*;
use crate::store::GameRecord;

const NOW: i64 = 1_790_856_000_000;
const W: u8 = status::WHITE_WINS;
const L: u8 = status::BLACK_WINS;
const D: u8 = status::DRAW;
const X: u8 = status::ABORTED;

async fn start() -> Server {
    let config = config(&[("HTTP_RATE_PER_IP", "100000"), ("USER_RATE_PER_MIN", "100000")]);
    let store = memory_store(&config).await;
    let s = store.clone();
    Server::start(config, store, NOW, move |router, config| {
        let log = test_logger("account-games-test");
        register(router, AccountGamesDeps { config: config.clone(), store: s, log });
    })
}

fn game(id: u64, white: &Player, black: &Player) -> GameRecord {
    let mut g = record(id, white, black, moves_of("e2e4"));
    (g.started_at, g.ended_at) = (Some(NOW - 2 * HOUR), Some(NOW - HOUR));
    g
}

fn ids(body: &Value) -> Vec<u64> {
    body["games"].as_array().unwrap().iter().map(|g| g["id"].as_u64().unwrap()).collect()
}

#[tokio::test]
async fn the_players_own_history_with_filters_cursor_total_and_outcome() {
    let s = start().await;
    let me = s.user("Mira").await;
    let op = s.user("Otto").await;
    let other = s.user("Pia").await;
    let mut list = Vec::new();
    for i in 0..30u64 {
        let id = 6_000_000_000_000 + i;
        let mut g = if i.is_multiple_of(2) { game(id, &me, &op) } else { game(id, &op, &me) };
        g.status = [W, L, D, W, X][(i % 5) as usize];
        g.reason = if g.status == X { 22 } else { 2 };
        g.rated = !i.is_multiple_of(3) && g.status != X;
        if i.is_multiple_of(7) {
            (g.category, g.base_ms, g.inc_ms, g.rated) = ("custom".into(), 60_000, 1000, false);
        }
        list.push(g);
    }
    list.push(game(6_000_000_000_100, &op, &other));
    s.commit(list.clone()).await;
    let mine: Vec<&GameRecord> = list[..30].iter().rev().collect();
    let outcome = |g: &GameRecord| match g.status {
        D => "draw",
        W if g.white_id == me.id => "win",
        L if g.white_id != me.id => "win",
        W | L => "loss",
        _ => "aborted",
    };
    let get = |q: String| s.t.get(&format!("/api/v1/account/games{q}")).bearer(&me.token).send();

    assert_eq!(s.t.get("/api/v1/account/games").send().await.status, 401);
    let r = get(String::new()).await;
    assert_eq!(r.status, 200);
    let body = r.json();
    let top: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(top, ["games", "next", "total"]);
    assert_eq!(ids(&body), mine[..20].iter().map(|g| g.id).collect::<Vec<_>>());
    assert_eq!(body["total"], 30);
    assert_eq!(body["next"], mine[19].id);
    let first = &body["games"][0];
    let keys: Vec<&str> = first.as_object().unwrap().keys().map(String::as_str).collect();
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
            "endedAt",
            "baseMs",
            "incMs",
            "outcome"
        ]
    );
    let stored = s.store.games().by_id(mine[0].id).await.unwrap().unwrap();
    assert_eq!(*first, Value::Object(history_summary(&stored.summary, me.id)));
    let r = get(format!("?before={}", body["next"])).await.json();
    assert_eq!(ids(&r).len(), 10);
    assert_eq!(r["next"], Value::Null, "the last page");
    assert_eq!(r["total"], 30, "total counts every page");
    let r = get("?limit=30".into()).await.json();
    assert_eq!((ids(&r).len(), r["next"].clone()), (30, Value::Null), "exactly one page: no next");

    // Filters.
    let check = |query: &'static str, pred: &dyn Fn(&GameRecord) -> bool| {
        let want: Vec<u64> = mine.iter().filter(|g| pred(g)).map(|g| g.id).collect();
        let req = get(format!("?limit=50&{query}"));
        async move {
            let res = req.await;
            assert_eq!(res.status, 200, "{query}");
            let body = res.json();
            assert_eq!(ids(&body), want, "{query}");
            assert_eq!(body["total"], want.len(), "{query}: total");
            body
        }
    };
    let wins = check("result=win", &|g| outcome(g) == "win").await;
    let win_list = wins["games"].as_array().unwrap();
    assert!(!win_list.is_empty() && win_list.iter().all(|x| x["outcome"] == "win"));
    assert!(win_list.iter().any(|x| x["color"] == "black"), "wins as Black count");
    check("result=loss", &|g| outcome(g) == "loss").await;
    check("result=draw", &|g| outcome(g) == "draw").await;
    check("rated=true", &|g| g.rated).await;
    check("rated=false", &|g| !g.rated).await;
    check("category=custom", &|g| g.category == "custom").await;
    check("category=3%2B2&rated=true&result=win", &|g| g.category == "3+2" && g.rated && outcome(g) == "win")
        .await;
    check("category=3+2", &|g| g.category == "3+2").await;
    check("category=&result=&rated=", &|_| true).await;
    let all = check("", &|_| true).await;
    let outcomes: BTreeSet<&str> =
        all["games"].as_array().unwrap().iter().map(|x| x["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes.into_iter().collect::<Vec<_>>(), ["aborted", "draw", "loss", "win"]);
    let custom = all["games"].as_array().unwrap().iter().find(|x| x["category"] == "custom").unwrap();
    assert_eq!((custom["baseMs"].clone(), custom["incMs"].clone()), (json!(60_000), json!(1000)));
    assert_eq!(custom["timeControl"], "60+1");
    // Paging through a filter.
    let page1 = get("?result=win&limit=3".into()).await.json();
    let page2 = get(format!("?result=win&limit=3&before={}", page1["next"])).await.json();
    let paged: Vec<u64> = ids(&page1).into_iter().chain(ids(&page2)).collect();
    assert_eq!(paged, ids(&wins)[..6]);
    // Someone else's history is their own.
    let r = s.t.get("/api/v1/account/games").bearer(&other.token).send().await.json();
    assert_eq!(r["total"], 1);
    assert_eq!(
        (r["games"][0]["outcome"].clone(), r["games"][0]["color"].clone()),
        (json!("loss"), json!("black"))
    );
}

#[tokio::test]
async fn query_errors_the_limit_cap_and_the_per_player_rate_limit() {
    let s = start().await;
    let me = s.user("Quin").await;
    let op = s.user("Rhea").await;
    let batch: Vec<GameRecord> = (0..60u64)
        .map(|i| {
            let mut g = game(6_100_000_000_000 + i, &me, &op);
            g.rated = false;
            g
        })
        .collect();
    s.commit(batch.clone()).await;
    let get = |q: &str| s.t.get(&format!("/api/v1/account/games{q}")).bearer(&me.token).send();
    let cases = [
        ("?before=abc", "invalid_cursor", "before", "before must be a game id."),
        ("?before=-1", "invalid_cursor", "before", "before must be a game id."),
        ("?before=0", "invalid_cursor", "before", "before must be a game id."),
        ("?before=99999999999999999999", "invalid_cursor", "before", "before must be a game id."),
        ("?limit=0", "invalid_limit", "limit", "limit must be 1 to 50."),
        ("?limit=x", "invalid_limit", "limit", "limit must be 1 to 50."),
        ("?limit=1000", "invalid_limit", "limit", "limit must be 1 to 50."),
        ("?category=4%2B2", "invalid_filter", "category", "category must be one of: 3+2, 5+0, 10+0, custom."),
        (
            "?category=CUSTOM",
            "invalid_filter",
            "category",
            "category must be one of: 3+2, 5+0, 10+0, custom.",
        ),
        ("?rated=yes", "invalid_filter", "rated", "rated must be true or false."),
        ("?rated=1", "invalid_filter", "rated", "rated must be true or false."),
        ("?result=aborted", "invalid_filter", "result", "result must be one of: win, loss, draw."),
        ("?result=WIN", "invalid_filter", "result", "result must be one of: win, loss, draw."),
    ];
    let categories: Vec<&str> = s.config.categories.iter().map(|c| c.id.as_str()).collect();
    for (q, error, field, message) in cases {
        let message = message.replace("3+2, 5+0, 10+0", &categories.join(", "));
        let r = get(q).await;
        assert_eq!(r.status, 400, "{q}");
        assert_eq!(r.json(), json!({ "error": error, "message": message, "field": field }), "{q}");
    }
    let r = get("?limit=500").await.json();
    assert_eq!(ids(&r).len(), 50, "capped at 50");
    assert_eq!(r["next"], batch[10].id);
    assert_eq!(get("?unknown=1").await.status, 200, "unknown parameters are ignored");
    // 60 a minute per player (the requests above: 15, refused ones included).
    let mut last = 0;
    for _ in 0..45 {
        last = get("?limit=1").await.status;
    }
    assert_eq!(last, 200);
    let r = get("?limit=1").await;
    assert_eq!(r.status, 429);
    assert!(r.json()["retryAfter"].as_u64().unwrap() >= 1);
    assert_eq!(r.header("retry-after"), Some(r.json()["retryAfter"].to_string().as_str()));
    // Per player: someone else is not limited.
    assert_eq!(s.t.get("/api/v1/account/games").bearer(&op.token).send().await.status, 200);
}

#[test]
fn outcome_for_each_status_and_side() {
    let base = |status: u8| GameSummary {
        id: 1,
        category: "3+2".into(),
        rated: true,
        base_ms: 180_000,
        inc_ms: 2000,
        white_id: 1,
        black_id: 2,
        white_name: "A".into(),
        black_name: "B".into(),
        white_rating: None,
        black_rating: None,
        started_at: 0,
        ended_at: 0,
        status,
        reason: 2,
        ply_count: 0,
        rematch_of: None,
        flags: 0,
        rating_changes: None,
    };
    assert_eq!(outcome_for(&base(W), 1), "win");
    assert_eq!(outcome_for(&base(W), 2), "loss");
    assert_eq!(outcome_for(&base(L), 2), "win");
    assert_eq!(outcome_for(&base(L), 1), "loss");
    assert_eq!(outcome_for(&base(D), 2), "draw");
    assert_eq!(outcome_for(&base(X), 1), "aborted");
    let h = history_summary(&base(D), 2);
    assert_eq!(
        (h["color"].clone(), h["outcome"].clone(), h["baseMs"].clone()),
        (json!("black"), json!("draw"), json!(180_000))
    );
}

#[tokio::test]
async fn registration_auth_required_and_the_account_games_rate() {
    let config = std::sync::Arc::new(Config::for_tests());
    let store = memory_store(&config).await;
    let mut router = Router::new();
    register(&mut router, AccountGamesDeps { config, store, log: test_logger("account-games-test") });
    let route = &router.routes()[0];
    assert_eq!(route.label(), "GET /api/v1/account/games");
    assert_eq!(route.opts().auth, AuthMode::Required);
    let rate = &route.opts().rates[0];
    assert_eq!(
        (rate.key.as_ref(), rate.limit, rate.window_ms, rate.by_user, rate.shared),
        ("account_games", 60.0, 60_000, true, false)
    );
}
