//! The game routes, ported from the Node suites `store.routes` (game records, rating changes) and
//! `http.games` (the PGN export and its fixtures, `GET /games/:id/pgn`, `you` / `reportable`),
//! through the real pipeline and an in-memory store.

use std::path::PathBuf;
use std::sync::Arc;

use http::Method;
use scacelith_chess::{PGN_LIMITS, read_pgn};
use serde_json::{Value, json};

use super::super::read_support::*;
use super::super::reports::{self, ReportsDeps};
use super::*;
use crate::http::json::stringify;
use crate::store::tests::support::LogCapture;
use crate::store::{GameRecord, RatingChanges, RatingDelta};

/// 2026-10-01T12:00:00Z.
const NOW: i64 = 1_790_856_000_000;
/// 2026-09-28T12:00:00Z.
const STARTED: i64 = 1_790_596_800_000;
/// Think times of the records of the Node suite `http.games`.
const SPENT: [u32; 12] = [0, 0, 1530, 2210, 4120, 980, 3333, 2047, 1500, 999, 1200, 1300];

/// Status and reason codes.
const WHITE_WINS: u8 = 1;
const BLACK_WINS: u8 = 2;
const DRAW: u8 = 3;
const ABORTED: u8 = 4;
const CHECKMATE: u8 = 1;
const RESIGNATION: u8 = 2;
const TIMEOUT: u8 = 3;
const TIMEOUT_VS_INSUFFICIENT: u8 = 7;
const ABANDONMENT: u8 = 20;
const NO_SHOW: u8 = 23;
const FORFEIT: u8 = 24;

/// The game, record and PGN routes, and the reports route, over `store`.
fn routes(router: &mut Router, config: &Arc<Config>, store: &Store, log: &str) {
    let desk = Arc::new(StoreDesk { store: store.clone(), reports_per_day: config.reports_per_day });
    register(
        router,
        GamesDeps {
            config: config.clone(),
            store: store.clone(),
            reports: desk.clone(),
            log: test_logger(log),
        },
    );
    reports::register(router, ReportsDeps { config: config.clone(), desk });
}

/// The server of the Node suite `http.games`: REPORTS_PER_DAY 2, the clock at [`NOW`].
async fn start(log: &str) -> Server {
    let config = config(&[
        ("HTTP_RATE_PER_IP", "100000"),
        ("USER_RATE_PER_MIN", "100000"),
        ("SERVER_NAME", "Test Server"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
        ("REPORTS_PER_DAY", "2"),
    ]);
    let store = memory_store(&config).await;
    let s = store.clone();
    let log = log.to_string();
    Server::start(config, store, NOW, move |router, config| routes(router, config, &s, &log))
}

/// A record of the Node suite `http.games`: the moves with their clocks (3+2), started two hours
/// before [`NOW`], ended an hour before, White wins by resignation.
fn rec(id: u64, white: &Player, black: &Player, uci: &str) -> GameRecord {
    let moves = moves_of(uci);
    let mut clocks = [180_000u32, 180_000];
    let (mut spent, mut clock) = (Vec::new(), Vec::new());
    for i in 0..moves.len() {
        let s = if i < 2 { 0 } else { SPENT[i % SPENT.len()] };
        if i >= 2 {
            clocks[i % 2] = clocks[i % 2] + 2000 - s;
        }
        spent.push(s);
        clock.push(clocks[i % 2]);
    }
    let mut g = record(id, white, black, moves);
    g.started_at = Some(NOW - 2 * HOUR);
    g.ended_at = Some(NOW - HOUR);
    g.spent_ms = Some(spent);
    g.clock_ms = Some(clock);
    g
}

/// A stored game as the store returns it, from a record (no rating change).
fn stored(r: &GameRecord) -> Game {
    Game {
        summary: GameSummary {
            id: r.id,
            category: r.category.clone(),
            rated: r.rated,
            base_ms: r.base_ms,
            inc_ms: r.inc_ms,
            white_id: r.white_id,
            black_id: r.black_id,
            white_name: r.white_name.clone(),
            black_name: r.black_name.clone(),
            white_rating: r.white_rating,
            black_rating: r.black_rating,
            started_at: r.started_at.unwrap_or(0),
            ended_at: r.ended_at.unwrap_or(0),
            status: r.status,
            reason: r.reason,
            ply_count: r.moves.len() as i64,
            rematch_of: r.rematch_of,
            flags: r.flags,
            rating_changes: None,
        },
        moves: r.moves.clone(),
        spent_ms: r.spent_ms.clone().unwrap_or_default(),
        clock_ms: r.clock_ms.clone().unwrap_or_default(),
    }
}

// ---- a small reader of the server PGN (tags, SAN, [%clk] / [%emt], comments) ---------------------

#[derive(Debug, Default)]
struct ParsedPgn {
    tags: Vec<(String, String)>,
    moves: Vec<u16>,
    sans: Vec<String>,
    clk: Vec<Option<i64>>,
    emt: Vec<Option<i64>>,
    result: String,
    comment: String,
}

impl ParsedPgn {
    fn tag(&self, name: &str) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn names(&self) -> Vec<&str> {
        self.tags.iter().map(|(k, _)| k.as_str()).collect()
    }
}

/// h:mm:ss.f in milliseconds.
fn clock_ms(s: &str) -> i64 {
    let (hms, tenths) = s.split_once('.').expect("h:mm:ss.f");
    let parts: Vec<i64> = hms.split(':').map(|p| p.parse().expect("digits")).collect();
    assert_eq!(parts.len(), 3, "{s}");
    ((parts[0] * 60 + parts[1]) * 60 + parts[2]) * 1000 + tenths.parse::<i64>().expect("tenths") * 100
}

fn parse_pgn(text: &str) -> ParsedPgn {
    let game = read_pgn(text, &PGN_LIMITS).expect("the server's PGN reads back");
    let mut p = ParsedPgn { tags: game.tags.clone(), moves: game.moves.clone(), ..ParsedPgn::default() };
    let movetext = text[text.find("\n\n").expect("a blank line after the tags") + 2..].replace('\n', " ");
    let mut rest = movetext.as_str();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(inner) = rest.strip_prefix('{') {
            let end = inner.find('}').expect("a closed comment");
            let mut words = inner[..end].to_string();
            for kind in ["clk", "emt"] {
                let open = format!("[%{kind} ");
                if let Some(at) = words.find(&open) {
                    let close = at + words[at..].find(']').expect("a closed command");
                    let ms = clock_ms(&words[at + open.len()..close]);
                    let slot = if kind == "clk" { &mut p.clk } else { &mut p.emt };
                    *slot.last_mut().expect("a command after a move") = Some(ms);
                    words.replace_range(at..=close, "");
                }
            }
            let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
            p.comment.push_str(&words);
            rest = &inner[end + 1..];
            continue;
        }
        let end = rest.find(' ').unwrap_or(rest.len());
        let tok = &rest[..end];
        rest = &rest[end..];
        let number = tok.trim_end_matches('.');
        if !number.is_empty() && number.len() < tok.len() && number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if ["1-0", "0-1", "1/2-1/2", "*"].contains(&tok) {
            p.result = tok.to_string();
        } else {
            p.sans.push(tok.to_string());
            p.clk.push(None);
            p.emt.push(None);
        }
    }
    p
}

