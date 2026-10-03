//! Journal tests of the room (ported from the reference `game.journal` suite): replays rebuild
//! identical rooms, the compact forms, the payload formats.

use scacelith_protocol::{EndReason as ER, GameEventKind as EV, GameStatus as GS, Move, MsgType, PlayerInfo};

use super::journal::{
    CHECKPOINT_BYTES, ENDED_REC_BYTES, EVENT_REC_BYTES, MOVE_REC_BYTES, RECOVERED_REC_BYTES, kind,
};
use super::tests::{B, W, decode, game_event, player};
use super::*;
use crate::game::testing::{FakeRules, Script, fake_move};

pub(super) const T0: i64 = 1_800_000_000_000;
const ID: GameId = 987654321;

pub(super) fn spec(auto_press: bool) -> RoomSpec {
    RoomSpec {
        id: ID,
        category: "5+3".to_owned(),
        base_ms: 300000,
        inc_ms: 3000,
        rated: true,
        white: player(5, "white-player", 1720, false),
        black: player(6, "black-player", 1690, false),
        created_at: T0,
        rematch_of: 55,
        auto_press,
    }
}

pub(super) fn replay(log: &[JournalRecord], script: &Script) -> GameRoom {
    GameRoom::from_journal(log, RoomSettings::default(), FakeRules::boxed(script.clone()), true)
        .expect("replayable")
}

/// A room that records every journal record the way the host appends them.
pub(super) struct Journaled {
    pub room: GameRoom,
    pub log: Vec<JournalRecord>,
    pub script: Script,
}

impl Journaled {
    pub fn new(script: Script) -> Self {
        Self::with_spec(spec(true), RoomSettings::default(), script)
    }

    pub fn with_spec(spec: RoomSpec, settings: RoomSettings, script: Script) -> Self {
        let room = GameRoom::new(spec, settings, FakeRules::boxed(script.clone())).expect("valid room");
        let log = vec![room.created_record()];
        Journaled { room, log, script }
    }

    pub fn run(&mut self, out: Outcome) -> Outcome {
        self.log.extend(out.journal.iter().cloned());
        out
    }

    pub fn mv(&mut self, side: Side, t: impl Into<Timing>) -> Outcome {
        self.mv_with(side, t, |_| {})
    }

    pub fn mv_with(&mut self, side: Side, t: impl Into<Timing>, f: impl FnOnce(&mut Move)) -> Outcome {
        let mut m = Move {
            seq: 1,
            game: self.room.id(),
            ply: self.room.ply() as u16,
            r#move: fake_move(self.room.ply(), 0),
            pos_hash: self.room.digest(),
            think_ms: 0,
            draw_offer: false,
        };
        f(&mut m);
        let out = self.room.on_move(side, &m, t);
        self.run(out)
    }

    pub fn replay(&self) -> GameRoom {
        replay(&self.log, &self.script)
    }
}

/// Everything a replay must reproduce (the round-trip averages are not journaled).
#[derive(Debug, PartialEq)]
pub(super) struct State {
    plies: Vec<PlyRecord>,
    gseq: u32,
    draw_offer: Option<Side>,
    draw_offers_used: [u16; 2],
    draw_declined_at: [i32; 2],
    desyncs: [u16; 2],
    connected: [bool; 2],
    disconnected_at: [i64; 2],
    disconnect_grace: [i64; 2],
    clock_held: bool,
    away_since_recovery: [bool; 2],
    clock_ms: [i64; 2],
    quota: [i64; 2],
    turn_start: i64,
    result: Option<GameResult>,
    end_gseq: u32,
    culprit: Option<Side>,
    flags: u32,
    next_deadline: Option<i64>,
    digest: u32,
    snapshot_w: GameSnapshot,
    snapshot_b: GameSnapshot,
    journal_state: Vec<JournalRecord>,
}

pub(super) fn state(room: &GameRoom, t: i64) -> State {
    State {
        plies: room.plies.clone(),
        gseq: room.gseq,
        draw_offer: room.draw_offer,
        draw_offers_used: room.draw_offers_used,
        draw_declined_at: room.draw_declined_at,
        desyncs: room.desyncs,
        connected: room.connected,
        disconnected_at: room.disconnected_at,
        disconnect_grace: room.disconnect_grace,
        clock_held: room.clock_held,
        away_since_recovery: room.away_since_recovery,
        clock_ms: [room.clock.ms(W), room.clock.ms(B)],
        quota: [room.clock.quota(W), room.clock.quota(B)],
        turn_start: room.clock.turn_start(),
        result: room.result,
        end_gseq: room.end_gseq,
        culprit: room.culprit,
        flags: room.flags,
        next_deadline: room.next_deadline(),
        digest: room.digest(),
        snapshot_w: room.snapshot(W, t),
        snapshot_b: room.snapshot(B, t),
        journal_state: room.journal_state(),
    }
}

