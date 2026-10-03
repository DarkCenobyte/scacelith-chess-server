//! PGN writer: the files of tests/data/server-pgn (written by the former server's
//! `GET /api/v1/games/:id/pgn`, `tools/gen-pgn-fixtures.js`) are reproduced byte for byte from
//! their moves, clocks, ending and tags.

mod common;

use common::{read_json, repo_path};
use scacelith_chess::{ChessGame, EndReason, GameStatus, PGN_LIMITS, PgnComment, PgnGame, PgnTags, read_pgn};
use serde_json::Value;

/// The ending each fixture was generated with (`tools/gen-pgn-fixtures.js`).
const ENDINGS: [(&str, GameStatus, EndReason); 8] = [
    ("01-checkmate.pgn", GameStatus::WhiteWins, EndReason::Checkmate),
    ("02-resignation-castling.pgn", GameStatus::WhiteWins, EndReason::Resignation),
    ("03-flag-fall.pgn", GameStatus::BlackWins, EndReason::Timeout),
    ("04-flag-fall-draw.pgn", GameStatus::Draw, EndReason::TimeoutVsInsufficient),
    ("05-abandonment-en-passant.pgn", GameStatus::WhiteWins, EndReason::Abandonment),
    ("06-aborted.pgn", GameStatus::Aborted, EndReason::NoShow),
    ("07-promotion-deleted-player.pgn", GameStatus::WhiteWins, EndReason::Resignation),
    ("08-forfeit-underpromotion.pgn", GameStatus::BlackWins, EndReason::Forfeit),
];

/// A `[%clk]` / `[%emt]` value: h:mm:ss.t, tenths truncated (the server's `pgnClock`).
fn pgn_clock(ms: u64) -> String {
    let tenths = ms / 100;
    let s = tenths / 10;
    format!("{}:{:02}:{:02}.{}", s / 3600, s / 60 % 60, s % 60, tenths % 10)
}

/// The tags of a parsed file in the writer's terms: the six fixed ones, the tags between
/// `Result` and `TimeControl`, and those after `Termination`.
fn tags_of(parsed: &PgnGame) -> PgnTags {
    let tag = |name: &str| parsed.tag(name).map(str::to_owned);
    let position = |name: &str| parsed.tags.iter().position(|(n, _)| n == name).unwrap();
    let (result, time_control, termination) =
        (position("Result"), position("TimeControl"), position("Termination"));
    PgnTags {
        event: tag("Event"),
        site: tag("Site"),
        date: tag("Date"),
        round: tag("Round"),
        white: tag("White"),
        black: tag("Black"),
        time_control: tag("TimeControl"),
        after_result: parsed.tags[result + 1..time_control].to_vec(),
        extra: parsed.tags[termination + 1..].to_vec(),
        comments: Vec::new(),
    }
}

fn numbers(v: &Value) -> Vec<u64> {
    v.as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).collect()
}

#[test]
fn the_server_pgn_files_are_reproduced_byte_for_byte() {
    let dir = repo_path("tests/data/server-pgn");
    let index = read_json(dir.join("index.json"));
    let games = index["games"].as_array().unwrap();
    assert_eq!(games.len(), ENDINGS.len());
    for (g, (file, status, reason)) in games.iter().zip(ENDINGS) {
        assert_eq!(g["file"].as_str(), Some(file));
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        let parsed = read_pgn(&text, &PGN_LIMITS).unwrap();

        // As the PGN route does: replay the stored moves, then apply the stored ending unless the
        // moves ended the game themselves (then both must agree).
        let mut game = ChessGame::from_moves(None, &parsed.moves).unwrap();
        if game.is_over() {
            assert_eq!((game.status(), game.reason()), (status, reason), "{file}");
        } else {
            assert_eq!(game.end(status, reason), Ok(true), "{file}");
        }
        let uci: Vec<&str> = g["uci"].as_array().unwrap().iter().map(|u| u.as_str().unwrap()).collect();
        assert_eq!(game.uci_moves(), uci, "{file}");

        let (clock, spent) = (numbers(&g["clockMs"]), numbers(&g["elapsedMs"]));
        let mut tags = tags_of(&parsed);
        tags.comments = (0..game.ply())
            .map(|i| {
                let mut words = Vec::new();
                if let Some(&ms) = clock.get(i) {
                    words.push(format!("[%clk {}]", pgn_clock(ms)));
                }
                if let Some(&ms) = spent.get(i) {
                    words.push(format!("[%emt {}]", pgn_clock(ms)));
                }
                Some(PgnComment::Words(words))
            })
            .collect();
        assert_eq!(game.pgn(&tags), text, "{file}");
        assert_eq!(game.result_string(), parsed.result);
    }
}

#[test]
fn clock_comments_are_truncated_to_tenths() {
    assert_eq!(pgn_clock(0), "0:00:00.0");
    assert_eq!(pgn_clock(178_699), "0:02:58.6");
    assert_eq!(pgn_clock(3_600_000 + 61_000), "1:01:01.0");
    let mut game = ChessGame::default();
    game.play(game.position().parse_uci("e2e4").unwrap()).unwrap();
    // Empty comments are not written.
    let tags = PgnTags {
        date: Some("2026.10.01".into()),
        comments: vec![Some(PgnComment::Words(vec![" {}".into(), String::new()]))],
        ..PgnTags::default()
    };
    let text = game.pgn(&tags);
    assert!(text.ends_with("\n\n1. e4 *\n"), "{text}");
}
