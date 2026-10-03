//! Room tests (ported from the reference `game.room` suite) and the helpers the journal and
//! recovery tests share.

use bytes::Bytes;
use scacelith_protocol::{
    Color, EndReason as ER, ErrorCode as EC, GameEventKind as EV, GameStatus as GS, Move, MsgType,
    PlayerInfo, ServerMsg, move_flag,
};

use super::*;
use crate::config::test_config;
use crate::game::testing::{FakeRules, Script, fake_move};

pub(super) const W: Side = Side::White;
pub(super) const B: Side = Side::Black;
pub(super) const T0: i64 = 1_800_000_000_000;
/// 3+2: clamp(180000 / 10, 15000, 60000).
pub(super) const GRACE: i64 = 18000;
/// min(quota 2000, default rtt 100 + 50, LAG_COMP_MAX_MS 1000).
pub(super) const CAP0: i64 = 150;
pub(super) const ID: GameId = 123456789;

pub(super) fn player(user_id: UserId, name: &str, rating: u16, provisional: bool) -> PlayerInfo {
    PlayerInfo { user_id, name: name.to_owned(), rating, provisional }
}

/// Room settings with configuration overrides.
pub(super) fn settings(overrides: &[(&str, &str)]) -> RoomSettings {
    RoomSettings::from_config(&test_config(overrides).expect("valid test configuration"))
}

/// The test game: 3+2 rated, alice (11, 1500) against bob (22, 1612 provisional), created at T0.
pub(super) struct Mk {
    pub script: Script,
    pub settings: RoomSettings,
    pub base_ms: u32,
    pub inc_ms: u32,
    pub rated: bool,
    pub auto_press: bool,
}

impl Default for Mk {
    fn default() -> Self {
        Mk {
            script: Script::default(),
            settings: RoomSettings::default(),
            base_ms: 180000,
            inc_ms: 2000,
            rated: true,
            auto_press: true,
        }
    }
}

impl Mk {
    pub fn spec(&self) -> RoomSpec {
        RoomSpec {
            id: ID,
            category: "3+2".to_owned(),
            base_ms: self.base_ms,
            inc_ms: self.inc_ms,
            rated: self.rated,
            white: player(11, "alice", 1500, false),
            black: player(22, "bob", 1612, true),
            created_at: T0,
            rematch_of: 0,
            auto_press: self.auto_press,
        }
    }

    pub fn room(self) -> GameRoom {
        GameRoom::new(self.spec(), self.settings, FakeRules::boxed(self.script.clone()))
            .expect("valid test room")
    }

    /// The room after the two first moves (no clock): White at T0 + 1000, Black at T0 + 2000.
    pub fn opened(self) -> GameRoom {
        let mut room = self.room();
        mv(&mut room, W, T0 + 1000);
        mv(&mut room, B, T0 + 2000);
        room
    }
}

pub(super) fn mk() -> GameRoom {
    Mk::default().room()
}

pub(super) fn opened() -> GameRoom {
    Mk::default().opened()
}

/// The move the side to move would play next, in the synchronised position.
pub(super) fn intent(room: &GameRoom) -> Move {
    Move {
        seq: 7,
        game: room.id(),
        ply: room.ply() as u16,
        r#move: fake_move(room.ply(), 0),
        pos_hash: room.digest(),
        think_ms: 0,
        draw_offer: false,
    }
}

/// Plays the next move for `side`.
pub(super) fn mv(room: &mut GameRoom, side: Side, t: impl Into<Timing>) -> Outcome {
    let m = intent(room);
    room.on_move(side, &m, t)
}

/// Plays the next move for `side` after `f` changed the intent.
pub(super) fn mv_with(
    room: &mut GameRoom,
    side: Side,
    t: impl Into<Timing>,
    f: impl FnOnce(&mut Move),
) -> Outcome {
    let mut m = intent(room);
    f(&mut m);
    room.on_move(side, &m, t)
}

/// Plays the next move of whoever is to move.
pub(super) fn next(room: &mut GameRoom, t: impl Into<Timing>) -> Outcome {
    let side = room.side_to_move();
    mv(room, side, t)
}

pub(super) fn decode(frame: &Bytes) -> ServerMsg {
    ServerMsg::decode_exact(frame).expect("a valid server frame")
}

pub(super) fn dec(frames: &[Bytes]) -> Vec<ServerMsg> {
    frames.iter().map(decode).collect()
}

pub(super) fn types(frames: &[Bytes]) -> Vec<MsgType> {
    frames.iter().map(|f| decode(f).msg_type()).collect()
}

pub(super) fn move_made(frame: &Bytes) -> scacelith_protocol::MoveMade {
    match decode(frame) {
        ServerMsg::MoveMade(m) => m,
        other => panic!("not a MoveMade: {other:?}"),
    }
}

pub(super) fn game_end(frame: &Bytes) -> scacelith_protocol::GameEnd {
    match decode(frame) {
        ServerMsg::GameEnd(m) => m,
        other => panic!("not a GameEnd: {other:?}"),
    }
}

pub(super) fn game_event(frame: &Bytes) -> scacelith_protocol::GameEvent {
    match decode(frame) {
        ServerMsg::GameEvent(m) => m,
        other => panic!("not a GameEvent: {other:?}"),
    }
}

pub(super) fn error(frame: &Bytes) -> scacelith_protocol::Error {
    match decode(frame) {
        ServerMsg::Error(m) => m,
        other => panic!("not an Error: {other:?}"),
    }
}

pub(super) fn rejected(frame: &Bytes) -> scacelith_protocol::MoveRejected {
    match decode(frame) {
        ServerMsg::MoveRejected(m) => m,
        other => panic!("not a MoveRejected: {other:?}"),
    }
}