// ---- helpers of the records ----------------------------------------------------------------------

#[test]
fn uci_of_squares_and_promotions() {
    assert_eq!(uci_of(12 | (28 << 6)), "e2e4");
    assert_eq!(uci_of(4 | (6 << 6)), "e1g1");
    assert_eq!(uci_of(52 | (60 << 6) | (5 << 12)), "e7e8q");
    assert_eq!(uci_of(9 | (2 << 12)), "b2a1n");
    assert_eq!(uci_of(63), "h8a1");
    assert_eq!(uci_of(12 | (28 << 6) | (1 << 12)), "e2e4", "no letter for a pawn or a king code");
}

#[test]
fn pgn_clock_h_mm_ss_f_tenths_truncated() {
    assert_eq!(pgn_clock(0), "0:00:00.0");
    assert_eq!(pgn_clock(99), "0:00:00.0");
    assert_eq!(pgn_clock(178_649), "0:02:58.6");
    assert_eq!(pgn_clock(59_999), "0:00:59.9");
    assert_eq!(pgn_clock(3_600_000 + 61_000), "1:01:01.0");
    assert_eq!(pgn_clock(36_000_000 * 3 + 5), "30:00:00.0");
    assert_eq!(pgn_clock(-5), "0:00:00.0");
}

#[test]
fn ids_limits_and_texts() {
    for good in ["1", "12345", "9007199254740991"] {
        assert!(parse_game_id(good).is_some(), "{good}");
    }
    for bad in
        ["", "abc", "0", "-1", "1.5", "99999999999999999", "12e3", "9007199254740992", "01", " 1", "+1"]
    {
        assert_eq!(parse_game_id(bad), None, "{bad}");
    }
    assert_eq!(parse_limit("7", 50), Some(7));
    assert_eq!(parse_limit("500", 50), Some(50));
    assert_eq!(parse_limit("007", 50), Some(7));
    for bad in ["0", "1000", "x", "", "-1", "1.5"] {
        assert_eq!(parse_limit(bad, 50), None, "{bad}");
    }
    assert_eq!(time_control_text(180_000, 2_000), "180+2");
    assert_eq!(time_control_text(90_500, 1_499), "91+1", "Math.round");
    assert_eq!(pgn_date(STARTED), "2026.09.28");
    assert_eq!(pgn_time(STARTED + 3_723_000), "13:02:03");
    assert_eq!(
        (result_text(1), result_text(2), result_text(3), result_text(4), result_text(0)),
        ("1-0", "0-1", "1/2-1/2", "*", "*")
    );
}

// ---- GET /games/:id (Node store.routes) ----------------------------------------------------------

