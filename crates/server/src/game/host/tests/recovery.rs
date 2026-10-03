//! Recovery at the host level, ported from the reference `game.host` suite (crash, unreplayable
//! journal, game ids) and from the host part of `game.recovery` (the held clock seen by the
//! opponent, compaction snapshots and a second crash) on a real journal.

use scacelith_protocol::{Color, EndReason as ER, GameEventKind as EV, GameStatus as GS};

use super::{
    B, Ep, JournalAt, Opts, Rig, T0, TempDir, W, copy_dir, game_event, journal_options, new_game, resign,
    result, snapshot,
};
use crate::clock::ManualClock;
use crate::events::NewGame;
use crate::game::room::journal_tests::state;
use crate::game::room::{GameRoom, RecordKind};
use crate::game::testing::{FakeRules, Script};
use crate::journal::{Journal, JournalOptions};

/// `RECOVERY_CLOCK_HOLD_MS`.
const HOLD: i64 = 20000;
/// `FIRST_MOVE_TIMEOUT_MS`.
const FIRST: u32 = 30000;
/// The deadline of a first move: the timeout plus the margin of a flag.
const FM: i64 = FIRST as i64 + 150;
/// `RECOVERY_GRACE_MS`.
const RECOVERY_GRACE: i64 = 90000;

/// Every flushed batch goes to its own segment, so compaction starts at once.
fn small_segments(o: &mut JournalOptions) {
    o.segment_bytes = 1;
    o.compact_segments = 1;
}

#[tokio::test]
async fn recovery_after_a_crash_running_games_restored_ended_but_uncommitted_games_committed() {
    let mut a = Rig::new().await;
    a.store.fail_next(1000); // the database is down: nothing gets committed
    let running = a.new_game(1, 2);
    let done = a.new_game(3, 4);
    for _ in 0..5 {
        a.play(running, 700);
    }
    for _ in 0..4 {
        a.play(done, 300);
    }
    a.send(4, resign(done, 2), None);
    let t = a.t() + 100;
    assert_eq!(a.poll(t).await, Some(false));
    assert!(a.store.batches().is_empty());
    let expected = a.room(done).record().expect("a finished game");
    let black_ms = a.room(running).clock().ms(B);
    // Crash: a new process with the same journal, an hour later.
    let restart = a.t() + 3_600_000;
    let (dir, _temp) = a.crash().await;
    let mut b = Rig::with(Opts { journal: JournalAt::Dir(dir), t: restart, ..Opts::default() }).await;
    assert_eq!(b.shard.recover(), 2);
    let room = b.room(running);
    assert_eq!(room.ply(), 5);
    assert!(!room.is_connected(W) && !room.is_connected(B));
    assert_eq!(b.shard.active_game_of(1), Some(running));
    assert_eq!(b.events.recovered(), [(running, 1, 2)]);
    // The ended game is committed as it was.
    let t = b.t() + 50;
    assert_eq!(b.poll(t).await, Some(true));
    assert_eq!(b.store.batches()[0], [expected]);
    assert!(b.journal_committed(done));
    // Black, the side to move, comes back 2 s later: its clock was held until then (nothing
    // charged) and starts at the reconnection, from its journaled value.
    assert_eq!(b.room(running).next_deadline(), Some(restart + HOLD), "the clock hold ends first");
    let eb = Ep::new(5, 2);
    b.set(restart + 2000);
    b.shard.attach(running, 2, eb.endpoint());
    let s = snapshot(eb.last());
    assert_eq!(
        (s.running, i64::from(s.black_ms), s.black_connected, s.white_connected),
        (Color::Black, black_ms, true, false)
    );
    assert_eq!(i64::from(b.room(running).snapshot(B, restart + 3500).black_ms), black_ms - 1500);
    // White never comes back: abandonment after the recovery grace, not the normal 18 s.
    assert_eq!(i64::from(s.grace_ms), RECOVERY_GRACE - 2000);
    b.run_timers(restart + 18000);
    assert!(!b.room(running).is_over());
    b.run_timers(restart + RECOVERY_GRACE);
    assert_eq!(result(b.room(running)), (GS::BlackWins, ER::Abandonment));
}