pub(super) fn snapshot_of(frame: &Bytes) -> GameSnapshot {
    match decode(frame) {
        ServerMsg::GameSnapshot(m) => m,
        other => panic!("not a GameSnapshot: {other:?}"),
    }
}

/// The code of the first reply frame (an `Error` or a `MoveRejected`).
pub(super) fn reply_code(out: &Outcome) -> ErrorCode {
    match decode(&out.reply[0]) {
        ServerMsg::Error(e) => e.code,
        ServerMsg::MoveRejected(r) => r.code,
        other => panic!("not a refusal: {other:?}"),
    }
}

pub(super) fn result(room: &GameRoom) -> (GameStatus, EndReason) {
    let r = room.result().expect("the game is over");
    (r.status, r.reason)
}

pub(super) fn anomaly(out: &Outcome) -> (&'static str, Side, bool) {
    let a = out.anomaly.as_ref().expect("an anomaly");
    (a.kind, a.side, a.pos_matched)
}

#[test]
fn first_moves_run_no_clock_and_move_made_is_one_buffer_for_both_players() {
    let mut room = mk();
    let o1 = mv_with(&mut room, W, T0 + 5000, |m| m.think_ms = 4000);
    assert_eq!(o1.broadcast.len(), 1);
    assert_eq!(o1.reply.len(), 0);
    let m1 = move_made(&o1.broadcast[0]);
    assert_eq!(
        (m1.gseq, m1.ply, m1.spent_ms, m1.white_ms, m1.black_ms, m1.first_move_ms, m1.server_time),
        (1, 0, 0, 180000, 180000, 30000, (T0 + 5000) as f64)
    );
    assert_eq!(o1.journal.len(), 1);
    let o2 = mv(&mut room, B, T0 + 9000);
    let m2 = move_made(&o2.broadcast[0]);
    assert_eq!(
        (m2.gseq, m2.ply, m2.spent_ms, m2.white_ms, m2.black_ms, m2.first_move_ms),
        (2, 1, 0, 180000, 180000, 0)
    );
    // White's clock starts with Black's first move.
    let s = room.snapshot(W, T0 + 10000);
    assert_eq!(s.running, Color::White);
    assert_eq!(s.white_ms, 179000);
    assert_eq!(s.black_ms, 180000);
    assert_eq!(s.first_move_ms, 0);
    assert_eq!(room.next_deadline(), Some(T0 + 9000 + 180000 + CAP0));
}

#[test]
fn clocked_move_charges_the_elapsed_time_adds_the_increment_and_compensates_lag_only() {
    let mut room = opened();
    // White thinks 10 s and the move arrives 10.03 s after the turn started: 30 ms of lag.
    let o = mv_with(&mut room, W, T0 + 2000 + 10030, |m| m.think_ms = 10000);
    let m = move_made(&o.broadcast[0]);
    assert_eq!(m.spent_ms, 10000);
    assert_eq!(m.white_ms, 180000 - 10000 + 2000);
    assert_eq!(m.black_ms, 180000);
    assert_eq!(room.clock().quota(W), 2000 - 30 + 100);
    assert_eq!(o.anomaly, None);
    // Black's clock runs from White's move.
    assert_eq!(room.snapshot(B, T0 + 12030 + 500).black_ms, 179500);
}

#[test]
fn lag_compensation_is_bounded_by_rtt_plus_50_lag_comp_max_and_the_quota() {
    let mut room = opened();
    room.on_rtt(W, 30.0); // cap = 80
    let o = mv_with(&mut room, W, T0 + 2000 + 5000, |m| m.think_ms = 4000); // 1000 ms of lag
    assert_eq!(move_made(&o.broadcast[0]).spent_ms, 5000 - 80);
    assert_eq!(room.clock().quota(W), 2000 - 80 + 100);

    let mut room = opened();
    room.on_rtt(W, 5000.0); // capped at 2000 -> cap = min(2050, 1000, 2000)
    assert_eq!(room.clock().rtt(W), 2000);
    let o = mv(&mut room, W, T0 + 2000 + 5000);
    assert_eq!(move_made(&o.broadcast[0]).spent_ms, 5000 - 1000);

    // Quota: 300 ms initially, no gain.
    let mut room = Mk {
        settings: settings(&[("LAG_QUOTA_INITIAL_MS", "300"), ("LAG_QUOTA_GAIN_MS", "0")]),
        ..Mk::default()
    }
    .opened();
    room.on_rtt(W, 950.0);
    room.on_rtt(B, 950.0); // rtt cap 1000
    let mut t = T0 + 2000;
    let mut spent = Vec::new();
    for _ in 0..3 {
        t += 2000;
        let o = next(&mut room, t); // 2000 ms of lag each time
        spent.push(move_made(&o.broadcast[0]).spent_ms);
    }
    // White: comp 300 (whole quota), Black: 300, White again: quota exhausted -> 0.
    assert_eq!(spent, [1700, 1700, 2000]);
    assert_eq!(room.clock().quota(W), 0);
}

#[test]
fn think_ms_never_adds_time_and_an_impossible_think_ms_is_clock_implausible() {
    let mut room = opened();
    let o = mv_with(&mut room, W, T0 + 2000 + 3000, |m| m.think_ms = 9000);
    assert_eq!(move_made(&o.broadcast[0]).spent_ms, 3000, "charged = elapsed, no compensation");
    assert_eq!(anomaly(&o).0, "clock_implausible");
    assert_eq!(anomaly(&o).1, W);
    assert_eq!(o.anomaly.as_ref().map(|a| a.detail.as_str()), Some("ply 2 thinkMs 9000 elapsed 3000"));
    assert!(!CERTAIN_KINDS.contains(&"clock_implausible"));
    // thinkMs = elapsed + 100 is still plausible (clock drift).
    let o2 = mv_with(&mut room, B, T0 + 5000 + 1000, |m| m.think_ms = 1100);
    assert_eq!(o2.anomaly, None);
    assert_eq!(move_made(&o2.broadcast[0]).spent_ms, 1000);
}