async fn record_server() -> Server {
    let config = config(&[
        ("PROVISIONAL_GAMES", "2"),
        ("SERVER_NAME", "Test Server"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
        ("HTTP_RATE_PER_IP", "100000"),
    ]);
    let store = memory_store(&config).await;
    let s = store.clone();
    Server::start(config, store, NOW, move |router, config| routes(router, config, &s, "games-record-test"))
}

/// The game of the Node suite `store.routes`: 1. e4 e5 2. Nf3 Nc6, White wins by resignation.
fn short_game(id: u64, white: &Player, black: &Player) -> GameRecord {
    let mut g = record(id, white, black, vec![12 | (28 << 6), 52 | (36 << 6), 6 | (21 << 6), 57 | (42 << 6)]);
    g.started_at = Some(STARTED);
    g.ended_at = Some(STARTED + 600_000);
    g.spent_ms = Some(vec![0, 0, 1500, 2100]);
    g.clock_ms = Some(vec![180_000, 180_000, 180_500, 179_900]);
    g
}

#[tokio::test]
async fn game_record_players_result_uci_moves_with_times_pgn_tags_rating_changes() {
    let s = record_server().await;
    let erin = s.user("Erin").await;
    let finn = s.user("Finn").await;
    let g = short_game(3_000_000_000_001, &erin, &finn);
    s.commit(vec![g.clone()]).await;
    let res = s.t.get(&format!("/api/v1/games/{}", g.id)).send().await;
    assert_eq!(res.status, 200);
    let body = res.json();
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "id",
            "category",
            "rated",
            "timeControl",
            "white",
            "black",
            "status",
            "reason",
            "result",
            "termination",
            "plies",
            "startedAt",
            "endedAt",
            "baseMs",
            "incMs",
            "statusName",
            "rematchOf",
            "moves",
            "pgn"
        ]
    );
    assert_eq!(body["id"], g.id);
    let uci: Vec<&str> =
        body["moves"].as_array().unwrap().iter().map(|m| m["uci"].as_str().unwrap()).collect();
    assert_eq!(uci, ["e2e4", "e7e5", "g1f3", "b8c6"]);
    assert_eq!(body["moves"][2], json!({ "uci": "g1f3", "spentMs": 1500, "clockMs": 180_500 }));
    assert_eq!(
        body["white"],
        json!({ "name": "Erin", "rating": 1500, "ratingAfter": 1510, "ratingDiff": 10 })
    );
    assert_eq!(
        body["black"],
        json!({ "name": "Finn", "rating": 1500, "ratingAfter": 1490, "ratingDiff": -10 })
    );
    assert_eq!(body["result"], "1-0");
    assert_eq!(body["statusName"], "WhiteWins");
    assert_eq!(body["termination"], "Resignation");
    assert_eq!(body["plies"], 4);
    assert_eq!(body["rematchOf"], Value::Null);
    assert_eq!((body["baseMs"].clone(), body["incMs"].clone()), (json!(180_000), json!(2000)));
    assert_eq!(
        (body["startedAt"].clone(), body["endedAt"].clone()),
        (json!(STARTED), json!(STARTED + 600_000))
    );
    assert_eq!(
        body["pgn"],
        json!({
            "Event": "Test Server rated 3+2", "Site": "chess.example.org", "Date": "2026.09.28", "Round": "-",
            "White": "Erin", "Black": "Finn", "Result": "1-0", "WhiteElo": 1500, "BlackElo": 1500,
            "TimeControl": "180+2", "Termination": "Resignation", "PlyCount": 4,
        })
    );
    let pgn_keys: Vec<&str> = body["pgn"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        pgn_keys,
        [
            "Event",
            "Site",
            "Date",
            "Round",
            "White",
            "Black",
            "Result",
            "WhiteElo",
            "BlackElo",
            "TimeControl",
            "Termination",
            "PlyCount"
        ]
    );
    assert!(!res.text().contains('@'), "no e-mail address");

    let mut casual = short_game(3_000_000_000_002, &erin, &finn);
    (casual.rated, casual.status, casual.reason) = (false, ABORTED, NO_SHOW);
    (casual.moves, casual.spent_ms, casual.clock_ms) = (Vec::new(), Some(Vec::new()), Some(Vec::new()));
    casual.rematch_of = Some(g.id);
    casual.white_rating = None;
    s.commit(vec![casual.clone()]).await;
    let c = s.t.get(&format!("/api/v1/games/{}", casual.id)).send().await.json();
    assert_eq!(c["result"], "*");
    assert_eq!(c["termination"], "NoShow");
    assert_eq!(c["statusName"], "Aborted");
    assert_eq!(c["moves"], json!([]));
    assert_eq!(c["white"]["ratingDiff"], Value::Null);
    assert_eq!(c["pgn"]["WhiteElo"], "-");
    assert_eq!(c["rematchOf"], g.id);

    let r = s.t.get("/api/v1/games/12345").send().await;
    assert_eq!((r.status, r.json()), (404, json!({ "error": "not_found", "message": "No such game." })));
    for bad in ["abc", "0", "-1", "1.5", "99999999999999999", "12e3"] {
        let r = s.t.get(&format!("/api/v1/games/{bad}")).send().await;
        assert_eq!(
            (r.status, r.json()),
            (400, json!({ "error": "invalid_game_id", "message": "Invalid game id." })),
            "{bad}"
        );
    }
}

// docs/API.md sections 10 and 11: a rated game always carries its rating changes, also when the
// rules leave both ratings where they were; null and no tags only for a game that does not count
// for the ratings (casual, custom, aborted).
#[tokio::test]
async fn rating_changes_a_rated_game_that_changes_no_rating_says_0_and_plus_0() {
    let config = config(&[("SERVER_NAME", "Test Server"), ("SERVER_PUBLIC_HOST", "chess.example.org")]);
    let store = elo_store(&config).await;
    let st = store.clone();
    let s = Server::start(config, store, NOW, move |router, config| {
        routes(router, config, &st, "rating-changes-test")
    });
    let zara = s.user("Zara").await;
    let yann = s.user("Yann").await;
    let zero = short_game(3_100_000_000_001, &zara, &yann);
    let mut casual = short_game(3_100_000_000_002, &zara, &yann);
    casual.rated = false;
    let mut custom = short_game(3_100_000_000_003, &zara, &yann);
    (custom.category, custom.base_ms, custom.inc_ms) = ("custom".into(), 240_000, 1000);
    let mut aborted = short_game(3_100_000_000_004, &zara, &yann);
    (aborted.status, aborted.reason, aborted.moves) = (ABORTED, NO_SHOW, Vec::new());
    (aborted.spent_ms, aborted.clock_ms) = (Some(Vec::new()), Some(Vec::new()));
    s.commit(vec![zero.clone(), casual.clone(), custom.clone(), aborted.clone()]).await;
    let z = s.t.get(&format!("/api/v1/games/{}", zero.id)).send().await.json();
    assert_eq!(z["rated"], true);
    assert_eq!(z["white"], json!({ "name": "Zara", "rating": 1500, "ratingAfter": 1500, "ratingDiff": 0 }));
    assert_eq!(z["black"], json!({ "name": "Yann", "rating": 1500, "ratingAfter": 1500, "ratingDiff": 0 }));
    let pgn = s.t.get(&format!("/api/v1/games/{}/pgn", zero.id)).send().await;
    assert!(pgn.text().lines().any(|l| l == "[WhiteRatingDiff \"+0\"]"), "{}", pgn.text());
    assert!(pgn.text().lines().any(|l| l == "[BlackRatingDiff \"+0\"]"));
    for g in [&casual, &custom, &aborted] {
        let body = s.t.get(&format!("/api/v1/games/{}", g.id)).send().await.json();
        for side in ["white", "black"] {
            assert_eq!(
                (body[side]["ratingAfter"].clone(), body[side]["ratingDiff"].clone()),
                (Value::Null, Value::Null)
            );
        }
        let text = s.t.get(&format!("/api/v1/games/{}/pgn", g.id)).send().await;
        assert!(!text.text().contains("RatingDiff"), "{}", text.text());
    }
    // What docs/API.md says of it.
    let api = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/API.md"))
        .expect("docs/API.md");
    let api = api.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        api.contains("A rated game always has them, also when the rating rules leave a rating where it was")
    );
    assert!(api.contains(
        "then `ratingDiff` is `0`. They are `null` only for a game that does not count for the ratings (casual, custom, aborted)."
    ));
    assert!(api.contains(
        "`WhiteRatingDiff` and `BlackRatingDiff`: the changes (`\"+10\"`, `\"-10\"`), in every rated game, `\"+0\"` when"
    ));
}