#[tokio::test]
async fn a_journal_that_cannot_be_replayed_ends_server_aborted_an_unreadable_one_is_dropped() {
    let mut a = Rig::new().await;
    let (g1, g2) = (a.new_game(1, 2), a.new_game(3, 4));
    for _ in 0..4 {
        a.play(g1, 100);
        a.play(g2, 100);
    }
    // A copy of the journal with two defects.
    let mut seen = 0;
    let mut records = Vec::new();
    for (game, rec) in &a.shard.journal_log {
        let mut rec = rec.clone();
        if *game == g1 {
            if seen == 3 {
                rec.payload[2..4].fill(0); // a move the rules refuse (from and to the same square)
            }
            seen += 1;
        }
        if *game == g2 && rec.kind == RecordKind::Created {
            rec.payload = b"not a created record".to_vec();
        }
        records.push((*game, rec));
    }
    let restart = a.t() + 1000;
    let copy = TempDir::new("defects");
    let journal = Journal::open(journal_options(copy.path(), super::SHARD, &a.clock)).await.expect("journal");
    for (game, rec) in &records {
        journal.append(rec.kind, *game, &rec.payload, rec.at as f64).expect("appended");
    }
    journal.flush().await.expect("written");
    journal.close().await.expect("closed");
    let mut b =
        Rig::with(Opts { journal: JournalAt::Dir(copy.path().to_owned()), t: restart, ..Opts::default() })
            .await;
    assert_eq!(b.shard.recover(), 1);
    let r1 = b.room(g1);
    assert_eq!((result(r1), r1.ply()), ((GS::Aborted, ER::ServerAborted), 2));
    assert!(b.shard.room(g2).is_none());
    assert!(b.journal_committed(g2), "dropped");
    assert_eq!(b.shard.counters().aborted, 1);
    let t = b.t() + 50;
    assert_eq!(b.poll(t).await, Some(true));
    assert_eq!(b.store.committed_ids(), [g1]);
    assert!(!b.store.batches()[0][0].rated);
    b.crash().await;
}

#[tokio::test]
async fn game_ids_are_never_given_again_after_a_restart_with_the_clock_behind() {
    // A restart in the same millisecond, then two minutes behind: after every game of the journal.
    let mut a = Rig::new().await;
    let (g1, g2) = (a.new_game(1, 2), a.new_game(3, 4));
    let t = a.t();
    let (dir, _temp) = a.crash().await;
    for dt in [0, -120_000] {
        let mut b =
            Rig::with(Opts { journal: JournalAt::Dir(dir.clone()), t: t + dt, ..Opts::default() }).await;
        b.shard.recover();
        assert!(b.new_game(5, 6) > g1.max(g2), "{dt}");
        b.crash().await;
    }
    // The database's largest id (a committed game is no longer in the journal).
    let mut c =
        Rig::with(Opts { journal: JournalAt::None, t: t - 120_000, last_game_id: g2, ..Opts::default() })
            .await;
    assert!(c.new_game(5, 6) > g2);
    // A clock ahead of the seeds gives the same ids as without them.
    let mut d =
        Rig::with(Opts { journal: JournalAt::None, t: t + 1000, last_game_id: g2, ..Opts::default() }).await;
    let mut e = Rig::with(Opts { journal: JournalAt::None, t: t + 1000, ..Opts::default() }).await;
    assert_eq!(d.new_game(5, 6), e.new_game(5, 6));
}

#[tokio::test]
async fn the_opponent_sees_the_held_clock_stopped_then_running_once_it_starts() {
    for how in ["hold", "reconnection"] {
        let mut a = Rig::new().await;
        let id = a.new_game(1, 2);
        for _ in 0..4 {
            a.play(id, 1000); // White to move
        }
        let w_ms = a.room(id).clock().ms(W);
        let restart = a.t() + 10000;
        let (dir, _temp) = a.crash().await;
        let mut b = Rig::with(Opts { journal: JournalAt::Dir(dir), t: restart, ..Opts::default() }).await;
        assert_eq!(b.shard.recover(), 1);
        let eb = Ep::new(2, 2);
        b.set(restart + 1000);
        b.shard.attach(id, 2, eb.endpoint());
        let s0 = snapshot(eb.last());
        assert_eq!(
            (s0.running, i64::from(s0.white_ms), s0.white_connected),
            (Color::None, w_ms, false),
            "{how}"
        );
        eb.clear();
        if how == "hold" {
            b.set(restart + HOLD);
            b.run_timers(restart + HOLD + 10);
            let m = eb.msgs();
            assert_eq!(m.len(), 1, "{how}");
            let s = snapshot(m[0].clone());
            assert_eq!((s.running, i64::from(s.white_ms)), (Color::White, w_ms), "{how}");
        } else {
            let ew = Ep::new(1, 1);
            b.set(restart + 5000);
            b.shard.attach(id, 1, ew.endpoint());
            let m = eb.msgs();
            assert_eq!(m.len(), 2, "{how}");
            assert_eq!(game_event(&m[0]).0, EV::PlayerReconnected);
            let s = snapshot(m[1].clone());
            assert_eq!(
                (s.running, i64::from(s.white_ms), s.white_connected),
                (Color::White, w_ms, true),
                "{how}"
            );
            let sw = snapshot(ew.last());
            assert_eq!((sw.running, i64::from(sw.white_ms)), (Color::White, w_ms), "{how}");
            b.run_timers(restart + HOLD + 10);
            assert_eq!(eb.sent().len(), 2, "nothing more at the end of the hold");
        }
        assert!(!state(b.room(id), b.t()).clock_held, "{how}");
        b.crash().await;
    }
}