#[test]
fn flag_at_the_exact_deadline_including_the_maximal_compensation() {
    let mut room = opened();
    let deadline = T0 + 2000 + 180000 + CAP0;
    assert_eq!(room.next_deadline(), Some(deadline));
    assert_eq!(room.tick(deadline - 1).broadcast.len(), 0);
    let o = room.tick(deadline);
    assert!(o.ended);
    let end = game_end(&o.broadcast[0]);
    assert_eq!((end.status, end.reason, end.white_ms, end.black_ms), (GS::BlackWins, ER::Timeout, 0, 180000));
    assert_eq!(
        room.result(),
        Some(&GameResult {
            status: GS::BlackWins,
            reason: ER::Timeout,
            white_ms: 0,
            black_ms: 180000,
            ended_at: deadline
        })
    );
}

#[test]
fn a_move_one_millisecond_before_the_deadline_counts_and_at_the_deadline_it_is_flag_fell() {
    let deadline = T0 + 2000 + 180000 + CAP0;
    let mut room = opened();
    let o = mv(&mut room, W, deadline - 1);
    let m = move_made(&o.broadcast[0]);
    assert_eq!(m.white_ms, 1 + 2000);

    let mut room = opened();
    let o = mv(&mut room, W, deadline);
    assert_eq!(types(&o.broadcast), [MsgType::GameEnd]);
    let rej = rejected(&o.reply[0]);
    assert_eq!(rej.code, EC::FlagFell);
    assert_eq!(types(&o.reply)[1], MsgType::GameSnapshot);
    assert_eq!(o.anomaly, None);
    assert_eq!(o.rejected, Some(EC::FlagFell));

    // An honest thinkMs lowers the compensation: flagged before the timer deadline.
    let mut room = opened();
    let o = mv_with(&mut room, W, deadline - 10, |m| m.think_ms = 180000);
    assert_eq!(reply_code(&o), EC::FlagFell);
    assert_eq!(result(&room).1, ER::Timeout);
}

#[test]
fn flag_against_a_lone_king_is_a_draw() {
    let mut room =
        Mk { script: Script { can_mate: [true, false], ..Script::default() }, ..Mk::default() }.opened();
    room.tick(T0 + 2000 + 180000 + CAP0);
    assert_eq!(result(&room), (GS::Draw, ER::TimeoutVsInsufficient));
}

#[test]
fn first_move_timeout_is_a_no_show_abort_with_a_conduct_incident_and_an_unrated_record() {
    // The deadline has the margin of a flag (CAP0); the countdown shown to the players does not.
    let mut room = mk();
    assert_eq!(room.next_deadline(), Some(T0 + 30000 + CAP0));
    assert_eq!(room.snapshot(W, T0 + 10000).first_move_ms, 20000);
    assert!(!room.tick(T0 + 30000 + CAP0 - 1).ended);
    let o = room.tick(T0 + 30000 + CAP0);
    assert!(o.ended);
    assert_eq!(result(&room), (GS::Aborted, ER::NoShow));
    assert_eq!(o.conduct, [(11, IncidentKind::NoShow)]);
    assert!(!room.record().expect("over").rated);

    let mut room = mk();
    mv(&mut room, W, T0 + 4000);
    assert_eq!(room.next_deadline(), Some(T0 + 4000 + 30000 + CAP0));
    let o = room.tick(T0 + 34000 + CAP0);
    assert_eq!(o.conduct, [(22, IncidentKind::NoShow)]);
    assert_eq!(result(&room).1, ER::NoShow);
    // A late first move finds the game over.
    let late = mv(&mut room, B, T0 + 34001 + CAP0);
    assert_eq!(reply_code(&late), EC::GameOver);
    assert_eq!(anomaly(&late), ("game_over", B, false));
}

#[test]
fn a_first_move_sent_in_time_over_a_slow_link_is_accepted_within_the_margin_which_follows_the_round_trip() {
    let mut room = mk();
    let o = mv_with(&mut room, W, T0 + 30000 + CAP0 - 1, |m| m.think_ms = 29900);
    assert!(o.moved);
    assert_eq!(room.next_deadline(), Some(T0 + 30000 + CAP0 - 1 + 30000 + CAP0));
    room.on_rtt(B, 400.0); // cap: min(quota 2000, 400 + 50, 1000)
    assert_eq!(room.next_deadline(), Some(T0 + 30000 + CAP0 - 1 + 30000 + 450));
}