/// A game with every kind of journaled event.
pub(super) fn busy_game() -> (Journaled, i64) {
    let mut j = Journaled::new(Script::default());
    let mut t = T0;
    t += 1200;
    j.mv(W, t);
    t += 2300;
    j.mv(B, t);
    t += 4100;
    j.mv_with(W, t, |m| m.think_ms = 4000);
    t += 50;
    let o = j.room.on_draw_offer(B, 0, t);
    j.run(o);
    t += 700;
    let o = j.room.on_draw_answer(W, false, 0, t);
    j.run(o);
    t += 3000;
    j.mv_with(B, t, |m| m.think_ms = 2500);
    t += 6000;
    j.mv_with(W, t, |m| m.draw_offer = true); // offer with the move
    t += 10;
    j.mv_with(B, t, |m| {
        m.r#move = 1;
        m.pos_hash = 99;
    }); // desync
    t += 100;
    let o = j.room.on_disconnect(B, t);
    j.run(o);
    t += 4000;
    let o = j.room.on_reconnect(B, t);
    j.run(o);
    t += 500;
    j.mv_with(B, t, |m| m.think_ms = 100000); // declines White's offer; implausible thinkMs
    t += 20;
    let o = j.room.on_disconnect(W, t);
    j.run(o);
    t += 3000;
    j.mv(W, t); // (moves while "disconnected" happen with relays)
    (j, t)
}

fn records(out: &Outcome) -> Vec<(u8, i64, Vec<u8>)> {
    out.journal.iter().map(|r| (r.kind, r.at, r.payload.clone())).collect()
}

#[test]
fn replaying_the_journal_rebuilds_an_identical_room() {
    let (j, t) = busy_game();
    let copy = j.replay();
    assert_eq!(state(&copy, t + 10), state(&j.room, t + 10));
    // Every record kind was exercised.
    let mut kinds: Vec<u8> = j.log.iter().map(|r| r.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds, [kind::CREATED, kind::MOVE, kind::EVENT]);
}

#[test]
fn a_replayed_room_behaves_identically_afterwards() {
    let (mut a, t0) = busy_game();
    let mut b = a.replay();
    let mut t = t0;
    type Step = Box<dyn Fn(&mut GameRoom, &mut i64) -> Outcome>;
    let steps: Vec<Step> = vec![
        Box::new(|r, t| {
            *t += 777;
            let m = Move {
                seq: 1,
                game: r.id(),
                ply: r.ply() as u16,
                r#move: fake_move(r.ply(), 0),
                pos_hash: r.digest(),
                think_ms: 10,
                draw_offer: false,
            };
            let side = r.side_to_move();
            r.on_move(side, &m, *t)
        }),
        Box::new(|r, t| {
            *t += 5;
            r.on_draw_offer(B, 0, *t)
        }),
        Box::new(|r, t| {
            *t += 3000;
            let m = Move {
                seq: 1,
                game: r.id(),
                ply: r.ply() as u16,
                r#move: fake_move(r.ply(), 0),
                pos_hash: r.digest(),
                think_ms: 0,
                draw_offer: false,
            };
            let side = r.side_to_move();
            r.on_move(side, &m, *t)
        }),
        // More than 5 s after White left.
        Box::new(|r, t| {
            *t += 5;
            r.on_disconnect(B, *t)
        }),
        // White's grace expired first: abandonment.
        Box::new(|r, t| {
            *t += 60000;
            r.tick(*t)
        }),
        Box::new(|r, t| {
            *t += 5;
            r.on_rematch(W, true, 0, *t)
        }),
    ];
    for step in &steps {
        let start = t;
        let oa = step(&mut a.room, &mut t);
        t = start;
        let ob = step(&mut b, &mut t);
        assert_eq!(ob.broadcast, oa.broadcast);
        assert_eq!(ob.reply, oa.reply);
        assert_eq!(records(&ob), records(&oa));
        assert_eq!((ob.ended, &ob.conduct), (oa.ended, &oa.conduct));
    }
    assert_eq!(a.room.result().map(|r| r.reason), Some(ER::Abandonment));
    assert_eq!(b.record(), a.room.record());
}

