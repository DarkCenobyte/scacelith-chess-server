//! Games: a history played through each server's realtime protocol (checkmate, resignation,
//! agreed draw, casual, aborted, custom time control), then the account history with its
//! filters and cursors, the game records, PGN files and GIFs, the players' profiles and games,
//! the leaderboard, reports, the export with ratings, and a deleted player in the records.

use std::net::IpAddr;

use serde_json::json;

use super::{BoxFut, PASSWORD, new_account};
use crate::duo::{Duo, Side, fresh_ip};
use crate::http::Req;
use crate::realtime::{End, GameScript};

/// A GET with the session `<user>.token`, its target computed from the side.
fn get_as(user: &str, target: impl Fn(&Side) -> String) -> impl Fn(&Side) -> Req {
    let token = format!("{user}.token");
    move |s: &Side| Req::get(target(s)).bearer(&s.v(&token))
}

/// A POST with the session `<user>.token` and a JSON body computed from the side.
fn post_as(path: &'static str, user: &str, body: impl Fn(&Side) -> serde_json::Value) -> impl Fn(&Side) -> Req {
    let token = format!("{user}.token");
    move |s: &Side| Req::post(path).bearer(&s.v(&token)).json(body(s))
}

fn script(base_sec: u16, inc_sec: u8, rated: bool, moves: &[&'static str], end: End) -> GameScript {
    GameScript { base_sec, inc_sec, rated, moves: moves.to_vec(), end }
}

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "gwen", "gwen@example.org").await;
        new_account(d, ip, "hugo", "hugo@example.org").await;
        new_account(d, ip, "ivy", "ivy@example.org").await;
        for p in ["gwen", "hugo", "ivy"] {
            if !d.connect(p, &format!("{p}.token")).await {
                return;
            }
        }
        // G1: Black mates (fool's mate), rated 3+2.
        d.play("G1", "gwen", "hugo", "hugo", &script(180, 2, true, &["f2f3", "e7e5", "g2g4", "d8h4"], End::OnBoard)).await;
        // G2: Black resigns, rated 3+2.
        d.play("G2", "gwen", "hugo", "hugo", &script(180, 2, true, &["e2e4", "e7e5", "g1f3"], End::Resign { white: false }))
            .await;
        // G3: an agreed draw, rated 3+2.
        d.play("G3", "gwen", "hugo", "hugo", &script(180, 2, true, &["e2e4", "e7e5", "g1f3", "b8c6"], End::DrawAgreed)).await;
        // G4: casual 5+0, White resigns.
        d.play("G4", "gwen", "hugo", "hugo", &script(300, 0, false, &["d2d4", "d7d5"], End::Resign { white: true })).await;
        // G5: aborted before the first move.
        d.play("G5", "gwen", "hugo", "hugo", &script(180, 2, true, &[], End::Abort { white: true })).await;
        // G6: a custom time control (7+3, never rated), Black resigns.
        d.play("G6", "gwen", "hugo", "hugo", &script(420, 3, false, &["c2c4", "e7e5", "b1c3"], End::Resign { white: false }))
            .await;
        // G7: ivy (White) against gwen, rated 3+2, a promotion, then Black resigns.
        d.play(
            "G7",
            "ivy",
            "gwen",
            "gwen",
            &script(180, 2, true, &["a2a4", "b7b5", "a4b5", "a7a6", "b5a6", "c8b7", "a6b7", "b8c6", "b7a8q"], End::Resign {
                white: false,
            }),
        )
        .await;

        // The account budget (`USER_RATE_PER_MIN`) is a bucket of half a minute (60 requests)
        // refilled at 2 per second: gwen's sections are spaced to stay within it.
        let budget_refill = std::time::Duration::from_secs(25);
        account_games(d).await;
        game_records(d).await;
        tokio::time::sleep(budget_refill).await;
        gifs(d).await;
        players(d).await;
        leaderboard(d).await;
        tokio::time::sleep(budget_refill).await;
        reports(d).await;
        export_and_deletion(d).await;
    })
}

