//! End-to-end multiplayer scenarios against the real server (the `scacelith-server` binary with
//! two shards, TLS, SQLite), driven through the client SDK: pairing, special moves, duplicates,
//! out of turn, illegal moves, the cheat sanction, reconnection, resignation, time out, clock
//! tampering, simultaneous results, protocol abuse, the gesture relay, the clock press across a
//! restart and a crash.
//!
//! Port of the Node.js `test/integration/multiplayer.test.js`, adapted to protocol v1: a message
//! with an unassigned type byte of the client's range is `Malformed` (4001), one of the server's
//! range a forgery (`CheatDetected`, 4302), a wrong protocol version in the `Hello`
//! `UnsupportedProtocol` (4002). Each test runs its own server.

#[macro_use]
mod support;

use std::time::Duration;

use scacelith_client::ws::Session;
use scacelith_client::ws::frame::{Frame, OP_TEXT};
use scacelith_client::{ClientError, ConnectOptions, Connection};
use scacelith_protocol::{
    ClientGesture, ClientPing, Color, EndReason, ErrorCode, GameEnd, GameEventKind, GameSnapshot, GameStatus,
    Hello, Message, ServerMsg, close, gesture_flag, move_flag,
};
use support::*;

/// The settings of the Node.js suite: two shards, a long first-move deadline, fast commits.
async fn server() -> TestServer {
    TestServer::options()
        .workers(2)
        .env("FIRST_MOVE_TIMEOUT_MS", "20000")
        .env("DB_COMMIT_MS", "20")
        .start()
        .await
}

/// The `GameEnd` of game `id` received by `p` since `since`.
async fn end_of(p: &mut Player, id: u64, since: usize) -> GameEnd {
    wait_msg!(p.client, since, Duration::from_secs(20), ServerMsg::GameEnd(e) if e.game == id => e.clone())
}

/// The last snapshot of game `id` that `p` received.
fn snapshot_of(p: &Player, id: u64) -> GameSnapshot {
    p.client
        .history()
        .iter()
        .rev()
        .find_map(|m| match m {
            ServerMsg::GameSnapshot(s) if s.game == id => Some(s.clone()),
            _ => None,
        })
        .expect("a snapshot of the game")
}

/// How many `MoveMade` of game `id` and ply `ply` `p` received.
fn move_made_count(p: &Player, id: u64, ply: u16) -> usize {
    p.client
        .history()
        .iter()
        .filter(|m| matches!(m, ServerMsg::MoveMade(mm) if mm.game == id && mm.ply == ply))
        .count()
}

/// A refused connection: the error code of the fatal `Error`, or the one its close code stands
/// for.
fn refusal(err: &ClientError) -> Option<ErrorCode> {
    match err {
        ClientError::Refused { code, .. } => Some(*code),
        other => other.close_info().and_then(|c| match c.code {
            close::BANNED => Some(ErrorCode::Banned),
            close::UNAUTHORIZED => Some(ErrorCode::Unauthorized),
            close::UNSUPPORTED_PROTOCOL => Some(ErrorCode::UnsupportedProtocol),
            _ => None,
        }),
    }
}