#[test]
fn double_moves_duplicates_and_stale_plies() {
    let mut room = mk();
    let first = mv(&mut room, W, T0 + 1000);
    let original = first.broadcast[0].clone();
    // Same ply, same move (resent after a reconnection): the original MoveMade, to the sender only.
    let resent = Move {
        seq: 9,
        game: ID,
        ply: 0,
        r#move: fake_move(0, 0),
        pos_hash: 0,
        think_ms: 0,
        draw_offer: false,
    };
    let dup = room.on_move(W, &resent, T0 + 1500);
    assert!(dup.duplicate);
    assert_eq!(dup.broadcast.len(), 0);
    assert_eq!(dup.reply, [original]);
    assert_eq!(dup.anomaly, None);
    assert_eq!(room.gseq(), 1);
    // Same ply, other move: stale (info).
    let other = Move { seq: 10, r#move: fake_move(5, 0), ..resent };
    let stale = room.on_move(W, &other, T0 + 1600);
    assert_eq!(reply_code(&stale), EC::StalePly);
    assert_eq!(anomaly(&stale).0, "stale_ply");
    assert_eq!(
        stale.anomaly.as_ref().map(|a| a.detail.clone()),
        Some(format!("ply 0 move {} (game at ply 1)", fake_move(5, 0)))
    );
    // White plays again in the synchronised position: out of turn, certain.
    let again = mv(&mut room, W, T0 + 1700);
    assert_eq!(again.broadcast.len(), 0);
    assert_eq!(types(&again.reply), [MsgType::MoveRejected, MsgType::GameSnapshot]);
    assert_eq!(reply_code(&again), EC::NotYourTurn);
    assert_eq!((anomaly(&again).0, anomaly(&again).2), ("out_of_turn", true));
    assert_eq!(room.ply(), 1);
}

#[test]
fn out_of_turn_with_a_non_matching_pos_hash_is_a_desync_and_three_desyncs_are_suspicious() {
    let mut room = mk();
    mv(&mut room, W, T0 + 1000);
    let mut kinds = Vec::new();
    for i in 0..3 {
        let m = Move {
            seq: 1,
            game: ID,
            ply: 1,
            r#move: fake_move(1, 0),
            pos_hash: 12345,
            think_ms: 0,
            draw_offer: false,
        };
        let o = room.on_move(W, &m, T0 + 2000 + i);
        assert_eq!(reply_code(&o), EC::Desync);
        assert_eq!(types(&o.reply)[1], MsgType::GameSnapshot);
        assert!(!anomaly(&o).2);
        assert_eq!(o.journal.len(), 1, "a desync is journaled");
        kinds.push(anomaly(&o).0);
    }
    assert_eq!(kinds, ["desync", "desync", "repeated_desync"]);
    // A future ply with the right hash is a desync as well.
    let fut = Move {
        seq: 1,
        game: ID,
        ply: 5,
        r#move: fake_move(1, 0),
        pos_hash: room.digest(),
        think_ms: 0,
        draw_offer: false,
    };
    let o = room.on_move(B, &fut, T0 + 3000);
    assert_eq!(reply_code(&o), EC::Desync);
    assert_eq!(room.ply(), 1);
    assert_eq!(room.gseq(), 1, "no gseq for a desync");
}

#[test]
fn illegal_move_is_certain_in_a_synchronised_position_a_desync_otherwise_and_never_sent_to_the_opponent() {
    let bad = fake_move(1, 0) ^ 0x40; // some other bit pattern
    let mut room = Mk { script: Script { illegal: vec![bad], ..Script::default() }, ..Mk::default() }.room();
    mv(&mut room, W, T0 + 1000);
    let m = Move {
        seq: 3,
        game: ID,
        ply: 1,
        r#move: bad,
        pos_hash: room.digest(),
        think_ms: 0,
        draw_offer: false,
    };
    let o = room.on_move(B, &m, T0 + 2000);
    assert_eq!(o.broadcast.len(), 0);
    assert_eq!(reply_code(&o), EC::IllegalMove);
    assert_eq!(anomaly(&o), ("illegal_move", B, true));
    assert!(CERTAIN_KINDS.contains(&"illegal_move"));
    let o2 = room.on_move(B, &Move { seq: 4, pos_hash: 1, ..m }, T0 + 2100);
    assert_eq!(reply_code(&o2), EC::Desync);
    assert_eq!(room.ply(), 1);
}

#[test]
fn promotion_bits_and_flags_pass_through() {
    let script = Script { flags: [(2, move_flag::CAPTURE | move_flag::CHECK)].into(), ..Script::default() };
    let mut room = Mk { script, ..Mk::default() }.room();
    mv(&mut room, W, T0 + 1000);
    mv(&mut room, B, T0 + 2000);
    let promo = fake_move(2, 5);
    let o = mv_with(&mut room, W, T0 + 3000, |m| m.r#move = promo);
    let m = move_made(&o.broadcast[0]);
    assert_eq!(m.r#move, promo);
    assert_eq!(m.flags, move_flag::PROMOTION | move_flag::CAPTURE | move_flag::CHECK);
    let s = room.snapshot(B, T0 + 3000);
    assert_eq!(s.moves[2].r#move, promo);
    room.on_resign(B, 0, T0 + 4000);
    assert_eq!(room.record().expect("over").moves[2], promo);
}

#[test]
fn abort_only_before_the_own_first_move_with_a_conduct_incident_and_unrated() {
    let mut room = mk();
    mv(&mut room, W, T0 + 1000);
    let o = room.on_abort(W, 44, T0 + 1500);
    let err = error(&o.reply[0]);
    assert_eq!((err.code, err.r#ref, err.game, err.fatal), (EC::AbortNotAllowed, 44, ID, false));
    let o = room.on_abort(B, 0, T0 + 1600);
    assert!(o.ended);
    assert_eq!(result(&room), (GS::Aborted, ER::Aborted));
    assert_eq!(o.conduct, [(22, IncidentKind::Abort)]);
    assert!(!room.record().expect("over").rated);

    let mut room = mk();
    room.on_abort(B, 0, T0 + 100); // Black before White's first move: allowed
    assert_eq!(result(&room).1, ER::Aborted);
    let mut room = opened();
    assert_eq!(reply_code(&room.on_abort(B, 0, T0 + 3000)), EC::AbortNotAllowed);
}

#[test]
fn resignation_and_a_resignation_after_the_flag_deadline_loses_to_the_flag() {
    let mut room = opened();
    let o = room.on_resign(W, 0, T0 + 5000);
    assert!(o.ended);
    let end = game_end(&o.broadcast[0]);
    assert_eq!((end.status, end.reason, end.white_ms), (GS::BlackWins, ER::Resignation, 177000));
    assert!(room.record().expect("over").rated);
    let again = room.on_resign(B, 5, T0 + 6000);
    assert_eq!(reply_code(&again), EC::GameOver);
    assert_eq!(error(&again.reply[0]).r#ref, 5);

    // White's flag deadline has passed but the timer has not fired yet: Black resigns too late.
    let mut room = opened();
    let deadline = T0 + 2000 + 180000 + CAP0;
    let o = room.on_resign(B, 0, deadline + 4);
    assert_eq!(result(&room), (GS::BlackWins, ER::Timeout));
    assert_eq!(types(&o.broadcast), [MsgType::GameEnd]);
    assert_eq!(reply_code(&o), EC::GameOver);
}

#[test]
fn draw_offer_alone_declined_by_answer_accepted_by_answer() {
    let mut room = opened();
    let o = room.on_draw_offer(W, 0, T0 + 3000);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color, ev.gseq), (EV::DrawOffered, Color::White, 3));
    assert_eq!(room.snapshot(B, T0 + 3000).draw_offer, Color::White);
    assert_eq!(room.on_draw_offer(W, 0, T0 + 3100), Outcome::default(), "already standing");
    assert_eq!(reply_code(&room.on_draw_answer(W, true, 3, T0 + 3200)), EC::NoPendingOffer);
    let o = room.on_draw_answer(B, false, 0, T0 + 3300);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color), (EV::DrawDeclined, Color::Black));
    assert_eq!(room.draw_offer, None);
    room.on_draw_offer(B, 0, T0 + 3400);
    room.on_draw_answer(W, true, 0, T0 + 3500);
    assert_eq!(result(&room), (GS::Draw, ER::Agreement));
}

#[test]
fn a_move_declines_the_pending_offer_offers_made_with_a_move_and_limits() {
    let mut room = opened();
    // White moves with an offer: MoveMade.drawOffer.
    let o = mv_with(&mut room, W, T0 + 3000, |m| m.draw_offer = true);
    assert!(move_made(&o.broadcast[0]).draw_offer);
    assert_eq!(room.draw_offer, Some(W));
    // Black moves instead of answering: declined (event after the MoveMade).
    let o = mv(&mut room, B, T0 + 4000);
    let (m, ev) = (move_made(&o.broadcast[0]), game_event(&o.broadcast[1]));
    assert_eq!((m.gseq, ev.kind, ev.color, ev.gseq), (4, EV::DrawDeclined, Color::Black, 5));
    assert_eq!(room.draw_offer, None);
    // White may not offer again before 10 plies after the decline.
    assert_eq!(reply_code(&room.on_draw_offer(W, 12, T0 + 4100)), EC::DrawOfferLimit);
    let o = mv_with(&mut room, W, T0 + 5000, |m| {
        m.draw_offer = true;
        m.seq = 13;
    });
    assert!(!move_made(&o.broadcast[0]).draw_offer, "the move goes on without the offer");
    let e = error(&o.reply[0]);
    assert_eq!((e.code, e.r#ref), (EC::DrawOfferLimit, 13));
    let mut t = T0 + 5000;
    while room.ply() < 4 + 10 {
        t += 100;
        next(&mut room, t);
    }
    assert_eq!(room.side_to_move(), W);
    t += 100;
    assert_eq!(room.on_draw_offer(W, 0, t).broadcast.len(), 1, "10 plies later: allowed");
    // Offering while the opponent's offer stands is an agreement.
    room.on_draw_offer(B, 0, t + 50);
    assert_eq!(result(&room), (GS::Draw, ER::Agreement));
}

#[test]
fn draw_offers_per_game_limits_the_offers_of_each_player() {
    let mut room = Mk { settings: settings(&[("DRAW_OFFERS_PER_GAME", "2")]), ..Mk::default() }.opened();
    let mut t = T0 + 3000;
    for i in 0..2 {
        t += 10;
        assert_eq!(room.on_draw_offer(B, 0, t).broadcast.len(), 1);
        t += 10;
        room.on_draw_answer(W, false, 0, t);
        while room.ply() < 2 + 10 * (i + 1) {
            t += 10;
            next(&mut room, t);
        }
    }
    t += 10;
    assert_eq!(reply_code(&room.on_draw_offer(B, 99, t)), EC::DrawOfferLimit);
    assert_eq!(room.draw_offers_used, [0, 2]);
}

#[test]
fn draw_claims_threefold_fifty_moves_nothing_to_claim() {
    let script = Script { threefold_at: vec![4], fifty_at: vec![6], ..Script::default() };
    let mut room = Mk { script, ..Mk::default() }.opened();
    let o = room.on_draw_claim(W, 21, T0 + 3000);
    assert_eq!(reply_code(&o), EC::NothingToClaim);
    assert_eq!(anomaly(&o), ("nothing_to_claim", W, true));
    assert_eq!(o.anomaly.as_ref().map(|a| a.detail.as_str()), Some("ply 2"));
    assert!(!CERTAIN_KINDS.contains(&"nothing_to_claim"));
    mv(&mut room, W, T0 + 3100);
    mv(&mut room, B, T0 + 3200);
    room.on_draw_claim(W, 0, T0 + 3300);
    assert_eq!(result(&room), (GS::Draw, ER::ThreefoldClaim));

    let mut room = Mk { script: Script { fifty_at: vec![4], ..Script::default() }, ..Mk::default() }.opened();
    mv(&mut room, W, T0 + 3100);
    mv(&mut room, B, T0 + 3200);
    room.on_draw_claim(B, 0, T0 + 3300);
    assert_eq!(result(&room).1, ER::FiftyMoveClaim);
}

#[test]
fn automatic_end_from_the_rules() {
    let script = Script { end_after: [(3, (GS::WhiteWins, ER::Checkmate))].into(), ..Script::default() };
    let mut room = Mk { script, ..Mk::default() }.room();
    mv(&mut room, W, T0 + 1000);
    mv(&mut room, B, T0 + 2000);
    let o = mv(&mut room, W, T0 + 3000);
    assert_eq!(types(&o.broadcast), [MsgType::MoveMade, MsgType::GameEnd]);
    assert!(o.ended);
    assert_eq!(result(&room), (GS::WhiteWins, ER::Checkmate));
    assert_eq!(room.next_deadline(), Some(T0 + 3000 + REMATCH_WINDOW_MS));
}

#[test]
fn disconnection_and_reconnection_within_the_grace_keeps_the_game() {
    let mut room = opened();
    let o = room.on_disconnect(B, T0 + 3000);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color, i64::from(ev.arg)), (EV::PlayerDisconnected, Color::Black, GRACE));
    assert_eq!(i64::from(room.snapshot(W, T0 + 4000).grace_ms), GRACE - 1000);
    assert!(!room.snapshot(W, T0 + 4000).black_connected);
    assert_eq!(room.next_deadline(), Some(T0 + 3000 + GRACE));
    let o = room.on_reconnect(B, T0 + 3000 + GRACE - 1);
    assert_eq!(game_event(&o.broadcast[0]).kind, EV::PlayerReconnected);
    assert!(!room.tick(T0 + 3000 + GRACE + 1).ended);
    assert_eq!(room.next_deadline(), Some(T0 + 2000 + 180000 + CAP0));
    // A second reconnection is idempotent.
    assert_eq!(room.on_reconnect(B, T0 + 30000).broadcast.len(), 0);
}

#[test]
fn abandonment_after_the_grace_draw_when_the_opponent_cannot_mate_no_show_before_two_plies() {
    let mut room = opened();
    room.on_disconnect(B, T0 + 3000);
    assert!(!room.tick(T0 + 3000 + GRACE - 1).ended);
    let o = room.tick(T0 + 3000 + GRACE);
    assert_eq!(result(&room), (GS::WhiteWins, ER::Abandonment));
    assert_eq!(o.conduct, [(22, IncidentKind::Abandon)]);

    let mut room =
        Mk { script: Script { can_mate: [false, true], ..Script::default() }, ..Mk::default() }.opened();
    room.on_disconnect(B, T0 + 3000);
    let o = room.tick(T0 + 3000 + GRACE);
    assert_eq!(result(&room), (GS::Draw, ER::AbandonmentVsInsufficient));
    assert_eq!(o.conduct, [(22, IncidentKind::Abandon)]);

    let mut room = mk();
    mv(&mut room, W, T0 + 1000);
    room.on_disconnect(W, T0 + 1500);
    let o = room.tick(T0 + 1500 + GRACE);
    assert_eq!(result(&room), (GS::Aborted, ER::NoShow));
    assert_eq!(o.conduct, [(11, IncidentKind::NoShow)]);

    // The disconnected player's clock keeps running: the flag comes first.
    let mut room = Mk { base_ms: 10000, inc_ms: 0, ..Mk::default() }.opened();
    room.on_disconnect(W, T0 + 2500); // grace 15000
    room.tick(T0 + 2000 + 10000 + CAP0);
    assert_eq!(result(&room).1, ER::Timeout);
}

#[test]
fn both_disconnected_within_5_s_is_aborted_after_the_longer_grace_otherwise_the_first_one_abandons() {
    let mut room = opened();
    room.on_disconnect(W, T0 + 3000);
    room.on_disconnect(B, T0 + 7000);
    assert_eq!(room.next_deadline(), Some(T0 + 7000 + GRACE));
    assert!(!room.tick(T0 + 3000 + GRACE).ended);
    let o = room.tick(T0 + 7000 + GRACE);
    assert_eq!(result(&room), (GS::Aborted, ER::BothDisconnected));
    assert_eq!(o.conduct, []);
    assert!(!room.record().expect("over").rated);

    let mut room = opened();
    room.on_disconnect(W, T0 + 3000);
    room.on_disconnect(B, T0 + 8001);
    room.tick(T0 + 3000 + GRACE);
    assert_eq!(result(&room), (GS::BlackWins, ER::Abandonment));
}

#[test]
fn rematch_offer_and_agreement_decline_expiry_leaving() {
    let mut room = opened();
    assert_eq!(reply_code(&room.on_rematch(W, true, 1, T0 + 2500)), EC::RematchUnavailable);
    room.on_resign(B, 0, T0 + 3000);
    let o = room.on_rematch(B, true, 0, T0 + 4000);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color), (EV::RematchOffered, Color::Black));
    assert_eq!(room.snapshot(W, T0 + 4000).rematch, Color::Black);
    let o = room.on_rematch(W, true, 0, T0 + 5000);
    assert_eq!(o.broadcast.len(), 0);
    assert_eq!(
        o.rematch,
        Some(RematchSpec {
            game: ID,
            white: room.player(B).clone(),
            black: room.player(W).clone(),
            category: "3+2".to_owned(),
            base_ms: 180000,
            inc_ms: 2000,
            rated: true,
            auto_press: true,
        })
    );
    assert_eq!(room.next_deadline(), None);
    assert_eq!(reply_code(&room.on_rematch(W, true, 2, T0 + 5100)), EC::RematchUnavailable);

    let mut room = opened();
    room.on_resign(B, 0, T0 + 3000);
    room.on_rematch(W, true, 0, T0 + 4000);
    let o = room.tick(T0 + 3000 + REMATCH_WINDOW_MS);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color), (EV::RematchDeclined, Color::None));
    assert!(!room.rematch_open());
    assert_eq!(room.next_deadline(), None);

    let mut room = opened();
    room.on_resign(B, 0, T0 + 3000);
    room.on_rematch(W, true, 0, T0 + 4000);
    let o = room.on_rematch(B, false, 0, T0 + 4500);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color), (EV::RematchDeclined, Color::Black));
    assert!(!room.rematch_open());

    let mut room = opened();
    room.on_resign(B, 0, T0 + 3000);
    room.on_rematch(W, true, 0, T0 + 4000);
    let o = room.on_disconnect(B, T0 + 4200);
    assert_eq!(game_event(&o.broadcast[0]).kind, EV::RematchDeclined);
    assert_eq!(o.journal.len(), 0, "nothing is journaled after the end");
    assert!(!room.rematch_open());
}