#[tokio::test]
async fn a_compaction_snapshot_and_a_crash_keep_the_first_move_restart_and_the_opponent_sees_the_new_timer() {
    let dir = TempDir::new("first");
    let copy = TempDir::new("first-copy");
    let opts = |dir: &TempDir, t: i64| Opts {
        shard: 0,
        journal: JournalAt::Dir(dir.path().to_owned()),
        journal_tweak: small_segments,
        t,
        ..Opts::default()
    };
    // 1. White plays its first move; then the process dies.
    let mut h0 = Rig::with(opts(&dir, T0)).await;
    let id = h0.new_game(1, 2);
    h0.play(id, 1500);
    let r1 = h0.t() + 30000;
    h0.crash().await;

    // 2. Restart at R1: White comes back, Black does not; the hold ends; the game is compacted.
    let mut h1 = Rig::with(opts(&dir, r1)).await;
    assert_eq!(h1.shard.recover(), 1);
    let ew = Ep::new(1, 1);
    h1.set(r1 + 2000);
    h1.shard.attach(id, 1, ew.endpoint());
    h1.set(r1 + HOLD);
    h1.run_timers(r1 + HOLD + 10);
    assert_eq!(h1.room(id).next_deadline(), Some(r1 + HOLD + FM));
    h1.journal().flush().await.expect("written");
    h1.set(r1 + HOLD + 1000);
    assert_eq!(h1.shard.compact(h1.t()), 1, "the journal asked for a snapshot of the game");
    h1.journal().flush().await.expect("written");

    // 3. A crash at S, after the hold: the copy replays to the same room, Black still away.
    let s = r1 + HOLD + 3000;
    h1.set(s);
    copy_dir(&dir.path().join("shard-0"), &copy.path().join("shard-0"));
    let mut h2 = Rig::with(opts(&copy, s)).await;
    let records = h2.journal().recover().get(&id).cloned().expect("the game's records");
    assert_eq!(records[0].kind, RecordKind::Snapshot, "the game starts from its snapshot");
    let replay = GameRoom::from_journal(&records, h2.settings(), FakeRules::boxed(Script::default()), true)
        .expect("replayed");
    assert_eq!(state(&replay, s), state(h1.room(id), s), "the room as it was at the crash");
    assert_eq!(state(&replay, s).away_since_recovery, [false, true]);

    // 4. On the live host, Black is back 26 s after the hold: White is sent the new timer.
    ew.clear();
    h1.set(r1 + HOLD + 26000);
    let eb = Ep::new(2, 2);
    h1.shard.attach(id, 2, eb.endpoint());
    let m = ew.msgs();
    assert_eq!(m.len(), 2);
    assert_eq!(game_event(&m[0]).0, EV::PlayerReconnected);
    assert_eq!(snapshot(m[1].clone()).first_move_ms, FIRST);
    assert_eq!(snapshot(eb.last()).first_move_ms, FIRST);
    assert_eq!(h1.room(id).next_deadline(), Some(h1.t() + FM));

    // 5. The second restart, from the copy at R2: Black's first reconnection after the new hold
    //    gives it the whole first-move time again.
    let r2 = s + 30000;
    h2.set(r2);
    assert_eq!(h2.shard.recover(), 1);
    assert_eq!(state(h2.room(id), r2).away_since_recovery, [true, true]);
    h2.set(r2 + HOLD);
    h2.run_timers(r2 + HOLD + 10);
    h2.set(r2 + HOLD + 15000);
    let eb2 = Ep::new(2, 2);
    h2.shard.attach(id, 2, eb2.endpoint());
    assert_eq!(snapshot(eb2.last()).first_move_ms, FIRST);
    assert_eq!(h2.room(id).next_deadline(), Some(h2.t() + FM));
    h2.crash().await;
    h1.crash().await;
}