#[tokio::test]
async fn rated_queue_castling_en_passant_promotion_resignation_ratings_committed_and_public() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["ann", "bob"]).await;
    let (id, a_white, _, _) = queue_game(&mut a, &mut b, "3+2").await;
    let (w, bl) = if a_white { (&mut a, &mut b) } else { (&mut b, &mut a) };
    let mut t = Table::new(id);
    let (mw, mb) = (w.client.mark(), bl.client.mark());
    t.play_all(w, bl, &["e2e4", "a7a6", "e4e5", "d7d5"]).await;
    let ep = t.play(w, bl, "e5d6").await;
    assert_ne!(ep.flags & move_flag::EN_PASSANT, 0, "en passant flag");
    t.play_all(w, bl, &["g8f6", "g1f3", "b8c6", "f1e2", "a6a5"]).await;
    let castle = t.play(w, bl, "e1g1").await;
    assert_ne!(castle.flags & move_flag::CASTLE_KING, 0, "castle flag");
    t.play_all(w, bl, &["a5a4", "d6c7", "a4a3"]).await;
    let promo = t.play(w, bl, "c7d8q").await;
    assert_ne!(promo.flags & move_flag::PROMOTION, 0, "promotion flag");

    let since = bl.client.mark();
    bl.client.resign(id);
    let end_b = end_of(bl, id, since).await;
    let end_w = end_of(w, id, mw).await;
    assert_eq!((end_b.status, end_b.reason), (GameStatus::WhiteWins, EndReason::Resignation));
    assert_eq!((end_w.status, end_w.reason), (end_b.status, end_b.reason));
    let ru = wait_msg!(w.client, mw, ServerMsg::RatingUpdate(r) if r.game == id => r.clone());
    assert_eq!(ru.category, "3+2");
    // Two newcomers: a game lost by a player who has not scored yet counts for neither rating
    // (FIDE's zero score), and the working rating stays until a first rating.
    let change = |c: &scacelith_protocol::RatingChange| (c.before, c.after, c.games, c.provisional);
    assert_eq!(change(&ru.white), (1500, 1500, 1, true));
    assert_eq!(change(&ru.black), (1500, 1500, 1, true));
    wait_msg!(bl.client, mb, ServerMsg::RatingUpdate(r) if r.game == id => ());

    // The public record: the moves as played, the result, both names.
    let api = srv.api();
    let rec = api.request("GET", &format!("/games/{id}"), None, None).await.expect("an answer");
    let text = String::from_utf8_lossy(&rec.body).into_owned();
    assert_eq!(rec.status, 200, "{text}");
    assert!(
        text.contains("e5d6") && text.contains("e1g1") && text.contains("c7d8q"),
        "{}",
        &text[..text.len().min(400)]
    );
    assert!(text.contains(w.name()) && text.contains(bl.name()), "both names: {text}");
    let prof = api.request("GET", &format!("/players/{}", w.name()), None, None).await.expect("an answer");
    assert_eq!(prof.status, 200);
    assert!(String::from_utf8_lossy(&prof.body).contains("3+2"));
}

#[tokio::test]
async fn checkmate_ends_the_game_for_both_players_with_the_same_result() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["cat", "dan"]).await;
    let id = challenge_game(&mut a, &mut b, 300, 0, false).await;
    let mut t = Table::new(id);
    let (ma, mb) = (a.client.mark(), b.client.mark());
    t.play_all(&mut a, &mut b, &["f2f3", "e7e5", "g2g4"]).await;
    t.send(&b, "d8h4");
    let ea = end_of(&mut a, id, ma).await;
    let eb = end_of(&mut b, id, mb).await;
    assert_eq!((ea.status, ea.reason), (GameStatus::BlackWins, EndReason::Checkmate));
    assert_eq!((eb.status, eb.reason), (GameStatus::BlackWins, EndReason::Checkmate));
}