/// `GET /account/games`: filters, cursors, limits (gwen: fewer than 60 requests).
async fn account_games(d: &mut Duo) {
    let ip = fresh_ip();
    let list = |q: &'static str| get_as("gwen", move |_| format!("/api/v1/account/games{q}"));
    d.step("history", ip, 200, list("")).await;
    d.step("history-no-token", ip, 401, |_| Req::get("/api/v1/account/games")).await;
    d.step("history-head", ip, 0, |s| Req::new("HEAD", "/api/v1/account/games").bearer(&s.v("gwen.token"))).await;
    let p = d.step("history-limit-2", ip, 200, list("?limit=2")).await;
    d.save(&p, "gwen.next", "next");
    d.step("history-page-2", ip, 200, get_as("gwen", |s| format!("/api/v1/account/games?limit=2&before={}", s.v("gwen.next"))))
        .await;
    d.step("history-before-g2", ip, 200, get_as("gwen", |s| format!("/api/v1/account/games?before={}", s.game("G2")))).await;
    d.step("history-before-g1", ip, 200, get_as("gwen", |s| format!("/api/v1/account/games?before={}", s.game("G1")))).await;
    for (id, q) in [
        ("history-limit-1", "?limit=1"),
        ("history-limit-50", "?limit=50"),
        ("history-limit-51", "?limit=51"),
        ("history-limit-999", "?limit=999"),
        ("history-limit-1000", "?limit=1000"),
        ("history-limit-0", "?limit=0"),
        ("history-limit-negative", "?limit=-1"),
        ("history-limit-fraction", "?limit=1.5"),
        ("history-limit-letters", "?limit=abc"),
        ("history-limit-plus", "?limit=+5"),
        ("history-limit-leading-zero", "?limit=05"),
        ("history-limit-empty", "?limit="),
        ("history-limit-twice", "?limit=1&limit=2"),
        ("history-before-letters", "?before=abc"),
        ("history-before-zero", "?before=0"),
        ("history-before-17-digits", "?before=12345678901234567"),
        ("history-before-16-digits", "?before=1234567890123456"),
        ("history-before-empty", "?before="),
        ("history-category-encoded", "?category=3%2B2"),
        ("history-category-plus", "?category=3+2"),
        ("history-category-custom", "?category=custom"),
        ("history-category-casual-5", "?category=5%2B0"),
        ("history-category-unofficial", "?category=7%2B3"),
        ("history-category-bad", "?category=blitz"),
        ("history-rated-true", "?rated=true"),
        ("history-rated-false", "?rated=false"),
        ("history-rated-bad", "?rated=yes"),
        ("history-result-win", "?result=win"),
        ("history-result-loss", "?result=loss"),
        ("history-result-draw", "?result=draw"),
        ("history-result-aborted", "?result=aborted"),
        ("history-result-bad", "?result=WIN"),
        ("history-combined", "?category=3%2B2&rated=true&result=loss&limit=1"),
        ("history-empty-values", "?category=&rated=&result=&before=&limit="),
        ("history-unknown-param", "?colour=white"),
    ] {
        d.step(id, ip, 0, list(q)).await;
    }
    d.step("history-hugo", ip, 200, get_as("hugo", |_| "/api/v1/account/games?limit=3".into())).await;
    d.step("history-ivy", ip, 200, get_as("ivy", |_| "/api/v1/account/games".into())).await;
}

