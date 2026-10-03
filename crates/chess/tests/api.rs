//! The crate's contract with the protocol and the game room: enum values, u16 moves, the digest,
//! copies, speed.

mod common;

use std::time::Instant;

use common::{Lcg, fen_prefix, fnv, mv, pos};
use scacelith_chess::{
    ChessGame, Color, EndReason, GameStatus, MoveFlags, PieceType, PlayResult, Position, START_FEN,
    encode_move, fnv1a32, move_from,
};

#[test]
fn enums_are_the_protocol_values() {
    // docs/PROTOCOL.md: Color, GameStatus, EndReason, MoveFlag.
    assert_eq!([Color::White as u8, Color::Black as u8], [0, 1]);
    assert_eq!(
        [
            GameStatus::Ongoing,
            GameStatus::WhiteWins,
            GameStatus::BlackWins,
            GameStatus::Draw,
            GameStatus::Aborted
        ]
        .map(GameStatus::as_u8),
        [0, 1, 2, 3, 4]
    );
    let reasons = [
        (EndReason::None, 0),
        (EndReason::Checkmate, 1),
        (EndReason::Resignation, 2),
        (EndReason::Timeout, 3),
        (EndReason::IllegalMoves, 4),
        (EndReason::Stalemate, 5),
        (EndReason::InsufficientMaterial, 6),
        (EndReason::TimeoutVsInsufficient, 7),
        (EndReason::FivefoldRepetition, 8),
        (EndReason::SeventyFiveMoves, 9),
        (EndReason::ThreefoldClaim, 10),
        (EndReason::FiftyMoveClaim, 11),
        (EndReason::Agreement, 12),
        (EndReason::IllegalMovesVsInsufficient, 13),
        (EndReason::Abandonment, 20),
        (EndReason::AbandonmentVsInsufficient, 21),
        (EndReason::Aborted, 22),
        (EndReason::NoShow, 23),
        (EndReason::Forfeit, 24),
        (EndReason::ServerAborted, 25),
        (EndReason::BothDisconnected, 26),
    ];
    for (r, v) in reasons {
        assert_eq!(r.as_u8(), v);
        assert_eq!(EndReason::try_from(v), Ok(r));
    }
    let flags = [
        (MoveFlags::CAPTURE, 0x01),
        (MoveFlags::EN_PASSANT, 0x02),
        (MoveFlags::CASTLE_KING, 0x04),
        (MoveFlags::CASTLE_QUEEN, 0x08),
        (MoveFlags::DOUBLE_PUSH, 0x10),
        (MoveFlags::PROMOTION, 0x20),
        (MoveFlags::CHECK, 0x40),
        (MoveFlags::MATE, 0x80),
    ];
    for (f, v) in flags {
        assert_eq!(f.bits(), v);
    }
    assert_eq!(PieceType::Knight as u8, 2);
    assert_eq!(PieceType::Queen as u8, 5);
    // Move encoding examples of docs/PROTOCOL.md.
    assert_eq!(encode_move(12, 28, 0), 0x070C);
    assert_eq!(encode_move(52, 60, 5), 0x5F34);
    assert_eq!(encode_move(4, 6, 0), 0x0184);
    assert_eq!(fnv1a32(b"rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -"), 0x3706_291C);
}

#[test]
fn digest_is_the_protocol_pos_hash() {
    let mut g = ChessGame::default();
    for i in 0..60 {
        if g.is_over() {
            break;
        }
        let p = g.position();
        assert_eq!(p.digest(), fnv(&fen_prefix(&p.fen())));
        assert_eq!(p.digest(), fnv1a32(fen_prefix(&p.fen()).as_bytes()));
        let mut legal = p.legal_moves();
        legal.sort_unstable();
        g.play(legal[(i * 31) % legal.len()]).unwrap();
    }
    assert!(Position::start().is_legal(encode_move(12, 28, 0)));
    assert!(pos("8/4P3/8/8/8/8/k7/4K3 w - - 0 1").is_legal(encode_move(52, 60, PieceType::Knight as u8)));
}

#[test]
fn moves_are_u16_with_castling_as_the_king_move() {
    let p = pos("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
    let legal = p.legal_moves();
    assert!(legal.contains(&(4 | 6 << 6))); // e1g1
    assert!(legal.contains(&(4 | 2 << 6))); // e1c1
    assert!(!legal.contains(&(4 | 7 << 6))); // e1h1 is not castling
    let promo = pos("8/4P3/8/8/8/8/k7/4K3 w - - 0 1");
    let mut promos: Vec<u16> = promo.legal_moves().into_iter().filter(|&m| move_from(m) == 52).collect();
    promos.sort_unstable();
    assert_eq!(promos, [2, 3, 4, 5].map(|t| 52 | 60 << 6 | t << 12));
    for m in legal.into_iter().chain(promos) {
        assert!(m <= 0x7fff);
    }
}

#[test]
fn play_applies_only_legal_moves_and_reports_flags() {
    let mut p = Position::start();
    assert!(p.play(0).is_err());
    assert!(p.play(12 | 28 << 6 | 5 << 12).is_err());
    assert_eq!(p.fen(), START_FEN);
    assert_eq!(p.play(12 | 28 << 6), Ok(MoveFlags::DOUBLE_PUSH));
    let mut g = ChessGame::default();
    assert_eq!(
        g.play(12 | 28 << 6),
        Ok(PlayResult {
            flags: MoveFlags::DOUBLE_PUSH,
            status: GameStatus::Ongoing,
            reason: EndReason::None
        })
    );
}

#[test]
fn copies_are_independent() {
    let p = Position::start();
    let mut q = p;
    q.play(mv("e2", "e4", 0)).unwrap();
    assert_eq!(p.fen(), START_FEN);
    assert_ne!(p.repetition_key(), q.repetition_key());
    assert_ne!(p.digest(), q.digest());
    let mut g = ChessGame::default();
    let snapshot = g.clone();
    g.play(mv("e2", "e4", 0)).unwrap();
    assert_eq!(snapshot.ply(), 0);
    assert_eq!(snapshot.position().fen(), START_FEN);
}

#[test]
fn types_are_thread_safe_values() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<Position>();
    assert_send_sync::<ChessGame>();
    assert_send_sync::<scacelith_chess::PgnGame>();
    assert_send_sync::<scacelith_chess::PgnError>();
    assert_eq!(std::mem::size_of::<Position>(), 168);
}

#[test]
fn speed_is_legal_and_play_well_under_20_us_per_move() {
    // Deterministic pseudo-random games, replayed as the server does: is_legal, then play.
    let mut rng = Lcg(12345);
    let mut games = Vec::new();
    for _ in 0..40 {
        let mut g = ChessGame::default();
        while !g.is_over() && g.ply() < 200 {
            let legal = g.position().legal_moves();
            g.play(legal[rng.below(legal.len())]).unwrap();
        }
        games.push(g.moves().to_vec());
    }
    let run = || {
        let mut moves = 0u32;
        for list in &games {
            let mut g = ChessGame::default();
            for &m in list {
                assert!(g.position().is_legal(m), "replay");
                g.play(m).unwrap();
                moves += 1;
            }
        }
        moves
    };
    run(); // warm-up
    let t0 = Instant::now();
    let moves = run() + run();
    let us = t0.elapsed().as_secs_f64() * 1e6 / f64::from(moves);
    println!("ChessGame is_legal + play: {us:.3} us per move over {moves} moves");
    assert!(us < 20.0, "{us} us per move");
}