#[test]
fn a_finished_game_replays_to_the_same_result_and_record() {
    let script = Script { end_after: [(5, (GS::WhiteWins, ER::Checkmate))].into(), ..Script::default() };
    let mut j = Journaled::new(script);
    let mut t = T0;
    for i in 0..5 {
        t += 1000;
        j.mv(Side::to_move(i), t);
    }
    assert!(j.room.is_over());
    assert_eq!(j.log.last().map(|r| r.kind), Some(kind::ENDED));
    let copy = j.replay();
    assert_eq!(copy.result(), j.room.result());
    assert_eq!(copy.gseq(), j.room.gseq());
    assert_eq!(copy.record(), j.room.record());
}

#[test]
fn journal_state_is_a_compact_journal_that_rebuilds_the_same_room() {
    let (mut j, t) = busy_game();
    let compact = j.room.journal_state();
    assert_eq!(compact.iter().filter(|r| r.kind == kind::EVENT).count(), 1, "one checkpoint");
    let copy = replay(&compact, &Script::default());
    assert_eq!(state(&copy, t + 10), state(&j.room, t + 10));
    j.room.on_resign(W, 0, t + 20);
    let ended = replay(&j.room.journal_state(), &Script::default());
    assert_eq!(ended.record(), j.room.record());
    assert_eq!(ended.gseq(), j.room.gseq());
}

#[test]
fn journal_snapshot_is_journal_state_in_one_record_and_a_replay_starts_from_the_latest() {
    let (mut j, t) = busy_game();
    let snap = j.room.journal_snapshot(t + 5);
    assert_eq!((snap.kind, snap.at), (kind::SNAPSHOT, t + 5));
    let copy = replay(std::slice::from_ref(&snap), &Script::default());
    assert_eq!(state(&copy, t + 10), state(&j.room, t + 10));
    // Records after the snapshot apply on top of it; the ones before it are ignored.
    let mut log = vec![JournalRecord { kind: kind::MOVE, at: 0, payload: vec![0xff; 3] }, snap];
    let o = j.room.on_resign(B, 0, t + 20);
    log.extend(o.journal);
    let copy = replay(&log, &Script::default());
    assert_eq!(state(&copy, t + 30), state(&j.room, t + 30));
    let finished = replay(std::slice::from_ref(&j.room.journal_snapshot(t + 40)), &Script::default());
    assert_eq!(finished.record(), j.room.record());
    // A truncated or padded snapshot is refused.
    let mut bad = j.room.journal_snapshot(t + 40);
    bad.payload.pop();
    let r =
        GameRoom::from_journal(&[bad], RoomSettings::default(), FakeRules::boxed(Script::default()), true);
    assert!(matches!(r, Err(RoomError::Journal(_))));
}

