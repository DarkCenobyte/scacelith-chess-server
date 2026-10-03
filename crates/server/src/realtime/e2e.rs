//! End-to-end tests of the realtime protocol: a whole server started by [`crate::app`] (store,
//! game hosts and their journals, lobby, auth, anti-cheat, API, listeners) on a loopback port,
//! and minimal WebSocket clients over TCP. Sessions come from the real auth service.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use scacelith_chess::ChessGame;
use scacelith_protocol::{
    self as proto, Color, EndReason, ErrorCode, GameStatus, Hello, Message, MsgType, NoticeCode,
    PROTOCOL_VERSION, QueueJoin, QueueState, Resync, ServerMsg, uci_to_move,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

use crate::app::{Instance, LaunchOptions};
use crate::config::{Config, test_config};
use crate::game::Rules;
use crate::ids::GameId;
use crate::net::server::ListenerKind;
use crate::net::upgrade::SUBPROTOCOL;
use crate::net::ws::tests::{frame, read_server_frame};
use crate::realtime::testing::name;
use crate::security::password::{Argon2Hasher, Argon2Params};
use crate::store::tests::support::TempDir;
use crate::store::{NewSanction, NewUser, SanctionKind, Source};

/// The longest wait for one frame before a test fails.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// `SHUTDOWN_GRACE_MS` of the test servers.
const GRACE_MS: u32 = 300;

/// A server of the tests, restartable on its data directory.
struct TestServer {
    instance: Option<Instance>,
    addr: SocketAddr,
    config: Arc<Config>,
    _dir: Arc<TempDir>,
}

impl TestServer {
    async fn start(overrides: &[(&str, &str)]) -> TestServer {
        let dir = Arc::new(TempDir::new("e2e"));
        let config = Arc::new(config_in(&dir, overrides));
        TestServer::launch(config, dir).await
    }

    async fn launch(config: Arc<Config>, dir: Arc<TempDir>) -> TestServer {
        let instance = Instance::launch(config.clone(), options()).await.expect("the server starts");
        instance.announce_ready();
        let addr = instance.address(ListenerKind::ApiWs).expect("one port for the API and the WebSocket");
        TestServer { instance: Some(instance), addr, config, _dir: dir }
    }

    fn instance(&self) -> &Instance {
        self.instance.as_ref().expect("a running server")
    }

    /// A verified account and a session token of it.
    async fn account(&self, name: &str) -> String {
        let store = self.instance().store();
        let user = NewUser {
            username: name.into(),
            email: Some(format!("{name}@example.org")),
            password_hash: None,
            email_verified: true,
            accept_challenges: true,
            created_at: crate::clock::wall_ms(),
        };
        let id = store.users().create(user).await.expect("an account");
        let user = store.users().by_id(id).await.expect("read").expect("the account");
        self.instance().auth().create_session(&user, Some("e2e"), None).await.expect("a session").token
    }

    async fn connect(&self) -> Client {
        Client::open(TcpStream::connect(self.addr).await.expect("connected")).await
    }

    async fn login(&self, token: &str) -> (Client, proto::Welcome) {
        let mut c = self.connect().await;
        c.hello(token).await;
        let w = c.welcome().await;
        (c, w)
    }

    /// A connection with `token` refused as banned (`Notice{Banned}`, `Error{Banned}`, 4004): the
    /// notice.
    async fn refused_as_banned(&self, token: &str) -> proto::Notice {
        let mut c = self.connect().await;
        c.hello(token).await;
        let (notice, error, code) = c.kicked().await;
        assert_eq!(notice.code, NoticeCode::Banned);
        assert_eq!((error.code, code), (ErrorCode::Banned, 4004));
        notice
    }

    /// Two accounts paired by the matchmaker in a rated 5+0 game: (White, Black, game).
    async fn pair(&self) -> (Seat, Seat, GameId) {
        let (alice, bob) = (self.account("alice").await, self.account("bob").await);
        let (mut a, _) = self.login(&alice).await;
        let (mut b, _) = self.login(&bob).await;
        for c in [&mut a, &mut b] {
            let seq = c.next_seq();
            c.send(QueueJoin { seq, category: "5+0".into(), rated: true }).await;
        }
        let mut snapshots = Vec::new();
        for c in [&mut a, &mut b] {
            loop {
                if let ServerMsg::QueueStatus(s) = c.until("QueueStatus").await
                    && s.state == QueueState::Matched
                {
                    break;
                }
            }
            let ServerMsg::GameSnapshot(s) = c.until("GameSnapshot").await else { unreachable!() };
            snapshots.push(s);
        }
        let game = snapshots[0].game;
        assert_eq!(snapshots[1].game, game);
        assert_eq!(
            (snapshots[0].status, snapshots[0].rated, snapshots[0].category.as_str()),
            (GameStatus::Ongoing, true, "5+0")
        );
        let (a, b) = (Seat { c: a, token: alice }, Seat { c: b, token: bob });
        if snapshots[0].you == Color::White { (a, b, game) } else { (b, a, game) }
    }

    /// The graceful shutdown, as on SIGTERM.
    async fn stop(&mut self) {
        let instance = self.instance.take().expect("a running server");
        instance.shutdown("SIGTERM").await;
    }

    /// Starts the server again on the same data directory (a new port).
    async fn restart(&mut self) {
        let config = self.config.clone();
        let dir = self._dir.clone();
        *self = TestServer::launch(config, dir).await;
    }
}

/// The configuration of a test server in `dir`: loopback, a port of the system's choice, no
/// engine analysis, a short drain.
fn config_in(dir: &TempDir, overrides: &[(&str, &str)]) -> Config {
    let data = dir.path().display().to_string();
    let (db, journal) = (dir.file("scacelith.db"), dir.file("journal"));
    let grace = GRACE_MS.to_string();
    let mut keys = vec![
        ("DATA_DIR", data.as_str()),
        ("DB_PATH", db.as_str()),
        ("JOURNAL_DIR", journal.as_str()),
        ("BIND_ADDRESS", "127.0.0.1"),
        ("API_PORT", "0"),
        ("ANALYSIS_WORKERS", "0"),
        ("SHUTDOWN_GRACE_MS", grace.as_str()),
        ("MATCH_TICK_MS", "50"),
    ];
    keys.extend_from_slice(overrides);
    test_config(&keys).expect("a valid configuration")
}

/// The launch options of the tests: a cheap password hash, no process metrics sampler.
fn options() -> LaunchOptions {
    let hasher =
        Argon2Hasher::new(Argon2Params { memory_kib: 64, passes: 1, lanes: 1, ..Argon2Params::DEFAULT });
    LaunchOptions {
        password_hasher: Some(Arc::new(hasher)),
        process_metrics: false,
        ..LaunchOptions::default()
    }
}

/// A player of a paired game: its connection and its session token.
struct Seat {
    c: Client,
    token: String,
}

/// What a client read.
#[derive(Debug)]
enum Rx {
    Msg(ServerMsg),
    Close(u16),
    End,
}

/// A WebSocket client of the protocol.
struct Client {
    io: TcpStream,
    seq: u32,
    /// The `Scacelith-Server-Id` of the `101` answer.
    server_id: String,
}

impl Client {
    /// Upgrades `io` (`GET /ws`).
    async fn open(io: TcpStream) -> Client {
        let (head, io) = upgrade(io).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        let server_id = header(&head, "scacelith-server-id").expect("the server id header");
        Client { io, seq: 0, server_id }
    }

    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    async fn raw(&mut self, bytes: &[u8]) {
        self.io.write_all(&frame(0x2, bytes)).await.expect("sent");
    }

    async fn send(&mut self, msg: impl Message) {
        self.raw(&msg.to_vec().expect("a valid message")).await;
    }

    async fn hello(&mut self, token: &str) {
        self.seq = 1;
        let hello = Hello {
            seq: 1,
            proto: PROTOCOL_VERSION,
            minor: 0,
            caps: 0,
            client: "e2e/1".into(),
            token: token.into(),
        };
        self.send(hello).await;
    }

    /// Plays `uci` as move `ply` of `game` from the position of `board`, which then plays it too.
    async fn play(&mut self, game: GameId, ply: u16, board: &mut ChessGame, uci: &str) {
        let m = uci_to_move(uci).expect("a UCI move");
        let seq = self.next_seq();
        let pos_hash = Rules::digest(board);
        self.send(proto::Move { seq, game, ply, r#move: m, pos_hash, think_ms: 200, draw_offer: false })
            .await;
        assert!(Rules::play(board, m).is_some(), "{uci} is legal");
    }

    async fn recv(&mut self) -> Rx {
        let read = tokio::time::timeout(READ_TIMEOUT, read_server_frame(&mut self.io)).await;
        match read.expect("a frame in time") {
            Some((0x2, payload)) => {
                Rx::Msg(ServerMsg::decode_exact(&payload).expect("a valid server message"))
            }
            Some((0x8, payload)) => {
                let _ = self.io.write_all(&frame(0x8, &payload)).await;
                Rx::Close(u16::from_be_bytes([payload[0], payload[1]]))
            }
            Some((op, _)) => panic!("unexpected opcode {op}"),
            None => Rx::End,
        }
    }

    async fn msg(&mut self) -> ServerMsg {
        match self.recv().await {
            Rx::Msg(m) => m,
            other => panic!("a message expected, got {other:?}"),
        }
    }

    /// The next message other than a heartbeat `Ping`.
    async fn next(&mut self) -> ServerMsg {
        loop {
            match self.msg().await {
                ServerMsg::Ping(_) => {}
                m => return m,
            }
        }
    }

    /// The next message named `wanted`, skipping the others.
    async fn until(&mut self, wanted: &str) -> ServerMsg {
        loop {
            let m = self.msg().await;
            if name(&m) == wanted {
                return m;
            }
        }
    }

    /// The close code, skipping the messages before it.
    async fn closed(&mut self) -> u16 {
        loop {
            match self.recv().await {
                Rx::Msg(_) => {}
                Rx::Close(code) => return code,
                Rx::End => panic!("the stream ended without a close frame"),
            }
        }
    }

    /// The notice that comes first, then the fatal `Error` and the close code.
    async fn kicked(&mut self) -> (proto::Notice, proto::Error, u16) {
        let ServerMsg::Notice(notice) = self.next().await else { panic!("a notice first") };
        let (error, code) = self.refused().await;
        (notice, error, code)
    }

    /// The fatal `Error` and the close code that follows it.
    async fn refused(&mut self) -> (proto::Error, u16) {
        let ServerMsg::Error(error) = self.until("Error").await else { unreachable!() };
        assert!(error.fatal, "{error:?}");
        (error, self.closed().await)
    }

    async fn welcome(&mut self) -> proto::Welcome {
        match self.next().await {
            ServerMsg::Welcome(w) => w,
            other => panic!("Welcome expected, got {other:?}"),
        }
    }
}

/// Sends the upgrade request on `io`; returns the answer's head and the stream after it.
async fn upgrade(mut io: TcpStream) -> (String, TcpStream) {
    let request = format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: {SUBPROTOCOL}\r\n\r\n"
    );
    io.write_all(request.as_bytes()).await.expect("request sent");
    let head = read_head(&mut io).await;
    (head, io)
}

/// The head of an HTTP answer, read byte by byte (nothing after it is consumed).
async fn read_head(io: &mut TcpStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(READ_TIMEOUT, io.read(&mut byte)).await.expect("an answer in time") {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// A header of an HTTP head (name in lower case).
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

/// `GET path` with `Connection: close`: the status and the body.
async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut io = TcpStream::connect(addr).await.expect("connected");
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    io.write_all(request.as_bytes()).await.expect("request sent");
    let mut answer = Vec::new();
    tokio::time::timeout(READ_TIMEOUT, io.read_to_end(&mut answer)).await.expect("in time").expect("read");
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let status = answer.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = answer.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

/// `scacelith_ws_closes_total{code="<code>"}` (the closes of every server of the process).
fn closes_with(code: u16) -> u64 {
    let prefix = format!("scacelith_ws_closes_total{{code=\"{code}\"}} ");
    crate::metrics::registry()
        .render()
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0)
}

#[tokio::test]
async fn serves_the_api_and_welcomes_a_session_of_the_auth_service() {
    let server = TestServer::start(&[("SERVER_NAME", "E2E")]).await;
    let token = server.account("alice").await;
    let (mut c, w) = server.login(&token).await;
    assert_eq!((w.username.as_str(), w.server_name.as_str(), w.active_game, w.proto), ("alice", "E2E", 0, 1));

    // The API is served on the same port; /info names the server as the upgrade answer does.
    let (status, body) = http_get(server.addr, "/api/v1/info").await;
    assert_eq!(status, 200, "{body}");
    let info: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(info["serverId"].as_str(), Some(c.server_id.as_str()));

    // Client pings are answered.
    let seq = c.next_seq();
    c.send(proto::ClientPing { seq, nonce: 7 }).await;
    let ServerMsg::Pong(p) = c.until("S_Pong").await else { unreachable!() };
    assert_eq!(p.nonce, 7);

    let mut stranger = server.connect().await;
    stranger.hello("sct_not-a-session-token-at-all-0000000000").await;
    let (e, code) = stranger.refused().await;
    assert_eq!((e.code, e.r#ref, code), (ErrorCode::Unauthorized, 1, 4003));
    let mut server = server;
    server.stop().await;
}

#[tokio::test]
async fn pairs_two_players_and_plays_a_game_to_its_rating_update() {
    let mut server = TestServer::start(&[]).await;
    let (white, black, game) = server.pair().await;
    let (mut white, mut black) = (white.c, black.c);
    let mut board = ChessGame::default();
    for (i, uci) in ["f2f3", "e7e5", "g2g4", "d8h4"].into_iter().enumerate() {
        let mover = if i % 2 == 0 { &mut white } else { &mut black };
        mover.play(game, i as u16, &mut board, uci).await;
        for c in [&mut white, &mut black] {
            let ServerMsg::MoveMade(m) = c.until("MoveMade").await else { unreachable!() };
            assert_eq!((m.game, m.ply as usize, m.r#move), (game, i, uci_to_move(uci).unwrap()));
        }
    }
    for c in [&mut white, &mut black] {
        let ServerMsg::GameEnd(end) = c.until("GameEnd").await else { unreachable!() };
        assert_eq!((end.game, end.status, end.reason), (game, GameStatus::BlackWins, EndReason::Checkmate));
        let ServerMsg::RatingUpdate(r) = c.until("RatingUpdate").await else { unreachable!() };
        assert_eq!((r.game, r.category.as_str()), (game, "5+0"));
        // Both players are in the unrated phase of the Elo rules: the game counts, the rating waits.
        assert_eq!((r.white.games, r.black.games, r.white.provisional), (1, 1, true), "{r:?}");
    }
    let stored =
        server.instance().store().games().by_id(game).await.expect("read").expect("the game is stored");
    assert_eq!(
        (stored.summary.id, stored.summary.ply_count, stored.summary.status),
        (game, 4, GameStatus::BlackWins as u8)
    );
    server.stop().await;
}

#[tokio::test]
async fn a_player_who_reconnects_gets_the_game_back_with_its_moves() {
    let mut server = TestServer::start(&[]).await;
    let (mut white, mut black, game) = server.pair().await;
    let mut board = ChessGame::default();
    white.c.play(game, 0, &mut board, "e2e4").await;
    black.c.until("MoveMade").await;
    drop(white.c);
    let (mut again, w) = server.login(&white.token).await;
    assert_eq!(w.active_game, game);
    let ServerMsg::GameSnapshot(s) = again.until("GameSnapshot").await else { unreachable!() };
    assert_eq!((s.game, s.you, s.moves.len()), (game, Color::White, 1));
    assert_eq!(s.moves[0].r#move, uci_to_move("e2e4").unwrap());
    // The game goes on from the new connection.
    black.c.play(game, 1, &mut board, "e7e5").await;
    let ServerMsg::MoveMade(m) = again.until("MoveMade").await else { unreachable!() };
    assert_eq!(m.ply, 1);
    server.stop().await;
}

#[tokio::test]
async fn a_second_connection_replaces_the_first() {
    let mut server = TestServer::start(&[]).await;
    let token = server.account("alice").await;
    let (mut first, _) = server.login(&token).await;
    let (mut second, w) = server.login(&token).await;
    assert_eq!(w.username, "alice");
    let (notice, error, code) = first.kicked().await;
    assert_eq!(notice.code, NoticeCode::ReplacedByNewConnection);
    assert_eq!((error.code, code), (ErrorCode::Replaced, 4007));
    // The second connection is the live one.
    let seq = second.next_seq();
    second.send(proto::ClientPing { seq, nonce: 3 }).await;
    assert!(matches!(second.until("S_Pong").await, ServerMsg::Pong(_)));
    server.stop().await;
}

#[tokio::test]
async fn a_revoked_session_closes_its_connection_and_opens_no_other() {
    let mut server = TestServer::start(&[]).await;
    let token = server.account("alice").await;
    let (mut c, _) = server.login(&token).await;
    let auth = server.instance().auth();
    let session = auth.validate_token(&token).await.expect("checked").expect("a session");
    auth.logout(&session, None).await.expect("logged out");
    let (notice, error, code) = c.kicked().await;
    assert_eq!(notice.code, NoticeCode::SessionRevoked);
    assert_eq!((error.code, code), (ErrorCode::Unauthorized, 4003));
    let mut again = server.connect().await;
    again.hello(&token).await;
    let (e, code) = again.refused().await;
    assert_eq!((e.code, code), (ErrorCode::Unauthorized, 4003));
    server.stop().await;
}

#[tokio::test]
async fn a_ban_refuses_the_hello_and_a_certain_cheat_bans_at_once() {
    let mut server = TestServer::start(&[]).await;
    // A ban written to the database by another process (the admin command) applies at the Hello.
    let carl = server.account("carl").await;
    let carl_id = server.instance().auth().validate_token(&carl).await.unwrap().unwrap().user_id;
    let now = crate::clock::wall_ms();
    let until = now + 3_600_000;
    let ban = NewSanction {
        user_id: carl_id,
        kind: SanctionKind::Ban,
        reason: Some("abuse".into()),
        source: Source::Moderator,
        game_id: None,
        starts_at: now,
        ends_at: Some(until),
        created_by: Some("admin".into()),
        created_at: now,
    };
    server.instance().store().sanctions().create(ban).await.expect("ban");
    let mut c = server.connect().await;
    c.hello(&carl).await;
    let (notice, error, code) = c.kicked().await;
    assert_eq!((notice.code, notice.arg), (NoticeCode::Banned, until as f64));
    assert_eq!((error.code, code), (ErrorCode::Banned, 4004));

    // A forged server message during a game: closed 4302, the game forfeited, and banned from
    // that moment: a connection right after the close is refused.
    let (mut white, mut black, game) = server.pair().await;
    white.c.raw(&[MsgType::Welcome.to_u8(), 0, 0, 0, 0]).await;
    let (e, code) = white.c.refused().await;
    assert_eq!((e.code, code), (ErrorCode::CheatDetected, 4302));
    let notice = server.refused_as_banned(&white.token).await;
    let ServerMsg::GameEnd(end) = black.c.until("GameEnd").await else { unreachable!() };
    assert_eq!((end.game, end.status, end.reason), (game, GameStatus::BlackWins, EndReason::Forfeit));
    // The refusal announced the end of the ban then written.
    let white_id = server.instance().auth().validate_token(&white.token).await.unwrap().unwrap().user_id;
    let store = server.instance().store();
    let mut ban = None;
    for _ in 0..250 {
        ban = store.sanctions().active_ban(white_id, crate::clock::wall_ms()).await.expect("read");
        if ban.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ban.expect("the ban is written").ends_at, Some(notice.arg as i64));
    server.stop().await;
}

#[tokio::test]
async fn a_certain_cheat_in_a_game_bans_before_its_close() {
    let mut server = TestServer::start(&[]).await;
    let (mut white, mut black, game) = server.pair().await;
    let mut board = ChessGame::default();
    white.c.play(game, 0, &mut board, "e2e4").await;
    for c in [&mut white.c, &mut black.c] {
        c.until("MoveMade").await;
    }
    // White again, in the position both sides have: out of turn, a certain cheat.
    let seq = white.c.next_seq();
    let r#move = uci_to_move("d2d4").expect("a UCI move");
    let pos_hash = Rules::digest(&board);
    white.c.send(proto::Move { seq, game, ply: 1, r#move, pos_hash, think_ms: 200, draw_offer: false }).await;
    let (e, code) = white.c.refused().await;
    assert_eq!((e.code, e.game, code), (ErrorCode::CheatDetected, game, 4302));
    server.refused_as_banned(&white.token).await;
    let ServerMsg::GameEnd(end) = black.c.until("GameEnd").await else { unreachable!() };
    assert_eq!((end.game, end.status, end.reason), (game, GameStatus::BlackWins, EndReason::Forfeit));
    server.stop().await;
}

#[tokio::test]
async fn closes_a_slow_consumer_4303_without_an_error_and_lets_it_resume() {
    let mut server = TestServer::start(&[
        ("WS_MSG_RATE", "1000000"),
        ("WS_MSG_BURST", "1000000"),
        ("WS_SEND_BUFFER_LIMIT", "4096"),
    ])
    .await;
    let (_white, mut black, game) = server.pair().await;
    let before = closes_with(4303);
    // A client that does not read and asks for snapshot after snapshot.
    let socket = TcpSocket::new_v4().expect("a socket");
    socket.set_recv_buffer_size(4096).expect("a small receive buffer");
    let mut slow = Client::open(socket.connect(server.addr).await.expect("connected")).await;
    slow.hello(&black.token).await;
    assert_eq!(slow.welcome().await.active_game, game);
    black.c.closed().await; // replaced by the slow connection
    let mut burst = Vec::new();
    'flood: for _ in 0..200 {
        burst.clear();
        for _ in 0..500 {
            let seq = slow.next_seq();
            burst.extend_from_slice(&frame(0x2, &Resync { seq, game }.to_vec().unwrap()));
        }
        if slow.io.write_all(&burst).await.is_err() {
            break 'flood;
        }
        if closes_with(4303) > before {
            break 'flood;
        }
    }
    for _ in 0..500 {
        if closes_with(4303) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closes_with(4303) > before, "the slow connection is closed with 4303");
    // What the slow client finally reads holds no Error: 4303 comes without one.
    loop {
        match slow.recv().await {
            Rx::Msg(m) => assert!(!matches!(m, ServerMsg::Error(_)), "{m:?}"),
            Rx::Close(code) => {
                assert_eq!(code, 4303);
                break;
            }
            Rx::End => break,
        }
    }
    // The player comes back and finds the game.
    let (mut back, w) = server.login(&black.token).await;
    assert_eq!(w.active_game, game);
    assert!(matches!(back.until("GameSnapshot").await, ServerMsg::GameSnapshot(s) if s.game == game));
    server.stop().await;
}

#[tokio::test]
async fn drains_at_the_shutdown_and_gives_the_game_back_after_the_restart() {
    let mut server = TestServer::start(&[]).await;
    let (white, black, game) = server.pair().await;
    let token = white.token;
    let (mut white, mut black) = (white.c, black.c);
    let mut board = ChessGame::default();
    white.play(game, 0, &mut board, "d2d4").await;
    black.until("MoveMade").await;

    let instance = server.instance.take().expect("running");
    let stopping = tokio::spawn(instance.shutdown("SIGTERM"));
    for c in [&mut white, &mut black] {
        let ServerMsg::Notice(n) = c.until("Notice").await else { unreachable!() };
        assert_eq!((n.code, n.arg), (NoticeCode::ServerShutdown, f64::from(GRACE_MS)));
    }
    // New connections are refused meanwhile.
    if let Ok(io) = TcpStream::connect(server.addr).await {
        let (head, _) = upgrade(io).await;
        assert!(!head.starts_with("HTTP/1.1 101"), "{head}");
    }
    for c in [&mut white, &mut black] {
        let (e, code) = c.refused().await;
        assert_eq!((e.code, e.r#ref, code), (ErrorCode::ShuttingDown, 0, 4008));
    }
    tokio::time::timeout(Duration::from_secs(20), stopping)
        .await
        .expect("stopped in time")
        .expect("no panic");

    // The game in progress survives the restart (journal).
    server.restart().await;
    let (mut again, w) = server.login(&token).await;
    assert_eq!(w.active_game, game);
    let ServerMsg::GameSnapshot(s) = again.until("GameSnapshot").await else { unreachable!() };
    assert_eq!((s.game, s.moves.len(), s.status), (game, 1, GameStatus::Ongoing));
    server.stop().await;
}

#[tokio::test]
async fn a_port_in_use_fails_the_start_and_stops_what_was_started() {
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = taken.local_addr().unwrap().port().to_string();
    let dir = Arc::new(TempDir::new("e2e"));
    let config = Arc::new(config_in(&dir, &[("API_PORT", port.as_str())]));
    let err = Instance::launch(config, options()).await.expect_err("the port is taken");
    assert!(err.to_string().starts_with("listeners: "), "{err}");
    // The store was closed and the journals left in order: the next start on the directory works.
    let server = TestServer::launch(Arc::new(config_in(&dir, &[])), dir).await;
    let (_, w) = server.login(&server.account("alice").await).await;
    assert_eq!(w.username, "alice");
    let mut server = server;
    server.stop().await;
}