#[tokio::test]
async fn a_resent_move_is_idempotent_a_stale_ply_and_a_desynchronised_move_are_refused_not_relayed() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["eve", "fay"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    let mut t = Table::new(id);
    t.play(&mut a, &mut b, "e2e4").await;

    // Duplicate: White resends its ply-0 move; nothing new is played.
    let ma = a.client.mark();
    a.client.send_move(id, 0, mv("e2e4"), Table::new(id).hash(), 500, false);
    let again = wait_msg!(a.client, ma, ServerMsg::MoveMade(m) if m.game == id && m.ply == 0 => m.clone());
    assert_eq!(again.r#move, mv("e2e4"));

    // Stale ply (Black answers for ply 0): refused with StalePly.
    let s1 = b.client.mark();
    b.client.send_move(id, 0, mv("e7e5"), t.hash(), 0, false);
    let rej = wait_msg!(b.client, s1, ServerMsg::MoveRejected(r) => r.code);
    assert_eq!(rej, ErrorCode::StalePly);

    // Desync (not the player's turn and a position it does not have): refused, connection kept,
    // and a snapshot to repair the client's state.
    let s2 = a.client.mark();
    a.client.send_move(id, 1, mv("d2d4"), 12345, 500, false);
    let rej2 = wait_msg!(a.client, s2, ServerMsg::MoveRejected(r) => r.code);
    assert_eq!(rej2, ErrorCode::Desync);
    wait_msg!(a.client, s2, ServerMsg::GameSnapshot(s) if s.game == id => ());

    // The opponent saw none of it: one MoveMade per ply.
    t.play(&mut a, &mut b, "e7e5").await;
    assert_eq!(b.client.moves_of(id).len(), 2);
    assert_eq!(move_made_count(&b, id, 0), 1, "the resent move is not relayed");
    assert!(a.client.is_open() && b.client.is_open());
}

#[tokio::test]
async fn an_illegal_move_in_a_synchronised_position_is_a_certain_cheat_forfeit_close_4302_ban() {
    let srv = server().await;
    let [mut cheater, mut honest] = players(&srv, ["gil", "hal"]).await;
    let id = challenge_game(&mut cheater, &mut honest, 180, 2, false).await;
    let mut t = Table::new(id);
    t.play_all(&mut cheater, &mut honest, &["e2e4", "e7e5"]).await;
    let mh = honest.client.mark();
    t.send(&cheater, "e1e3"); // the king cannot move two squares up
    let closed = cheater.client.closed(Duration::from_secs(5)).await;
    assert_eq!(closed.code, close::CHEAT_DETECTED);
    let end = end_of(&mut honest, id, mh).await;
    assert_eq!((end.status, end.reason), (GameStatus::BlackWins, EndReason::Forfeit));
    // The move was never relayed.
    assert_eq!(honest.client.moves_of(id).len(), 2);
    // Banned: the next connection is refused.
    banned(&srv, cheater.token()).await;
}

/// Waits until a connection with `token` is refused as banned. The ban is written a moment after
/// the sanction's close: a connection in between is still accepted (it sees the forfeited game).
async fn banned(srv: &TestServer, token: &str) {
    eventually(Duration::from_secs(10), "the ban", || async {
        match connect(srv, token).await {
            Ok(mut early) => {
                early.close().await;
                None
            }
            Err(err) => {
                assert_eq!(refusal(&err), Some(ErrorCode::Banned), "{err}");
                Some(())
            }
        }
    })
    .await;
}

#[tokio::test]
async fn out_of_turn_with_the_right_position_is_a_certain_cheat_too() {
    let srv = server().await;
    let [mut cheater, mut honest] = players(&srv, ["ida", "jon"]).await;
    let id = challenge_game(&mut honest, &mut cheater, 180, 2, false).await; // the cheater is Black
    let mut t = Table::new(id);
    t.play_all(&mut honest, &mut cheater, &["d2d4", "d7d5"]).await;
    let mh = honest.client.mark();
    t.send(&cheater, "e7e6"); // White to move
    assert_eq!(cheater.client.closed(Duration::from_secs(5)).await.code, close::CHEAT_DETECTED);
    let end = end_of(&mut honest, id, mh).await;
    assert_eq!((end.status, end.reason), (GameStatus::WhiteWins, EndReason::Forfeit));
    assert_eq!(honest.client.moves_of(id).len(), 2, "the move was never relayed");
    banned(&srv, cheater.token()).await;
}

#[tokio::test]
async fn connection_lost_mid_game_the_opponent_is_told_the_player_reconnects_and_resumes() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["kim", "lou"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    let mut t = Table::new(id);
    t.play_all(&mut a, &mut b, &["e2e4", "c7c5", "g1f3"]).await;

    let mb = b.client.mark();
    a.client.conn().abort(); // abrupt loss, no close frame
    let ev = wait_msg!(b.client, mb, ServerMsg::GameEvent(e) if e.kind == GameEventKind::PlayerDisconnected => e.clone());
    assert_eq!(ev.color, Color::White);
    assert!(ev.arg >= 15_000, "grace {}", ev.arg);

    // Black keeps playing while White is away.
    let mb2 = b.client.mark();
    t.send(&b, "d7d6");
    wait_msg!(b.client, mb2, ServerMsg::MoveMade(m) if m.game == id && m.ply == 3 => ());
    t.apply("d7d6");

    // White comes back: Welcome names the game, a snapshot carries all 4 moves.
    let mut c2 = connect(&srv, a.token()).await.expect("connected again");
    assert_eq!(c2.welcome().active_game, id);
    let snap = wait_msg!(c2, 0, ServerMsg::GameSnapshot(s) if s.game == id => s.clone());
    assert_eq!(snap.moves.len(), 4);
    assert!(snap.white_connected);
    wait_msg!(b.client, mb2, ServerMsg::GameEvent(e) if e.kind == GameEventKind::PlayerReconnected => ());
    a.client = c2;
    t.play(&mut a, &mut b, "d2d4").await;
}

#[tokio::test]
async fn a_second_connection_replaces_the_first_4007_and_gets_the_game() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["max", "ned"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    let mut c2 = connect(&srv, a.token()).await.expect("a second connection");
    assert_eq!(a.client.closed(Duration::from_secs(5)).await.code, close::REPLACED);
    assert_eq!(c2.welcome().active_game, id);
    wait_msg!(c2, 0, ServerMsg::GameSnapshot(s) if s.game == id => ());
    a.client = c2;
    Table::new(id).play(&mut a, &mut b, "e2e4").await;
}

#[tokio::test]
async fn flag_fall_the_side_whose_clock_runs_out_loses_on_time_server_clock() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["oli", "pat"]).await;
    let id = challenge_game(&mut a, &mut b, 15, 0, false).await;
    let mut t = Table::new(id);
    let ma = a.client.mark();
    // White's clock starts with Black's first move.
    t.play_all(&mut a, &mut b, &["e2e4", "e7e5"]).await;
    // Nobody moves: 15 s later White's flag falls.
    let end =
        wait_msg!(a.client, ma, Duration::from_secs(30), ServerMsg::GameEnd(e) if e.game == id => e.clone());
    assert_eq!((end.status, end.reason), (GameStatus::BlackWins, EndReason::Timeout));
    assert_eq!(end.white_ms, 0);
}