// ---- the PGN text (Node http.games, tools/gen-pgn-fixtures.js) ------------------------------------

/// The configuration of the fixtures.
fn fixture_config() -> Config {
    config(&[("SERVER_NAME", "Scacelith Test Server"), ("SERVER_PUBLIC_HOST", "chess.example.org")])
}

/// 2026-09-28T18:30:05Z.
const T0: i64 = 1_790_620_205_000;
const MIN: i64 = 60_000;

/// A fixture of `tools/gen-pgn-fixtures.js`.
struct Spec {
    file: &'static str,
    category: &'static str,
    rated: bool,
    base_ms: i64,
    inc_ms: i64,
    white: &'static str,
    black: &'static str,
    ratings: Option<(i64, i64)>,
    changes: Option<(i64, i64)>,
    uci: &'static str,
    think: fn(usize) -> u32,
    status: u8,
    reason: u8,
}

/// Think times of the moves that run a clock: a fixed spread with millisecond noise.
fn casual_think(i: usize) -> u32 {
    800 + (i as u32 + 7).wrapping_mul(2_654_435_761) % 5200
}

fn flag_fall_think(i: usize) -> u32 {
    if i.is_multiple_of(2) {
        [7310, 6905, 8120, 7655, 6420, 7880, 7215, 7123][(i - 2) / 2]
    } else {
        900 + (i as u32 * 131) % 700
    }
}

fn flag_fall_draw_think(i: usize) -> u32 {
    if i.is_multiple_of(2) { 6250 + (i as u32 * 37) % 160 } else { 1100 + (i as u32 * 53) % 900 }
}

fn specs() -> Vec<Spec> {
    vec![
        Spec {
            file: "01-checkmate.pgn",
            category: "3+2",
            rated: true,
            base_ms: 3 * MIN,
            inc_ms: 2000,
            white: "Alice",
            black: "Bob",
            ratings: Some((1500, 1520)),
            changes: Some((11, -11)),
            uci: "e2e4 e7e5 f1c4 b8c6 d1h5 g8f6 h5f7",
            think: casual_think,
            status: WHITE_WINS,
            reason: CHECKMATE,
        },
        Spec {
            file: "02-resignation-castling.pgn",
            category: "5+3",
            rated: true,
            base_ms: 5 * MIN,
            inc_ms: 3000,
            white: "Carol",
            black: "Dave",
            ratings: Some((1610, 1580)),
            changes: Some((9, -9)),
            uci: "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1 d7d6 d2d3 c8g4 b1c3 d8d7 c1e3 e8c8 e3c5 d6c5 c3d5",
            think: casual_think,
            status: WHITE_WINS,
            reason: RESIGNATION,
        },
        Spec {
            file: "03-flag-fall.pgn",
            category: "1+0",
            rated: true,
            base_ms: MIN,
            inc_ms: 0,
            white: "Erin",
            black: "Finn",
            ratings: Some((1702, 1688)),
            changes: Some((-8, 8)),
            uci: "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5",
            think: flag_fall_think,
            status: BLACK_WINS,
            reason: TIMEOUT,
        },
        Spec {
            file: "04-flag-fall-draw.pgn",
            category: "3+0",
            rated: true,
            base_ms: 3 * MIN,
            inc_ms: 0,
            white: "Gina",
            black: "Hugo",
            ratings: Some((1500, 1500)),
            changes: Some((0, 0)),
            uci: "a2a4 a7a5 g2g3 a8a7 h2h4 g7g5 h4g5 e7e6 h1h7 g8h6 h7h8 a7a6 g5h6 a6a8 h8f8 e8e7 f8d8 b7b6 d8c8 c7c5 \
                  c8b8 f7f5 b8a8 b6b5 a8a5 c5c4 a4b5 e7d6 a5a2 d6d5 a2a5 d5e4 b1a3 c4c3 b2c3 d7d5 a3c4 e6e5 c4e5 d5d4 \
                  c3d4 e4d4 f2f4 d4c5 a5a6 c5b4 e5c6 b4b5 c6e5 b5c5 c2c3 c5d5 f1g2 d5c5 g2h3 c5b5 h3f5 b5c5",
            think: flag_fall_draw_think,
            status: DRAW,
            reason: TIMEOUT_VS_INSUFFICIENT,
        },
        Spec {
            file: "05-abandonment-en-passant.pgn",
            category: "10+5",
            rated: true,
            base_ms: 10 * MIN,
            inc_ms: 5000,
            white: "Ivan",
            black: "Jade",
            ratings: Some((1455, 1490)),
            changes: Some((12, -12)),
            uci: "e2e4 a7a6 e4e5 d7d5 e5d6 c7d6 d2d4 g8f6 g1f3",
            think: casual_think,
            status: WHITE_WINS,
            reason: ABANDONMENT,
        },
        Spec {
            file: "06-aborted.pgn",
            category: "custom",
            rated: false,
            base_ms: 90_000,
            inc_ms: 5000,
            white: "Kim",
            black: "Lou",
            ratings: None,
            changes: None,
            uci: "e2e4",
            think: casual_think,
            status: ABORTED,
            reason: NO_SHOW,
        },
        Spec {
            file: "07-promotion-deleted-player.pgn",
            category: "5+0",
            rated: true,
            base_ms: 5 * MIN,
            inc_ms: 0,
            white: "Mia",
            black: "deleted#42",
            ratings: Some((1530, 1450)),
            changes: Some((7, -7)),
            uci: "e2e4 d7d5 e4d5 c7c6 d5c6 g8f6 c6b7 b8d7 b7a8q",
            think: casual_think,
            status: WHITE_WINS,
            reason: RESIGNATION,
        },
        Spec {
            file: "08-forfeit-underpromotion.pgn",
            category: "3+2",
            rated: true,
            base_ms: 3 * MIN,
            inc_ms: 2000,
            white: "Ned",
            black: "Ola",
            ratings: Some((1800, 1795)),
            changes: Some((-9, 9)),
            uci: "a2a4 h7h5 a4a5 h5h4 a5a6 h4h3 a6b7 h3g2 b7a8n g2h1n",
            think: casual_think,
            status: BLACK_WINS,
            reason: FORFEIT,
        },
    ]
}

