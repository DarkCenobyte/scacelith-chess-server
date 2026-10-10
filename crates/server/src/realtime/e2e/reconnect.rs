//! The closure test of audit A10's lifecycle residue: a whole server whose game host stops
//! handling its inbox while a player in a game closes and opens connections over and over. The
//! attach and detach messages of that player's game stay at one earlier connection's (they used
//! to pile up, two per reconnection), the later connections wait for their attach, and once the
//! host runs again the last connection gets the game back: the opponent sees the player leave
//! and come back once per connection the host handled, and the game goes on to its result.

use scacelith_chess::ChessGame;
use scacelith_protocol::{Color, EndReason, GameEventKind, GameStatus, Resign, ServerMsg};
use tokio::io::AsyncWriteExt;

use super::{Client, TestServer};
use crate::game::host::LINK_PENDING_MAX;
use crate::net::ws::tests::frame;

/// Reconnections of the player while its game's host is held.
const ROUNDS: usize = 50;

/// Closes the connection the way a client does (a close frame) and waits for the server's: by
/// then the session ended and detached its games.
async fn close(c: &mut Client) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_reconnecting_over_and_over_to_a_held_host_leaves_its_inbox_bounded() {
    let mut server = TestServer::start(&[("ABUSE_EXEMPT", "127.0.0.1")]).await;
    let (white, mut black, game) = server.pair().await;
    let mut c = white.c;
    let mut board = ChessGame::default();
    c.play(game, 0, &mut board, "e2e4").await;
    black.c.until("MoveMade").await;
    let host = server.instance().hosts().get(game).expect("the game's host").clone();
    let release = host.hold().await;

    // The host does not run: White's connection closes and a new one opens, ROUNDS times.
    let mut user = 0;
    let mut most = 0;
    for round in 0..ROUNDS {
        close(&mut c).await;
        let before = host.attaches_deferred();
        let (next, w) = server.login(&white.token).await;
        assert_eq!(w.active_game, game);
        (c, user) = (next, w.user_id);
        if round > 0 {
            // The first connection's detach and the second one's attach and detach wait: the
            // attach of this one waits in its connection.
            until("the attach deferred", || host.attaches_deferred() > before).await;
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