#[test]
fn recover_marks_both_players_away_with_the_recovery_grace_charges_no_downtime_and_holds_the_clock() {
    let (j, t) = busy_game();
    // The side to move is Black; its clock value at the last journaled move:
    assert_eq!(j.room.side_to_move(), B);
    let black_ms = j.room.clock().ms(B);
    let restart_at = t + 3_600_000; // the server was down for an hour
    const HOLD: i64 = 20000; // RECOVERY_CLOCK_HOLD_MS
    let mut copy = j.replay();
    let out = copy.recover(restart_at);
    assert!(!out.ended);
    assert_eq!(out.journal.len(), 1);
    assert_eq!(copy.gseq(), j.room.gseq() + RECOVERY_GSEQ_JUMP);
    assert_eq!(copy.connected, [false, false]);
    assert_ne!(copy.flags & record_flag::RECOVERED, 0);
    // Black is away: its clock does not run during the hold, and snapshots show it stopped.
    let s = copy.snapshot(W, restart_at + 1000);
    assert_eq!((i64::from(s.black_ms), s.running), (black_ms, scacelith_protocol::Color::None));
    assert!(!s.white_connected);
    assert!(!s.black_connected);
    assert_eq!(s.grace_ms, 90000 - 1000, "RECOVERY_GRACE_MS (the normal 5+3 grace is 30 s)");
    let p = &out.journal[0].payload;
    assert_eq!(u32::from_le_bytes([p[8], p[9], p[10], p[11]]), 90000, "the recovery record keeps the grace");
    assert_eq!(i64::from(u32::from_le_bytes([p[12], p[13], p[14], p[15]])), HOLD, "and the hold");
    assert_eq!(copy.next_deadline(), Some(restart_at + HOLD), "the end of the hold");
    // The recovery record replays too (before the ticks below end the hold and the game).
    let log: Vec<JournalRecord> = j.log.iter().chain(&out.journal).cloned().collect();
    let again = replay(&log, &Script::default());
    assert_eq!(again.connected, [false, false]);
    assert_eq!(again.clock().turn_start(), restart_at + HOLD);
    assert!(again.clock_held);
    assert_eq!(again.gseq(), j.room.gseq() + RECOVERY_GSEQ_JUMP);
    assert_eq!(again.next_deadline(), Some(restart_at + HOLD));
    // Nobody comes back: Black's clock runs once the hold is over (a journaled checkpoint)...
    let released = copy.tick(restart_at + HOLD);
    assert_eq!(released.clock_started, Some(B));
    assert_eq!(
        released.journal.iter().map(|r| (r.kind, r.payload[0])).collect::<Vec<_>>(),
        [(kind::EVENT, EventKind::Checkpoint as u8)]
    );
    assert!(!copy.clock_held);
    let s2 = copy.snapshot(W, restart_at + HOLD + 1000);
    assert_eq!((i64::from(s2.black_ms), s2.running), (black_ms - 1000, scacelith_protocol::Color::Black));
    let log: Vec<JournalRecord> = log.iter().chain(&released.journal).cloned().collect();
    let replayed = replay(&log, &Script::default());
    assert_eq!(state(&replayed, restart_at + HOLD + 1000), state(&copy, restart_at + HOLD + 1000));
    // ... then both are aborted (disconnected together) when the recovery grace ends, unrated.
    assert_eq!(copy.next_deadline(), Some(restart_at + 90000));
    copy.tick(restart_at + 90000 - 1);
    assert!(!copy.is_over());
    copy.tick(restart_at + 90000);
    assert_eq!(copy.result().map(|r| (r.status, r.reason)), Some((GS::Aborted, ER::BothDisconnected)));
}

#[test]
fn recover_then_one_player_comes_back_and_the_other_abandons() {
    let (j, t) = busy_game();
    let mut copy = j.replay();
    let at = t + 50000;
    copy.recover(at);
    let o = copy.on_reconnect(W, at + 2000);
    assert_eq!(game_event(&o.broadcast[0]).kind, EV::PlayerReconnected);
    copy.tick(at + 30000); // the normal grace of a 5+3 game: not yet
    assert!(!copy.is_over());
    copy.tick(at + 90000);
    assert_eq!(copy.result().map(|r| (r.status, r.reason)), Some((GS::WhiteWins, ER::Abandonment)));
}

#[test]
fn recover_ends_a_game_whose_ended_record_was_torn_off() {
    let script = Script { end_after: [(3, (GS::WhiteWins, ER::Checkmate))].into(), ..Script::default() };
    let mut j = Journaled::new(script.clone());
    let mut t = T0;
    for i in 0..3 {
        t += 1000;
        j.mv(Side::to_move(i), t);
    }
    let torn: Vec<JournalRecord> = j.log.iter().filter(|r| r.kind != kind::ENDED).cloned().collect();
    let mut copy = replay(&torn, &script);
    assert!(!copy.is_over());
    let out = copy.recover(t + 99999);
    assert!(out.ended);
    let r = copy.result().expect("over");
    assert_eq!((r.status, r.reason, r.ended_at), (GS::WhiteWins, ER::Checkmate, t));
    assert_eq!(out.journal[0].kind, kind::ENDED);
    assert!(!copy.rematch_open());
}

#[test]
fn recover_ends_a_game_at_the_ply_limit_whose_ended_record_was_torn_off_as_live_play_did() {
    let mut j = Journaled::new(Script::default());
    let mut t = T0;
    while !j.room.is_over() {
        t += 100;
        let side = j.room.side_to_move();
        j.mv(side, t);
    }
    assert_eq!(j.room.ply(), MAX_PLIES);
    let ended = j.log.last().cloned().expect("records");
    assert_eq!(ended.kind, kind::ENDED);
    let mut copy = replay(&j.log[..j.log.len() - 1], &Script::default());
    assert!(!copy.is_over());
    let out = copy.recover(t + 99999);
    assert!(out.ended);
    assert_eq!(copy.result(), j.room.result());
    let r = copy.result().expect("over");
    assert_eq!((r.status, r.reason, r.ended_at), (GS::Aborted, ER::ServerAborted, t));
    assert!(!copy.record().expect("over").rated);
    assert_eq!(out.journal, [ended]);
}