/// The fixture games as the store returns them, with their file names.
fn fixture_games() -> Vec<(&'static str, &'static str, Game)> {
    specs()
        .into_iter()
        .enumerate()
        .map(|(n, s)| {
            let moves = moves_of(s.uci);
            let mut clocks = [s.base_ms, s.base_ms];
            let (mut spent_ms, mut clock_ms) = (Vec::new(), Vec::new());
            for i in 0..moves.len() {
                let side = i % 2;
                let spent = if i < 2 { 0 } else { (s.think)(i) };
                if i >= 2 {
                    clocks[side] = clocks[side] - i64::from(spent) + s.inc_ms;
                }
                assert!(clocks[side] > 0, "{}: the clock of ply {i} ran out", s.file);
                spent_ms.push(spent);
                clock_ms.push(u32::try_from(clocks[side]).unwrap());
            }
            let n64 = n as i64;
            let started_at = T0 + n64 * 3_600_000;
            let ended_at = started_at + spent_ms.iter().map(|&x| i64::from(x)).sum::<i64>() + 4000;
            let rating_changes = s.changes.zip(s.ratings).map(|((dw, db), (w, b))| RatingChanges {
                white: RatingDelta { before: w, after: w + dw },
                black: RatingDelta { before: b, after: b + db },
            });
            let game = Game {
                summary: GameSummary {
                    id: 4_100_000_000_000 + n as u64 * 1013 + 7,
                    category: s.category.into(),
                    rated: s.rated,
                    base_ms: s.base_ms,
                    inc_ms: s.inc_ms,
                    white_id: 100 + 2 * n as u32,
                    black_id: 101 + 2 * n as u32,
                    white_name: s.white.into(),
                    black_name: s.black.into(),
                    white_rating: s.ratings.map(|r| r.0),
                    black_rating: s.ratings.map(|r| r.1),
                    started_at,
                    ended_at,
                    status: s.status,
                    reason: s.reason,
                    ply_count: moves.len() as i64,
                    rematch_of: None,
                    flags: 0,
                    rating_changes,
                },
                moves,
                spent_ms,
                clock_ms,
            };
            (s.file, s.uci, game)
        })
        .collect()
}

/// The files of tests/data/server-pgn (written by the former server's `tools/gen-pgn-fixtures.js`):
/// each PGN, then `index.json`.
fn render_fixtures() -> Vec<(String, String)> {
    let config = fixture_config();
    let tenths = |v: &[u32]| v.iter().map(|&ms| ms / 100 * 100).collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut index = Vec::new();
    for (file, uci, g) in fixture_games() {
        let text = game_pgn(&g, &config).unwrap_or_else(|| panic!("{file}: the record is refused"));
        let tag = |name: &str| -> String {
            let open = format!("[{name} \"");
            let line = text.lines().find(|l| l.starts_with(&open)).expect("the tag");
            line[open.len()..line.len() - 2].to_string()
        };
        let entry = json!({
            "file": file, "gameId": g.summary.id.to_string(), "white": g.summary.white_name,
            "black": g.summary.black_name, "result": tag("Result"), "termination": tag("Termination"),
            "timeControl": tag("TimeControl"), "plies": g.moves.len(), "uci": uci.split_whitespace().collect::<Vec<_>>(),
            "clockMs": tenths(&g.clock_ms), "elapsedMs": tenths(&g.spent_ms),
        });
        index.push(format!("    {}", stringify(&entry)));
        out.push((file.to_string(), text));
    }
    out.push((
        "index.json".into(),
        format!(
            "{{\n  \"generator\": \"the former Node.js server (tools/gen-pgn-fixtures.js, GET /api/v1/games/:id/pgn); reproduced byte for byte by dedicated-server/crates/chess/tests/pgn_write.rs\",\n  \"games\": [\n{}\n  ]\n}}\n",
            index.join(",\n")
        ),
    ));
    out
}

