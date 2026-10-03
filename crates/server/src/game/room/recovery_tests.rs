//! Recovered games (DESIGN 6.4, server restart), ported from the room part of the reference
//! `game.recovery` suite: both players get RECOVERY_GRACE_MS to come back instead of the normal
//! grace, the grace is journaled with the recovery, and a later normal disconnection gets the
//! normal grace again. The clock of the side to move waits for that player,
//! RECOVERY_CLOCK_HOLD_MS at most, and the hold replays identically from the journal and from a
//! snapshot. A restored game aborted NoShow because its player never came back records no
//! conduct incident. Before the second ply, the first reconnection of the side to move since the
//! recovery restarts its first-move timer, even after the hold.

use scacelith_protocol::{Color, EndReason as ER, GameEventKind as EV, GameStatus as GS, Move};

use super::journal::kind;
use super::journal_tests::{Journaled, state};
use super::tests::{B, W, game_event, player, settings};
use super::*;
use crate::game::testing::{FakeRules, Script, fake_move};

const T0: i64 = 1_800_000_000_000;
const HOLD: i64 = 20000;
/// The comp cap with the default round trip: the margin of a flag or a first-move deadline.
const CAP: i64 = 150;
const FIRST: i64 = 30000;
/// The deadline of a first move: FIRST plus the margin of a flag.
const FM: i64 = FIRST + CAP;

fn spec(id: GameId, category: &str, base_ms: u32, inc_ms: u32, white: UserId, black: UserId) -> RoomSpec {
    RoomSpec {
        id,
        category: category.to_owned(),
        base_ms,
        inc_ms,
        rated: true,
        white: player(white, &format!("user{white}"), 1500, false),
        black: player(black, &format!("user{black}"), 1500, false),
        created_at: T0,
        rematch_of: 0,
        auto_press: true,
    }
}

/// A 5+3 game (normal grace 30 s) at ply 6, journaled.
fn journaled(settings: RoomSettings) -> (Journaled, i64) {
    let mut s = spec(424242, "5+3", 300000, 3000, 5, 6);
    s.white = player(5, "white-player", 1720, false);
    s.black = player(6, "black-player", 1690, false);
    let mut j = Journaled::with_spec(s, settings, Script::default());
    let mut t = T0;
    for _ in 0..6 {
        t += 2000;
        let side = j.room.side_to_move();
        j.mv(side, t);
    }
    (j, t)
}

/// A restart: a new process rebuilds the room from the journal and recovers it at `at`.
struct Restarted {
    room: GameRoom,
    out: Outcome,
    log: Vec<JournalRecord>,
}

impl Restarted {
    fn run(&mut self, out: Outcome) -> Outcome {
        self.log.extend(out.journal.iter().cloned());
        out
    }

    fn replay(&self) -> GameRoom {
        rebuild(&self.log, RoomSettings::default())
    }
}

fn rebuild(log: &[JournalRecord], settings: RoomSettings) -> GameRoom {
    GameRoom::from_journal(log, settings, FakeRules::boxed(Script::default()), true).expect("replayable")
}

fn restart(log: &[JournalRecord], at: i64, settings: RoomSettings) -> Restarted {
    let mut room = rebuild(log, settings);
    let out = room.recover(at);
    let log = log.iter().chain(&out.journal).cloned().collect();
    Restarted { room, out, log }
}

fn move_of(room: &GameRoom, think_ms: u32) -> Move {
    Move {
        seq: 1,
        game: room.id(),
        ply: room.ply() as u16,
        r#move: fake_move(room.ply(), 0),
        pos_hash: room.digest(),
        think_ms,
        draw_offer: false,
    }
}

fn u32_at(p: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(p[o..o + 4].try_into().expect("4 bytes"))
}

fn result(room: &GameRoom) -> (GameStatus, EndReason) {
    let r = room.result().expect("over");
    (r.status, r.reason)
}