#[tokio::test]
async fn a_forged_think_time_gains_nothing_the_server_charges_the_time_it_measured() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["quin", "rae"]).await;
    let id = challenge_game(&mut a, &mut b, 60, 0, false).await;
    let mut t = Table::new(id);
    t.play_all(&mut a, &mut b, &["e2e4", "e7e5"]).await;
    // White thinks for 1.5 s (the scenario, not a wait for a state), then claims 1 ms.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let made = t.play_with(&mut a, &mut b, "g1f3", 1).await;
    assert!(made.spent_ms >= 1000, "charged {} ms", made.spent_ms);
    assert!(made.white_ms <= 59_000, "white has {} ms", made.white_ms);
}

#[tokio::test]
async fn both_players_resign_at_the_same_moment_one_result_the_same_for_both() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["sam", "tia"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    let mut t = Table::new(id);
    t.play_all(&mut a, &mut b, &["e2e4", "e7e5"]).await;
    let (ma, mb) = (a.client.mark(), b.client.mark());
    let ra = a.client.resign(id);
    let rb = b.client.resign(id);
    let ea = end_of(&mut a, id, ma).await;
    let eb = end_of(&mut b, id, mb).await;
    assert_eq!(ea.reason, EndReason::Resignation);
    assert_eq!((ea.status, ea.reason, ea.gseq), (eb.status, eb.reason, eb.gseq));
    // The resignation handled second found the game over: an Error answers it.
    let refused_of = |seq: u32| {
        move |m: &ServerMsg| match m {
            ServerMsg::Error(e) if e.r#ref == seq => Some(e.code),
            _ => None,
        }
    };
    let late = tokio::select! {
        code = a.client.try_wait_for(ma, WAIT, refused_of(ra)) => code,
        code = b.client.try_wait_for(mb, WAIT, refused_of(rb)) => code,
    };
    assert!(
        matches!(late, Some(ErrorCode::GameOver | ErrorCode::NotInGame)),
        "the second resignation: {late:?}"
    );
    let ends = |p: &Player| {
        p.client.history().iter().filter(|m| matches!(m, ServerMsg::GameEnd(e) if e.game == id)).count()
    };
    assert_eq!((ends(&a), ends(&b)), (1, 1), "one GameEnd each");
    assert_eq!(snapshot_of(&a, id).game, id);
}