#[test]
fn the_server_pgn_tag_order_san_clocks_wrapping_endings_deleted_players() {
    const ORDER: [&str; 17] = [
        "Event",
        "Site",
        "Date",
        "Round",
        "White",
        "Black",
        "Result",
        "UTCDate",
        "UTCTime",
        "WhiteElo",
        "BlackElo",
        "WhiteRatingDiff",
        "BlackRatingDiff",
        "TimeControl",
        "Termination",
        "PlyCount",
        "ScacelithGameId",
    ];
    let config = fixture_config();
    let games = fixture_games();
    let mut terminations = std::collections::BTreeSet::new();
    let mut flags = 0;
    for (file, _, g) in &games {
        let s = &g.summary;
        let text = game_pgn(g, &config).unwrap();
        assert!(!text.contains('\r'), "LF line endings");
        assert!(text.ends_with('\n') && !text.ends_with("\n\n"));
        for line in text.split('\n') {
            assert!(line.len() < 80, "{file}: {line}");
        }
        let p = parse_pgn(&text);
        let names = p.names();
        let ordered: Vec<&str> = ORDER.iter().copied().filter(|k| names.contains(k)).collect();
        assert_eq!(names, ordered, "{file}: tag order");
        let fixed: Vec<&str> = ORDER.iter().copied().filter(|k| !k.ends_with("RatingDiff")).collect();
        assert_eq!(
            names.iter().copied().filter(|k| !k.ends_with("RatingDiff")).collect::<Vec<_>>(),
            fixed,
            "{file}"
        );
        assert_eq!(
            names.contains(&"WhiteRatingDiff"),
            s.rating_changes.is_some(),
            "{file}: diffs of a rated result"
        );
        let kind = if s.rated { "rated" } else { "casual" };
        assert_eq!(p.tag("Event"), Some(format!("Scacelith Test Server {kind} {}", s.category).as_str()));
        assert_eq!(p.tag("Site"), Some("chess.example.org"));
        assert_eq!(p.tag("White"), Some(s.white_name.as_str()));
        assert_eq!(p.tag("Black"), Some(s.black_name.as_str()));
        assert_eq!(p.tag("Date"), p.tag("UTCDate"));
        let time = p.tag("UTCTime").unwrap();
        assert!(time.len() == 8 && time.as_bytes()[2] == b':' && time.as_bytes()[5] == b':', "{time}");
        assert_eq!(p.tag("PlyCount"), Some(g.moves.len().to_string().as_str()));
        assert_eq!(p.tag("ScacelithGameId"), Some(s.id.to_string().as_str()));
        assert_eq!(p.tag("TimeControl"), Some(format!("{}+{}", s.base_ms / 1000, s.inc_ms / 1000).as_str()));
        let elo = s.white_rating.map_or("-".to_string(), |r| r.to_string());
        assert_eq!(p.tag("WhiteElo"), Some(elo.as_str()));
        assert_eq!(Some(p.result.as_str()), p.tag("Result"));
        assert_eq!(p.result, result_text(s.status));
        if let Some(c) = &s.rating_changes {
            let d = c.white.after - c.white.before;
            let text = if d < 0 { d.to_string() } else { format!("+{d}") };
            assert_eq!(p.tag("WhiteRatingDiff"), Some(text.as_str()));
        }
        assert_eq!(p.moves, g.moves, "{file}: SAN replays the stored moves");
        for i in 0..g.moves.len() {
            assert_eq!(p.clk[i], Some(i64::from(g.clock_ms[i] / 100 * 100)), "{file} ply {i} clk");
            assert_eq!(p.emt[i], Some(i64::from(g.spent_ms[i] / 100 * 100)), "{file} ply {i} emt");
        }
        terminations.insert(p.tag("Termination").unwrap().to_string());
        if p.sans.iter().any(|x| x.contains('=')) {
            flags |= 1;
        }
        if p.sans.iter().any(|x| x.starts_with("O-O")) {
            flags |= 2;
        }
        if p.sans.iter().any(|x| x.starts_with("O-O-O")) {
            flags |= 4;
        }
        if format!("{}{}", s.white_name, s.black_name).contains("deleted#") {
            flags |= 8;
        }
        if !p.comment.is_empty() {
            flags |= 16;
        }
    }
    assert_eq!(
        terminations.into_iter().collect::<Vec<_>>(),
        ["abandoned", "normal", "rules infraction", "time forfeit", "unterminated"]
    );
    assert_eq!(flags, 31, "promotion, both castlings, a deleted player, end comments");
    let by_file = |name: &str| {
        let (_, _, g) = games.iter().find(|(f, _, _)| *f == name).unwrap();
        parse_pgn(&game_pgn(g, &config).unwrap())
    };
    let draw = by_file("04-flag-fall-draw.pgn");
    assert_eq!((draw.tag("Termination"), draw.result.as_str()), (Some("time forfeit"), "1/2-1/2"));
    let aborted = by_file("06-aborted.pgn");
    assert_eq!(aborted.tag("Termination"), Some("unterminated"));
    assert_eq!(aborted.comment, "Aborted: first move not played in time");
    assert!(by_file("05-abandonment-en-passant.pgn").sans.iter().any(|x| x == "exd6"), "en passant");
    assert!(by_file("08-forfeit-underpromotion.pgn").sans.iter().any(|x| x == "gxh1=N"), "underpromotion");
}

#[test]
fn the_fixtures_of_the_game_pgn_reader_are_reproduced_byte_for_byte() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/data/server-pgn");
    if !dir.is_dir() {
        eprintln!("skipped: not in a full checkout of the repository");
        return;
    }
    let files = render_fixtures();
    assert_eq!(files.iter().filter(|(n, _)| n.ends_with(".pgn")).count(), 8);
    for (name, text) in &files {
        let on_disk = std::fs::read_to_string(dir.join(name)).unwrap().replace("\r\n", "\n");
        assert_eq!(&on_disk, text, "{name}");
    }
    let index: Value = serde_json::from_str(&files.last().unwrap().1).unwrap();
    assert_eq!(index["games"].as_array().unwrap().len(), 8);
    assert_eq!(index["games"][0]["clockMs"].as_array().unwrap()[0..2], [json!(180_000), json!(180_000)]);
}

