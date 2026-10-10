//! The closure tests of audit A10's lifecycle residue and of N05: a whole server whose game host
//! stops handling its inbox while a player in a game closes and opens connections over and over.
//! The attach and detach messages of that player's game stay at one earlier connection's (they
//! used to pile up, two per reconnection), the later connections wait for their attach, and once
//! the host runs again the last connection gets the game back: the opponent sees the player leave
//! and come back once per connection the host handled, and the game goes on to its result. A
//! request of a connection whose attach waits (N05) never binds it at the host: a `Resync` waits
//! for the attach's snapshot, any other request is refused with `RateLimited`, and a connection
//! that closes before its attach, whether the host caught up before the close or after it, is
//! never announced back.

use scacelith_chess::ChessGame;
use scacelith_protocol::{
    Color, EndReason, ErrorCode, GameEventKind, GameSnapshot, GameStatus, Resign, Resync, ServerMsg,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use super::{Client, TestServer};
use crate::game::host::{HostHandle, LINK_PENDING_MAX};
use crate::ids::{GameId, UserId};
use crate::net::ws::tests::frame;

/// Reconnections of the player while its game's host is held.
const ROUNDS: usize = 50;

/// Closes the connection the way a client does: a close frame, the server's in return, then the
/// socket (the session ends once it is gone, and detaches its games).
async fn close(mut c: Client) {
    c.io.write_all(&frame(0x8, &1000u16.to_be_bytes())).await.expect("sent");
    assert_eq!(c.closed().await, 1000);
}

/// Waits (10 s at most) until `done` holds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let limit = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        assert!(std::time::Instant::now() < limit, "timed out waiting for {what}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// White's presence events read by `c` until `n` of them: `true` for `PlayerReconnected`,
/// `false` for `PlayerDisconnected`.
async fn white_presence(c: &mut Client, n: usize) -> Vec<bool> {
    let mut seen = Vec::new();
    while seen.len() < n {
        if let ServerMsg::GameEvent(e) = c.until("GameEvent").await
            && e.color == Color::White
        {
            match e.kind {
                GameEventKind::PlayerReconnected => seen.push(true),
                GameEventKind::PlayerDisconnected => seen.push(false),
                _ => {}
            }
        }
    }
    seen
}

/// Sends `Resync` on `c`, and returns White's presence events read until the snapshot that
/// answers it (`true` for `PlayerReconnected`), and that snapshot.
async fn presence_to_snapshot(c: &mut Client, game: GameId) -> (Vec<bool>, GameSnapshot) {
    let seq = c.next_seq();
    c.send(Resync { seq, game }).await;
    let mut seen = Vec::new();
    loop {
        match c.next().await {
            ServerMsg::GameEvent(e) if e.color == Color::White => match e.kind {
                GameEventKind::PlayerReconnected => seen.push(true),
                GameEventKind::PlayerDisconnected => seen.push(false),
                _ => {}
            },
            ServerMsg::GameSnapshot(s) if s.game == game => return (seen, s),
            _ => {}
        }
    }
}

/// A held game and the third connection of its White, whose attach waits.
struct Deferred {
    third: Client,
    black: Client,
    game: GameId,
    user: UserId,
    token: String,
    host: HostHandle,
    release: oneshot::Sender<()>,
}

/// A held game (White played e2e4) whose White closed two connections, the second one attached
/// while the host was held: the third connection waits for its attach.
async fn third_connection_deferred(server: &TestServer) -> Deferred {
    let (white, mut black, game) = server.pair().await;
    let mut first = white.c;
    let mut board = ChessGame::default();
    first.play(game, 0, &mut board, "e2e4").await;
    black.c.until("MoveMade").await;
    let user = server.instance().auth().validate_token(&white.token).await.unwrap().unwrap().user_id;
    let host = server.instance().hosts().get(game).expect("the game's host").clone();
    let release = host.hold().await;
    close(first).await;
    until("the first detach", || host.links(game, user).0 == 1).await;
    let (second, _) = server.login(&white.token).await;
    until("the second attach", || host.links(game, user).0 == 2).await;
    close(second).await;
    until("the second detach", || host.links(game, user).0 == 3).await;
    let before = host.attaches_deferred();
    let (third, w) = server.login(&white.token).await;
    assert_eq!(w.active_game, game);
    until("the third attach deferred", || host.attaches_deferred() > before).await;
    Deferred { third, black: black.c, game, user, token: white.token, host, release }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_of_a_connection_whose_attach_waits_never_binds_it() {
    // N05, as the audit's probe: the third connection sends a Resync (and a Resign), then closes,
    // and only then does the host run again.
    let mut server = TestServer::start(&[("ABUSE_EXEMPT", "127.0.0.1")]).await;
    let Deferred { mut third, mut black, game, user, token, host, release } =
        third_connection_deferred(&server).await;
    let resync = third.next_seq();
    third.send(Resync { seq: resync, game }).await;
    let resign = third.next_seq();
    third.send(Resign { seq: resign, game }).await;
    // The Resync waits for the attach's snapshot; the Resign is refused, as by a host far behind.
    let ServerMsg::Error(e) = third.next().await else { panic!("the Resign's refusal first") };
    assert_eq!((e.r#ref, e.code, e.fatal, e.game), (resign, ErrorCode::RateLimited, false, game));
    assert_eq!(host.links(game, user).0, 3, "nothing posted for the third connection");
    close(third).await;
    assert_eq!(host.links(game, user).0, 3, "no detach for a game the connection never attached");

    drop(release);
    let (seen, s) = presence_to_snapshot(&mut black, game).await;
    assert_eq!(seen, [false, true, false], "White left, came back (second connection), left");
    assert!(!s.white_connected, "no closed connection of White is bound");
    assert_eq!(s.status, GameStatus::Ongoing, "the refused Resign ended nothing");
    until("nothing waits", || host.links(game, user) == (0, 0)).await;

    // White comes back for good: the opponent sees it once.
    let (mut back, _) = server.login(&token).await;
    let ServerMsg::GameSnapshot(s) = back.until("GameSnapshot").await else { unreachable!() };
    assert_eq!((s.game, s.you, s.moves.len()), (game, Color::White, 1));
    let (seen, s) = presence_to_snapshot(&mut black, game).await;
    assert_eq!(seen, [true]);
    assert!(s.white_connected);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_whose_attach_waits_closing_after_the_host_caught_up_is_not_bound() {
    // N05, the host running again between the Resync and the close: the connection closes as soon
    // as the host has handled the earlier connections (its next attach retry most likely not come
    // yet). Whether the retry came first (attached, then detached) or not (never attached), White
    // ends away and the opponent last saw it leave.
    let mut server = TestServer::start(&[("ABUSE_EXEMPT", "127.0.0.1")]).await;
    let Deferred { mut third, mut black, game, user, host, release, .. } =
        third_connection_deferred(&server).await;
    let seq = third.next_seq();
    third.send(Resync { seq, game }).await;
    drop(release);
    until("the host caught up", || host.links(game, user).0 == 0).await;
    close(third).await;
    let (seen, s) = presence_to_snapshot(&mut black, game).await;
    assert!(
        seen == [false, true, false] || seen == [false, true, false, true, false],
        "White's presence: {seen:?}"
    );
    assert!(!s.white_connected, "no closed connection of White is bound");
    until("nothing waits", || host.links(game, user) == (0, 0)).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_reconnecting_over_and_over_to_a_held_host_leaves_its_inbox_bounded() {
    let mut server = TestServer::start(&[("ABUSE_EXEMPT", "127.0.0.1")]).await;
    let (white, mut black, game) = server.pair().await;
    let mut c = white.c;
    let mut board = ChessGame::default();
    c.play(game, 0, &mut board, "e2e4").await;
    black.c.until("MoveMade").await;
    let user = server.instance().auth().validate_token(&white.token).await.unwrap().unwrap().user_id;
    let host = server.instance().hosts().get(game).expect("the game's host").clone();
    let release = host.hold().await;

    // The host does not run: White's connection closes and a new one opens, ROUNDS times.
    let mut most = 0;
    for round in 0..ROUNDS {
        close(c).await;
        // The next connection opens once the closed one's detach is posted (the first two
        // connections, whose attach was posted), so that the host gets them in this order: the
        // opponent then sees White leave and come back once per connection the host handled.
        let posted = if round == 0 { 1 } else { LINK_PENDING_MAX + 1 };
        until("the detach posted", || host.links(game, user).0 == posted).await;
        let before = host.attaches_deferred();
        let (next, w) = server.login(&white.token).await;
        assert_eq!((w.active_game, w.user_id), (game, user));
        c = next;
        if round > 0 {
            // The first connection's detach and the second one's attach and detach wait: the
            // attach of this one waits in its connection.
            until("the attach deferred", || host.attaches_deferred() > before).await;
            // Its Resync waits with its attach (N05): nothing more in the inbox, nor bound.
            let seq = c.next_seq();
            c.send(Resync { seq, game }).await;
        }
        let (waiting, games) = host.links(game, user);
        assert!(
            waiting <= LINK_PENDING_MAX + 1 && games <= 1,
            "round {round}: {waiting} messages, {games} games"
        );
        most = most.max(host.inbox().0);
    }
    assert_eq!(host.links(game, user), (LINK_PENDING_MAX + 1, 1));
    assert!(most <= LINK_PENDING_MAX + 4, "the inbox held {most} messages at most");

    // The host runs again: the open connection's attach goes through, the game comes back.
    drop(release);
    let ServerMsg::GameSnapshot(s) = c.until("GameSnapshot").await else { unreachable!() };
    assert_eq!((s.game, s.you, s.moves.len()), (game, Color::White, 1));
    until("nothing waits", || host.links(game, user) == (0, 0)).await;
    assert_eq!(white_presence(&mut black.c, 4).await, [false, true, false, true], "White is back");

    // The game goes on from the last connection, to its result.
    black.c.play(game, 1, &mut board, "e7e5").await;
    let ServerMsg::MoveMade(m) = c.until("MoveMade").await else { unreachable!() };
    assert_eq!((m.game, m.ply), (game, 1));
    let seq = black.c.next_seq();
    black.c.send(Resign { seq, game }).await;
    for client in [&mut c, &mut black.c] {
        let ServerMsg::GameEnd(end) = client.until("GameEnd").await else { unreachable!() };
        assert_eq!((end.game, end.status, end.reason), (game, GameStatus::WhiteWins, EndReason::Resignation));
    }
    let store = server.instance().store().clone();
    let mut stored = None;
    for _ in 0..200 {
        stored = store.games().by_id(game).await.expect("read");
        if stored.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let stored = stored.expect("the game is stored");
    assert_eq!((stored.summary.ply_count, stored.summary.status), (2, GameStatus::WhiteWins as u8));
    server.stop().await;
}