#[test]
fn recovery_grace_is_recovery_grace_ms_or_the_normal_grace_when_longer() {
    let g = settings(&[]).grace;
    assert_eq!(g.recovery_grace_for(180000), 90000);
    assert_eq!(g.recovery_grace_for(5400000), 90000); // 90+30: normal grace 60 s
    let short = settings(&[("RECOVERY_GRACE_MS", "20000"), ("RECOVERY_CLOCK_HOLD_MS", "10000")]).grace;
    assert_eq!(short.recovery_grace_for(180000), 20000); // 3+2: normal grace 18 s
    assert_eq!(short.recovery_grace_for(5400000), short.grace_for(5400000));
    assert_eq!(short.grace_for(5400000), 60000);
    assert_eq!(GracePolicy::default().recovery_grace_for(180000), 90000);
}

#[test]
fn recovery_hold_is_recovery_clock_hold_ms_never_beyond_the_grace() {
    assert_eq!(settings(&[]).grace.recovery_hold_for(180000), 20000);
    assert_eq!(GracePolicy::default().recovery_hold_for(180000), 20000);
    assert_eq!(settings(&[("RECOVERY_CLOCK_HOLD_MS", "0")]).grace.recovery_hold_for(180000), 0);
    let g = GracePolicy { recovery_ms: 30000, recovery_hold_ms: 50000, ..GracePolicy::default() };
    assert_eq!(g.recovery_hold_for(180000), 30000, "never beyond the grace");
    for grace in [15000, 19999, 20000, 20001, 3600000] {
        let g = settings(&[("RECOVERY_GRACE_MS", &grace.to_string())]).grace;
        assert!(g.recovery_hold_for(180000) < g.recovery_grace_for(180000), "RECOVERY_GRACE_MS={grace}");
    }
}

#[test]
fn a_restart_after_a_shutdown_the_disconnections_of_the_drain_do_not_shorten_the_recovery_grace() {
    let (mut j, t) = journaled(RoomSettings::default());
    // The drain closes both connections (ShuttingDown): two Disconnect records, normal grace 30 s.
    let d = j.room.on_disconnect(W, t + 3000);
    let d = j.run(d);
    assert_eq!(game_event(&d.broadcast[0]).arg, 30000);
    let d = j.room.on_disconnect(B, t + 3000);
    j.run(d);
    // The server comes back 45 s later: the normal grace would already be over.
    let r = t + 48000;
    let mut copy = restart(&j.log, r, RoomSettings::default()).room;
    assert!(!copy.is_over());
    assert_eq!(copy.connected, [false, false]);
    assert_eq!(copy.disconnect_grace, [90000, 90000]);
    assert_eq!(copy.next_deadline(), Some(r + 20000), "the clock hold of the side to move ends first");
    copy.tick(r + 20000);
    assert_eq!(copy.next_deadline(), Some(r + 90000));
    assert_eq!(copy.snapshot(W, r).grace_ms, 90000);
    copy.tick(r + 89999);
    assert!(!copy.is_over());
    // White comes back after a minute; Black never does: Black abandons when its grace ends.
    copy.on_reconnect(W, r + 60000);
    assert_eq!(copy.snapshot(W, r + 60000).grace_ms, 30000); // what White waits for
    let out = copy.tick(r + 90000);
    assert_eq!(result(&copy), (GS::WhiteWins, ER::Abandonment));
    assert_eq!(out.conduct, [(6, IncidentKind::Abandon)]);
}

#[test]
fn after_coming_back_a_new_disconnection_gets_the_normal_grace_again() {
    let (j, t) = journaled(RoomSettings::default());
    let r = t + 10000;
    let mut room = restart(&j.log, r, RoomSettings::default()).room;
    room.on_reconnect(W, r + 1000);
    let d = room.on_disconnect(W, r + 10000);
    let ev = game_event(&d.broadcast[0]);
    assert_eq!((ev.kind, ev.color, ev.arg), (EV::PlayerDisconnected, Color::White, 30000));
    assert_eq!(room.disconnect_grace, [30000, 90000]);
    // Both are away, not together: the first grace to end is White's (R + 40 s; Black's R + 90 s).
    assert_eq!(room.next_deadline(), Some(r + 40000));
    assert_eq!(room.snapshot(B, r + 20000).grace_ms, 20000);
    let out = room.tick(r + 40000);
    assert_eq!(result(&room), (GS::BlackWins, ER::Abandonment));
    assert_eq!(out.conduct, [(5, IncidentKind::Abandon)]);
}