#[test]
fn game_pgn_refuses_moves_that_do_not_replay_or_replay_to_another_ending() {
    let config = fixture_config();
    let (a, b) = (someone(1, "A"), someone(2, "B"));
    let ok = stored(&rec(1, &a, &b, "e2e4 e7e5"));
    assert!(game_pgn(&ok, &config).unwrap().ends_with("{Resignation} 1-0\n"));
    let mut e2e5 = ok.clone();
    e2e5.moves = vec![12 | (36 << 6)];
    assert_eq!(game_pgn(&e2e5, &config), None, "e2e5 is not a move");
    let mut mate_rec = rec(2, &a, &b, "f2f3 e7e5 g2g4 d8h4");
    (mate_rec.status, mate_rec.reason) = (BLACK_WINS, CHECKMATE);
    let mate = stored(&mate_rec);
    let flat = game_pgn(&mate, &config).unwrap().replace('\n', " ");
    let at = flat.find("Qh4# {[%clk ").unwrap_or_else(|| panic!("{flat}"));
    let close = at + flat[at..].find('}').unwrap();
    assert_eq!(&flat[close..], "} {Checkmate} 0-1 ", "{flat}");
    let mut resigned = mate.clone();
    resigned.summary.reason = RESIGNATION;
    assert_eq!(game_pgn(&resigned, &config), None, "mated, yet stored as a resignation");
    let mut after = mate.clone();
    after.moves.push(12 | (20 << 6));
    assert_eq!(game_pgn(&after, &config), None, "a move after the mate");
    let mut ongoing = ok.clone();
    ongoing.summary.status = 0;
    assert_eq!(game_pgn(&ongoing, &config), None, "a stored game that did not end");
    // Clock lists shorter than the moves: the missing values are left out.
    let mut partial = ok.clone();
    (partial.clock_ms, partial.spent_ms) = (vec![180_000], Vec::new());
    assert!(
        game_pgn(&partial, &config).unwrap().contains("1. e4 {[%clk 0:03:00.0]} 1... e5 {Resignation} 1-0")
    );
}

// ---- GET /api/v1/games/:id/pgn (Node http.games) --------------------------------------------------

#[tokio::test]
async fn pgn_route_the_file_with_its_headers_400_404_and_500_for_an_unreplayable_record() {
    let logs = LogCapture::start();
    let s = start("games-pgn-route-test").await;
    let alice = s.user_at("Alice", NOW - 400 * DAY).await;
    let bob = s.user_at("Bob", NOW - 400 * DAY).await;
    let g = rec(5_000_000_000_001, &alice, &bob, "e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5c6 d7c6 e1g1");
    let mut bad = rec(5_000_000_000_002, &alice, &bob, "e2e4");
    bad.moves = vec![12 | (28 << 6), 12 | (28 << 6)];
    (bad.spent_ms, bad.clock_ms) = (Some(vec![0, 0]), Some(vec![180_000, 180_000]));
    s.commit(vec![g.clone(), bad.clone()]).await;

    let path = format!("/api/v1/games/{}/pgn", g.id);
    let r = s.t.get(&path).send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some(PGN_CONTENT_TYPE));
    assert_eq!(
        r.header("content-disposition"),
        Some(format!("attachment; filename=\"scacelith-{}.pgn\"", g.id).as_str())
    );
    assert_eq!(r.header("content-security-policy"), Some("default-src 'none'; frame-ancestors 'none'"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("content-length"), Some(r.body.len().to_string().as_str()));
    let game = s.store.games().by_id(g.id).await.unwrap().unwrap();
    assert_eq!(r.text(), game_pgn(&game, &s.config).unwrap());
    let p = parse_pgn(r.text());
    assert_eq!(p.tag("Event"), Some("Test Server rated 3+2"));
    assert_eq!(p.tag("Site"), Some("chess.example.org"));
    assert_eq!(p.tag("WhiteRatingDiff"), Some("+10"));
    assert_eq!(p.tag("BlackRatingDiff"), Some("-10"));
    assert_eq!(p.tag("Termination"), Some("normal"));
    assert_eq!(p.sans[8], "O-O");
    assert_eq!(p.clk[2], Some(180_400), "truncated to tenths (180470 ms)");
    // The same answer with a session; HEAD without a body.
    assert_eq!(s.t.get(&path).bearer(&alice.token).send().await.text(), r.text());
    let head = s.t.request(Method::HEAD, &path).send().await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(head.header("content-length"), r.header("content-length"));
    // Errors.
    let r404 = s.t.get("/api/v1/games/123/pgn").send().await;
    assert_eq!((r404.status, r404.json()["error"].clone()), (404, json!("not_found")));
    assert_eq!(s.t.get("/api/v1/games/abc/pgn").send().await.json()["error"], "invalid_game_id");
    assert_eq!(s.t.get("/api/v1/games/0/pgn").send().await.status, 400);
    let invalid = s.t.get(&path).bearer(&format!("sct_{}", "z".repeat(43))).send().await;
    assert_eq!(invalid.status, 401, "a token sent must be valid");
    let r500 = s.t.get(&format!("/api/v1/games/{}/pgn", bad.id)).send().await;
    assert_eq!(
        (r500.status, r500.json()),
        (
            500,
            json!({ "error": "internal_error", "message": "The moves stored for this game cannot be replayed." })
        )
    );
    let logged = logs.records("games-pgn-route-test");
    assert!(
        logged.iter().any(|l| l["level"] == "error"
            && l["msg"] == "stored game cannot be replayed"
            && l["gameId"] == bad.id
            && l["plies"] == 2
            && l["status"] == 1
            && l["reason"] == 2),
        "{logged:?}"
    );
}

