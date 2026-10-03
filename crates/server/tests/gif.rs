//! The animated GIF of a played game, end to end on the real server (TLS, SQLite, the rendering
//! threads): two players play a fool's mate through the realtime protocol, the game is committed,
//! and its owner downloads `GET /api/v1/games/:id/gif`. The file is walked here: one frame per
//! position, the last one held, the chosen size. A second download comes from the cache (no
//! render), the PGN of the game sent to `POST /api/v1/gif` gives the same number of frames, and the
//! metrics count the renders. Then SIGTERM after renders: the server exits cleanly.
//!
//! Port of the Node.js `test/integration/gif.test.js`.

#[macro_use]
mod support;

use std::time::{Duration, Instant};

use scacelith_client::ApiClient;
use scacelith_client::http::Response;
use scacelith_protocol::ServerMsg;
use serde_json::{Value, json};
use support::web::read_gif;
use support::*;

const RENDERS_OK: &str = "scacelith_gif_renders_total{result=\"ok\"}";
const CACHE_HITS: &str = "scacelith_gif_cache_total{result=\"hit\"}";

/// A request whose answer is read as bytes.
async fn raw(
    api: &ApiClient,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Response {
    api.request(method, path, token, body.as_ref()).await.unwrap_or_else(|e| panic!("{method} {path}: {e}"))
}

/// The JSON body of an answer.
fn json_of(res: &Response) -> Value {
    serde_json::from_slice(&res.body).unwrap_or(Value::Null)
}

/// The start of a body, for failure messages.
fn head(res: &Response) -> String {
    String::from_utf8_lossy(&res.body[..res.body.len().min(200)]).into_owned()
}

/// A server like the Node.js suite's: one worker, a long first-move deadline, fast commits.
async fn server() -> TestServer {
    TestServer::options().env("FIRST_MOVE_TIMEOUT_MS", "20000").env("DB_COMMIT_MS", "20").start().await
}

#[tokio::test]
async fn gif_of_a_played_game_download_decode_cache_the_same_game_from_its_pgn() {
    let srv = server().await;
    let [mut alice, mut bob] = players(&srv, ["alice_gif", "bob_gif"]).await;
    let api = srv.api();

    // Fool's mate: Black (Bob) mates in two.
    let id = challenge_game(&mut alice, &mut bob, 180, 2, false).await;
    let mut table = Table::new(id);
    let m = alice.client.mark();
    table.play_all(&mut alice, &mut bob, &["f2f3", "e7e5", "g2g4", "d8h4"]).await;
    wait_msg!(alice.client, m, ServerMsg::GameEnd(e) if e.game == id => ());

    // The game record is there once committed.
    let rec = eventually(Duration::from_secs(10), "the committed game", || async {
        let res = raw(&api, "GET", &format!("/games/{id}"), Some(alice.token()), None).await;
        (res.status == 200).then_some(res)
    })
    .await;
    assert_eq!(json_of(&rec)["result"], "0-1");

    // A session is needed.
    assert_eq!(raw(&api, "GET", &format!("/games/{id}/gif"), None, None).await.status, 401);

    // The download.
    let before = srv.metrics().await;
    let path = format!("/games/{id}/gif?size=small&delay=250");
    let r = raw(&api, "GET", &path, Some(alice.token()), None).await;
    assert_eq!(r.status, 200, "{}", head(&r));
    assert_eq!(r.header("content-type"), Some("image/gif"));
    let disposition = format!("attachment; filename=\"scacelith-{id}.gif\"");
    assert_eq!(r.header("content-disposition"), Some(disposition.as_str()));
    assert_eq!(r.header("content-length").and_then(|v| v.parse::<usize>().ok()), Some(r.body.len()));
    assert_eq!(&r.body[..6], b"GIF89a");
    let gif = read_gif(&r.body);
    assert_eq!(gif.delays.len(), 5, "the start position and one frame per move");
    assert_eq!(gif.delays, [100, 25, 25, 25, 300], "the start held 1 s, the mate 3 s");
    assert!(gif.width > 0 && gif.width < 300, "small: {}", gif.width);

    // The same picture again: from the cache, byte for byte, and the other player gets it too.
    let again = raw(&api, "GET", &path, Some(bob.token()), None).await;
    assert_eq!(again.status, 200);
    assert_eq!(again.body, r.body);

    // The PGN of the game, sent to POST /gif: the same moves, so the same frames.
    let pgn = raw(&api, "GET", &format!("/games/{id}/pgn"), Some(alice.token()), None).await;
    assert_eq!(pgn.status, 200);
    let body = json!({"pgn": String::from_utf8_lossy(&pgn.body), "size": "small", "delayMs": 250});
    let p = raw(&api, "POST", "/gif", Some(alice.token()), Some(body)).await;
    assert_eq!(p.status, 200, "{}", head(&p));
    assert_eq!(p.header("content-disposition"), Some("attachment; filename=\"scacelith-game.gif\""));
    assert_eq!(read_gif(&p.body).delays.len(), 5);

    let bad = raw(&api, "POST", "/gif", Some(alice.token()), Some(json!({"pgn": "1. e4 e5 2. Ke3 *"}))).await;
    let err = json_of(&bad);
    assert_eq!((bad.status, &err["error"], &err["line"]), (400, &json!("invalid_pgn"), &json!(1)));

    // Two renders (the GET and the POST), one cache hit.
    let after = srv.metrics().await;
    assert_eq!(metric_value(&after, RENDERS_OK) - metric_value(&before, RENDERS_OK), 2.0);
    assert_eq!(metric_value(&after, CACHE_HITS) - metric_value(&before, CACHE_HITS), 1.0);
}

#[tokio::test]
async fn sigterm_after_renders_the_rendering_threads_stop_and_the_server_exits_0() {
    let mut srv = server().await;
    let acc = account(&srv, "carol_gif").await;
    let body = json!({"pgn": "1. f3 e5 2. g4 Qh4# 0-1", "size": "small", "delayMs": 250});
    let rendered = raw(&acc.api, "POST", "/gif", Some(&acc.token), Some(body)).await;
    assert_eq!(rendered.status, 200, "{}", head(&rendered));
    assert_eq!(srv.metric(RENDERS_OK).await, 1.0);

    let t0 = Instant::now();
    let status = srv.stop().await;
    assert!(status.success(), "{status}:\n{}", srv.logs.tail(10));
    assert!(t0.elapsed() < Duration::from_secs(10), "stopped in {:?}", t0.elapsed());
    let errors = srv
        .logs
        .matching(|l| l["level"] == "error" || l["raw"].as_str().is_some_and(|r| r.contains("Error")));
    assert!(errors.is_empty(), "no error logged: {errors:?}");
}