#[test]
fn both_players_still_away_when_the_recovery_grace_ends_is_aborted_unrated() {
    let (j, t) = journaled(RoomSettings::default());
    let r = t + 5000;
    let mut room = restart(&j.log, r, RoomSettings::default()).room;
    room.tick(r + 89999);
    assert!(!room.is_over());
    room.tick(r + 90000);
    assert_eq!(result(&room), (GS::Aborted, ER::BothDisconnected));
    assert!(!room.record().expect("over").rated);
}

#[test]
fn the_recovery_record_keeps_the_grace_and_checkpoints_carry_it() {
    let custom = settings(&[("RECOVERY_GRACE_MS", "40000")]);
    let (j, t) = journaled(custom);
    let r = t + 7000;
    let mut first = restart(&j.log, r, custom);
    let rec = &first.out.journal[0];
    assert_eq!(rec.kind, kind::EVENT);
    assert_eq!(u32_at(&rec.payload, 8), 40000);
    // Replayed with another configuration: the journaled grace wins (a replay rebuilds the room).
    let again = rebuild(&first.log, RoomSettings::default());
    assert_eq!(again.disconnect_grace, [40000, 40000]);
    assert_eq!(again.next_deadline(), first.room.next_deadline());
    // journal_state() (created, moves, checkpoint) rebuilds the graces too.
    first.room.on_reconnect(B, r + 3000);
    first.room.on_disconnect(B, r + 4000); // Black: normal grace, White: recovery grace
    let compact = first.room.journal_state();
    let copy = rebuild(&compact, RoomSettings::default());
    assert_eq!(copy.disconnect_grace, first.room.disconnect_grace);
    assert_eq!(copy.next_deadline(), first.room.next_deadline());
    assert_eq!(copy.snapshot(W, r + 5000), first.room.snapshot(W, r + 5000));
    // A recovery record without a grace (arg 0) gives RECOVERY_GRACE_MS of the configuration.
    let mut zero = first.log.clone();
    let last = zero.last_mut().expect("records");
    last.payload[8..12].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(rebuild(&zero, RoomSettings::default()).disconnect_grace, [90000, 90000]);
    // A disconnection replays with its journaled grace, whatever the configuration says now.
    let short = settings(&[("RECONNECT_GRACE_MIN_MS", "20000"), ("RECONNECT_GRACE_MAX_MS", "20000")]);
    let replayed = rebuild(&first.room.journal_state(), short);
    assert_eq!(replayed.disconnect_grace, [40000, 30000]);
    let mut log = first.log.clone();
    log.extend(first.room.on_reconnect(W, r + 6000).journal);
    log.extend(first.room.on_disconnect(W, r + 7000).journal);
    assert_eq!(rebuild(&log, short).disconnect_grace[0], 30000);
}

// ---- Clock hold after a recovery (RECOVERY_CLOCK_HOLD_MS) --------------------------------------

/// A 1+0 game at ply 22 whose side to move, White, has less time left than the clock hold: the
/// time scramble of a bullet game. Returns the journal, the time and White's clock.
fn bullet() -> (Journaled, i64, i64) {
    let mut j =
        Journaled::with_spec(spec(515151, "1+0", 60000, 0, 7, 8), RoomSettings::default(), Script::default());
    let mut t = T0;
    for i in 0..22 {
        t += if i & 1 == 1 { 300 } else { 4500 };
        let side = j.room.side_to_move();
        j.mv(side, t);
    }
    assert_eq!(j.room.side_to_move(), W);
    let w_ms = j.room.clock().ms(W);
    (j, t, w_ms)
}