#[tokio::test]
async fn a_snapshot_taken_while_the_side_to_move_is_away_then_a_second_crash_keeps_the_hold_graces_and_clocks()
 {
    let dir = TempDir::new("hold");
    let copy = TempDir::new("hold-copy");
    let opts = |dir: &TempDir, t: i64| Opts {
        shard: 0,
        journal: JournalAt::Dir(dir.path().to_owned()),
        journal_tweak: small_segments,
        t,
        ..Opts::default()
    };
    // 1. A 5+3 game at ply 7 (Black to move); then the process dies.
    let mut h0 = Rig::with(opts(&dir, T0)).await;
    let id = h0.create(NewGame { category: "5+3".into(), base_ms: 300_000, inc_ms: 3000, ..new_game(1, 2) });
    for _ in 0..7 {
        h0.play(id, 1500);
    }
    let clocks = [h0.room(id).clock().ms(W), h0.room(id).clock().ms(B)];
    let r1 = h0.t() + 60000;
    h0.crash().await;

    // 2. Restart at R1: White comes back, Black does not; the game is compacted meanwhile.
    let mut h1 = Rig::with(opts(&dir, r1)).await;
    assert_eq!(h1.shard.recover(), 1);
    let st = state(h1.room(id), r1);
    assert_eq!((st.clock_held, st.turn_start), (true, r1 + HOLD));
    h1.set(r1 + 2000);
    h1.shard.attach(id, 1, Ep::new(1, 1).endpoint());
    h1.journal().flush().await.expect("written");
    h1.set(r1 + 4000);
    assert_eq!(h1.shard.compact(h1.t()), 1, "the journal asked for a snapshot of the game");
    h1.journal().flush().await.expect("written");

    // 3. A crash at S, during the hold: a copy of the directory is reopened.
    let s = r1 + 6000;
    h1.set(s);
    copy_dir(&dir.path().join("shard-0"), &copy.path().join("shard-0"));
    let mut h2 = Rig::with(opts(&copy, s)).await;
    let records = h2.journal().recover().get(&id).cloned().expect("the game's records");
    assert_eq!(records[0].kind, RecordKind::Snapshot, "the game starts from its snapshot");
    let replay = GameRoom::from_journal(&records, h2.settings(), FakeRules::boxed(Script::default()), true)
        .expect("replayed");
    assert_eq!(state(&replay, s), state(h1.room(id), s), "the room as it was at the crash");
    let st = state(&replay, s);
    assert_eq!((st.clock_held, st.turn_start, st.clock_ms), (true, r1 + HOLD, clocks));
    assert_eq!(
        (st.connected, st.disconnected_at, st.disconnect_grace),
        ([true, false], [r1, r1], [RECOVERY_GRACE, RECOVERY_GRACE])
    );
    assert_eq!(i64::from(replay.snapshot(W, s).grace_ms), RECOVERY_GRACE - 6000);

    // 4. The second restart, at R2: a new hold and a new grace from R2, the journaled clocks.
    let r2 = s + 30000;
    h2.set(r2);
    assert_eq!(h2.shard.recover(), 1);
    let st = state(h2.room(id), r2);
    assert_eq!((st.clock_held, st.turn_start, st.clock_ms), (true, r2 + HOLD, clocks));
    assert_eq!(
        (st.connected, st.disconnected_at, st.disconnect_grace),
        ([false, false], [r2, r2], [RECOVERY_GRACE, RECOVERY_GRACE])
    );
    assert_eq!(h2.room(id).next_deadline(), Some(r2 + HOLD));
    // Black comes back: its clock starts then, with the time it had before the first crash.
    h2.set(r2 + 5000);
    let eb = Ep::new(2, 2);
    h2.shard.attach(id, 2, eb.endpoint());
    let snap = snapshot(eb.last());
    assert_eq!(
        (snap.running, i64::from(snap.black_ms), i64::from(snap.grace_ms)),
        (Color::Black, clocks[1], RECOVERY_GRACE - 5000)
    );
    h2.crash().await;
    h1.crash().await;
}

#[tokio::test]
async fn the_journal_of_a_crash_copy_is_read_with_the_records_the_host_appended() {
    // The host appends integer times; the journal hands them back as such.
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    h.play(id, 1234);
    let appended: Vec<(RecordKind, i64)> = h.shard.journal_log.iter().map(|(_, r)| (r.kind, r.at)).collect();
    let (dir, _temp) = h.crash().await;
    let clock = ManualClock::new(T0 as f64, T0);
    let j = Journal::open(journal_options(&dir, super::SHARD, &clock)).await.expect("journal");
    let read: Vec<(RecordKind, i64)> =
        j.recover().get(&id).expect("the game").iter().map(|r| (r.kind, r.at as i64)).collect();
    assert_eq!(read, appended);
    j.close().await.expect("closed");
}
