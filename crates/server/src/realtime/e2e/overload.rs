//! The closure test of audit A10: a whole server whose database writer stalls behind a full
//! queue, under a mixed load (authenticated reads, writes, a reconnection, a game's end, a new
//! game asked for), then recovers. The queues stay within their bounds, the requests that would
//! write are refused with 503, the reads answer without waiting for the writer and renew no
//! session while it is backlogged, the game's commit is kept in the writer's critical reserve,
//! and once the writer is back every critical commit lands and each session due is renewed
//! once.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use scacelith_protocol::{self as proto, ErrorCode, GameStatus, QueueJoin, ServerMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{READ_TIMEOUT, TestServer};
use crate::auth::TouchOutcome;
use crate::game::host::{INBOX_BUSY, INBOX_MAX};
use crate::store::{StoreError, WRITE_BACKLOG_BUSY, WRITE_CRITICAL_RESERVE, WRITE_QUEUE_MAX};

/// An API request with `Connection: close`, a session token and a JSON body: the status and the
/// body.
async fn call(addr: SocketAddr, method: &str, path: &str, token: &str, body: Option<&str>) -> (u16, String) {
    let mut io = TcpStream::connect(addr).await.expect("connected");
    let body = body.unwrap_or("");
    let length = if method == "GET" { String::new() } else { format!("Content-Length: {}\r\n", body.len()) };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n{length}\r\n{body}"
    );
    io.write_all(request.as_bytes()).await.expect("request sent");
    let mut answer = Vec::new();
    tokio::time::timeout(READ_TIMEOUT, io.read_to_end(&mut answer)).await.expect("in time").expect("read");
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let status = answer.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = answer.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

/// `n` authenticated `GET /account/me` of each token at once: every status, and the longest
/// answer time.
async fn reads(addr: SocketAddr, tokens: &[String], n: usize) -> (Vec<u16>, Duration) {
    let calls: Vec<_> = tokens
        .iter()
        .flat_map(|t| std::iter::repeat_n(t.clone(), n))
        .map(|t| {
            tokio::spawn(async move {
                let t0 = Instant::now();
                let (status, _) = call(addr, "GET", "/api/v1/account/me", &t, None).await;
                (status, t0.elapsed())
            })
        })
        .collect();
    let mut statuses = Vec::new();
    let mut longest = Duration::ZERO;
    for c in calls {
        let (status, took) = c.await.expect("joined");
        statuses.push(status);
        longest = longest.max(took);
    }
    (statuses, longest)
}

