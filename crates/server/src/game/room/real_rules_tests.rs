//! The room with the real chess rules instead of the scripted ones (ported from the reference
//! `game.realrules` suite): the room and the rules agree on digests, move flags and automatic
//! endings.

use scacelith_chess::ChessGame;
use scacelith_protocol::{
    EndReason as ER, ErrorCode as EC, GameStatus as GS, Move, ServerMsg, move_flag, uci_to_move,
};

use super::tests::{decode, player};
use super::*;

const T0: i64 = 1_800_000_000_000;

fn mk_room(base_ms: u32, inc_ms: u32) -> GameRoom {
    let spec = RoomSpec {
        id: 987654321,
        category: "3+2".to_owned(),
        base_ms,
        inc_ms,
        rated: true,
        white: player(1, "alice", 1500, false),
        black: player(2, "bob", 1500, false),
        created_at: T0,
        rematch_of: 0,
        auto_press: true,
    };
    GameRoom::new(spec, RoomSettings::default(), Box::new(ChessGame::default())).expect("valid room")
}

fn uci(s: &str) -> u16 {
    uci_to_move(s).expect("a UCI move")
}

/// Plays the moves alternately, one second apart; returns the last outcome, the MoveMade frames
/// and the time.
fn play(room: &mut GameRoom, moves: &[&str]) -> (Outcome, Vec<scacelith_protocol::MoveMade>, i64) {
    let mut t = T0;
    let mut last = Outcome::default();
    let mut made = Vec::new();
    for m in moves {
        t += 1000;
        let msg = Move {
            seq: room.ply() as u32 + 1,
            game: room.id(),
            ply: room.ply() as u16,
            r#move: uci(m),
            pos_hash: room.digest(),
            think_ms: 900,
            draw_offer: false,
        };
        let side = room.side_to_move();
        last = room.on_move(side, &msg, t);
        assert_eq!(last.rejected, None, "move {m} refused");
        made.extend(last.broadcast.iter().filter_map(|b| match decode(b) {
            ServerMsg::MoveMade(m) => Some(m),
            _ => None,
        }));
    }
    (last, made, t)
}

fn end_of(frames: &[bytes::Bytes]) -> Option<scacelith_protocol::GameEnd> {
    frames.iter().find_map(|b| match decode(b) {
        ServerMsg::GameEnd(e) => Some(e),
        _ => None,
    })
}

#[test]
fn fools_mate_ends_the_game_by_checkmate() {
    let mut room = mk_room(180000, 2000);
    let (last, _, _) = play(&mut room, &["f2f3", "e7e5", "g2g4", "d8h4"]);
    assert!(last.ended);
    let end = end_of(&last.broadcast).expect("GameEnd");
    assert_eq!((end.status, end.reason), (GS::BlackWins, ER::Checkmate));
}

#[test]
fn castling_en_passant_and_promotion_flags_reach_move_made() {
    let mut room = mk_room(180000, 2000);
    let (_, made, _) = play(
        &mut room,
        &[
            "e2e4", "a7a6", "e4e5", "d7d5", "e5d6", // en passant
            "g8f6", "g1f3", "b8c6", "f1e2", "a6a5", "e1g1", // castling
            "a5a4", "d6c7", "a4a3", "c7d8q", // promotion with capture
        ],
    );
    assert_ne!(made[4].flags & move_flag::EN_PASSANT, 0, "en passant");
    assert_ne!(made[10].flags & move_flag::CASTLE_KING, 0, "castle");
    assert_ne!(made[14].flags & move_flag::PROMOTION, 0, "promotion");
    assert_eq!(room.ply(), 15);
}

#[test]
fn an_illegal_move_in_a_synchronised_position_is_a_certain_anomaly() {
    let mut room = mk_room(180000, 2000);
    play(&mut room, &["e2e4", "e7e5"]);
    let m = Move {
        seq: 9,
        game: room.id(),
        ply: 2,
        r#move: uci("e1e3"),
        pos_hash: room.digest(),
        think_ms: 0,
        draw_offer: false,
    };
    let o = room.on_move(Side::White, &m, T0 + 5000);
    assert_eq!(o.rejected, Some(EC::IllegalMove));
    let a = o.anomaly.expect("an anomaly");
    assert_eq!(a.kind, "illegal_move");
    assert!(a.pos_matched);
}

#[test]
fn the_player_whose_flag_falls_loses_when_the_opponent_can_mate() {
    let mut room = mk_room(10000, 0);
    let (_, _, t) = play(&mut room, &["e2e4", "e7e5"]);
    // White's clock runs from Black's first move; nobody moves: White flags. Black can mate.
    let o = room.on_resync(Side::White, t + 20000);
    let frames: Vec<bytes::Bytes> = o.broadcast.iter().chain(&o.reply).cloned().collect();
    let end = end_of(&frames).expect("game ended on time");
    assert_eq!((end.status, end.reason), (GS::BlackWins, ER::Timeout));
}

#[test]
fn the_digest_is_the_protocol_position_hash() {
    let room = mk_room(180000, 2000);
    assert_eq!(room.digest(), 0x3706_291C, "the start position of PROTOCOL.md");
}
