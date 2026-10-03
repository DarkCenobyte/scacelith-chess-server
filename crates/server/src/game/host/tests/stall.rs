//! Stall credit, ported from the stall tests of the reference `game.host` suite: after the host
//! could not run for longer than `GAME_STALL_MIN_MS`, the requests that waited are handled before
//! the timers, as arrived when the stall began (`GAME_STALL_CREDIT_MAX_MS` earlier at most), and a
//! deadline that fell during the stall records no no-show.

use scacelith_protocol::{
    ClientMsg, Color, EndReason as ER, ErrorCode as EC, GameStatus as GS, ServerMsg, close,
};

use tokio::sync::mpsc;

use super::{Ep, Opts, Rig, T0, ended_at, move_msg, rematch, result, resync, snapshot};
use crate::config::test_config;
use crate::events::IncidentKind;
use crate::game::host::{Msg, beat};
use crate::ids::GameId;

/// A game where White's clock runs (both first moves played), with its connections and its flag
/// deadline.
async fn running(o: Opts) -> (Rig, GameId, Ep, Ep, i64) {
    let mut h = Rig::with(o).await;
    let id = h.new_game(1, 2);
    let (ew, eb) = (Ep::new(10, 1), Ep::new(20, 2));
    h.shard.attach(id, 1, ew.endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    h.play(id, 1000);
    h.play(id, 1000);
    let deadline = h.deadline(id);
    (h, id, ew, eb, deadline)
}

#[tokio::test]
async fn the_configuration_of_the_stall_detection() {
    let c = test_config(&[]).expect("configuration");
    assert_eq!((c.auto_press_clock, c.game_stall_min_ms, c.game_stall_credit_max_ms), (true, 30, 5000));
    assert!(test_config(&[("GAME_STALL_MIN_MS", "4")]).is_err());
    let mut h = Rig::with(Opts { config: vec![("GAME_STALL_MIN_MS", "100")], ..Opts::default() }).await;
    h.beat(T0);
    assert_eq!(h.credit(T0 + 105), 0, "under 10 + 100 ms: not a stall");
    assert_eq!(h.credit(T0 + 2000), 1990);
    h.shard.settings.stall_credit_max_ms = 0;
    assert_eq!(h.credit(T0 + 2000), 0, "a credit of 0 gives nothing back");
}

#[tokio::test]
async fn a_move_read_after_a_stall_beats_the_flag_that_fell_during_it() {
    let (mut h, id, ew, eb, deadline) = running(Opts::default()).await;
    assert!(!h.beat(deadline - 1000), "the last beat before the stall");
    assert!(h.beat(deadline + 2000), "3 s without a beat: detected, the timers wait for the inbox");
    assert!(!h.room(id).is_over());
    h.move_now(id, Some(&ew));
    match eb.last() {
        ServerMsg::MoveMade(m) => assert_eq!(m.server_time, h.t() as f64),
        other => panic!("not a MoveMade: {other:?}"),
    }
    h.drain(); // the timers run after the stall
    assert!(!h.room(id).is_over());
    assert_eq!(h.deadline(id), h.t() + 180000 + 150, "Black's clock starts at the MoveMade");
    let c = h.shard.counters();
    assert_eq!((c.stalls, c.stall_credit_ms), (1, (h.t() - (deadline - 1000 + 10)) as u64));
    assert_eq!(h.credit(h.t() + 5), 0, "no credit once the timers ran");
    let t = h.t();
    assert!(h.shard.stall_during(deadline - 1500, t));
    assert!(!h.shard.stall_during(t + 1, t + 5));
    // The handles see the same.
    let shared = h.shard.shared();
    assert!(shared.stall_during((deadline - 1500) as f64, t));
    assert!(!shared.stall_during((t + 1) as f64, t + 5));
}

#[tokio::test]
async fn without_a_move_the_flag_falls_when_the_timers_run_after_the_stall_and_at_once_without_a_stall() {
    let (mut h, id, _, _, deadline) = running(Opts::default()).await;
    h.beat(deadline - 1000);
    h.beat(deadline + 2000);
    assert!(!h.room(id).is_over());
    h.drain();
    assert_eq!((result(h.room(id)).1, ended_at(h.room(id))), (ER::Timeout, deadline + 2000));
    // The control case: the same move without a detected stall flags.
    let (mut h, id, ew, _, deadline) = running(Opts::default()).await;
    h.set(deadline + 2000);
    h.move_now(id, Some(&ew));
    assert_eq!(result(h.room(id)).1, ER::Timeout);
    assert!(ew.msgs().iter().any(|m| matches!(m, ServerMsg::MoveRejected(r) if r.code == EC::FlagFell)));
}

#[tokio::test]
async fn no_credit_under_the_stall_minimum_a_capped_one_and_one_from_a_late_beat() {
    let (mut h, _, _, _, deadline) = running(Opts::default()).await;
    let t0 = deadline - 1000;
    h.beat(t0);
    assert!(!h.beat(t0 + 10 + 20), "20 ms late: no stall");
    assert_eq!(h.credit(t0 + 30), 0);
    h.beat(t0 + 40);
    assert!(h.beat(t0 + 40 + 10 + 31));
    assert_eq!(h.credit(t0 + 81), 81 - 50);
    h.drain();

    let (mut h, id, ew, _, deadline) =
        running(Opts { config: vec![("GAME_STALL_CREDIT_MAX_MS", "1000")], ..Opts::default() }).await;
    h.beat(deadline - 3000);
    h.beat(deadline + 1500);
    assert_eq!(h.credit(h.t()), 1000);
    h.move_now(id, Some(&ew));
    assert_eq!(result(h.room(id)).1, ER::Timeout, "a stall longer than the cap is not all given back");
    h.drain();

    // The beat has not come yet: a request handled while it is late gets the credit.
    let (mut h, id, ew, eb, deadline) = running(Opts::default()).await;
    h.beat(deadline - 200);
    h.set(deadline + 300);
    assert_eq!(h.credit(h.t()), 490);
    h.move_now(id, Some(&ew));
    assert!(matches!(eb.last(), ServerMsg::MoveMade(_)));
}

#[tokio::test]
async fn the_opponents_close_or_resync_read_in_the_same_drain_does_not_flag_a_queued_move() {
    let (mut h, id, ew, eb, deadline) = running(Opts::default()).await;
    h.beat(deadline - 1000);
    h.beat(deadline + 2000);
    h.send(2, resync(id, 7), Some(&eb));
    assert_eq!(snapshot(eb.last()).status, GS::Ongoing);
    h.shard.detach(id, 2, Some(20));
    assert!(!h.room(id).is_over());
    h.move_now(id, Some(&ew));
    assert_eq!(h.room(id).ply(), 3);
    let eb2 = Ep::new(11, 2);
    h.shard.attach(id, 2, eb2.endpoint());
    assert!(matches!(eb2.last(), ServerMsg::GameSnapshot(_)));
    h.drain();
    assert!(!h.room(id).is_over());
}

#[tokio::test]
async fn a_first_move_timeout_that_fell_during_a_stall_aborts_without_a_no_show_and_lateness_is_measured() {
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    h.shard.attach(id, 1, Ep::new(1, 1).endpoint());
    h.beat(T0 + 29000);
    assert!(h.beat(T0 + 30000 + 150 + 1000));
    h.drain();
    assert_eq!(result(h.room(id)), (GS::Aborted, ER::NoShow));
    assert!(h.events.conduct().is_empty());
    assert_eq!(h.shard.counters().timer_firings, 1);
    // Without a stall the no-show is recorded.
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    h.run_timers(T0 + 30000 + 150);
    assert_eq!(result(h.room(id)).1, ER::NoShow);
    assert_eq!(h.events.conduct(), [(1, IncidentKind::NoShow)]);
}

#[tokio::test]
async fn a_move_that_reached_its_socket_while_the_backlog_of_a_stall_was_read_beats_the_flag() {
    for moves in [true, false] {
        let (mut h, id, ew, eb, deadline) = running(Opts::default()).await;
        h.beat(deadline - 1010); // the last beat before the stall
        assert!(h.beat(deadline - 400), "a 600 ms stall, detected");
        // Reading the backlog of the stall takes 600 ms. White's move reaches its socket at
        // deadline - 100, after that read looked at it: it waits for the next one.
        h.set(deadline + 200);
        h.drain(); // the timers due by the detecting beat
        assert!(!h.room(id).is_over(), "the flag falls after the deadline the drain covered");
        assert!(h.beat(deadline + 202), "that drain was a stall of its own");
        if moves {
            h.set(deadline + 203);
            assert_eq!(h.credit(h.t()), 593);
            h.move_now(id, Some(&ew));
            assert!(matches!(eb.last(), ServerMsg::MoveMade(_)));
        }
        h.drain();
        let room = h.room(id);
        if moves {
            assert_eq!((room.is_over(), room.ply()), (false, 3));
        } else {
            assert_eq!((result(room).1, ended_at(room)), (ER::Timeout, deadline + 202));
        }
    }
}

#[tokio::test]
async fn the_forfeit_of_a_certain_cheat_read_after_a_stall_takes_the_arrival_of_the_request_that_revealed_it()
{
    for plies in [2, 0] {
        let mut h = Rig::new().await;
        let id = h.new_game(1, 2);
        let (ew, eb) = (Ep::new(10, 1), Ep::new(20, 2));
        h.shard.attach(id, 1, ew.endpoint());
        h.shard.attach(id, 2, eb.endpoint());
        for _ in 0..plies {
            h.play(id, 1000);
        }
        let dl = h.deadline(id); // White's flag or first-move deadline
        h.beat(dl - 1000);
        h.beat(dl + 2000); // a 3 s stall, the deadline 1 s into it
        let arrival = h.shard.stall_start(h.t());
        // Black's out-of-turn move waited in its socket from before White's deadline.
        let msg = ClientMsg::Move(move_msg(h.room(id), 31));
        h.send(2, msg, Some(&eb));
        let room = h.room(id);
        assert_eq!(
            (result(room), ended_at(room)),
            ((GS::WhiteWins, ER::Forfeit), arrival),
            "after {plies} plies"
        );
        assert_eq!(h.events.sanctions(), [(2, id, "out_of_turn")]);
        assert_eq!(eb.closed().map(|c| c.code), Some(close::CHEAT_DETECTED));
        h.drain();
        assert!(h.events.conduct().is_empty(), "after {plies} plies");
    }
}

#[tokio::test]
async fn a_first_move_timeout_that_fell_during_a_stall_longer_than_the_credit_records_no_no_show() {
    for first in ["resync", "move", "close", "attach", "rematch", "forfeit"] {
        let mut h = Rig::new().await;
        let id = h.new_game(1, 2);
        let (ew, eb) = (Ep::new(1, 1), Ep::new(2, 2));
        h.shard.attach(id, 1, ew.endpoint());
        h.shard.attach(id, 2, eb.endpoint());
        let dl = h.deadline(id); // White's first-move deadline
        h.beat(dl - 2010);
        h.beat(dl + 18000); // a 20 s stall, the deadline 2 s into it
        assert_eq!(h.credit(h.t()), 5000, "the request counts as arrived 3 s after the deadline");
        match first {
            "resync" => h.send(2, resync(id, 7), Some(&eb)),
            "move" => h.move_now(id, Some(&ew)),
            "close" => assert!(h.shard.detach(id, 2, Some(2))),
            "attach" => assert!(h.shard.attach(id, 2, Ep::new(3, 2).endpoint())),
            "rematch" => h.send(2, rematch(id, 7, true), Some(&eb)),
            _ => assert!(h.shard.forfeit_user(2), "a sanction from elsewhere"),
        }
        assert_eq!(result(h.room(id)), (GS::Aborted, ER::NoShow), "{first}");
        h.drain();
        assert!(h.events.conduct().is_empty(), "{first}");
    }
    // A deadline due before the stall began (at the beat that did not come) records one all the same.
    let mut h = Rig::new().await;
    let id = h.new_game(1, 2);
    let eb = Ep::new(2, 2);
    h.shard.attach(id, 1, Ep::new(1, 1).endpoint());
    h.shard.attach(id, 2, eb.endpoint());
    let dl = h.deadline(id);
    h.beat(dl - 5);
    h.beat(dl + 18000);
    h.send(2, resync(id, 7), Some(&eb));
    assert_eq!(result(h.room(id)).1, ER::NoShow);
    assert_eq!(h.events.conduct(), [(1, IncidentKind::NoShow)]);
    assert_eq!(snapshot(eb.last()).running, Color::None);
    h.drain();
}

#[tokio::test]
async fn a_beat_handles_the_requests_already_queued_before_its_timers() {
    let (mut h, id, ew, eb, deadline) = running(Opts::default()).await;
    assert!(!h.beat(deadline - 5));
    // White's move is read 3 ms before its flag deadline and still waits in the inbox when the
    // next beat comes due, 2 ms after the deadline (a busy actor: both are ready at once).
    let (tx, mut inbox) = mpsc::unbounded_channel();
    let msg = ClientMsg::Move(move_msg(h.room(id), 5));
    let read_at = (deadline - 3) as f64;
    tx.send(Msg::Client { user: 1, msg, ep: ew.endpoint(), recv_at: read_at }).expect("inbox open");
    h.set(deadline + 2);
    assert!(beat(&mut h.shard, &mut inbox).await.is_continue());
    assert!(matches!(eb.last(), ServerMsg::MoveMade(_)), "the move arrived in time: {:?}", eb.last());
    let room = h.room(id);
    assert_eq!((room.is_over(), room.ply()), (false, 3));
    assert_eq!(h.deadline(id), deadline + 2 + 180000 + 150, "Black's clock starts at the MoveMade");
    assert_eq!(h.shard.counters().stalls, 0, "no stall: an ordinary beat");
    // Without a request, the same beat flags.
    let (mut h, id, _, _, deadline) = running(Opts::default()).await;
    assert!(!h.beat(deadline - 5));
    let (_tx, mut inbox) = mpsc::unbounded_channel();
    h.set(deadline + 2);
    assert!(beat(&mut h.shard, &mut inbox).await.is_continue());
    assert_eq!((result(h.room(id)).1, ended_at(h.room(id))), (ER::Timeout, deadline + 2));
}