/// Waits (10 s at most) until `done` holds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let limit = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_writer_under_mixed_load_keeps_its_bounds_refuses_writes_and_recovers_every_game() {
    let mut server = TestServer::start(&[("ABUSE_EXEMPT", "127.0.0.1")]).await;
    let addr = server.addr;
    let (mut white, black, game) = server.pair().await;
    let readers: Vec<String> = {
        let mut tokens = Vec::new();
        for name in ["carol", "dave", "erin", "frank"] {
            tokens.push(server.account(name).await);
        }
        tokens
    };
    let writer_token = server.account("gina").await;
    let store = server.instance().store().clone();
    // Every session was last used 10 minutes ago: each validation is due for a renewal.
    store
        .write(|db| {
            db.connection().execute("UPDATE sessions SET last_seen_at = last_seen_at - 600000", [])?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("sessions aged");
    let touches = server.instance().auth().session_touches();

    // ---- the database writer stalls (a disk that does not answer), its queue full ----------
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (running_tx, running) = tokio::sync::oneshot::channel();
    let blocker = tokio::spawn(store.write(move |_| {
        let _ = running_tx.send(());
        let _ = held.recv();
        Ok::<_, StoreError>(())
    }));
    running.await.expect("the writer runs the blocker");
    let fillers: Vec<_> =
        (0..WRITE_QUEUE_MAX).map(|_| tokio::spawn(store.write(|_| Ok::<_, StoreError>(())))).collect();
    assert_eq!(store.write_backlog(), WRITE_QUEUE_MAX);
    assert!(store.writes_backlogged());
    let refused = store.write(|_| Ok::<_, StoreError>(())).await.unwrap_err();
    assert_eq!(refused.kind(), crate::store::ErrorKind::Busy, "an ordinary job beyond the cap");

    // Authenticated reads answer without the writer, and renew no session meanwhile.
    let (statuses, longest) = reads(addr, &readers, 10).await;
    assert!(statuses.iter().all(|&s| s == 200), "{statuses:?}");
    assert!(longest < Duration::from_secs(2), "a read waited {longest:?}");
    assert_eq!(touches.get(TouchOutcome::Queued), 0, "no renewal queued under pressure");
    assert!(touches.get(TouchOutcome::Deferred) >= 4, "the renewals due were deferred");
    assert_eq!(store.write_backlog(), WRITE_QUEUE_MAX, "the reads queued nothing");

    // A request that would write is refused before it runs.
    let (status, body) = call(addr, "POST", "/api/v1/auth/logout", &writer_token, Some("{}")).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("server_busy"), "{body}");

    // Black reconnects: its game comes back, without a write.
    let (mut black2, welcome) = server.login(&black.token).await;
    assert_eq!(welcome.active_game, game);
    let ServerMsg::GameSnapshot(snap) = black2.until("GameSnapshot").await else { unreachable!() };
    assert_eq!(snap.game, game);

    // White resigns: the game ends at once, and its commit is a critical job of the writer,
    // queued beyond the full ordinary queue.
    let seq = white.c.next_seq();
    white.c.send(proto::Resign { seq, game }).await;
    for c in [&mut white.c, &mut black2] {
        let ServerMsg::GameEnd(end) = c.until("GameEnd").await else { unreachable!() };
        assert_eq!((end.game, end.status), (game, GameStatus::BlackWins));
    }
    until("the commit queued", || store.critical_write_backlog() >= 1).await;

    // Another player connects (a hello, without a write) and asks for a game: none is created
    // while the writer is that far behind.
    let (mut carol, _) = server.login(&readers[0]).await;
    let seq = carol.next_seq();
    carol.send(QueueJoin { seq, category: "5+0".into(), rated: true }).await;
    let ServerMsg::Error(e) = carol.until("Error").await else { unreachable!() };
    assert_eq!((e.r#ref, e.code), (seq, ErrorCode::RateLimited));

    // The bounds held through the load.
    let backlog = store.write_backlog();
    assert!(backlog <= WRITE_QUEUE_MAX + WRITE_CRITICAL_RESERVE, "{backlog} jobs");
    assert_eq!(backlog, WRITE_QUEUE_MAX + store.critical_write_backlog());
    for h in server.instance().hosts().handles() {
        let (messages, _) = h.inbox();
        assert!(messages < INBOX_BUSY.min(INBOX_MAX), "{messages} messages for shard {}", h.shard());
    }
    assert_eq!(touches.get(TouchOutcome::Queued), 0);

    // ---- the writer comes back ------------------------------------------------------------
    let t0 = Instant::now();
    release.send(()).expect("the writer waits");
    blocker.await.expect("joined").expect("written");
    for f in fillers {
        f.await.expect("joined").expect("written");
    }
    // The commit lands: the players get their rating update, the database holds the game.
    for c in [&mut white.c, &mut black2] {
        let ServerMsg::RatingUpdate(r) = c.until("RatingUpdate").await else { unreachable!() };
        assert_eq!(r.game, game);
    }
    until("the writer drained", || store.write_backlog() == 0).await;
    let recovery = t0.elapsed();
    assert!(recovery < Duration::from_secs(10), "recovered in {recovery:?}");
    assert!(!store.writes_backlogged() && WRITE_BACKLOG_BUSY > 0);
    let stored = store.games().by_id(game).await.expect("read").expect("the game is stored");
    assert_eq!(stored.summary.status, GameStatus::BlackWins as u8);

    // Each session due is renewed once, however many requests it makes at once.
    let (statuses, _) = reads(addr, &readers, 10).await;
    assert!(statuses.iter().all(|&s| s == 200), "{statuses:?}");
    assert_eq!(touches.get(TouchOutcome::Queued), readers.len() as u64, "one renewal per session");
    let (statuses, _) = reads(addr, &readers, 5).await;
    assert!(statuses.iter().all(|&s| s == 200), "{statuses:?}");
    assert_eq!(touches.get(TouchOutcome::Queued), readers.len() as u64, "renewed: nothing more is due");
    // Writes are taken again.
    let (status, body) = call(addr, "POST", "/api/v1/auth/logout", &writer_token, Some("{}")).await;
    assert_eq!(status, 200, "{body}");
    server.stop().await;
}