#[tokio::test]
async fn draw_offer_and_acceptance() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["uma", "vic"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    let mut t = Table::new(id);
    t.play_all(&mut a, &mut b, &["e2e4", "e7e5"]).await;
    let (ma, mb) = (a.client.mark(), b.client.mark());
    a.client.offer_draw(id);
    wait_msg!(b.client, mb, ServerMsg::GameEvent(e) if e.kind == GameEventKind::DrawOffered => ());
    b.client.answer_draw(id, true);
    let end = end_of(&mut a, id, ma).await;
    assert_eq!((end.status, end.reason), (GameStatus::Draw, EndReason::Agreement));
}

#[tokio::test]
async fn protocol_abuse_garbage_text_frames_and_floods_close_the_connection_the_server_stays_up() {
    let srv = server().await;
    let [acc, forger] = accounts(&srv, ["wes", "wil"]).await;

    // A type byte of the client's range that no message has: the message does not decode.
    let mut c = connect(&srv, &acc.token).await.expect("connected");
    c.conn().session().send_unsequenced(&[0x7E, 1, 2, 3]).expect("sent");
    assert_eq!(c.closed(Duration::from_secs(5)).await.code, close::MALFORMED);

    // The Node.js suite's garbage starts with 0xEE: a type byte of the server's range, a forged
    // message whatever its type (a certain cheat: closed 4302, then banned).
    let mut f = connect(&srv, &forger.token).await.expect("connected");
    f.conn().session().send_unsequenced(&[0xEE, 1, 2, 3]).expect("sent");
    assert_eq!(f.closed(Duration::from_secs(5)).await.code, close::CHEAT_DETECTED);
    banned(&srv, &forger.token).await;

    let mut c = connect(&srv, &acc.token).await.expect("connected");
    c.conn().session().send_frame(&Frame::new(OP_TEXT, &b"hello"[..]), true).expect("sent");
    assert_eq!(c.closed(Duration::from_secs(5)).await.code, close::UNSUPPORTED);

    let mut c = connect(&srv, &acc.token).await.expect("connected");
    for nonce in 0..400 {
        if c.conn().send(ClientPing { seq: 0, nonce }).is_err() {
            break; // already closed
        }
    }
    assert_eq!(c.closed(Duration::from_secs(10)).await.code, close::FLOOD);

    let info = acc.api.info().await.expect("the API still answers");
    assert!(info.is_object());
    let mut again = connect(&srv, &acc.token).await.expect("a new connection is accepted");
    again.close().await;
}

#[tokio::test]
async fn wrong_protocol_version_and_bad_token_are_refused_at_hello() {
    let srv = server().await;
    let acc = account(&srv, "xan").await;

    // The Hello of another protocol version.
    let opts = ConnectOptions::default();
    let mut session =
        Session::connect(&srv.endpoint(), &Connection::session_options(&opts)).await.expect("upgraded");
    let hello =
        Hello { seq: 1, proto: 2, minor: 0, caps: 0, client: "integration".into(), token: acc.token.clone() };
    session.send(&hello.to_vec().expect("encoded")).expect("sent");
    let first = tokio::time::timeout(WAIT, session.recv()).await.expect("an answer").expect("a message");
    match ServerMsg::decode(&first.payload) {
        Ok(Some(ServerMsg::Error(e))) => {
            assert_eq!((e.code, e.fatal), (ErrorCode::UnsupportedProtocol, true))
        }
        other => panic!("expected the UnsupportedProtocol error, got {other:?}"),
    }
    let closed = tokio::time::timeout(WAIT, session.wait_closed()).await.expect("closed");
    assert_eq!(closed.code, close::UNSUPPORTED_PROTOCOL);

    let err =
        connect(&srv, &format!("sct_{}", "A".repeat(43))).await.expect_err("an unknown token is refused");
    assert_eq!(refusal(&err), Some(ErrorCode::Unauthorized), "{err}");
}

