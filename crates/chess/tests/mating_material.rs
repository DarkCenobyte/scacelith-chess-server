//! FIDE 5.1.2 and 6.9: who can still checkmate, and what a resignation or a flag fall gives,
//! against the hand-verified positions of test/fixtures/mating-material.json (shared with the
//! game, whose tests/chess_tests.cpp reads its copy tests/data/mating-material.json). The mating
//! lines of the file prove its "can mate" claims independently of the rule.

mod common;

use common::{read_json, repo_path};
use scacelith_chess::{ChessGame, Color, EndReason, GameStatus, Position};
use serde_json::Value;

const COLORS: [Color; 2] = [Color::White, Color::Black];

fn pair(v: &Value) -> (u8, u8) {
    let n = |i: usize| u8::try_from(v[i].as_u64().unwrap_or_else(|| panic!("number expected: {v}"))).unwrap();
    (n(0), n(1))
}

fn result(g: &ChessGame) -> (u8, u8) {
    (g.status().as_u8(), g.reason().as_u8())
}

#[test]
fn mating_material_vectors() {
    let doc = read_json(repo_path("test/fixtures/mating-material.json"));
    let positions = doc["positions"].as_array().expect("positions");
    assert!(positions.len() >= 25);
    let mut mates = 0;
    for e in positions {
        let fen = e["fen"].as_str().expect("fen");
        let what = format!("{} ({fen})", e["material"].as_str().unwrap_or("?"));
        let p = Position::from_fen(fen).unwrap_or_else(|| panic!("{what}: FEN refused"));
        let can = [0, 1].map(|i| e["canMate"][i].as_bool().expect("canMate"));
        assert_eq!(COLORS.map(|c| p.can_color_mate(c)), can, "{what}: canMate");
        let dead = e["dead"].as_bool().expect("dead");
        assert_eq!(p.has_insufficient_material(), dead, "{what}: dead");
        assert_eq!(dead, !can[0] && !can[1], "{what}: dead exactly when neither side can mate");
        let start = ChessGame::new(Some(fen)).unwrap();
        if dead {
            assert_eq!(
                (start.status(), start.reason()),
                (GameStatus::Draw, EndReason::InsufficientMaterial),
                "{what}"
            );
        } else {
            assert!(!start.is_over(), "{what}: over at once");
        }
        for (i, color) in COLORS.into_iter().enumerate() {
            let mut g = start.clone();
            g.resign(color);
            assert_eq!(result(&g), pair(&e["resign"][i]), "{what}: {color} resigns");
            let mut g = start.clone();
            g.flag_fall(color);
            assert_eq!(result(&g), pair(&e["flag"][i]), "{what}: {color} loses on time");
        }
        if let Some(mate) = e.get("mate") {
            let by = usize::try_from(mate["by"].as_u64().expect("by")).unwrap();
            assert!(can[by], "{what}: a mate by a side that cannot mate");
            let mut g = start.clone();
            for m in mate["moves"].as_array().expect("moves") {
                let uci = m.as_str().expect("uci");
                let mv = g.position().parse_uci(uci).unwrap_or_else(|| panic!("{what}: {uci} is not legal"));
                g.play(mv).unwrap_or_else(|e| panic!("{what}: {uci}: {e}"));
            }
            let winner = if by == 0 { GameStatus::WhiteWins } else { GameStatus::BlackWins };
            assert_eq!(
                (g.status(), g.reason()),
                (winner, EndReason::Checkmate),
                "{what}: the line does not mate"
            );
            mates += 1;
        }
    }
    assert!(mates >= 10);
}
