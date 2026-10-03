//! The GIF routes, ported from the Node suites `http.gif` and `integration/gif` through the real
//! pipeline, an in-memory store and the real GIF service: sessions, options, the job built from a
//! stored record or a PGN, invalid PGN, game length, the cache and the renders in flight, the
//! render quotas per account and per address, the busy pool and its refunds, failures, the lazy
//! pool, and stored games rendered by the real renderer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http::Method;
use parking_lot::{Condvar, Mutex};
use scacelith_gif::{Options, Orientation, Size};
use serde_json::{Value, json};

use super::super::games::game_pgn;
use super::super::read_support::*;
use super::*;
use crate::gifsvc::{ChessRules, GameRenderer, PoolSettings, Renderer};
use crate::store::tests::support::LogCapture;
use crate::store::{GameRecord, status};

const NOW: i64 = 1_790_856_000_000;
const OPERA_UCI: &str = "e2e4 e7e5 g1f3 d7d6 d2d4 c8g4 d4e5 g4f3 d1f3 d6e5 f1c4 g8f6 f3b3 d8e7 b1c3 c7c6 c1g5 b7b5 \
                         c3b5 c6b5 c4b5 b8d7 e1c1 a8d8 d1d7 d8d7 h1d1 e7e6 b5d7 f6d7 b3b8 d7b8 d1d8";
/// 40 plies (every pawn one square then another, four knight moves each), as UCI and as SAN.
const LONG_UCI: &str = "a2a3 a7a6 b2b3 b7b6 c2c3 c7c6 d2d3 d7d6 e2e3 e7e6 f2f3 f7f6 g2g3 g7g6 h2h3 h7h6 a3a4 a6a5 \
                        b3b4 b6b5 c3c4 c6c5 d3d4 d6d5 e3e4 e6e5 f3f4 f6f5 g3g4 g6g5 h3h4 h6h5 b1d2 b8d7 g1e2 g8e7 \
                        d2b3 d7b6 e2g3 e7g6";
const LONG_SAN: &str = "1. a3 a6 2. b3 b6 3. c3 c6 4. d3 d6 5. e3 e6 6. f3 f6 7. g3 g6 8. h3 h6 9. a4 a5 10. b4 b5 \
                        11. c4 c5 12. d4 d5 13. e4 e5 14. f4 f5 15. g4 g5 16. h4 h5 17. Nd2 Nd7 18. Ne2 Ne7 \
                        19. Nb3 Nb6 20. Ng3 Ng6";