/// `GET /games/:id` and its PGN, with and without a token, invalid ids.
async fn game_records(d: &mut Duo) {
    let ip = fresh_ip();
    for g in ["G1", "G2", "G3", "G4", "G5", "G6", "G7"] {
        d.step(&format!("record-{g}"), ip, 200, move |s: &Side| Req::get(format!("/api/v1/games/{}", s.game(g)))).await;
        d.step(&format!("pgn-{g}"), ip, 200, move |s: &Side| Req::get(format!("/api/v1/games/{}/pgn", s.game(g)))).await;
    }
    // The players see `you` and `reportable`; another player sees the public record.
    let ip = fresh_ip();
    d.step("record-as-white", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}", s.game("G1")))).await;
    d.step("record-as-black", ip, 200, get_as("hugo", |s| format!("/api/v1/games/{}", s.game("G1")))).await;
    d.step("record-as-other", ip, 200, get_as("ivy", |s| format!("/api/v1/games/{}", s.game("G1")))).await;
    d.step("record-aborted-as-player", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}", s.game("G5")))).await;
    d.step("record-bad-token", ip, 401, |s| Req::get(format!("/api/v1/games/{}", s.game("G1"))).bearer("sct_nope")).await;
    d.step("pgn-with-token", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}/pgn", s.game("G2")))).await;
    d.step("pgn-head", ip, 0, |s| Req::new("HEAD", format!("/api/v1/games/{}/pgn", s.game("G2")))).await;
    d.step("record-head", ip, 0, |s| Req::new("HEAD", format!("/api/v1/games/{}", s.game("G2")))).await;
    let ip = fresh_ip();
    for (id, path) in [
        ("record-zero", "/api/v1/games/0"),
        ("record-negative", "/api/v1/games/-1"),
        ("record-letters", "/api/v1/games/abc"),
        ("record-exponent", "/api/v1/games/1e3"),
        ("record-17-digits", "/api/v1/games/12345678901234567"),
        ("record-16-digits", "/api/v1/games/1234567890123456"),
        ("record-leading-zeros", "/api/v1/games/0001"),
        ("record-unknown", "/api/v1/games/999999"),
        ("record-trailing-slash", "/api/v1/games/999999/"),
        ("pgn-letters", "/api/v1/games/abc/pgn"),
        ("pgn-unknown", "/api/v1/games/999999/pgn"),
        ("pgn-zero", "/api/v1/games/0/pgn"),
        ("record-unknown-sub", "/api/v1/games/999999/moves"),
    ] {
        d.step(id, ip, 0, move |_| Req::get(path)).await;
    }
    d.step("pgn-post", ip, 405, |s| Req::post(format!("/api/v1/games/{}/pgn", s.game("G2")))).await;
}