#[tokio::test]
async fn gestures_are_relayed_live_to_the_opponent_only_never_echoed_and_moves_stay_in_step() {
    let srv = server().await;
    let [mut a, mut b] = players(&srv, ["gus", "ivy"]).await;
    let w = a.client.welcome();
    assert_eq!((w.gesture_rate, w.gesture_burst), (4, 8));
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    assert!(snapshot_of(&a, id).auto_press, "AUTO_PRESS_CLOCK default");
    let mut t = Table::new(id);
    let (ma, mb) = (a.client.mark(), b.client.mark());
    a.client.send(ClientGesture {
        seq: 0,
        game: id,
        ply: 0,
        touch: 12,
        aim: 28,
        placed: 0,
        flags: gesture_flag::GLANCE,
        yaw: -300,
        pitch: 100,
        lean: 20,
    });
    let got = wait_msg!(b.client, mb, ServerMsg::Gesture(g) if g.game == id => g.clone());
    assert_eq!(
        (got.ply, got.touch, got.aim, got.placed, got.flags, got.yaw, got.pitch, got.lean),
        (0, 12, 28, 0, gesture_flag::GLANCE, -300, 100, 20)
    );
    b.client.send(ClientGesture {
        seq: 0,
        game: id,
        ply: 0,
        touch: 64,
        aim: 64,
        placed: 0,
        flags: gesture_flag::SIDE,
        yaw: 450,
        pitch: 0,
        lean: 0,
    });
    let back = wait_msg!(a.client, ma, ServerMsg::Gesture(g) if g.game == id => g.clone());
    assert_eq!((back.yaw, back.flags, back.touch), (450, gesture_flag::SIDE, 64));
    t.play_all(&mut a, &mut b, &["e2e4", "e7e5"]).await;
    // An echo of White's gesture would have come before the answer to White's later move.
    let echoed = a.client.history()[ma..].iter().any(|m| matches!(m, ServerMsg::Gesture(g) if g.yaw == -300));
    assert!(!echoed, "never echoed to the sender");
    assert!(a.client.is_open() && b.client.is_open());
}

#[tokio::test]
async fn a_restored_game_keeps_its_clock_press_when_auto_press_clock_changed_across_the_restart() {
    let mut own = TestServer::options().workers(2).env("AUTO_PRESS_CLOCK", "false").start().await;
    let [mut a, mut b] = players(&own, ["jon", "kim"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    assert!(!snapshot_of(&a, id).auto_press);
    let mut t = Table::new(id);
    t.play_all(&mut a, &mut b, &["d2d4", "d7d5"]).await;
    let status = own.stop().await;
    assert!(status.success(), "graceful stop: {status}");

    let s2 =
        TestServer::options().workers(2).dir(own.dir.clone()).env("AUTO_PRESS_CLOCK", "true").start().await;
    a.reconnect(&s2).await;
    b.reconnect(&s2).await;
    let snap = wait_msg!(a.client, 0, ServerMsg::GameSnapshot(s) if s.game == id => s.clone());
    assert_eq!((snap.moves.len(), snap.auto_press), (2, false), "the journaled setting wins");
    wait_msg!(b.client, 0, ServerMsg::GameSnapshot(s) if s.game == id => ());
    let m = a.client.mark();
    a.client.resign(id);
    end_of(&mut a, id, m).await;

    // A new game follows the new setting.
    let [mut c, mut d] = players(&s2, ["lea", "mia"]).await;
    let g2 = challenge_game(&mut c, &mut d, 180, 2, false).await;
    assert!(snapshot_of(&c, g2).auto_press);
}

#[tokio::test]
async fn server_crash_sigkill_mid_game_after_the_restart_both_players_get_the_game_back() {
    let mut own = TestServer::options().workers(2).start().await;
    let [mut a, mut b] = players(&own, ["yul", "zed"]).await;
    let id = challenge_game(&mut a, &mut b, 180, 2, false).await;
    let mut t = Table::new(id);
    t.play_all(&mut a, &mut b, &["e2e4", "e7e5", "g1f3", "b8c6"]).await;
    // The moves are on disk (the journal's flush) before the crash.
    eventually(Duration::from_secs(10), "the journaled moves", || {
        let n = own.journaled_moves(id);
        async move { (n >= 4).then_some(()) }
    })
    .await;
    own.crash().await;

    let s2 = TestServer::options().workers(2).dir(own.dir.clone()).start().await;
    a.reconnect(&s2).await;
    b.reconnect(&s2).await;
    assert_eq!(a.client.welcome().active_game, id);
    let snap = wait_msg!(a.client, 0, ServerMsg::GameSnapshot(s) if s.game == id => s.clone());
    assert_eq!(snap.moves.len(), 4);
    wait_msg!(b.client, 0, ServerMsg::GameSnapshot(s) if s.game == id => ());
    t.play(&mut a, &mut b, "f1b5").await;
}