const OPERA_PGN: &str = "[Event \"Paris\"]
[Site \"Paris FRA\"]
[Date \"1858.??.??\"]
[White \"Paul Morphy\"]
[Black \"Duke Karl / Count Isouard\"]
[Result \"1-0\"]

1. e4 e5 2. Nf3 d6 3. d4 Bg4 4. dxe5 Bxf3 5. Qxf3 dxe5 6. Bc4 Nf6 7. Qb3 Qe7
8. Nc3 c6 9. Bg5 b5 10. Nxb5 cxb5 11. Bxb5+ Nbd7 12. O-O-O Rd8 13. Rxd7 Rxd7
14. Rd1 Qe6 15. Bxd7+ Nxd7 16. Qb8+ Nxb8 17. Rd8# 1-0
";

// ---- a renderer standing in for the real one -------------------------------------------------------

#[derive(Default)]
struct Gate {
    /// Renders wait while it is set.
    closed: bool,
    /// Renders waiting.
    waiting: usize,
}

/// Records its jobs; answers `GIF89a#<n>` (n: the job's number), fails while `fail` is set, and
/// waits while the gate is closed.
#[derive(Default)]
struct FakeRenderer {
    jobs: Mutex<Vec<GifJob>>,
    fail: AtomicBool,
    gate: Mutex<Gate>,
    opened: Condvar,
}

impl FakeRenderer {
    fn jobs(&self) -> Vec<GifJob> {
        self.jobs.lock().clone()
    }

    fn hold(&self) {
        self.gate.lock().closed = true;
    }

    fn release(&self) {
        self.gate.lock().closed = false;
        self.opened.notify_all();
    }

    fn waiting(&self) -> usize {
        self.gate.lock().waiting
    }

    /// Waits (a few seconds at most) until `n` renders wait at the gate.
    async fn wait_for_waiting(&self, n: usize) {
        for _ in 0..800 {
            if self.waiting() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{} renders waiting, {n} expected", self.waiting());
    }
}

impl Renderer for FakeRenderer {
    fn render(&self, job: &GifJob, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
        let n = {
            let mut jobs = self.jobs.lock();
            jobs.push(job.clone());
            jobs.len()
        };
        let mut gate = self.gate.lock();
        gate.waiting += 1;
        while gate.closed && !cancel.load(Ordering::Relaxed) {
            self.opened.wait_for(&mut gate, Duration::from_millis(20));
        }
        gate.waiting -= 1;
        drop(gate);
        if self.fail.load(Ordering::Relaxed) {
            return Err("illegal move at ply 3".into());
        }
        Ok(format!("GIF89a#{n}").into_bytes())
    }
}

// ---- the server ----------------------------------------------------------------------------------

struct GifServer {
    s: Server,
    gifs: GifService,
    fake: Arc<FakeRenderer>,
}

impl std::ops::Deref for GifServer {
    type Target = Server;

    fn deref(&self) -> &Server {
        &self.s
    }
}

fn gif_config(env: &[(&str, &str)]) -> Config {
    let mut all = vec![
        ("HTTP_RATE_PER_IP", "100000"),
        ("USER_RATE_PER_MIN", "100000"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
    ];
    all.extend_from_slice(env);
    config(&all)
}

/// The GIF routes over `renderer`, with `settings` (default: those of the configuration).
async fn server_with(
    env: &[(&str, &str)],
    renderer: Arc<dyn Renderer>,
    settings: Option<PoolSettings>,
) -> (Server, GifService) {
    let config = gif_config(env);
    let gifs = match settings {
        Some(settings) => {
            GifService::with_settings(settings, usize::try_from(config.gif_cache_mb).unwrap() << 20, renderer)
        }
        None => GifService::new(&config, renderer),
    };
    let store = memory_store(&config).await;
    let (st, g) = (store.clone(), gifs.clone());
    let s = Server::start(config, store, NOW, move |router, config| {
        let log = test_logger("gif-test");
        register(router, GifDeps { config: config.clone(), store: st, gifs: g, log });
    });
    (s, gifs)
}

async fn server(env: &[(&str, &str)]) -> GifServer {
    let fake = Arc::new(FakeRenderer::default());
    let (s, gifs) = server_with(env, fake.clone(), None).await;
    GifServer { s, gifs, fake }
}

/// A finished game record: the Opera game (or `uci`), White wins by checkmate, ratings 1612 / 1588.
fn game_record(id: u64, white: &Player, black: &Player, uci: &str) -> GameRecord {
    let mut g = record(id, white, black, moves_of(uci));
    (g.white_rating, g.black_rating) = (Some(1612), Some(1588));
    (g.status, g.reason) = (status::WHITE_WINS, 1);
    g
}

/// `GET /games/:id/gif` with `query`.
async fn gif(s: &Server, id: u64, token: &str, query: &str) -> crate::http::testing::TestResponse {
    let req = s.t.get(&format!("/api/v1/games/{id}/gif{query}"));
    let req = if token.is_empty() { req } else { req.bearer(token) };
    req.send().await
}

/// `POST /gif` with `body`.
async fn post_gif(s: &Server, body: &Value, token: &str) -> crate::http::testing::TestResponse {
    let req = s.t.post("/api/v1/gif").json(body);
    let req = if token.is_empty() { req } else { req.bearer(token) };
    req.send().await
}

/// The units taken from a shared quota.
fn taken(s: &Server, key: &str) -> f64 {
    s.t.api.rates().shared().peek(key)
}

fn options(size: Size, orientation: Orientation, delay_ms: u32, coords: bool) -> Options {
    Options { size, orientation, delay_ms, coords }
}

// ---- sessions, options, jobs ---------------------------------------------------------------------

#[tokio::test]
async fn both_routes_need_a_session_and_gif_enabled_false_answers_404_gif_disabled() {
    let g = server(&[]).await;
    let alice = g.user("alice").await;
    let bob = g.user("bob").await;
    let rec = game_record(1, &alice, &bob, OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    let r = gif(&g, rec.id, "", "").await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("unauthorized")));
    let r = post_gif(&g, &json!({ "pgn": OPERA_PGN }), "").await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("unauthorized")));
    assert!(!g.gifs.started());

    let off = server(&[("GIF_ENABLED", "false")]).await;
    let bob = off.user("bob").await;
    let carol = off.user("carol").await;
    off.commit(vec![game_record(2, &bob, &carol, OPERA_UCI)]).await;
    // The route's tokens are given back: 31 calls in a minute are all answered 404.
    for _ in 0..31 {
        let r = gif(&off, 2, &bob.token, "").await;
        assert_eq!(
            (r.status, r.json()),
            (
                404,
                json!({ "error": "gif_disabled", "message": "Animated GIFs are turned off on this server." })
            )
        );
    }
    let r = post_gif(&off, &json!({ "pgn": OPERA_PGN }), &bob.token).await;
    assert_eq!((r.status, r.json()["error"].clone()), (404, json!("gif_disabled")));
    assert!(!off.gifs.started(), "no rendering thread");
}

#[tokio::test]
async fn options_defaults_validation_game_ids_unknown_games_nothing_rendered_or_counted() {
    let g = server(&[]).await;
    let alice = g.user("alice").await;
    let bob = g.user("bob").await;
    let rec = game_record(3, &alice, &bob, OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    let bad = [
        ("?size=huge", "size", "size must be one of small, medium, large."),
        ("?size=", "size", "size must be one of small, medium, large."),
        ("?orientation=left", "orientation", "orientation must be white or black."),
        ("?delay=99", "delay", "delay must be an integer from 100 to 3000 (milliseconds per move)."),
        ("?delay=3001", "delay", "delay must be an integer from 100 to 3000 (milliseconds per move)."),
        ("?delay=fast", "delay", "delay must be an integer from 100 to 3000 (milliseconds per move)."),
        ("?delay=500.5", "delay", "delay must be an integer from 100 to 3000 (milliseconds per move)."),
        ("?coords=2", "coords", "coords must be 0 or 1."),
        ("?coords=true", "coords", "coords must be 0 or 1."),
    ];
    for (q, field, message) in bad {
        let r = gif(&g, rec.id, &alice.token, q).await;
        assert_eq!(
            (r.status, r.json()),
            (400, json!({ "error": "invalid_option", "message": message, "field": field })),
            "{q}"
        );
    }
    for (id, code, status) in [
        ("abc", "invalid_game_id", 400),
        ("0", "invalid_game_id", 400),
        ("99999999999999999", "invalid_game_id", 400),
        ("12345", "not_found", 404),
    ] {
        let r = g.t.get(&format!("/api/v1/games/{id}/gif")).bearer(&alice.token).send().await;
        assert_eq!((r.status, r.json()["error"].clone()), (status, json!(code)), "{id}");
    }
    let bad_bodies: Vec<(Value, &str, Option<&str>)> = vec![
        (json!({ "pgn": OPERA_PGN, "size": "xl" }), "invalid_option", Some("size")),
        (json!({ "pgn": OPERA_PGN, "delayMs": 50 }), "invalid_option", Some("delayMs")),
        (json!({ "pgn": OPERA_PGN, "delayMs": "500" }), "invalid_option", Some("delayMs")),
        (json!({ "pgn": OPERA_PGN, "coords": "yes" }), "invalid_option", Some("coords")),
        (json!({ "pgn": OPERA_PGN, "orientation": 1 }), "invalid_option", Some("orientation")),
        (json!({ "pgn": OPERA_PGN, "speed": 2 }), "invalid_request", Some("speed")),
        (json!({}), "invalid_request", Some("pgn")),
        (json!({ "pgn": 42 }), "invalid_request", Some("pgn")),
        (json!({ "pgn": null }), "invalid_request", Some("pgn")),
        (json!([OPERA_PGN]), "invalid_request", None),
        (json!("text"), "invalid_request", None),
    ];
    for (body, code, field) in bad_bodies {
        let r = post_gif(&g, &body, &alice.token).await;
        let j = r.json();
        assert_eq!(
            (r.status, j["error"].as_str(), j.get("field").and_then(Value::as_str)),
            (400, Some(code), field),
            "{body}"
        );
    }
    // The route's limit (30 a minute per account) counts the refused requests too.
    g.advance(60_000);
    let messages = [
        (json!({ "pgn": OPERA_PGN, "speed": 2 }), "unknown field \"speed\""),
        (json!({ "size": "small" }), "\"pgn\" is required"),
        (json!({ "pgn": 42 }), "\"pgn\" must be a string"),
        (json!([1]), "The body must be a JSON object."),
        (json!({ "pgn": OPERA_PGN, "coords": 1 }), "coords must be true or false."),
        (
            json!({ "pgn": OPERA_PGN, "delayMs": 50 }),
            "delayMs must be an integer from 100 to 3000 (milliseconds per move).",
        ),
    ];
    for (body, message) in messages {
        assert_eq!(post_gif(&g, &body, &alice.token).await.json()["message"], message, "{body}");
    }
    // The first unknown key in the order of Object.keys (array indexes first).
    let r =
        g.t.post("/api/v1/gif")
            .bearer(&alice.token)
            .body("application/json", r#"{"zeta":1,"7":2,"pgn":"1. e4 *"}"#)
            .send()
            .await;
    assert_eq!(r.json()["field"], "7");
    assert!(g.fake.jobs().is_empty());
    assert!(!g.gifs.started(), "nothing rendered");
    assert_eq!(taken(&g, &format!("gif_user_min:u{}", alice.id)), 0.0, "no render quota taken");

    gif(&g, rec.id, &alice.token, "").await;
    gif(&g, rec.id, &alice.token, "?size=large&orientation=black&delay=100&coords=0").await;
    let body = json!({ "pgn": OPERA_PGN, "size": "small", "orientation": "black", "delayMs": 3000, "coords": false });
    post_gif(&g, &body, &alice.token).await;
    let opts: Vec<Options> = g.fake.jobs().iter().map(|j| j.options).collect();
    assert_eq!(
        opts,
        [
            options(Size::Medium, Orientation::White, 500, true),
            options(Size::Large, Orientation::Black, 100, false),
            options(Size::Small, Orientation::Black, 3000, false),
        ]
    );
}

#[tokio::test]
async fn get_the_job_from_the_stored_record_and_the_answer_is_the_file() {
    let g = server(&[]).await;
    let alice = g.user("alice").await;
    let other = g.user("other").await;
    let mut rec = game_record(4, &alice, &other, "e2e4 e7e5 g1f3 d7d6 d2d4 c8g4 d4e5 g4f3 d1f3 d6e5");
    (rec.status, rec.reason, rec.white_rating, rec.black_name) =
        (status::BLACK_WINS, 2, None, "deleted#99".into());
    g.commit(vec![rec.clone()]).await;
    let r = gif(&g, rec.id, &alice.token, "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("image/gif"));
    assert_eq!(
        r.header("content-disposition"),
        Some(format!("attachment; filename=\"scacelith-{}.gif\"", rec.id).as_str())
    );
    assert_eq!(r.header("content-length"), Some(r.body.len().to_string().as_str()));
    assert_eq!(&r.body[..6], b"GIF89a");
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(
        g.fake.jobs()[0],
        GifJob {
            start_fen: None,
            moves: rec.moves.clone(),
            white: JobPlayer { name: "alice".into(), rating: None },
            black: JobPlayer { name: "deleted#99".into(), rating: Some(1588) },
            result: GameResult::BlackWins,
            footer: Some("Resignation".into()),
            options: options(Size::Medium, Orientation::White, 500, true),
        }
    );
    let head =
        g.t.request(Method::HEAD, &format!("/api/v1/games/{}/gif", rec.id)).bearer(&alice.token).send().await;
    assert_eq!(
        (head.status, head.body.len(), head.header("content-length")),
        (200, 0, r.header("content-length"))
    );

    let mut aborted = game_record(5, &alice, &other, "e2e4");
    (aborted.status, aborted.reason) = (status::ABORTED, 22);
    g.commit(vec![aborted.clone()]).await;
    assert_eq!(gif(&g, aborted.id, &alice.token, "").await.status, 200);
    let last = g.fake.jobs().pop().unwrap();
    assert_eq!((last.result, last.footer), (GameResult::Unfinished, Some("Game aborted".into())));
}

#[tokio::test]
async fn post_the_first_game_of_the_pgn_its_tags_invalid_pgn_with_line_and_column_game_length() {
    let g = server(&[("GIF_MAX_PLIES", "40")]).await;
    let alice = g.user("alice").await;
    let other = g.user("other").await;
    let pgn = "[Event \"Club\"]\n[White \"Müller, Hans\"]\n[Black \"Ljubojević ♞\"]\n[WhiteElo \"2650\"]\n[BlackElo \"?\"]\n\
               [Result \"1/2-1/2\"]\n[Termination \"time forfeit\"]\n\n1. e4 e5 2. Nf3 Nc6 {a comment} 3. Bb5 (3. Bc4 Bc5) a6 \
               1/2-1/2\n\n[Event \"Second\"]\n\n1. d4 d5 *\n";
    let r = post_gif(&g, &json!({ "pgn": pgn }), &alice.token).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-disposition"), Some("attachment; filename=\"scacelith-game.gif\""));
    assert_eq!(
        g.fake.jobs()[0],
        GifJob {
            start_fen: None,
            moves: moves_of("e2e4 e7e5 g1f3 b8c6 f1b5 a7a6"),
            white: JobPlayer { name: "Muller, Hans".into(), rating: Some(2650) },
            black: JobPlayer { name: "Ljubojevic ?".into(), rating: None },
            result: GameResult::Draw,
            footer: Some("Time forfeit".into()),
            options: Options::default(),
        }
    );
    let fen =
        "[Termination \"Normal\"]\n[SetUp \"1\"]\n[FEN \"4k3/8/8/8/8/8/4P3/4K3 w - - 0 1\"]\n\n1. e4 Kd7 *";
    assert_eq!(post_gif(&g, &json!({ "pgn": fen }), &alice.token).await.status, 200);
    let job = &g.fake.jobs()[1];
    assert_eq!(job.start_fen.as_deref(), Some("4k3/8/8/8/8/8/4P3/4K3 w - - 0 1"));
    assert_eq!((job.footer.clone(), job.result, job.white.name.as_str()), (None, GameResult::Unfinished, ""));

    let r = post_gif(&g, &json!({ "pgn": "[White \"a\"]\n\n1. e4 e5\n2. Ke3 Nf6 *" }), &alice.token).await;
    let j = r.json();
    assert_eq!(
        (r.status, j["error"].clone(), j["line"].clone(), j["column"].clone()),
        (400, json!("invalid_pgn"), json!(4), json!(4))
    );
    assert!(j["message"].as_str().unwrap().contains("illegal move 'Ke3'"), "{j}");
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["error", "message", "line", "column"]);
    let j = post_gif(&g, &json!({ "pgn": "just text" }), &alice.token).await.json();
    assert_eq!(j["error"], "invalid_pgn");
    assert!(j["line"].is_u64() && j["column"].is_u64());
    let long_comment = format!("1. e4 {{{}}} *", "x".repeat(66_000));
    let r = post_gif(&g, &json!({ "pgn": long_comment }), &alice.token).await;
    let j = r.json();
    assert_eq!(
        (r.status, j["error"].clone(), j["line"].clone(), j["column"].clone()),
        (400, json!("invalid_pgn"), json!(1), json!(1)),
        "more than 65536 bytes"
    );
    assert!(j["message"].as_str().unwrap().contains("too large"), "{j}");
    // Above HTTP_BODY_LIMIT (16 KiB): the route's own body limit.
    let big = format!("{{{}}}\n1. d4 d5 *", "x".repeat(40_000));
    assert_eq!(post_gif(&g, &json!({ "pgn": big }), &alice.token).await.status, 200);

    // GIF_MAX_PLIES = 40: 41 plies answer 422 on both routes.
    assert_eq!(
        post_gif(&g, &json!({ "pgn": format!("{LONG_SAN} *") }), &alice.token).await.status,
        200,
        "40 plies"
    );
    let r = post_gif(&g, &json!({ "pgn": format!("{LONG_SAN} 21. Be2 *") }), &alice.token).await;
    assert_eq!(
        (r.status, r.json()),
        (
            422,
            json!({ "error": "game_too_long", "message": "The game is too long for a GIF (more than 40 plies, at most 40)." })
        )
    );
    let mut rec = game_record(6, &alice, &other, &format!("{LONG_UCI} f1e2"));
    (rec.status, rec.reason) = (status::DRAW, 12);
    g.commit(vec![rec.clone()]).await;
    let r = gif(&g, rec.id, &alice.token, "").await;
    assert_eq!(
        (r.status, r.json()),
        (
            422,
            json!({ "error": "game_too_long", "message": "The game is too long for a GIF (41 plies, at most 40)." })
        )
    );
    assert_eq!(taken(&g, &format!("gif_user_min:u{}", alice.id)), 4.0, "only the four renders took quotas");
}

#[tokio::test]
async fn the_post_body_limit_is_fixed_whatever_http_body_limit() {
    let g = server(&[("HTTP_BODY_LIMIT", "1024")]).await;
    let carol = g.user("carol").await;
    let sized = |n: usize| {
        let text = format!("{{\"pgn\":\"{{{}}}\\n1. d4 d5 *\"}}", "x".repeat(n - 24));
        assert_eq!(text.len(), n);
        text
    };
    let post =
        |text: String| g.t.post("/api/v1/gif").bearer(&carol.token).body("application/json", text).send();
    assert_eq!(post(sized(40_000)).await.status, 200, "above HTTP_BODY_LIMIT");
    let r = post(sized(GIF_BODY_LIMIT_BYTES)).await;
    assert_eq!(
        (r.status, r.json()["error"].clone()),
        (400, json!("invalid_pgn")),
        "read, refused by the PGN reader"
    );
    let r = post(sized(GIF_BODY_LIMIT_BYTES + 1)).await;
    assert_eq!((r.status, r.json()["error"].clone()), (413, json!("payload_too_large")));
    assert_eq!(GIF_BODY_LIMIT_BYTES, 135_168);
}

// ---- the cache and the renders in flight ---------------------------------------------------------

#[tokio::test]
async fn a_cached_or_in_flight_gif_costs_no_render_and_no_quota_a_new_name_is_a_new_picture() {
    let g = server(&[("GIF_USER_RENDERS_PER_MIN", "50"), ("GIF_USER_RENDERS_PER_HOUR", "100")]).await;
    let alice = g.user("alice").await;
    let bob = g.user("bob").await;
    let rec = game_record(7, &alice, &bob, OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    let quota = format!("gif_user_min:u{}", alice.id);
    let a = gif(&g, rec.id, &alice.token, "").await;
    let b = gif(&g, rec.id, &alice.token, "").await;
    assert_eq!((a.status, b.status), (200, 200));
    assert_eq!(a.body, b.body);
    assert_eq!(g.fake.jobs().len(), 1);
    assert_eq!(taken(&g, &quota), 1.0, "the second request took no render quota");
    gif(&g, rec.id, &alice.token, "?orientation=black").await;
    assert_eq!(g.fake.jobs().len(), 2, "other options: another picture");
    // Black's account is deleted: its name changes in the record, the old picture is not served.
    g.store.users().anonymize(bob.id, NOW).await.unwrap();
    let c = gif(&g, rec.id, &alice.token, "").await;
    assert_eq!(g.fake.jobs().len(), 3);
    assert_eq!(g.fake.jobs()[2].black.name, format!("deleted#{}", bob.id));
    assert_ne!(c.body, a.body);
    // The same game through POST: the job is the key, never the PGN's text.
    post_gif(&g, &json!({ "pgn": OPERA_PGN }), &alice.token).await;
    let retagged = format!("{}\n\n", OPERA_PGN.replace("[Site \"Paris FRA\"]\n", ""));
    post_gif(&g, &json!({ "pgn": retagged }), &alice.token).await;
    assert_eq!(g.fake.jobs().len(), 4, "a PGN that differs only by tags the picture does not show");

    // Two requests at once for one GIF: one render, one quota.
    g.fake.hold();
    let first =
        tokio::spawn(g.t.get(&format!("/api/v1/games/{}/gif?delay=700", rec.id)).bearer(&alice.token).send());
    g.fake.wait_for_waiting(1).await;
    let spent = taken(&g, &quota);
    let second =
        tokio::spawn(g.t.get(&format!("/api/v1/games/{}/gif?delay=700", rec.id)).bearer(&alice.token).send());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(g.fake.jobs().len(), 5, "the second waits for the same render");
    assert_eq!(taken(&g, &quota), spent, "and takes no quota");
    g.fake.release();
    let (r1, r2) = (first.await.unwrap(), second.await.unwrap());
    assert_eq!((r1.status, r2.status), (200, 200));
    assert_eq!(r1.body, r2.body);
    assert_eq!(g.fake.jobs().len(), 5);
}

#[tokio::test]
async fn without_a_cache_every_request_renders() {
    let g = server(&[("GIF_CACHE_MB", "0")]).await;
    let alice = g.user("alice").await;
    let bob = g.user("bob").await;
    let rec = game_record(8, &alice, &bob, OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    gif(&g, rec.id, &alice.token, "").await;
    gif(&g, rec.id, &alice.token, "").await;
    assert_eq!(g.fake.jobs().len(), 2);
}

// ---- render quotas -------------------------------------------------------------------------------

#[tokio::test]
async fn render_quotas_per_account_4_a_minute_and_30_an_hour() {
    let g = server(&[]).await;
    let alice = g.user("alice").await;
    let bob = g.user("bob").await;
    let rec = game_record(9, &alice, &bob, OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    let mut delay = 100;
    let mut next = |who: &Player| {
        let req = g.t.get(&format!("/api/v1/games/{}/gif?delay={delay}", rec.id)).bearer(&who.token);
        delay += 1;
        req.send()
    };
    for _ in 0..4 {
        assert_eq!(next(&alice).await.status, 200);
    }
    let r = next(&alice).await;
    let j = r.json();
    assert_eq!((r.status, j["error"].clone()), (429, json!("rate_limited")), "the fifth of the minute");
    assert_eq!(r.header("retry-after"), Some(j["retryAfter"].to_string().as_str()));
    let after = j["retryAfter"].as_u64().unwrap();
    assert!((1..=75).contains(&after), "{after}");
    assert_eq!(gif(&g, rec.id, &alice.token, "?delay=100").await.status, 200, "a cached GIF is still served");
    assert_eq!(next(&bob).await.status, 200, "another account");

    // The hour: 26 more at 3 a minute (30 in all), then the hourly quota.
    g.advance(120_000);
    for i in 0..26 {
        assert_eq!(next(&alice).await.status, 200, "render {}", 5 + i);
        g.advance(20_000);
    }
    g.advance(60_000);
    let r = next(&alice).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")), "the 31st of the hour");
    assert!(r.json()["retryAfter"].as_u64().unwrap() > 60);
}

#[tokio::test]
async fn render_quotas_per_address_shared_by_the_accounts_behind_it_ipv6_three_times_per_48() {
    let g = server(&[
        ("GIF_IP_RENDERS_PER_MIN", "2"),
        ("GIF_IP_RENDERS_PER_HOUR", "100"),
        ("GIF_USER_RENDERS_PER_MIN", "50"),
        ("GIF_USER_RENDERS_PER_HOUR", "100"),
    ])
    .await;
    let users = [g.user("alice").await, g.user("bob").await, g.user("carol").await, g.user("dave").await];
    let rec = game_record(10, &users[0], &users[1], OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    let mut delay = 100;
    let mut next = |u: &Player, ip: &str| {
        let req =
            g.t.clone().at(ip).get(&format!("/api/v1/games/{}/gif?delay={delay}", rec.id)).bearer(&u.token);
        delay += 1;
        req.send()
    };
    assert_eq!(next(&users[0], "192.0.2.9").await.status, 200);
    assert_eq!(next(&users[1], "192.0.2.9").await.status, 200);
    let r = next(&users[2], "192.0.2.9").await;
    assert_eq!(
        (r.status, r.json()["error"].clone()),
        (429, json!("rate_limited")),
        "a third account behind the address"
    );
    assert_eq!(next(&users[2], "192.0.2.10").await.status, 200, "another address");
    for net in 1..=3 {
        assert_eq!(next(&users[net - 1], &format!("2001:db8:9:{net}::1")).await.status, 200);
        assert_eq!(next(&users[net], &format!("2001:db8:9:{net}::2")).await.status, 200);
    }
    assert_eq!(next(&users[3], "2001:db8:9:4::1").await.status, 429, "the /48: 6");
    assert_eq!(taken(&g, "gif_ip_min/48:2001:db8:9::/48"), 6.0);
}

// ---- the busy pool, failures, the lazy pool --------------------------------------------------------

#[tokio::test]
async fn busy_pool_503_server_busy_with_retry_after_and_every_token_given_back() {
    let fake = Arc::new(FakeRenderer::default());
    let settings = PoolSettings {
        threads: 1,
        queue_max: 0,
        queue_timeout: Duration::from_secs(10),
        render_timeout: Duration::from_secs(30),
        idle: None,
    };
    let env = [
        ("GIF_USER_RENDERS_PER_MIN", "1"),
        ("GIF_USER_RENDERS_PER_HOUR", "1"),
        ("GIF_IP_RENDERS_PER_MIN", "1"),
        ("GIF_IP_RENDERS_PER_HOUR", "1"),
    ];
    let (s, _gifs) = server_with(&env, fake.clone(), Some(settings)).await;
    let alice = s.user("alice").await;
    let bob = s.user("bob").await;
    let rec = game_record(11, &alice, &bob, OPERA_UCI);
    s.commit(vec![rec.clone()]).await;
    // Bob, from another address, holds the only thread (alice's quotas stay whole).
    fake.hold();
    let held = tokio::spawn(
        s.t.clone()
            .at("198.51.100.50")
            .post("/api/v1/gif")
            .bearer(&bob.token)
            .json(&json!({ "pgn": "1. e4 *" }))
            .send(),
    );
    fake.wait_for_waiting(1).await;
    for i in 0..31 {
        let r = gif(&s, rec.id, &alice.token, "").await;
        let j = r.json();
        assert_eq!((r.status, j["error"].clone()), (503, json!("server_busy")), "attempt {i}");
        assert_eq!(j["message"], "The server is busy making other GIFs; try again in a few seconds.");
        let after = j["retryAfter"].as_u64().unwrap();
        assert!((3..=10).contains(&after), "{after}");
        assert_eq!(r.header("retry-after"), Some(after.to_string().as_str()));
    }
    for key in [
        format!("gif_user_min:u{}", alice.id),
        format!("gif_user_hour:u{}", alice.id),
        "gif_ip_min:203.0.113.10".to_string(),
        "gif_ip_hour:203.0.113.10".to_string(),
    ] {
        assert_eq!(taken(&s, &key), 0.0, "{key} given back");
    }
    fake.release();
    assert_eq!(held.await.unwrap().status, 200);
    // 31 attempts > the route's 30 a minute, every quota is 1, and no time passed: all of it was
    // given back each time.
    assert_eq!(gif(&s, rec.id, &alice.token, "").await.status, 200);
    assert_eq!(
        gif(&s, rec.id, &alice.token, "?delay=900").await.status,
        429,
        "the quotas are spent by the render"
    );
}

#[tokio::test]
async fn a_failed_render_answers_500_render_failed_and_keeps_the_quotas() {
    let logs = LogCapture::start();
    let g = server(&[("GIF_USER_RENDERS_PER_MIN", "1")]).await;
    let alice = g.user("alice").await;
    let bob = g.user("bob").await;
    let rec = game_record(12, &alice, &bob, OPERA_UCI);
    g.commit(vec![rec.clone()]).await;
    assert_eq!(
        post_gif(&g, &json!({ "pgn": "1. d4 *" }), &alice.token).await.status,
        200,
        "the minute's quota"
    );
    g.advance(120_000);
    g.fake.fail.store(true, Ordering::Relaxed);
    let r = gif(&g, rec.id, &alice.token, "").await;
    assert_eq!(
        (r.status, r.json()),
        (500, json!({ "error": "render_failed", "message": "The GIF could not be made." }))
    );
    let logged = logs.records("gif-test");
    assert!(
        logged.iter().any(|l| l["level"] == "warn"
            && l["msg"] == "GIF render failed"
            && l["err"]["message"] == "GIF render failed: illegal move at ply 3"
            && l["plies"] == 33
            && l["route"] == "/api/v1/games/:id/gif"),
        "{logged:?}"
    );
    g.fake.fail.store(false, Ordering::Relaxed);
    assert_eq!(gif(&g, rec.id, &alice.token, "").await.status, 429, "the failed render counted");
}

#[tokio::test]
async fn the_pool_starts_with_the_first_render_and_closes_with_the_api() {
    let g = server(&[
        ("GIF_THREADS", "2"),
        ("GIF_QUEUE_MAX", "7"),
        ("GIF_QUEUE_TIMEOUT_MS", "1234"),
        ("GIF_RENDER_TIMEOUT_MS", "5678"),
    ])
    .await;
    let alice = g.user("alice").await;
    assert!(!g.gifs.started());
    assert_eq!(post_gif(&g, &json!({ "pgn": "x" }), &alice.token).await.status, 400);
    assert!(!g.gifs.started(), "not for a refused request");
    post_gif(&g, &json!({ "pgn": "1. e4 *" }), &alice.token).await;
    post_gif(&g, &json!({ "pgn": "1. d4 *" }), &alice.token).await;
    assert!(g.gifs.started());
    assert_eq!(g.gifs.pool_stats().unwrap().threads, 2);
    let timeouts: Vec<_> =
        g.t.api.router().routes().iter().map(|r| (r.label().to_string(), r.opts().timeout)).collect();
    let expected = Some(Duration::from_millis(1234 + 5678 + 5000));
    assert_eq!(
        timeouts,
        [("GET /api/v1/games/:id/gif".to_string(), expected), ("POST /api/v1/gif".to_string(), expected)],
        "the handler waits for the queue and the render"
    );
    g.t.api.close().await;
    assert!(!g.gifs.started(), "closed with the API");
    let job = g.fake.jobs()[0].clone();
    assert_eq!(g.gifs.render(job).await, Err(GifError::Busy("GIF renderer closed".into())));
    assert_eq!(tag_text("Ünïcödé ✓  name", 48), "Unicode ? name");
}

#[tokio::test]
async fn registration_session_rate_body_limit() {
    let g = server(&[]).await;
    let routes = g.t.api.router().routes();
    assert_eq!(routes.len(), 2);
    for r in routes {
        assert_eq!(r.opts().auth, AuthMode::Required);
        let rate = &r.opts().rates[0];
        assert_eq!(
            (rate.key.as_ref(), rate.limit, rate.window_ms, rate.by_user, rate.shared),
            ("gif", 30.0, 60_000, true, false)
        );
    }
    assert!(routes[1].opts().own_body_validation);
    assert_eq!(routes[1].opts().body_limit, Some(135_168));
}

// ---- real renders --------------------------------------------------------------------------------

/// The frames of a GIF: the delay of each (centiseconds), the width and the loop count.
#[derive(Debug, Default)]
struct DecodedGif {
    width: u16,
    loops: Option<u16>,
    delays: Vec<u16>,
}

/// Walks the blocks of a GIF89a file (no pixel decoding).
fn decode_gif(b: &[u8]) -> DecodedGif {
    assert_eq!(&b[..6], b"GIF89a");
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let mut out = DecodedGif { width: u16_at(6), ..DecodedGif::default() };
    let table = |packed: u8| if packed & 0x80 != 0 { 3 << ((packed & 7) + 1) } else { 0 };
    let mut i = 13 + table(b[10]);
    let skip_blocks = |mut i: usize| {
        while b[i] != 0 {
            i += usize::from(b[i]) + 1;
        }
        i + 1
    };
    let mut delay = 0;
    loop {
        match b[i] {
            0x21 if b[i + 1] == 0xf9 => {
                delay = u16_at(i + 4);
                i = skip_blocks(i + 2);
            }
            0x21 if b[i + 1] == 0xff => {
                if &b[i + 3..i + 14] == b"NETSCAPE2.0" {
                    out.loops = Some(u16_at(i + 16));
                }
                i = skip_blocks(i + 2);
            }
            0x21 => i = skip_blocks(i + 2),
            0x2c => {
                out.delays.push(delay);
                i += 10 + table(b[i + 9]);
                i = skip_blocks(i + 1);
            }
            0x3b => return out,
            other => panic!("unexpected block {other:#x} at {i}"),
        }
    }
}

async fn real_server() -> (Server, GifService) {
    server_with(&[], Arc::new(GameRenderer::<ChessRules>::new()), None).await
}

#[tokio::test]
async fn a_stored_game_rendered_by_the_real_renderer_one_frame_per_position_its_pgn_gives_the_same() {
    let (s, _gifs) = real_server().await;
    let morphy = s.user("Morphy").await;
    let isouard = s.user("Isouard").await;
    let mut rec = game_record(13, &morphy, &isouard, OPERA_UCI);
    rec.rated = false;
    s.commit(vec![rec.clone()]).await;
    let r = gif(&s, rec.id, &morphy.token, "?size=small&delay=200").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let decoded = decode_gif(&r.body);
    assert_eq!(decoded.delays.len(), rec.moves.len() + 1, "the start position and one frame per move");
    assert_eq!(decoded.loops, Some(0), "loops forever");
    assert_eq!(decoded.delays[1], 20);
    assert_eq!(decoded.delays.last(), Some(&300), "the final position held 3 s");
    assert_eq!(decoded.width, 284, "small, with coordinates");

    let stored = s.store.games().by_id(rec.id).await.unwrap().unwrap();
    let pgn = game_pgn(&stored, &s.config).unwrap();
    let p = post_gif(&s, &json!({ "pgn": pgn, "size": "small", "delayMs": 200 }), &morphy.token).await;
    assert_eq!(p.status, 200);
    assert_eq!(decode_gif(&p.body).delays.len(), rec.moves.len() + 1);
}

#[tokio::test]
async fn fools_mate_download_decode_cache_and_the_same_game_from_its_pgn() {
    let (s, _gifs) = real_server().await;
    let alice = s.user("alice_gif").await;
    let bob = s.user("bob_gif").await;
    let mut rec = game_record(14, &alice, &bob, "f2f3 e7e5 g2g4 d8h4");
    (rec.status, rec.reason) = (status::BLACK_WINS, 1);
    s.commit(vec![rec.clone()]).await;
    assert_eq!(gif(&s, rec.id, "", "").await.status, 401);
    let r = gif(&s, rec.id, &alice.token, "?size=small&delay=250").await;
    assert_eq!(r.status, 200);
    let decoded = decode_gif(&r.body);
    assert_eq!(decoded.delays, [100, 25, 25, 25, 300], "the start held 1 s, the mate 3 s");
    assert!(decoded.width > 0 && decoded.width < 300, "small: {}", decoded.width);
    // The same picture again: from the cache, byte for byte, for the other player too.
    let again = gif(&s, rec.id, &bob.token, "?size=small&delay=250").await;
    assert_eq!((again.status, &again.body), (200, &r.body));
    let stored = s.store.games().by_id(rec.id).await.unwrap().unwrap();
    let pgn = game_pgn(&stored, &s.config).unwrap();
    let p = post_gif(&s, &json!({ "pgn": pgn, "size": "small", "delayMs": 250 }), &alice.token).await;
    assert_eq!(p.header("content-disposition"), Some("attachment; filename=\"scacelith-game.gif\""));
    assert_eq!(decode_gif(&p.body).delays.len(), 5);
    let bad = post_gif(&s, &json!({ "pgn": "1. e4 e5 2. Ke3 *" }), &alice.token).await.json();
    assert_eq!((bad["error"].clone(), bad["line"].clone()), (json!("invalid_pgn"), json!(1)));
}