#[test]
fn bad_journals_fail_a_strict_replay_and_stop_a_lenient_one_at_the_bad_record() {
    let (j, _) = busy_game();
    let mut bad = j.log.clone();
    let i = bad.iter().enumerate().position(|(k, r)| k > 3 && r.kind == kind::MOVE).expect("a move");
    bad[i].payload[0..2].copy_from_slice(&999u16.to_le_bytes()); // wrong ply
    let strict =
        GameRoom::from_journal(&bad, RoomSettings::default(), FakeRules::boxed(Script::default()), true);
    assert!(matches!(strict, Err(RoomError::Journal(_))));
    let partial =
        GameRoom::from_journal(&bad, RoomSettings::default(), FakeRules::boxed(Script::default()), false)
            .expect("lenient");
    assert!(partial.replay_error().is_some());
    assert_eq!(partial.ply(), bad[..i].iter().filter(|r| r.kind == kind::MOVE).count());
    let garbage = [JournalRecord { kind: kind::CREATED, at: 0, payload: b"{oops".to_vec() }];
    for log in [&garbage[..], &[], &j.log[1..]] {
        let r =
            GameRoom::from_journal(log, RoomSettings::default(), FakeRules::boxed(Script::default()), false);
        assert!(matches!(r, Err(RoomError::Journal(_))), "{r:?}");
    }
    // A second created record, a short record, an unknown event kind, a bad ended status.
    let mut log = j.log.clone();
    log.push(j.log[0].clone());
    assert!(
        GameRoom::from_journal(&log, RoomSettings::default(), FakeRules::boxed(Script::default()), true)
            .is_err()
    );
    let mut log = j.log.clone();
    log.push(JournalRecord { kind: kind::EVENT, at: T0, payload: vec![9; EVENT_REC_BYTES] });
    assert!(
        GameRoom::from_journal(&log, RoomSettings::default(), FakeRules::boxed(Script::default()), true)
            .is_err()
    );
    let mut log = j.log.clone();
    log.push(JournalRecord { kind: kind::ENDED, at: T0, payload: vec![0; ENDED_REC_BYTES] });
    assert!(
        GameRoom::from_journal(&log, RoomSettings::default(), FakeRules::boxed(Script::default()), true)
            .is_err()
    );
    let mut log = j.log.clone();
    log.push(JournalRecord { kind: kind::MOVE, at: T0, payload: vec![0; MOVE_REC_BYTES - 1] });
    assert!(
        GameRoom::from_journal(&log, RoomSettings::default(), FakeRules::boxed(Script::default()), true)
            .is_err()
    );
    // `committed` and unknown kinds carry no room state.
    let mut log = j.log.clone();
    log.push(JournalRecord { kind: kind::COMMITTED, at: T0, payload: Vec::new() });
    log.push(JournalRecord { kind: 200, at: T0, payload: vec![1, 2, 3] });
    assert_eq!(replay(&log, &Script::default()).ply(), j.room.ply());
}