#[test]
fn the_side_to_moves_clock_waits_for_its_player_and_starts_at_its_reconnection_within_the_hold() {
    let (g, t, w_ms) = bullet();
    assert!(w_ms > 5000 && w_ms + 2000 < HOLD, "White has {w_ms} ms left");
    let r = t + 30000;
    let mut s = restart(&g.log, r, RoomSettings::default());
    assert_eq!(s.out.journal[0].payload.len(), 16);
    assert_eq!(i64::from(u32_at(&s.out.journal[0].payload, 12)), HOLD, "the recovery record keeps the hold");
    assert_eq!(
        (s.room.clock_held, s.room.clock().turn_start(), s.room.next_deadline()),
        (true, r + HOLD, Some(r + HOLD))
    );
    // Black is back first: White's clock stays stopped, and Black's snapshot shows it stopped.
    let o = s.room.on_reconnect(B, r + 1000);
    assert_eq!(s.run(o).clock_started, None);
    let sb = s.room.snapshot(B, r + 1000 + w_ms);
    assert_eq!((sb.running, i64::from(sb.white_ms)), (Color::None, w_ms));
    // White comes back later than its time left (it would have lost on time without the hold).
    let back = r + w_ms + 2000;
    let o = s.room.on_reconnect(W, back);
    let o = s.run(o);
    assert!(!s.room.is_over());
    assert_eq!(o.clock_started, Some(W), "the host tells Black that White's clock runs");
    assert_eq!((s.room.clock_held, s.room.clock().turn_start()), (false, back));
    let sn = s.room.snapshot(B, back + 1000);
    assert_eq!((sn.running, i64::from(sn.white_ms)), (Color::White, w_ms - 1000));
    assert_eq!(s.room.next_deadline(), Some(back + w_ms + s.room.clock().comp_cap(W)));
    assert_eq!(
        state(&s.replay(), back + 1000),
        state(&s.room, back + 1000),
        "the journal replays to the same room"
    );
    // White moves 1.5 s after coming back: charged from its reconnection.
    let m = move_of(&s.room, 1400);
    let o = s.room.on_move(W, &m, back + 1500);
    assert!(s.run(o).moved);
    assert_eq!(s.room.plies[22].spent, 1400);
    assert_eq!(i64::from(s.room.plies[22].clock_after), w_ms - 1400);
}

#[test]
fn the_hold_ends_without_its_player_the_clock_runs_a_checkpoint_is_journaled_and_the_replay_agrees() {
    let (g, t, w_ms) = bullet();
    let r = t + 30000;
    let mut s = restart(&g.log, r, RoomSettings::default());
    let o = s.room.on_reconnect(B, r + 1000);
    s.run(o);
    let o = s.room.tick(r + HOLD - 1);
    assert_eq!(s.run(o).journal.len(), 0);
    let o = s.room.tick(r + HOLD);
    let o = s.run(o);
    assert_eq!(o.clock_started, Some(W));
    assert_eq!(
        o.journal.iter().map(|r| (r.kind, r.at, r.payload[0], r.payload.len())).collect::<Vec<_>>(),
        [(kind::EVENT, r + HOLD, EventKind::Checkpoint as u8, 68)]
    );
    assert!(!s.room.clock_held);
    let sn = s.room.snapshot(B, r + HOLD + 1000);
    assert_eq!((sn.running, i64::from(sn.white_ms)), (Color::White, w_ms - 1000));
    assert_eq!(state(&s.replay(), r + HOLD + 1000), state(&s.room, r + HOLD + 1000));
    // Still away when its time runs out (before the recovery grace): White loses on time.
    let flag_at = r + HOLD + w_ms + s.room.clock().comp_cap(W);
    assert_eq!(s.room.next_deadline(), Some(flag_at));
    s.room.tick(flag_at - 1);
    assert!(!s.room.is_over());
    s.room.tick(flag_at);
    assert_eq!(result(&s.room), (GS::BlackWins, ER::Timeout));
}