#[tokio::test]
async fn pgn_route_aborted_games_deleted_players_and_the_public_read_limit() {
    let s = start("games-pgn-aborted-test").await;
    let carl = s.user("Carl").await;
    let dina = s.user("Dina").await;
    let mut aborted = rec(5_100_000_000_001, &carl, &dina, "");
    (aborted.rated, aborted.status, aborted.reason) = (false, ABORTED, NO_SHOW);
    (aborted.category, aborted.base_ms, aborted.inc_ms) = ("custom".into(), 90_000, 5000);
    (aborted.white_rating, aborted.black_rating) = (None, None);
    let mut won = rec(5_100_000_000_002, &dina, &carl, "d2d4 d7d5");
    (won.status, won.reason) = (BLACK_WINS, TIMEOUT);
    s.commit(vec![aborted.clone(), won.clone()]).await;
    let p = parse_pgn(s.t.get(&format!("/api/v1/games/{}/pgn", aborted.id)).send().await.text());
    assert_eq!((p.tag("Result"), p.result.as_str()), (Some("*"), "*"));
    assert_eq!(p.tag("Termination"), Some("unterminated"));
    assert_eq!(p.tag("Event"), Some("Test Server casual custom"));
    assert_eq!(p.tag("TimeControl"), Some("90+5"));
    assert_eq!(p.tag("WhiteElo"), Some("-"));
    assert_eq!(p.tag("WhiteRatingDiff"), None);
    assert!(p.sans.is_empty());
    assert_eq!(p.comment, "Aborted: first move not played in time");
    s.store.users().anonymize(carl.id, NOW).await.unwrap();
    let p = parse_pgn(s.t.get(&format!("/api/v1/games/{}/pgn", won.id)).send().await.text());
    assert_eq!(p.tag("Black"), Some(format!("deleted#{}", carl.id).as_str()));
    assert_eq!(p.tag("Termination"), Some("time forfeit"));
    assert_eq!(p.result, "0-1");
    assert_eq!(p.comment, "Loss on time");
    // public_read: 60 a minute per client, the record and the PGN together (2 requests above).
    let mut last = None;
    for i in 0..58 {
        let suffix = if i % 2 == 1 { "/pgn" } else { "" };
        last = Some(s.t.get(&format!("/api/v1/games/{}{suffix}", won.id)).send().await.status);
    }
    assert_eq!(last, Some(200));
    let r = s.t.get(&format!("/api/v1/games/{}/pgn", won.id)).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
}

// ---- GET /api/v1/games/:id with a session (Node http.games) ---------------------------------------

#[tokio::test]
async fn game_you_and_reportable_for_the_players_only_the_public_answer_unchanged() {
    let s = start("games-reportable-test").await;
    let alice = s.user_at("Alice", NOW - 400 * DAY).await;
    let bob = s.user_at("Bob", NOW - 400 * DAY).await;
    let eve = s.user_at("Eve", NOW - 400 * DAY).await;
    let g = rec(5_200_000_000_001, &alice, &bob, "e2e4 e7e5");
    let mut old = rec(5_200_000_000_002, &bob, &alice, "d2d4");
    (old.ended_at, old.started_at) = (Some(NOW - 8 * DAY), Some(NOW - 8 * DAY - HOUR));
    s.commit(vec![g.clone(), old.clone()]).await;
    let path = format!("/api/v1/games/{}", g.id);
    let get = |token: &str| {
        let req = s.t.get(&path);
        let req = if token.is_empty() { req } else { req.bearer(token) };
        async move { req.send().await.json() }
    };
    let public = get("").await;
    for absent in ["you", "reportable", "color"] {
        assert!(public.get(absent).is_none(), "{absent}");
    }
    assert_eq!(get(&eve.token).await, public, "a session of someone else: the public answer");
    let as_alice = get(&alice.token).await;
    assert_eq!((as_alice["you"].clone(), as_alice["reportable"].clone()), (json!("white"), json!(true)));
    let mut rest = as_alice.as_object().unwrap().clone();
    let tail: Vec<&str> = rest.keys().rev().take(2).map(String::as_str).collect();
    assert_eq!(tail, ["reportable", "you"], "added at the end");
    rest.remove("you");
    rest.remove("reportable");
    assert_eq!(Value::Object(rest), public, "otherwise the same answer");
    let as_bob = get(&bob.token).await;
    assert_eq!((as_bob["you"].clone(), as_bob["reportable"].clone()), (json!("black"), json!(true)));
    let old_path = format!("/api/v1/games/{}", old.id);
    let r = s.t.get(&old_path).bearer(&alice.token).send().await.json();
    assert_eq!(
        (r["you"].clone(), r["reportable"].clone()),
        (json!("black"), json!(false)),
        "ended more than 7 days ago"
    );
    // After a report of Bob for this game, Alice cannot report again; Bob still can.
    let filed =
        s.t.post("/api/v1/reports")
            .bearer(&alice.token)
            .json(&json!({ "gameId": g.id.to_string(), "reported": "Bob", "category": "cheating" }))
            .send()
            .await;
    assert_eq!((filed.status, filed.json()), (202, json!({ "status": "received" })));
    assert_eq!(get(&alice.token).await["reportable"], false);
    assert_eq!(get(&bob.token).await["reportable"], true);
    // The game becomes too old for a report.
    s.set_now(NOW + 7 * DAY);
    assert_eq!(get(&bob.token).await["reportable"], false);
    let invalid = s.t.get(&path).bearer(&format!("sct_{}", "q".repeat(43))).send().await;
    assert_eq!(invalid.status, 401, "an invalid token is refused");
    let filed = s.store.reports().for_reporter(alice.id, 10).await.unwrap();
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0].reported_name, "Bob");
}

#[tokio::test]
async fn reportable_false_when_the_daily_quota_is_spent() {
    let s = start("games-quota-test").await;
    let alice = s.user("Alice").await;
    let [bob, carol, dave] = [s.user("Bob").await, s.user("Carol").await, s.user("Dave").await];
    let games: Vec<GameRecord> = [&bob, &carol, &dave]
        .iter()
        .enumerate()
        .map(|(i, o)| rec(5_300_000_000_001 + i as u64, &alice, o, "e2e4"))
        .collect();
    s.commit(games.clone()).await;
    for (g, name) in games.iter().zip(["Bob", "Carol"]) {
        let body = json!({ "gameId": g.id, "reported": name, "category": "abuse" });
        assert_eq!(s.t.post("/api/v1/reports").bearer(&alice.token).json(&body).send().await.status, 202);
    }
    let r = s.t.get(&format!("/api/v1/games/{}", games[2].id)).bearer(&alice.token).send().await.json();
    assert_eq!(r["reportable"], false, "REPORTS_PER_DAY = 2 reached");
    let r = s.t.get(&format!("/api/v1/games/{}", games[2].id)).bearer(&dave.token).send().await.json();
    assert_eq!(r["reportable"], true);
}