/// The GIFs: options, errors, the cache, `POST /gif` and the render limit of a player (4 per
/// minute). Every request counts in the `gif` limit (30 per minute per player): hugo asks for
/// the game GIFs and reaches the render limit, gwen sends the invalid `POST /gif` bodies.
async fn gifs(d: &mut Duo) {
    let ip = fresh_ip();
    let gif = |q: &'static str| get_as("hugo", move |s| format!("/api/v1/games/{}/gif{q}", s.game("G1")));
    d.step("gif-no-token", ip, 401, |s| Req::get(format!("/api/v1/games/{}/gif", s.game("G1")))).await;
    d.step("gif-default", ip, 200, gif("")).await;
    d.step("gif-default-cached", ip, 200, gif("")).await;
    d.step("gif-default-explicit", ip, 200, gif("?size=medium&orientation=white&delay=500&coords=1")).await;
    d.step("gif-small-black", ip, 200, gif("?size=small&orientation=black&delay=100&coords=0")).await;
    for (id, q) in [
        ("gif-size-bad", "?size=huge"),
        ("gif-size-case", "?size=Small"),
        ("gif-orientation-bad", "?orientation=left"),
        ("gif-delay-low", "?delay=99"),
        ("gif-delay-high", "?delay=3001"),
        ("gif-delay-letters", "?delay=abc"),
        ("gif-delay-fraction", "?delay=500.5"),
        ("gif-coords-bad", "?coords=true"),
        ("gif-size-empty", "?size="),
        ("gif-unknown-param", "?speed=fast"),
    ] {
        d.step(id, ip, 0, gif(q)).await;
    }
    d.step("gif-letters", ip, 400, get_as("hugo", |_| "/api/v1/games/abc/gif".into())).await;
    d.step("gif-unknown", ip, 404, get_as("hugo", |_| "/api/v1/games/999999/gif".into())).await;
    d.step("gif-head", ip, 0, |s| Req::new("HEAD", format!("/api/v1/games/{}/gif", s.game("G1"))).bearer(&s.v("hugo.token"))).await;
    d.step("gif-large-g3", ip, 200, get_as("hugo", |s| format!("/api/v1/games/{}/gif?size=large&delay=3000", s.game("G3")))).await;

    let fools = "1. f3 e5 2. g4 Qh4# 0-1";
    let post = |body: serde_json::Value| post_as("/api/v1/gif", "gwen", move |_| body.clone());
    d.step("post-gif-no-token", ip, 401, move |_| Req::post("/api/v1/gif").json(json!({"pgn": fools}))).await;
    d.step("post-gif", ip, 200, post(json!({"pgn": fools, "coords": false}))).await;
    d.step("post-gif-cached", ip, 200, post(json!({"coords": false, "pgn": fools}))).await;
    for (id, body) in [
        ("post-gif-illegal", json!({"pgn": "1. e4 e5 2. Ke3 *"})),
        ("post-gif-broken-tag", json!({"pgn": "[White \"x\n1. e4 *"})),
        ("post-gif-variant", json!({"pgn": "[Variant \"Atomic\"]\n\n1. e4 *"})),
        ("post-gif-missing-pgn", json!({"size": "small"})),
        ("post-gif-pgn-number", json!({"pgn": 5})),
        ("post-gif-unknown-field", json!({"pgn": fools, "speed": 1})),
        ("post-gif-size-bad", json!({"pgn": fools, "size": "huge"})),
        ("post-gif-orientation-bad", json!({"pgn": fools, "orientation": "up"})),
        ("post-gif-delay-string", json!({"pgn": fools, "delayMs": "500"})),
        ("post-gif-delay-low", json!({"pgn": fools, "delayMs": 99})),
        ("post-gif-delay-fraction", json!({"pgn": fools, "delayMs": 100.5})),
        ("post-gif-coords-string", json!({"pgn": fools, "coords": "true"})),
        ("post-gif-coords-number", json!({"pgn": fools, "coords": 1})),
        ("post-gif-empty-pgn", json!({"pgn": ""})),
        ("post-gif-no-moves", json!({"pgn": "[White \"a\"]\n\n*"})),
        ("post-gif-too-long", json!({"pgn": format!("{fools}{}", " ".repeat(65_537))})),
        ("post-gif-array", json!([fools])),
    ] {
        d.step(id, ip, 0, post(body)).await;
    }
    d.step("post-gif-too-large", ip, 413, post(json!({"pgn": "x".repeat(140_000)}))).await;
    // hugo's 4th and 5th renders within the minute: the player's render limit.
    let scholar = "[White \"Ann\"]\n[Black \"Bob\"]\n[WhiteElo \"1500\"]\n\n1. e4 e5 2. Qh5 Nc6 3. Bc4 Nf6 4. Qxf7# 1-0";
    d.step("post-gif-hugo", ip, 200, post_as("/api/v1/gif", "hugo", move |_| json!({"pgn": scholar, "size": "small"}))).await;
    d.step("post-gif-render-limit", ip, 429, post_as("/api/v1/gif", "hugo", move |_| json!({"pgn": scholar, "size": "large"}))).await;
    d.step("gif-cached-after-limit", ip, 200, gif("")).await;
}

/// Public profiles and game lists.
async fn players(d: &mut Duo) {
    let ip = fresh_ip();
    for (id, path) in [
        ("profile-gwen", "/api/v1/players/gwen"),
        ("profile-case", "/api/v1/players/GWEN"),
        ("profile-hugo", "/api/v1/players/hugo"),
        ("profile-one-char", "/api/v1/players/g"),
        ("profile-25-chars", "/api/v1/players/abcdefghijklmnopqrstuvwxy"),
        ("profile-bad-char", "/api/v1/players/gw%20en"),
        ("profile-unknown", "/api/v1/players/nobody"),
        ("profile-trailing-slash", "/api/v1/players/gwen/"),
        ("games-gwen", "/api/v1/players/gwen/games"),
        ("games-case", "/api/v1/players/Gwen/games"),
        ("games-limit-1", "/api/v1/players/gwen/games?limit=1"),
        ("games-limit-7", "/api/v1/players/gwen/games?limit=7"),
        ("games-limit-0", "/api/v1/players/gwen/games?limit=0"),
        ("games-limit-999", "/api/v1/players/gwen/games?limit=999"),
        ("games-limit-1000", "/api/v1/players/gwen/games?limit=1000"),
        ("games-before-bad", "/api/v1/players/gwen/games?before=x"),
        ("games-filter-ignored", "/api/v1/players/gwen/games?rated=true&result=win"),
        ("games-unknown", "/api/v1/players/nobody/games"),
        ("games-bad-name", "/api/v1/players/g/games"),
    ] {
        d.step(id, ip, 0, move |_| Req::get(path)).await;
    }
    let p = d.step("games-page-1", ip, 200, |_| Req::get("/api/v1/players/gwen/games?limit=3")).await;
    d.save(&p, "gwen.pnext", "next");
    let p = d.step("games-page-2", ip, 200, |s| Req::get(format!("/api/v1/players/gwen/games?limit=3&before={}", s.v("gwen.pnext")))).await;
    d.save(&p, "gwen.pnext", "next");
    d.step("games-page-3", ip, 200, |s| Req::get(format!("/api/v1/players/gwen/games?limit=3&before={}", s.v("gwen.pnext")))).await;
    d.step("profile-with-token", ip, 200, get_as("ivy", |_| "/api/v1/players/hugo".into())).await;
    d.step("profile-bad-token", ip, 401, |_| Req::get("/api/v1/players/hugo").bearer("sct_nope")).await;
}