#[test]
fn a_snapshot_or_compact_journal_taken_during_the_hold_rebuilds_the_same_room_which_goes_on_identically() {
    let (g, t, w_ms) = bullet();
    let r = t + 30000;
    let mut first = restart(&g.log, r, RoomSettings::default());
    let o = first.room.on_reconnect(B, r + 1000);
    first.run(o);
    let at = r + 5000;
    for (label, records) in [
        ("journal_state", first.room.journal_state()),
        ("journal_snapshot", vec![first.room.journal_snapshot(at)]),
    ] {
        let mut reference = first.replay();
        let mut copy = rebuild(&records, RoomSettings::default());
        assert!(copy.clock_held, "{label}");
        assert_eq!(state(&copy, at), state(&reference, at), "{label}");
        for when in [r + HOLD, r + HOLD + w_ms + 150] {
            let (a, b) = (reference.tick(when), copy.tick(when));
            assert_eq!(b.journal, a.journal, "{label} at {}", when - r);
            assert_eq!(
                (b.clock_started, b.ended, &b.broadcast),
                (a.clock_started, a.ended, &a.broadcast),
                "{label}"
            );
            assert_eq!(state(&copy, when), state(&reference, when), "{label} at {}", when - r);
        }
        assert_eq!(result(&copy), (GS::BlackWins, ER::Timeout));
    }
}

#[test]
fn recovery_clock_hold_0_restarts_the_clock_at_once_and_a_short_recovered_record_is_refused() {
    let (g, t, w_ms) = bullet();
    let r = t + 30000;
    let none = settings(&[("RECOVERY_CLOCK_HOLD_MS", "0")]);
    let r0 = restart(&g.log, r, none);
    assert_eq!(u32_at(&r0.out.journal[0].payload, 12), 0);
    assert_eq!(
        (r0.room.clock_held, r0.room.clock().turn_start(), r0.room.next_deadline()),
        (false, r, Some(r + w_ms + r0.room.clock().comp_cap(W)))
    );
    let mut short = r0.log.clone();
    short.last_mut().expect("records").payload.truncate(12);
    let e = GameRoom::from_journal(&short, none, FakeRules::boxed(Script::default()), true);
    assert!(matches!(e, Err(RoomError::Journal(_))));
}

#[test]
fn a_restored_game_at_ply_1_records_no_incident_when_its_player_never_came_back_and_one_when_it_does_not_move()
 {
    let mut room0 = GameRoom::new(
        spec(616161, "3+2", 180000, 2000, 3, 4),
        RoomSettings::default(),
        FakeRules::boxed(Script::default()),
    )
    .expect("room");
    let mut log = vec![room0.created_record()];
    let m = move_of(&room0, 0);
    log.extend(room0.on_move(W, &m, T0 + 2000).journal);
    let r = T0 + 12000;
    // White is back, Black (to move) never is: its first-move time starts at the end of the hold.
    let mut a = restart(&log, r, RoomSettings::default()).room;
    a.on_reconnect(W, r + 1000);
    assert_eq!(a.next_deadline(), Some(r + HOLD));
    assert_eq!(i64::from(a.snapshot(W, r + 1000).first_move_ms), HOLD - 1000 + 30000);
    a.tick(r + HOLD);
    assert_eq!(a.next_deadline(), Some(r + HOLD + 30000 + CAP));
    let out = a.tick(r + HOLD + 30000 + CAP);
    assert_eq!(result(&a), (GS::Aborted, ER::NoShow));
    assert_eq!(out.conduct, [], "the server broke the connection, not the player");
    assert!(!a.record().expect("over").rated);
    // Black is back (its first-move time starts then) and does not move: that is a no-show.
    let mut b = restart(&log, r, RoomSettings::default()).room;
    b.on_reconnect(B, r + 5000);
    assert_eq!(b.next_deadline(), Some(r + 5000 + 30000 + CAP));
    let out2 = b.tick(r + 35000 + CAP);
    assert_eq!(result(&b), (GS::Aborted, ER::NoShow));
    assert_eq!(out2.conduct, [(4, IncidentKind::NoShow)]);
    // A game that was never restored keeps its conduct incident.
    let mut c = rebuild(&log, RoomSettings::default());
    assert_eq!(c.tick(T0 + 2000 + 30000 + CAP).conduct, [(4, IncidentKind::NoShow)]);
}

// ---- First-move timer of a game restored before its second ply ---------------------------------