#[test]
fn an_expiry_without_offer_is_silent_but_a_departure_always_tells() {
    // R9: leaving or declining after the end always broadcasts RematchDeclined(color).
    let mut room = opened();
    room.on_resign(B, 0, T0 + 3000);
    let o = room.on_disconnect(W, T0 + 3500);
    let ev = game_event(&o.broadcast[0]);
    assert_eq!((ev.kind, ev.color), (EV::RematchDeclined, Color::White));

    let mut room = opened();
    room.on_resign(B, 0, T0 + 3000);
    assert_eq!(room.tick(T0 + 3000 + REMATCH_WINDOW_MS).broadcast.len(), 0);
    assert!(!room.rematch_open());
    // The window also opens after an abort; only ServerAborted closes it.
    let mut room = mk();
    room.on_abort(W, 0, T0 + 100);
    assert!(room.rematch_open());
}

#[test]
fn gseq_increments_on_every_broadcast_event_and_the_snapshot_carries_it() {
    let mut room = opened();
    room.on_draw_offer(B, 0, T0 + 2500);
    room.on_disconnect(W, T0 + 2600);
    room.on_reconnect(W, T0 + 2700);
    let o = mv(&mut room, W, T0 + 3000);
    let seqs: Vec<u32> = dec(&o.broadcast)
        .iter()
        .map(|m| match m {
            ServerMsg::MoveMade(m) => m.gseq,
            ServerMsg::GameEvent(e) => e.gseq,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(seqs, [6, 7]);
    let s = snapshot_of(&room.snapshot_frame(B, T0 + 3000));
    assert_eq!(s.gseq, 7);
    assert_eq!(s.you, Color::Black);
    assert_eq!(s.moves.len(), 3);
    assert_eq!(s.white.name, "alice");
    assert_eq!(s.black.rating, 1612);
    assert_eq!(s.started_at, T0 as f64);
    assert_eq!(s.running, Color::Black);
    assert_eq!(s.status, GS::Ongoing);
}

#[test]
fn the_game_ends_server_aborted_at_the_protocol_ply_limit() {
    let mut room = mk();
    let mut t = T0;
    while !room.is_over() {
        t += 1;
        next(&mut room, t);
    }
    assert_eq!(room.ply(), MAX_PLIES);
    assert_eq!(result(&room).1, ER::ServerAborted);
    assert_eq!(room.record().expect("over").moves.len(), MAX_PLIES);
    assert!(!room.rematch_open());
}

#[test]
fn forfeit_and_server_abort() {
    let mut room = opened();
    let o = room.forfeit(B, T0 + 3000);
    assert_eq!(result(&room), (GS::WhiteWins, ER::Forfeit));
    assert!(o.ended);
    let rec = room.record().expect("over");
    assert!(rec.rated);
    assert_ne!(rec.flags & record_flag::FORFEIT, 0);
    assert_eq!(room.forfeit(W, T0 + 3001), Outcome::default());

    let mut room = opened();
    room.server_abort(T0 + 3000);
    assert_eq!(result(&room), (GS::Aborted, ER::ServerAborted));
    assert!(!room.rematch_open());
}

#[test]
fn record_matches_the_finished_game_record_of_design_5_5() {
    let mut room = opened();
    mv_with(&mut room, W, T0 + 4000, |m| m.think_ms = 2000);
    room.on_resign(B, 0, T0 + 5000);
    let r = room.record().expect("over");
    assert_eq!(r.spent_ms, [0, 0, 2000]);
    assert_eq!(r.clock_ms, [180000, 180000, 180000]);
    assert_eq!(
        r,
        GameRecord {
            id: ID,
            category: "3+2".to_owned(),
            rated: true,
            base_ms: 180000,
            inc_ms: 2000,
            white_id: 11,
            black_id: 22,
            white_name: "alice".to_owned(),
            black_name: "bob".to_owned(),
            white_rating: 1500,
            black_rating: 1612,
            started_at: T0,
            ended_at: T0 + 5000,
            status: GS::WhiteWins,
            reason: ER::Resignation,
            moves: room.moves(),
            spent_ms: vec![0, 0, 2000],
            clock_ms: vec![180000, 180000, 180000],
            rematch_of: 0,
            flags: record_flag::RATED_REQUESTED,
        }
    );
    assert_eq!(mk().record(), None);
}

#[test]
fn auto_press_in_the_snapshot_and_the_rematch_and_manual_press_in_the_record_flags() {
    let mut room = opened();
    assert!(room.auto_press());
    assert!(room.snapshot(W, T0 + 3000).auto_press);
    room.on_resign(B, 0, T0 + 5000);
    assert_eq!(room.record().expect("over").flags & record_flag::MANUAL_PRESS, 0);

    let mut room = Mk { auto_press: false, ..Mk::default() }.opened();
    assert!(!room.snapshot(B, T0 + 3000).auto_press);
    assert!(!snapshot_of(&room.snapshot_frame(W, T0 + 3000)).auto_press);
    room.on_resign(B, 0, T0 + 5000);
    assert_eq!(room.record().expect("over").flags, record_flag::RATED_REQUESTED | record_flag::MANUAL_PRESS);
    room.on_rematch(W, true, 0, T0 + 6000);
    let o = room.on_rematch(B, true, 0, T0 + 7000);
    assert!(!o.rematch.expect("agreed").auto_press, "a rematch keeps the finished game's setting");
}

#[test]
fn players_and_category_are_normalised_and_a_bad_id_is_refused() {
    let mut spec = Mk::default().spec();
    spec.white.name = "\0".to_owned();
    spec.black.name = "é".repeat(20);
    spec.category = String::new();
    let room = GameRoom::new(spec.clone(), RoomSettings::default(), FakeRules::boxed(Script::default()))
        .expect("valid");
    assert_eq!(room.player(W).name, "?");
    assert_eq!(room.player(B).name, "é".repeat(12));
    assert_eq!(room.category(), "custom");
    snapshot_of(&room.snapshot_frame(W, T0)); // encodes
    for id in [0, crate::ids::ID53_LIMIT] {
        let r = GameRoom::new(
            RoomSpec { id, ..spec.clone() },
            RoomSettings::default(),
            FakeRules::boxed(Script::default()),
        );
        assert_eq!(r.err(), Some(RoomError::InvalidGameId(id)));
    }
}

#[test]
fn stall_credit_times_a_move_from_its_credited_arrival_and_starts_the_next_turn_at_once() {
    let deadline = T0 + 2000 + 180000 + CAP0;
    let (now, recv_at) = (deadline + 2000, deadline - 500);
    let mut room = opened();
    let o = mv(&mut room, W, Timing::credited(now, recv_at));
    assert!(o.moved);
    let m = move_made(&o.broadcast[0]);
    // elapsed 179650 from the credited arrival, all of it lag: compensation = the cap (150).
    assert_eq!((m.spent_ms, m.white_ms, m.server_time), (179500, 180000 - 179500 + 2000, now as f64));
    assert_eq!((room.plies[2].at, room.clock().turn_start()), (now, now), "the stall is charged to nobody");
    assert_eq!(room.clock().quota(W), 2000 - CAP0 + 100);
    assert_eq!(room.next_deadline(), Some(now + 180000 + CAP0));
    // Without the credit, the same move flags.
    let mut room = opened();
    assert_eq!(reply_code(&mv(&mut room, W, now)), EC::FlagFell);
    // The arrival is never taken before the latest move (nor after `now`).
    let mut room = opened();
    let early = mv(&mut room, W, Timing::credited(T0 + 2500, T0 - 5000));
    assert_eq!(move_made(&early.broadcast[0]).spent_ms, 0);
}

#[test]
fn stall_credit_keeps_the_real_elapsed_time_for_the_implausible_think_ms_test() {
    let mut room = opened();
    // White thinks 6 s; the move waits in a socket from T0 + 5000 (the stall's start, 3 s into the
    // turn) to T0 + 9000.
    let o = mv_with(&mut room, W, Timing::credited(T0 + 9000, T0 + 5000), |m| m.think_ms = 6000);
    assert_eq!(o.anomaly, None);
    assert_eq!(move_made(&o.broadcast[0]).spent_ms, 3000, "thinkMs is clamped to the credited elapsed time");
    // A thinkMs longer than the real elapsed time is still implausible.
    let o2 = mv_with(&mut room, B, Timing::credited(T0 + 12000, T0 + 10000), |m| m.think_ms = 5000);
    assert_eq!(anomaly(&o2).0, "clock_implausible");
}

#[test]
fn stall_credit_lets_a_resignation_a_draw_agreement_or_an_abort_that_waited_beat_the_flag() {
    let deadline = T0 + 2000 + 180000 + CAP0;
    let mut room = opened();
    let o = room.on_resign(B, 0, Timing::credited(deadline + 4, deadline - 100));
    let r = room.result().expect("over");
    assert_eq!((r.status, r.reason, r.ended_at), (GS::WhiteWins, ER::Resignation, deadline - 100));
    assert!(o.ended);

    let mut room = opened();
    room.on_draw_offer(W, 0, T0 + 3000);
    room.on_draw_answer(B, true, 0, Timing::credited(deadline + 1000, deadline - 1000));
    assert_eq!(result(&room), (GS::Draw, ER::Agreement));

    let mut room = mk();
    room.on_abort(W, 0, Timing::credited(T0 + 30000 + CAP0 + 500, T0 + 29000));
    assert_eq!(result(&room), (GS::Aborted, ER::Aborted));
    // A first move that waited is accepted too.
    let mut room = mk();
    let o =
        mv_with(&mut room, W, Timing::credited(T0 + 30000 + CAP0 + 2000, T0 + 29500), |m| m.think_ms = 29000);
    assert!(o.moved);
}

#[test]
fn stall_credit_never_lets_the_opponents_deadline_overtake_a_disconnection_read_after_a_stall() {
    let deadline = T0 + 2000 + 180000 + CAP0;
    let mut room = opened();
    let o = room.on_disconnect(B, Timing::credited(deadline + 1000, deadline - 1000));
    assert!(!o.ended);
    assert_eq!(game_event(&o.broadcast[0]).kind, EV::PlayerDisconnected);
    // White's move waited in the same drain: accepted.
    assert!(mv(&mut room, W, Timing::credited(deadline + 1001, deadline - 1000)).moved);
    // Resync and reconnection process the deadlines due at the credited arrival only.
    let mut r = opened();
    assert!(!r.on_resync(B, Timing::credited(deadline + 1000, deadline - 1)).ended);
    assert!(r.on_resync(B, deadline + 1000).ended);
}

#[test]
fn stall_credit_a_first_move_timeout_that_fell_during_a_stall_aborts_without_a_no_show() {
    let mut room = mk();
    let o = room.tick(Timing::at(T0 + 30000 + CAP0 + 2000).stalled_since(T0 + 30000));
    assert_eq!(result(&room), (GS::Aborted, ER::NoShow));
    assert_eq!(o.conduct, []);
    // A stall that began after the deadline changes nothing.
    let mut room = mk();
    let o = room.tick(Timing::at(T0 + 30000 + CAP0 + 2000).stalled_since(T0 + 30000 + CAP0 + 1));
    assert_eq!(o.conduct, [(11, IncidentKind::NoShow)]);
}