/// The leaderboard of each kind of category.
async fn leaderboard(d: &mut Duo) {
    let ip = fresh_ip();
    for (id, q) in [
        ("leaderboard-3+2", "?category=3%2B2"),
        ("leaderboard-plus", "?category=3+2"),
        ("leaderboard-limit-1", "?category=3%2B2&limit=1"),
        ("leaderboard-limit-100", "?category=3%2B2&limit=100"),
        ("leaderboard-limit-101", "?category=3%2B2&limit=101"),
        ("leaderboard-limit-999", "?category=3%2B2&limit=999"),
        ("leaderboard-limit-1000", "?category=3%2B2&limit=1000"),
        ("leaderboard-limit-0", "?category=3%2B2&limit=0"),
        ("leaderboard-limit-bad", "?category=3%2B2&limit=ten"),
        ("leaderboard-empty-category", "?category=1%2B0"),
        ("leaderboard-custom", "?category=custom"),
        ("leaderboard-unofficial", "?category=7%2B3"),
        ("leaderboard-no-category", ""),
        ("leaderboard-blank-category", "?category="),
    ] {
        d.step(id, ip, 0, move |_| Req::get(format!("/api/v1/leaderboard{q}"))).await;
    }
    d.step("leaderboard-with-token", ip, 200, get_as("gwen", |_| "/api/v1/leaderboard?category=3%2B2".into())).await;
    d.step("leaderboard-post", ip, 405, |_| Req::post("/api/v1/leaderboard")).await;
}