/// A 3+2 game at ply 1 (Black to move), and its journal.
fn at_ply1() -> Vec<JournalRecord> {
    let mut room = GameRoom::new(
        spec(626262, "3+2", 180000, 2000, 3, 4),
        RoomSettings::default(),
        FakeRules::boxed(Script::default()),
    )
    .expect("room");
    let mut log = vec![room.created_record()];
    let m = move_of(&room, 0);
    log.extend(room.on_move(W, &m, T0 + 2000).journal);
    log
}

fn at_ply0() -> Vec<JournalRecord> {
    let room = GameRoom::new(
        spec(636363, "3+2", 180000, 2000, 3, 4),
        RoomSettings::default(),
        FakeRules::boxed(Script::default()),
    )
    .expect("room");
    vec![room.created_record()]
}

#[test]
fn a_restored_game_at_ply_1_gives_the_side_to_move_its_whole_first_move_time_from_its_first_reconnection() {
    let r = T0 + 12000;
    let mut s = restart(&at_ply1(), r, RoomSettings::default());
    assert_eq!(s.out.journal[0].payload[2], 1, "the recovery record asks for the restart");
    assert_eq!(s.room.away_since_recovery, [true, true]);
    let o = s.room.on_reconnect(W, r + 1000);
    s.run(o);
    assert_eq!(s.room.away_since_recovery, [false, true]);
    // The hold ends without Black: its first-move timer runs from then on.
    let o = s.room.tick(r + HOLD);
    assert_eq!(s.run(o).clock_started, Some(B));
    assert_eq!(s.room.next_deadline(), Some(r + HOLD + FM));
    // Black is back 2 s before that deadline, within its recovery grace.
    let back = r + HOLD + FIRST - 2000;
    let o = s.room.on_reconnect(B, back);
    assert_eq!(s.run(o).clock_started, Some(B), "the host sends White a snapshot with the new timer");
    assert_eq!((s.room.clock().turn_start(), s.room.next_deadline()), (back, Some(back + FM)));
    assert_eq!(i64::from(s.room.snapshot(B, back).first_move_ms), FIRST);
    assert_eq!(i64::from(s.room.snapshot(W, back + 1000).first_move_ms), FIRST - 1000);
    assert_eq!(state(&s.replay(), back), state(&s.room, back), "the journal replays to the same room");
    let o = s.room.tick(r + HOLD + FM);
    assert!(!s.run(o).ended, "still running at the deadline it had before");
    // Leaving and coming back again does not restart it a second time.
    let o = s.room.on_disconnect(B, back + 5000);
    s.run(o);
    let o = s.room.on_reconnect(B, back + 10000);
    assert_eq!(s.run(o).clock_started, None);
    assert_eq!((s.room.clock().turn_start(), s.room.next_deadline()), (back, Some(back + FM)));
    assert_eq!(i64::from(s.room.snapshot(B, back + 10000).first_move_ms), FIRST - 10000);
    assert_eq!(state(&s.replay(), back + 10000), state(&s.room, back + 10000));
    // Back with its whole first-move time and no move: a no-show.
    let o = s.room.tick(back + FM - 1);
    assert!(!s.run(o).ended);
    let end = s.room.tick(back + FM);
    let end = s.run(end);
    assert_eq!(result(&s.room), (GS::Aborted, ER::NoShow));
    assert_eq!(end.conduct, [(4, IncidentKind::NoShow)]);
}