#[test]
fn journal_payload_formats() {
    let mut j = Journaled::new(Script::default());
    let created = &j.log[0];
    assert_eq!((created.kind, created.at), (kind::CREATED, T0));
    assert_eq!(created.payload[0], 1, "format");
    assert_eq!(created.payload[1], 1 | 2, "rated, autoPress");
    j.mv(W, T0 + 1000);
    let mv_rec = j.log[1].clone();
    assert_eq!(mv_rec.kind, kind::MOVE);
    assert_eq!(mv_rec.payload.len(), MOVE_REC_BYTES);
    assert_eq!(mv_rec.at, T0 + 1000);
    assert_eq!(u16::from_le_bytes([mv_rec.payload[2], mv_rec.payload[3]]), fake_move(0, 0));
    assert_eq!(i64::from_le_bytes(mv_rec.payload[24..32].try_into().expect("8 bytes")), T0 + 1000);
    let o = j.room.on_disconnect(B, T0 + 1500);
    j.run(o);
    assert_eq!(j.log[2].payload.len(), EVENT_REC_BYTES);
    assert_eq!(u32::from_le_bytes(j.log[2].payload[8..12].try_into().expect("4 bytes")), 30000);
    assert_eq!(j.log[2].payload[2], 0);
    // The recovered record: 16 bytes, byte 2 = 1 (each player's first reconnection restarts its
    // first-move timer); a checkpoint's presence byte: 1 White connected, 2 Black connected, 4
    // clock held, 8 White and 16 Black not back since the recovery.
    let mut rec = j.replay();
    let recovered = rec.recover(T0 + 1800).journal[0].payload.clone();
    assert_eq!(recovered.len(), RECOVERED_REC_BYTES);
    assert_eq!(recovered[..4], [EventKind::Recovered as u8, 2, 1, 0]);
    let presence = |r: &GameRoom| {
        let cp = r.journal_state().into_iter().find(|r| r.kind == kind::EVENT).expect("checkpoint");
        assert_eq!(cp.payload.len(), CHECKPOINT_BYTES);
        cp.payload[2]
    };
    assert_eq!(presence(&rec), 4 | 8 | 16);
    rec.on_reconnect(W, T0 + 1900);
    assert_eq!(presence(&rec), 1 | 4 | 16);
    let o = j.room.on_resign(W, 0, T0 + 2000);
    j.run(o);
    let end = &j.log[3];
    assert_eq!(end.kind, kind::ENDED);
    assert_eq!(end.payload.len(), ENDED_REC_BYTES);
    assert_eq!(end.payload[..3], [GS::BlackWins.to_u8(), ER::Resignation.to_u8(), 0]);
    // A MoveMade resent from the stored data is byte-identical to the original.
    let mut room = GameRoom::new(spec(true), RoomSettings::default(), FakeRules::boxed(Script::default()))
        .expect("room");
    let m = Move {
        seq: 1,
        game: ID,
        ply: 0,
        r#move: fake_move(0, 0),
        pos_hash: room.digest(),
        think_ms: 0,
        draw_offer: true,
    };
    let o = room.on_move(W, &m, T0 + 10);
    assert_eq!(decode(&o.broadcast[0]).msg_type(), MsgType::MoveMade);
    assert_eq!(room.encode_move_made(0), o.broadcast[0]);
}

#[test]
fn the_created_record_keeps_every_field_of_the_game() {
    let mut s = spec(false);
    s.white =
        PlayerInfo {
            user_id: u32::MAX, name: "ünïcödé-name".to_owned(), rating: 65535, provisional: true
        };
    let room =
        GameRoom::new(s.clone(), RoomSettings::default(), FakeRules::boxed(Script::default())).expect("room");
    let copy = replay(&[room.created_record()], &Script::default());
    assert_eq!(copy.player(W), &s.white);
    assert_eq!(copy.player(B), &s.black);
    assert_eq!(
        (copy.category(), copy.rated(), copy.auto_press(), copy.created_at()),
        ("5+3", true, false, T0)
    );
    assert_eq!((copy.base_ms, copy.inc_ms, copy.rematch_of), (300000, 3000, 55));
    let mut bad = room.created_record();
    bad.payload[0] = 2;
    assert!(
        GameRoom::from_journal(&[bad], RoomSettings::default(), FakeRules::boxed(Script::default()), true)
            .is_err()
    );
    let mut bad = room.created_record();
    bad.payload[2..10].copy_from_slice(&0u64.to_le_bytes());
    let r =
        GameRoom::from_journal(&[bad], RoomSettings::default(), FakeRules::boxed(Script::default()), true);
    assert_eq!(r.err(), Some(RoomError::InvalidGameId(0)));
}

#[test]
fn auto_press_is_journaled_and_a_replay_and_a_compacted_journal_keep_it() {
    let mut j = Journaled::with_spec(spec(false), RoomSettings::default(), Script::default());
    j.mv(W, T0 + 1000);
    j.mv(B, T0 + 2000);
    assert!(!j.replay().auto_press());
    assert!(!replay(&j.room.journal_state(), &Script::default()).auto_press());
    let compacted = replay(&[j.room.journal_snapshot(T0 + 3000)], &Script::default());
    assert!(!compacted.auto_press());
    assert!(!compacted.snapshot(W, T0 + 3000).auto_press);
}

#[test]
fn a_move_and_a_disconnection_credited_after_a_stall_replay_to_identical_clocks() {
    let mut j = Journaled::new(Script::default());
    j.mv(W, T0 + 1000);
    j.mv(B, T0 + 2000);
    let deadline = j.room.next_deadline().expect("running");
    j.mv(W, Timing::credited(deadline + 3000, deadline - 200));
    let o = j.room.on_disconnect(B, Timing::credited(deadline + 3001, deadline - 200));
    j.run(o);
    assert_eq!((j.room.ply(), j.room.is_over()), (3, false));
    let copy = j.replay();
    assert_eq!(state(&copy, deadline + 4000), state(&j.room, deadline + 4000));
}