/// Reports: errors, the duplicate, `reportable`, the daily quota.
async fn reports(d: &mut Duo) {
    let ip = fresh_ip();
    let report = |body: fn(&Side) -> serde_json::Value| post_as("/api/v1/reports", "gwen", body);
    d.step("report-no-token", ip, 401, |s| {
        Req::post("/api/v1/reports").json(json!({"gameId": s.game("G1"), "reported": "hugo", "category": "cheating"}))
    })
    .await;
    d.step("report-bad-category", ip, 400, report(|s| json!({"gameId": s.game("G1"), "reported": "hugo", "category": "rude"}))).await;
    d.step("report-missing-reported", ip, 400, report(|s| json!({"gameId": s.game("G1"), "category": "other"}))).await;
    d.step("report-bad-game-id", ip, 0, report(|_| json!({"gameId": "12a", "reported": "hugo", "category": "other"}))).await;
    d.step("report-negative-game-id", ip, 0, report(|_| json!({"gameId": -1, "reported": "hugo", "category": "other"}))).await;
    d.step("report-long-comment", ip, 400, report(|s| {
        json!({"gameId": s.game("G1"), "reported": "hugo", "category": "other", "comment": "x".repeat(501)})
    }))
    .await;
    d.step("report-long-name", ip, 400, report(|s| json!({"gameId": s.game("G1"), "reported": "h".repeat(25), "category": "other"}))).await;
    d.step("report-extra-field", ip, 400, report(|s| json!({"gameId": s.game("G1"), "reported": "hugo", "category": "other", "x": 1}))).await;
    d.step("report-not-opponent", ip, 403, report(|s| json!({"gameId": s.game("G1"), "reported": "ivy", "category": "other"}))).await;
    d.step("report-self", ip, 403, report(|s| json!({"gameId": s.game("G1"), "reported": "gwen", "category": "other"}))).await;
    d.step("report-unknown-game", ip, 403, report(|_| json!({"gameId": 999_999, "reported": "hugo", "category": "other"}))).await;
    d.step("report-unknown-player", ip, 403, report(|s| json!({"gameId": s.game("G1"), "reported": "nobody", "category": "other"}))).await;
    d.step("reportable-before", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}", s.game("G1")))).await;
    d.step("report", ip, 202, report(|s| {
        json!({"gameId": s.game("G1"), "reported": "hugo", "category": "cheating", "comment": "engine-like play"})
    }))
    .await;
    d.step("report-duplicate", ip, 202, report(|s| json!({"gameId": s.game("G1"), "reported": "hugo", "category": "abuse"}))).await;
    d.step("reportable-after", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}", s.game("G1")))).await;
    d.step("report-case-string-id", ip, 202, report(|s| {
        json!({"gameId": s.game("G2").to_string(), "reported": "HUGO", "category": "abuse", "comment": "tab\tand\u{7}bell"})
    }))
    .await;
    d.step("report-g3", ip, 202, report(|s| json!({"gameId": s.game("G3"), "reported": "hugo", "category": "other"}))).await;
    d.step("report-g4", ip, 202, report(|s| json!({"gameId": s.game("G4"), "reported": "hugo", "category": "other"}))).await;
    d.step("report-aborted", ip, 0, report(|s| json!({"gameId": s.game("G5"), "reported": "hugo", "category": "other"}))).await;
    d.step("report-g6", ip, 0, report(|s| json!({"gameId": s.game("G6"), "reported": "hugo", "category": "other"}))).await;
    d.step("report-g7", ip, 0, report(|s| json!({"gameId": s.game("G7"), "reported": "ivy", "category": "other"}))).await;
    d.step("reportable-quota-used", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}", s.game("G7")))).await;
    d.step("report-by-hugo", ip, 202, post_as("/api/v1/reports", "hugo", |s| {
        json!({"gameId": s.game("G1"), "reported": "gwen", "category": "other"})
    }))
    .await;
}

/// The export with games, ratings and reports; a deleted player in the records.
async fn export_and_deletion(d: &mut Duo) {
    let ip = fresh_ip();
    d.step("me-ratings", ip, 200, get_as("gwen", |_| "/api/v1/account/me".into())).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    d.step("export-gwen", ip, 200, post_as("/api/v1/account/export", "gwen", |_| json!({"password": PASSWORD}))).await;
    d.step("export-hugo", ip, 200, post_as("/api/v1/account/export", "hugo", |_| json!({"password": PASSWORD}))).await;
    d.step("delete-ivy", ip, 200, post_as("/api/v1/account/delete", "ivy", |_| json!({"password": PASSWORD}))).await;
    deleted_views(d, ip).await;
}

async fn deleted_views(d: &mut Duo, ip: IpAddr) {
    d.step("deleted-record", ip, 200, |s| Req::get(format!("/api/v1/games/{}", s.game("G7")))).await;
    d.step("deleted-pgn", ip, 200, |s| Req::get(format!("/api/v1/games/{}/pgn", s.game("G7")))).await;
    d.step("deleted-history", ip, 200, get_as("gwen", |_| "/api/v1/account/games?limit=1".into())).await;
    d.step("deleted-player-games", ip, 200, |_| Req::get("/api/v1/players/gwen/games?limit=1")).await;
    d.step("deleted-profile", ip, 404, |_| Req::get("/api/v1/players/ivy")).await;
    d.step("deleted-profile-games", ip, 404, |_| Req::get("/api/v1/players/ivy/games")).await;
    d.step("deleted-gif", ip, 200, get_as("gwen", |s| format!("/api/v1/games/{}/gif?size=small", s.game("G7")))).await;
    d.step("deleted-leaderboard", ip, 200, |_| Req::get("/api/v1/leaderboard?category=3%2B2")).await;
}