#[test]
fn a_restored_game_at_ply_0_restarts_the_timer_of_the_side_to_move_at_its_first_reconnection_also_after_the_first_move()
 {
    let r = T0 + 10000;
    let mut s = restart(&at_ply0(), r, RoomSettings::default());
    let o = s.room.tick(r + HOLD);
    s.run(o);
    // White (to move) is back 25 s after the hold: 30 s from then.
    let w_back = r + HOLD + 25000;
    let o = s.room.on_reconnect(W, w_back);
    assert_eq!(s.run(o).clock_started, Some(W));
    assert_eq!(s.room.next_deadline(), Some(w_back + FM));
    // White moves; Black, still away since the recovery, has its first-move time from that move...
    let w_move = w_back + 1000;
    let m = move_of(&s.room, 0);
    let o = s.room.on_move(W, &m, w_move);
    assert!(s.run(o).moved);
    assert_eq!(s.room.next_deadline(), Some(w_move + FM));
    // ...and the whole of it again from its first reconnection (before its recovery grace ends).
    let b_back = w_move + 25000;
    assert!(b_back < r + 90000);
    let o = s.room.on_reconnect(B, b_back);
    assert_eq!(s.run(o).clock_started, Some(B));
    assert_eq!(s.room.next_deadline(), Some(b_back + FM));
    assert_eq!(state(&s.replay(), b_back), state(&s.room, b_back));
    let m = move_of(&s.room, 0);
    let o = s.room.on_move(B, &m, b_back + 29000);
    assert!(s.run(o).moved);
    assert_eq!(s.room.away_since_recovery, [false, false]);
    // A player who is not to move gets no restart from its reconnection.
    let mut other = restart(&at_ply0(), r, RoomSettings::default()).room;
    other.tick(r + HOLD);
    assert_eq!(other.on_reconnect(B, r + HOLD + 1000).clock_started, None);
    assert_eq!((other.clock().turn_start(), other.away_since_recovery), (r + HOLD, [true, false]));
    // A game restored at ply 2 or later sets nothing: its clock rules are the ones of the hold.
    let (j, _) = journaled(RoomSettings::default());
    let late = restart(&j.log, r + 20000, RoomSettings::default()).room;
    assert_eq!(late.away_since_recovery, [false, false]);
}

#[test]
fn the_first_move_restart_survives_journal_state_a_journal_snapshot_and_a_replay_and_a_second_recovery_gives_it_again()
 {
    let r = T0 + 12000;
    let at = r + HOLD + 5000;
    let live = || {
        let mut g = restart(&at_ply1(), r, RoomSettings::default());
        let o = g.room.on_reconnect(W, r + 1000);
        g.run(o);
        let o = g.room.tick(r + HOLD);
        g.run(o);
        g
    };
    let g = live();
    let st = g.room.journal_state();
    let cp = st
        .iter()
        .find(|r| r.kind == kind::EVENT && r.payload[0] == EventKind::Checkpoint as u8)
        .expect("checkpoint");
    assert_eq!(cp.payload[2] & 0x18, 16, "Black's away bit in the checkpoint presence byte");
    for (label, records) in
        [("journal_state", st.clone()), ("journal_snapshot", vec![g.room.journal_snapshot(at)])]
    {
        let mut reference = live();
        let mut copy = rebuild(&records, RoomSettings::default());
        assert_eq!(copy.away_since_recovery, [false, true], "{label}");
        assert_eq!(state(&copy, at), state(&reference.room, at), "{label}");
        let a = reference.room.on_reconnect(B, at + 10000);
        let a = reference.run(a);
        let b = copy.on_reconnect(B, at + 10000);
        assert_eq!(b.journal, a.journal, "{label}");
        assert_eq!((b.clock_started, copy.next_deadline()), (Some(B), Some(at + 10000 + FM)), "{label}");
        assert_eq!(state(&copy, at + 10000), state(&reference.room, at + 10000), "{label}");
    }
    // Restored at ply 0, both still away at the snapshot: both bits, and White (to move) restarts.
    let mut zero = restart(&at_ply0(), r, RoomSettings::default()).room;
    zero.tick(r + HOLD);
    let zs = zero.journal_snapshot(at);
    let zcp = rebuild(std::slice::from_ref(&zs), RoomSettings::default())
        .journal_state()
        .into_iter()
        .find(|r| r.kind == kind::EVENT)
        .expect("checkpoint");
    assert_eq!(zcp.payload[2] & 0x18, 0x18);
    let mut zc = rebuild(std::slice::from_ref(&zs), RoomSettings::default());
    assert_eq!(zc.away_since_recovery, [true, true]);
    assert_eq!(
        (zc.on_reconnect(W, at + 1000).clock_started, zc.next_deadline()),
        (Some(W), Some(at + 1000 + FM))
    );
    // Black came back and left again before the snapshot: no restart after it either.
    let mut h = live();
    let o = h.room.on_reconnect(B, at);
    h.run(o);
    let o = h.room.on_disconnect(B, at + 2000);
    h.run(o);
    let snap = h.room.journal_snapshot(at + 3000);
    let mut copy = rebuild(std::slice::from_ref(&snap), RoomSettings::default());
    assert_eq!(copy.away_since_recovery, [false, false]);
    assert_eq!((copy.on_reconnect(B, at + 4000).clock_started, copy.next_deadline()), (None, Some(at + FM)));
    // A second crash: the new recovery gives both players their first reconnection again.
    let r2 = at + 20000;
    let mut second = restart(std::slice::from_ref(&snap), r2, RoomSettings::default());
    assert_eq!(second.room.away_since_recovery, [true, true]);
    second.room.tick(r2 + HOLD);
    assert_eq!(second.room.on_reconnect(B, r2 + HOLD + 10000).clock_started, Some(B));
    assert_eq!(second.room.next_deadline(), Some(r2 + HOLD + 10000 + FM));
}

#[test]
fn without_the_restart_flag_or_the_away_bits_a_journal_replays_without_the_first_move_restart() {
    let r = T0 + 12000;
    let back = r + HOLD + FIRST - 2000;
    let mut s = restart(&at_ply1(), r, RoomSettings::default());
    let o = s.room.on_reconnect(W, r + 1000);
    s.run(o);
    let o = s.room.tick(r + HOLD);
    s.run(o);
    let o = s.room.on_reconnect(B, back);
    s.run(o);
    let is_event = |r: &JournalRecord, k: EventKind| r.kind == kind::EVENT && r.payload[0] == k as u8;
    let edit = |records: &[JournalRecord], flag: bool, bits: bool| -> Vec<JournalRecord> {
        records
            .iter()
            .cloned()
            .map(|mut r| {
                if flag && is_event(&r, EventKind::Recovered) {
                    r.payload[2] = 0;
                }
                if bits && is_event(&r, EventKind::Checkpoint) {
                    r.payload[2] &= 7;
                }
                r
            })
            .collect()
    };
    assert_eq!(s.replay().clock().turn_start(), back, "with the restart");
    for (label, old) in [("both", edit(&s.log, true, true)), ("checkpoint", edit(&s.log, false, true))] {
        let a = rebuild(&old, RoomSettings::default());
        assert_eq!(
            (a.clock().turn_start(), a.next_deadline(), a.away_since_recovery),
            (r + HOLD, Some(r + HOLD + FM), [false, false]),
            "{label}"
        );
    }
    // No hold: no checkpoint in between, so the recovered record alone decides.
    let none = settings(&[("RECOVERY_CLOCK_HOLD_MS", "0")]);
    let mut z = restart(&at_ply1(), r, none);
    let o = z.room.on_reconnect(B, r + 20000);
    z.run(o);
    assert_eq!(rebuild(&z.log, none).clock().turn_start(), r + 20000);
    assert_eq!(rebuild(&edit(&z.log, true, false), none).clock().turn_start(), r);
}

#[test]
fn the_first_move_after_a_recovery_counts_think_ms_from_the_previous_move() {
    let (g, t, _) = bullet();
    let r = t + 30000;
    let at = r + 4000;
    let since = at - g.room.plies[21].at; // the client's turn began with Black's last move
    let mut a = restart(&g.log, r, RoomSettings::default()).room;
    a.on_reconnect(W, r + 3000);
    let m = move_of(&a, (since - 50) as u32);
    let ok = a.on_move(W, &m, at);
    assert!(ok.moved);
    assert_eq!(ok.anomaly, None);
    assert_eq!(a.plies[22].spent, 1000, "charged from the reconnection");
    let mut b = restart(&g.log, r, RoomSettings::default()).room;
    b.on_reconnect(W, r + 3000);
    let m = move_of(&b, (since + 101) as u32);
    let bad = b.on_move(W, &m, at);
    assert!(bad.moved);
    assert_eq!(
        bad.anomaly.map(|a| a.kind),
        Some("clock_implausible"),
        "longer than the time since the previous move"
    );
}
