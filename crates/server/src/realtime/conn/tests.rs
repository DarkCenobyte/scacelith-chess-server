//! Tests of the connection tasks (the former router tests): a real lobby actor and store, fake
//! game hosts and tokens, and a WebSocket client over an in-memory stream, under tokio's paused
//! clock (the connection's timers and timestamps follow it).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use scacelith_protocol::{
    self as proto, ClientGesture, ClientPing, ClientPong, ErrorCode, Hello, Message, MsgType, NoticeCode,
    PROTOCOL_VERSION, QueueJoin, QueueLeave, Resign, ServerMsg,
};
use tokio::io::{AsyncWriteExt, DuplexStream};

use super::{ConnContext, ConnSettings, serve};
use crate::clock::{ManualClock, SharedClock};
use crate::config::{Config, test_config};
use crate::events::{GameEnded, HostEvents};
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::matching::elo::Categories;
use crate::net::limits::SharedLimits;
use crate::net::ws::tests::{frame, read_server_frame};
use crate::net::ws::{AdmissionPermit, CLOSE_TIMEOUT, WsConnection, WsSettings};
use crate::realtime::drain::{Drain, DrainPhase};
use crate::realtime::endpoint::Endpoint;
use crate::realtime::link::ConnLink;
use crate::realtime::lobby::{ClaimOutcome, Lobby, LobbyDeps, LobbyMsg, Timer};
use crate::realtime::testing::{FakeHosts, FakeTokens, HostCall, RecordingAnomalies, TokioClock, name};
use crate::store::tests::support::TempDir;
use crate::store::{NewSanction, NewUser, SanctionKind, Source, Store, StoreOptions};

const START: i64 = 1_780_315_200_000; // 2026-06-01T12:00:00Z
const ALICE: &str = "alice-token-0123456789abcdef";
const BOB: &str = "bob-token-0123456789abcdef00";
const CARL: &str = "carl-token-0123456789abcdef0"; // e-mail address not verified
const GESTURE: ClientGesture = ClientGesture {
    seq: 0,
    game: 0,
    ply: 3,
    touch: 12,
    aim: 28,
    placed: 0,
    flags: 1,
    yaw: -700,
    pitch: 250,
    lean: 40,
};

struct Rig {
    ctx: Arc<ConnContext>,
    hosts: Arc<FakeHosts>,
    tokens: Arc<FakeTokens>,
    anomalies: Arc<RecordingAnomalies>,
    lobby: Lobby,
    store: Store,
    clock: SharedClock,
    _dir: TempDir,
}

/// What the client read.
#[derive(Debug)]
enum Rx {
    Msg(ServerMsg),
    Close(u16),
    End,
}

struct Client {
    io: DuplexStream,
    seq: u32,
}

impl Client {
    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    async fn raw(&mut self, bytes: &[u8]) {
        let _ = self.io.write_all(&frame(0x2, bytes)).await;
    }

    async fn send(&mut self, msg: impl Message) {
        self.raw(&msg.to_vec().expect("a valid message")).await;
    }

    async fn hello(&mut self, token: &str) {
        self.seq = 1;
        self.send(hello(1, token)).await;
    }

    async fn queue_leave(&mut self) -> u32 {
        let seq = self.next_seq();
        self.send(QueueLeave { seq }).await;
        seq
    }

    async fn gesture(&mut self, game: GameId) {
        let seq = self.next_seq();
        self.send(ClientGesture { seq, game, ..GESTURE }).await;
    }

    /// The next frame (a close frame is answered, which ends the handshake).
    async fn recv(&mut self) -> Rx {
        match read_server_frame(&mut self.io).await {
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

    /// The next message (panics on the close).
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

    /// The fatal `Error` and the close code that follows it.
    async fn refused(&mut self) -> (proto::Error, u16) {
        let error = match self.until("Error").await {
            ServerMsg::Error(e) => e,
            _ => unreachable!(),
        };
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

fn hello(seq: u32, token: &str) -> Hello {
    Hello {
        seq,
        proto: PROTOCOL_VERSION,
        minor: 3,
        caps: u64::MAX,
        client: "test/1".into(),
        token: token.into(),
    }
}

fn ack_of(m: &ServerMsg) -> Option<u32> {
    match m {
        ServerMsg::Ack(a) => Some(a.r#ref),
        _ => None,
    }
}

impl Rig {
    async fn new(overrides: &[(&str, &str)]) -> Rig {
        Rig::with_clock(overrides, TokioClock::new(1_000_000.0, START)).await
    }

    async fn with_clock(overrides: &[(&str, &str)], clock: SharedClock) -> Rig {
        let mut all = vec![
            ("WS_HELLO_TIMEOUT_MS", "1000"),
            ("HEARTBEAT_INTERVAL_MS", "1000"),
            ("HEARTBEAT_TIMEOUT_MS", "3250"),
            ("REQUIRE_EMAIL_VERIFICATION", "true"),
        ];
        all.extend_from_slice(overrides);
        let config = Arc::new(test_config(&all).expect("a valid configuration"));
        // A database file: its reads run on tokio's blocking threads, which hold the paused clock
        // (the reads of an in-memory database go through the writer's thread, which does not).
        let dir = TempDir::new("realtime-conn");
        let options = StoreOptions {
            path: Some(dir.file("scacelith.db")),
            clock: Some(clock.clone()),
            ..Default::default()
        };
        let store = Store::open(&config, options).await.expect("store");
        store.migrate().await.expect("migrations");
        for name in ["alice", "bob", "carl"] {
            let user = NewUser {
                username: name.into(),
                email: None,
                password_hash: None,
                email_verified: name != "carl",
                accept_challenges: true,
                created_at: START,
            };
            store.users().create(user).await.expect("user");
        }
        let tokens = FakeTokens::new();
        tokens.add(ALICE, 1, "alice", true);
        tokens.add(BOB, 2, "bob", true);
        tokens.add(CARL, 3, "carl", false);
        let hosts = FakeHosts::new(4);
        let anomalies = RecordingAnomalies::new();
        let (lobby, inbox) = Lobby::channel();
        let limits = Arc::new(SharedLimits::new(clock.clone()));
        inbox.start_manual(LobbyDeps::new(
            config.clone(),
            clock.clone(),
            store.clone(),
            hosts.clone(),
            limits,
        ));
        let ctx = Arc::new(ConnContext {
            settings: ConnSettings::from_config(&config),
            categories: Categories::from_config(&config),
            config: config.clone(),
            clock: clock.clone(),
            store: store.clone(),
            hosts: hosts.clone(),
            tokens: tokens.clone(),
            anomalies: anomalies.clone(),
            lobby: lobby.clone(),
            drain: Drain::new(),
            log: Logger::root().child("ws"),
        });
        Rig { ctx, hosts, tokens, anomalies, lobby, store, clock, _dir: dir }
    }

    fn config(&self) -> &Config {
        &self.ctx.config
    }

    fn ws(&self, io: DuplexStream) -> WsConnection {
        let settings = WsSettings {
            max_message_bytes: 512,
            close_timeout: CLOSE_TIMEOUT,
            clock: self.clock.clone(),
            log: Logger::root().child("ws"),
        };
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        WsConnection::new(Box::new(io), Bytes::new(), ip, AdmissionPermit::none(), &settings)
    }

    /// A new connection served by a connection task.
    fn connect(&self) -> Client {
        self.connect_with(&self.ctx)
    }

    fn connect_with(&self, ctx: &Arc<ConnContext>) -> Client {
        let (client, server) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(self.ws(server), ctx.clone()));
        Client { io: client, seq: 0 }
    }

    async fn login(&self, token: &str) -> (Client, proto::Welcome) {
        let mut c = self.connect();
        c.hello(token).await;
        let w = c.welcome().await;
        (c, w)
    }

    /// Waits until every connection ended and the lobby handled their releases.
    async fn idle(&self) {
        assert!(self.ctx.drain.wait_idle(Duration::from_secs(10)).await, "connections still open");
        self.settle().await;
    }

    /// Waits until the lobby handled everything posted so far.
    async fn settle(&self) {
        while self.lobby.pending_tasks().await != Some(0) {
            tokio::task::yield_now().await;
        }
    }

    /// A game of alice (white) and bob in progress, on shard 0.
    async fn game_in_progress(&self) -> GameId {
        let game = self.hosts.next_id(0);
        self.lobby.game_recovered(game, 1, 2);
        self.settle().await;
        game
    }

    fn game_ended(&self, game: GameId) {
        self.lobby.game_ended(GameEnded {
            game,
            white: 1,
            black: 2,
            status: proto::GameStatus::Draw,
            reason: proto::EndReason::Agreement,
            rated: false,
            category: "5+0".into(),
        });
    }

    /// Waits until the hosts were told `n` calls matching `pred`.
    async fn host_calls(&self, n: usize, pred: impl Fn(&HostCall) -> bool) -> Vec<HostCall> {
        for _ in 0..1000 {
            let calls: Vec<HostCall> = self.hosts.calls().into_iter().filter(|c| pred(c)).collect();
            if calls.len() >= n {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("{n} host calls expected: {:?}", self.hosts.calls());
    }
}

// ---- hello --------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn welcomes_a_valid_token_and_releases_the_presence_on_close() {
    let rig = Rig::new(&[("MAX_CONNECTIONS", "1"), ("CLIENT_PING_INTERVAL_MS", "25000")]).await;
    let (mut c, w) = rig.login(ALICE).await;
    assert_eq!((w.proto, w.minor, w.caps), (1, proto::MINOR, proto::CAPS), "negotiated: min and and");
    assert_eq!((w.user_id, w.username.as_str(), w.active_game), (1, "alice", 0));
    assert_eq!(w.server_name, rig.config().server_name);
    assert_eq!((w.heartbeat_ms, w.client_ping_ms), (1000, 25000));
    assert_eq!((w.max_msg_per_sec, w.msg_burst), (20, 40));
    assert_eq!((w.gesture_rate, w.gesture_burst), (4, 8));
    assert!((w.server_time - rig.clock.mono_ms()).abs() < 1000.0, "the clock of the game hosts");
    // The server is full while alice is online...
    let (mut b, _) = (rig.connect(), ());
    b.hello(BOB).await;
    let (e, code) = b.refused().await;
    assert_eq!((e.r#ref, e.code, code), (1, ErrorCode::ServerFull, 4006));
    // ...and no more once her connection closed.
    let _ = c.io.write_all(&frame(0x8, &1000u16.to_be_bytes())).await;
    assert_eq!(c.closed().await, 1000);
    rig.idle().await;
    let (_b, w) = rig.login(BOB).await;
    assert_eq!(w.user_id, 2);
}

#[tokio::test(start_paused = true)]
async fn handles_the_messages_pipelined_behind_the_hello_after_the_welcome() {
    let rig = Rig::new(&[]).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    rig.tokens.hold_next(gate.clone());
    let mut c = rig.connect();
    c.hello(ALICE).await;
    let seq = c.next_seq();
    c.send(QueueJoin { seq, category: "5+0".into(), rated: true }).await;
    c.queue_leave().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    gate.notify_one();
    c.welcome().await;
    let mut acks = Vec::new();
    while acks.len() < 2 {
        if let Some(r) = ack_of(&c.next().await) {
            acks.push(r);
        }
    }
    assert_eq!(acks, [2, 3]);

    // More than 8 is a flood.
    rig.tokens.hold_next(Arc::new(tokio::sync::Notify::new()));
    let mut c = rig.connect();
    c.hello(BOB).await;
    for _ in 0..9 {
        c.queue_leave().await;
    }
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (0, ErrorCode::Flood, 4301));
}

#[tokio::test(start_paused = true)]
async fn refuses_a_bad_hello_in_the_order_of_the_specification() {
    let rig = Rig::new(&[]).await;
    let full = hello(1, ALICE).to_vec().unwrap();
    let mut bad_token = full.clone();
    bad_token.truncate(full.len() - 3); // the token's length byte says more
    let mut next_proto = full.clone();
    next_proto[5] = 2;
    next_proto.truncate(20); // whatever follows the prefix
    let cases: Vec<(&str, Vec<u8>, u32, ErrorCode, u16)> = vec![
        ("another message first", QueueLeave { seq: 1 }.to_vec().unwrap(), 0, ErrorCode::HelloRequired, 4010),
        ("an empty message", Vec::new(), 0, ErrorCode::HelloRequired, 4010),
        ("a Hello shorter than its prefix", full[..12].to_vec(), 1, ErrorCode::Malformed, 4001),
        ("a later protocol", next_proto, 1, ErrorCode::UnsupportedProtocol, 4002),
        ("a Hello that does not decode", bad_token, 1, ErrorCode::Malformed, 4001),
        ("a Hello with seq 2", hello(2, ALICE).to_vec().unwrap(), 2, ErrorCode::ProtocolViolation, 4300),
        (
            "an unknown token",
            hello(1, "zzzz-unknown-token-000000").to_vec().unwrap(),
            1,
            ErrorCode::Unauthorized,
            4003,
        ),
        (
            "an unverified e-mail address",
            hello(1, CARL).to_vec().unwrap(),
            1,
            ErrorCode::EmailUnverified,
            4011,
        ),
    ];
    for (what, bytes, r#ref, code, close) in cases {
        let mut c = rig.connect();
        c.raw(&bytes).await;
        let (e, got) = c.refused().await;
        assert_eq!((e.r#ref, e.code, got), (r#ref, code, close), "{what}");
    }
    rig.idle().await;
    let (_c, w) = rig.login(ALICE).await;
    assert_eq!(w.user_id, 1, "no claim was left behind");
}

#[tokio::test(start_paused = true)]
async fn accepts_the_hello_of_a_later_minor_with_fields_it_does_not_know() {
    let rig = Rig::new(&[]).await;
    let mut bytes = hello(1, ALICE).to_vec().unwrap();
    bytes.extend_from_slice(&[1, 2, 3]);
    let mut c = rig.connect();
    c.raw(&bytes).await;
    assert_eq!(c.welcome().await.minor, proto::MINOR);
}

#[tokio::test(start_paused = true)]
async fn closes_a_banned_account_with_the_end_of_its_ban() {
    let rig = Rig::new(&[]).await;
    let until = START + 86_400_000;
    let ban = NewSanction {
        user_id: 1,
        kind: SanctionKind::Ban,
        reason: Some("engine".into()),
        source: Source::Moderator,
        game_id: None,
        starts_at: START,
        ends_at: Some(until),
        created_by: None,
        created_at: START,
    };
    rig.store.sanctions().create(ban).await.expect("ban");
    let mut c = rig.connect();
    c.hello(ALICE).await;
    match c.next().await {
        ServerMsg::Notice(n) => assert_eq!((n.code, n.arg), (NoticeCode::Banned, until as f64)),
        other => panic!("the notice comes first: {other:?}"),
    }
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (1, ErrorCode::Banned, 4004));
}

#[tokio::test(start_paused = true)]
async fn closes_4009_and_releases_the_claim_when_the_welcome_cannot_be_encoded() {
    let rig = Rig::new(&[("MAX_CONNECTIONS", "1")]).await;
    let mut ctx = ConnContext { ..clone_ctx(&rig.ctx) };
    ctx.settings.server_name = "é".repeat(40); // 80 bytes: over Welcome's 64
    let ctx = Arc::new(ctx);
    let mut c = rig.connect_with(&ctx);
    c.hello(ALICE).await;
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (1, ErrorCode::Internal, 4009));
    rig.idle().await;
    let (_b, w) = rig.login(BOB).await;
    assert_eq!(w.user_id, 2, "alice's claim was released");
}

fn clone_ctx(ctx: &ConnContext) -> ConnContext {
    ConnContext {
        settings: ctx.settings.clone(),
        config: ctx.config.clone(),
        categories: ctx.categories.clone(),
        clock: ctx.clock.clone(),
        store: ctx.store.clone(),
        hosts: ctx.hosts.clone(),
        tokens: ctx.tokens.clone(),
        anomalies: ctx.anomalies.clone(),
        lobby: ctx.lobby.clone(),
        drain: ctx.drain.clone(),
        log: ctx.log.clone(),
    }
}

#[tokio::test(start_paused = true)]
async fn closes_a_silent_connection_at_the_hello_deadline() {
    let rig = Rig::new(&[]).await;
    let t0 = rig.clock.mono_ms();
    let mut c = rig.connect();
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (0, ErrorCode::HelloRequired, 4010));
    let waited = rig.clock.mono_ms() - t0;
    assert!((1000.0..1100.0).contains(&waited), "{waited}");
}

#[tokio::test(start_paused = true)]
async fn closes_a_hello_whose_session_is_revoked_during_the_claim_or_whose_check_fails() {
    let rig = Rig::new(&[("MAX_CONNECTIONS", "1")]).await;
    rig.tokens.valid_times(ALICE, 1);
    let mut c = rig.connect();
    c.hello(ALICE).await;
    match c.next().await {
        ServerMsg::Notice(n) => assert_eq!(n.code, NoticeCode::SessionRevoked),
        other => panic!("{other:?}"),
    }
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (1, ErrorCode::Unauthorized, 4003));
    rig.idle().await;
    rig.tokens.set_failing(true);
    let mut c = rig.connect();
    c.hello(BOB).await;
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (1, ErrorCode::Internal, 4009));
    rig.tokens.set_failing(false);
    let (_b, w) = rig.login(BOB).await;
    assert_eq!(w.user_id, 2, "the revoked claim was released");
}

#[tokio::test(start_paused = true)]
async fn a_hello_that_stops_waiting_after_its_claim_was_answered_releases_it() {
    // The lobby answers the claim while the connection task still waits for the answer, and the
    // task stops before it reads it (its client went away meanwhile): the claim is released.
    let rig = Rig::new(&[]).await;
    let (lobby, mut inbox) = Lobby::manual();
    let ctx = ConnContext { lobby, ..clone_ctx(&rig.ctx) };
    let hello = hello(1, ALICE);
    let mut auth = Box::pin(super::hello::authenticate(&ctx, 77, &hello));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let reply = loop {
        std::future::poll_fn(|cx| {
            assert!(auth.as_mut().poll(cx).is_pending(), "the claim is not answered yet");
            std::task::Poll::Ready(())
        })
        .await;
        match inbox.try_recv() {
            Ok(LobbyMsg::Claim { reply, .. }) => break reply,
            Ok(_) => panic!("a claim expected first"),
            Err(_) => {
                assert!(std::time::Instant::now() < deadline, "no claim posted");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    };
    assert!(reply.send(ClaimOutcome::Admitted { active_game: 0 }).is_ok(), "the task still waits");
    drop(auth);
    assert!(matches!(inbox.try_recv(), Ok(LobbyMsg::Release { user: 1, conn: 77 })), "the claim is released");
}

#[tokio::test(start_paused = true)]
async fn attaches_the_game_in_progress_after_the_welcome() {
    let rig = Rig::new(&[]).await;
    let game = rig.game_in_progress().await;
    let (_c, w) = rig.login(ALICE).await;
    assert_eq!(w.active_game, game);
    let calls = rig.host_calls(1, |c| matches!(c, HostCall::Attach { .. })).await;
    assert!(matches!(calls[0], HostCall::Attach { game: g, user: 1, .. } if g == game));
}

#[tokio::test(start_paused = true)]
async fn replaces_an_older_connection_of_the_account() {
    let rig = Rig::new(&[]).await;
    let game = rig.game_in_progress().await;
    let (mut old, _) = rig.login(ALICE).await;
    let (_new, w) = rig.login(ALICE).await;
    assert_eq!(w.active_game, game);
    match old.next().await {
        ServerMsg::Notice(n) => assert_eq!(n.code, NoticeCode::ReplacedByNewConnection),
        other => panic!("{other:?}"),
    }
    let (e, code) = old.refused().await;
    assert_eq!((e.r#ref, e.code, code), (0, ErrorCode::Replaced, 4007));
    let attaches = rig.host_calls(2, |c| matches!(c, HostCall::Attach { .. })).await;
    let (HostCall::Attach { conn: first, .. }, HostCall::Attach { conn: second, .. }) =
        (&attaches[0], &attaches[1])
    else {
        unreachable!()
    };
    let detach = rig.host_calls(1, |c| matches!(c, HostCall::Detach { .. })).await;
    assert_eq!(detach, [HostCall::Detach { game, user: 1, conn: *first }], "the old one only");
    assert_ne!(first, second);
}

#[tokio::test(start_paused = true)]
async fn refuses_new_connections_while_draining_and_drains_the_others() {
    let rig = Rig::new(&[]).await;
    let (mut c, _) = rig.login(ALICE).await;
    let drain = rig.ctx.drain.clone();
    drain.set(DrainPhase::Grace { grace_ms: 50 });
    match c.next().await {
        ServerMsg::Notice(n) => assert_eq!((n.code, n.arg), (NoticeCode::ServerShutdown, 50.0)),
        other => panic!("{other:?}"),
    }
    let mut late = rig.connect();
    let (e, code) = late.refused().await;
    assert_eq!((e.r#ref, e.code, code), (0, ErrorCode::ShuttingDown, 4008));
    drain.set(DrainPhase::Closing);
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (0, ErrorCode::ShuttingDown, 4008));
    assert!(drain.wait_idle(Duration::from_secs(5)).await);
}

// ---- sessions -----------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn drops_a_bad_seq_with_one_anomaly_and_resynchronises() {
    let rig = Rig::new(&[]).await;
    let (mut c, _) = rig.login(ALICE).await;
    c.seq = 9;
    c.queue_leave().await; // 10 instead of 2: dropped
    c.queue_leave().await; // 11: accepted
    assert_eq!(ack_of(&c.until("Ack").await), Some(11));
    c.seq = 20;
    c.queue_leave().await;
    c.seq = 21;
    c.queue_leave().await; // 22 after 21
    assert_eq!(ack_of(&c.until("Ack").await), Some(22));
    let a = rig.anomalies.anomalies();
    assert_eq!(a.len(), 1);
    assert_eq!(
        (a[0].kind, a[0].user, a[0].detail.to_string()),
        ("bad_seq", 1, r#"{"expected":2,"got":10}"#.into())
    );
}

#[tokio::test(start_paused = true)]
async fn rate_limits_with_one_error_and_closes_a_flood() {
    let rig = Rig::new(&[("WS_MSG_RATE", "1"), ("WS_MSG_BURST", "3")]).await;
    let (mut c, _) = rig.login(ALICE).await;
    for _ in 0..5 {
        c.queue_leave().await;
    }
    let mut got = Vec::new();
    while got.len() < 4 {
        match c.next().await {
            ServerMsg::Ack(a) => got.push(format!("Ack {}", a.r#ref)),
            ServerMsg::Error(e) => got.push(format!("{:?} {}", e.code, e.r#ref)),
            _ => {}
        }
    }
    got.sort();
    assert_eq!(got, ["Ack 2", "Ack 3", "Ack 4", "RateLimited 5"]);
    for _ in 0..10 {
        c.queue_leave().await;
    }
    let (e, code) = c.refused().await;
    assert_eq!((e.code, code), (ErrorCode::Flood, 4301));
    assert_eq!(rig.anomalies.kinds(), ["flood"]);
    assert_eq!(rig.anomalies.anomalies()[0].detail.to_string(), r#"{"drops":11}"#);
}

#[tokio::test(start_paused = true)]
async fn treats_a_server_type_as_a_certain_cheat() {
    let rig = Rig::new(&[]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    c.raw(&[MsgType::Welcome.to_u8(), 0, 0, 0, 0]).await;
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (0, ErrorCode::CheatDetected, 4302));
    let a = rig.anomalies.anomalies();
    assert_eq!(
        (a[0].kind, a[0].game, a[0].detail.to_string()),
        ("forged_type", game, r#"{"type":128}"#.into())
    );
    assert_eq!(rig.anomalies.sanctions(), [(1, game, "forged_type")]);
    assert!(rig.hosts.calls().contains(&HostCall::Forfeit { game, user: 1 }));
}

#[tokio::test(start_paused = true)]
async fn closes_a_forgery_4300_without_automatic_sanctions_and_a_malformed_message_4001() {
    let rig = Rig::new(&[("AUTO_SANCTION_CERTAIN_CHEATS", "false")]).await;
    let (mut c, _) = rig.login(ALICE).await;
    c.raw(&[MsgType::Ack.to_u8(), 1, 0, 0, 0]).await;
    let (e, code) = c.refused().await;
    assert_eq!((e.code, code), (ErrorCode::ProtocolViolation, 4300));
    assert!(rig.anomalies.sanctions().is_empty());
    let (mut c, _) = rig.login(BOB).await;
    let mut trailing = QueueLeave { seq: 2 }.to_vec().unwrap();
    trailing.push(0);
    c.raw(&trailing).await;
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (2, ErrorCode::Malformed, 4001), "ref: the seq of the bytes");
    let a = rig.anomalies.anomalies();
    let malformed = a.iter().find(|a| a.kind == "malformed").expect("a malformed anomaly");
    assert_eq!(
        (malformed.user, malformed.detail.to_string()),
        (2, r#"{"reason":"trailing bytes","type":17}"#.into())
    );
}

#[tokio::test(start_paused = true)]
async fn answers_client_pings_once_a_second_and_measures_the_round_trip() {
    let rig = Rig::new(&[]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    for nonce in [77, 78] {
        let seq = c.next_seq();
        c.send(ClientPing { seq, nonce }).await;
    }
    let ServerMsg::Pong(pong) = c.until("S_Pong").await else { unreachable!() };
    assert_eq!(pong.nonce, 77);
    assert!((pong.server_time - rig.clock.mono_ms()).abs() < 5.0);
    let ServerMsg::Ping(ping) = c.until("S_Ping").await else { unreachable!() };
    tokio::time::sleep(Duration::from_millis(15)).await;
    let seq = c.next_seq();
    c.send(ClientPong { seq, nonce: ping.nonce }).await;
    let rtt = rig.host_calls(1, |c| matches!(c, HostCall::Rtt { .. })).await;
    let HostCall::Rtt { game: g, user, rtt_ms } = rtt[0].clone() else { unreachable!() };
    assert_eq!((g, user), (game, 1));
    assert!((15..=16).contains(&rtt_ms), "{rtt_ms}");
    let ep: Endpoint = rig.hosts.endpoint(game, 1).expect("attached");
    assert_eq!(ep.rtt_ms(), rtt_ms);
    // The second Pong: none (the next message is the next heartbeat).
    assert_eq!(name(&c.msg().await), "S_Ping");
}

#[tokio::test(start_paused = true)]
async fn pings_every_interval_from_half_an_interval_and_closes_a_silent_connection_1001() {
    let rig = Rig::new(&[]).await;
    let t0 = rig.clock.mono_ms();
    let (mut c, _) = rig.login(ALICE).await;
    let mut at = Vec::new();
    let mut nonces = Vec::new();
    for _ in 0..3 {
        let ServerMsg::Ping(p) = c.msg().await else { panic!("a ping") };
        at.push((rig.clock.mono_ms() - t0).round() as i64);
        nonces.push(p.nonce);
    }
    assert_eq!(at, [500, 1500, 2500]);
    assert_eq!(nonces, [1, 2, 3]);
    assert_eq!(c.closed().await, 1001);
    let silent = (rig.clock.mono_ms() - t0).round() as i64;
    assert!((3250..3300).contains(&silent), "{silent}");
}

#[tokio::test(start_paused = true)]
async fn leaves_out_of_the_round_trip_a_pong_whose_ping_preceded_a_stall() {
    let rig = Rig::new(&[]).await;
    rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    rig.hosts.set_stalled(true);
    let ServerMsg::Ping(p) = c.until("S_Ping").await else { unreachable!() };
    let seq = c.next_seq();
    c.send(ClientPong { seq, nonce: p.nonce }).await;
    let seq = c.next_seq();
    c.send(ClientPing { seq, nonce: 5 }).await;
    c.until("S_Pong").await;
    assert!(!rig.hosts.calls().iter().any(|c| matches!(c, HostCall::Rtt { .. })));
    rig.hosts.set_stalled(false);
    let ServerMsg::Ping(p) = c.until("S_Ping").await else { unreachable!() };
    let seq = c.next_seq();
    c.send(ClientPong { seq, nonce: p.nonce }).await;
    rig.host_calls(1, |c| matches!(c, HostCall::Rtt { .. })).await;
}

#[tokio::test(start_paused = true)]
async fn joins_the_queue_closing_the_rematch_window_and_maps_refusals() {
    let rig = Rig::new(&[]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    let seq = c.next_seq();
    c.send(QueueJoin { seq, category: "5+0".into(), rated: true }).await;
    let ServerMsg::Error(busy) = c.until("Error").await else { unreachable!() };
    assert_eq!((busy.r#ref, busy.code, busy.fatal), (seq, ErrorCode::AlreadyInGame, false));
    rig.game_ended(game);
    rig.settle().await;
    let seq = c.next_seq();
    c.send(QueueJoin { seq, category: "5+0".into(), rated: true }).await;
    assert_eq!(ack_of(&c.until("Ack").await), Some(seq));
    let ServerMsg::QueueStatus(q) = c.until("QueueStatus").await else { unreachable!() };
    assert_eq!((q.state, q.category.as_str()), (proto::QueueState::Searching, "5+0"));
    let declined: Vec<HostCall> =
        rig.hosts.calls().into_iter().filter(|c| matches!(c, HostCall::DeclineRematch { .. })).collect();
    assert_eq!(declined.len(), 2, "each join closes the window");
    assert_eq!(declined[0], HostCall::DeclineRematch { game, user: 1 });
    let seq = c.next_seq();
    c.send(QueueJoin { seq, category: "9+9".into(), rated: true }).await;
    let ServerMsg::Error(bad) = c.until("Error").await else { unreachable!() };
    assert_eq!((bad.r#ref, bad.code, bad.fatal), (seq, ErrorCode::InvalidCategory, false));
}

#[tokio::test(start_paused = true)]
async fn routes_game_messages_to_the_host_of_their_shard_or_refuses_them() {
    let rig = Rig::new(&[]).await;
    let (mut c, _) = rig.login(ALICE).await;
    let local = rig.hosts.next_id(2);
    let seq = c.next_seq();
    c.send(Resign { seq, game: local }).await;
    let calls = rig.host_calls(1, |c| matches!(c, HostCall::Client { .. })).await;
    let HostCall::Client { game, user, kind, recv_at } = calls[0].clone() else { unreachable!() };
    assert_eq!((game, user, kind), (local, 1, MsgType::Resign));
    assert!((recv_at - rig.clock.mono_ms()).abs() < 5.0);
    let foreign = crate::ids::GameIdAllocator::new(9).next(START);
    let seq = c.next_seq();
    c.send(Resign { seq, game: foreign }).await;
    let ServerMsg::Error(e) = c.until("Error").await else { unreachable!() };
    assert_eq!((e.r#ref, e.code, e.game, e.fatal), (seq, ErrorCode::NotInGame, foreign, false));
    let seq = c.next_seq();
    c.send(Resign { seq, game: 0 }).await;
    let ServerMsg::Error(e) = c.until("Error").await else { unreachable!() };
    assert_eq!((e.r#ref, e.code, e.game), (seq, ErrorCode::NotInGame, 0));
}

#[tokio::test(start_paused = true)]
async fn attaches_the_game_the_lobby_creates() {
    let rig = Rig::new(&[]).await;
    let (mut a, _) = rig.login(ALICE).await;
    let (mut b, _) = rig.login(BOB).await;
    for c in [&mut a, &mut b] {
        let seq = c.next_seq();
        c.send(QueueJoin { seq, category: "5+0".into(), rated: false }).await;
        assert_eq!(ack_of(&c.until("Ack").await), Some(seq));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    rig.lobby.post(LobbyMsg::Timer(Timer::MatchTick));
    for c in [&mut a, &mut b] {
        loop {
            if let ServerMsg::QueueStatus(q) = c.next().await
                && q.state == proto::QueueState::Matched
            {
                break;
            }
        }
    }
    let attached = rig.host_calls(2, |c| matches!(c, HostCall::Attach { .. })).await;
    let created = rig.hosts.created();
    assert_eq!(created.len(), 1);
    let users: Vec<UserId> = attached
        .iter()
        .map(|c| match c {
            HostCall::Attach { user, .. } => *user,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(users.len(), 2);
    assert!(users.contains(&1) && users.contains(&2));
}

#[tokio::test]
async fn keeps_the_eight_most_recent_games_attached() {
    let b = BucketRig::new(&[]).await;
    let mut session = b.session;
    let games: Vec<GameId> = (0..8).map(|_| b.rig.hosts.next_id(1)).collect();
    for &g in &games {
        session.attach(g);
    }
    let detached: Vec<HostCall> =
        b.rig.hosts.calls().into_iter().filter(|c| matches!(c, HostCall::Detach { .. })).collect();
    // The rig's own game was the first: it is the one detached.
    assert!(matches!(detached.as_slice(), [HostCall::Detach { game, user: 1, .. }] if *game == b.game));
    session.attach(games[0]);
    session.attach(b.rig.hosts.next_id(2));
    let detached: Vec<HostCall> =
        b.rig.hosts.calls().into_iter().filter(|c| matches!(c, HostCall::Detach { .. })).collect();
    assert!(
        matches!(detached.last(), Some(HostCall::Detach { game, .. }) if *game == games[1]),
        "the least recent"
    );
}

#[tokio::test(start_paused = true)]
async fn closes_a_slow_consumer_4303_without_an_error() {
    let rig = Rig::new(&[("WS_SEND_BUFFER_LIMIT", "4096")]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    rig.host_calls(1, |c| matches!(c, HostCall::Attach { .. })).await;
    let ep = rig.hosts.endpoint(game, 1).expect("attached");
    let notice = crate::realtime::frames::notice(NoticeCode::RatingRestored, 12.0);
    // The client reads nothing: the socket's buffer fills, then the queue.
    let mut sent = 0;
    while ep.send(notice.clone()) {
        sent += 1;
        if sent % 64 == 0 {
            tokio::task::yield_now().await;
        }
        assert!(sent < 1_000_000);
    }
    assert!(!ep.send(notice.clone()));
    let mut errors = 0;
    let code = loop {
        match c.recv().await {
            Rx::Msg(ServerMsg::Error(_)) => errors += 1,
            Rx::Msg(_) => {}
            Rx::Close(code) => break code,
            Rx::End => panic!("no close frame"),
        }
    };
    assert_eq!((code, errors), (4303, 0));
}

// ---- gestures -----------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn announces_no_gesture_relay_when_the_rate_is_zero() {
    let rig = Rig::new(&[("GESTURE_RATE", "0"), ("GESTURE_BURST", "20")]).await;
    let (_c, w) = rig.login(ALICE).await;
    assert_eq!((w.gesture_rate, w.gesture_burst), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn relays_the_raw_frame_drops_the_excess_silently_and_keeps_the_seq_in_step() {
    let rig = Rig::new(&[("GESTURE_RATE", "1"), ("GESTURE_BURST", "3")]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    let mut frames = Vec::new();
    for i in 0..6 {
        let seq = c.next_seq();
        let f = ClientGesture { seq, game, yaw: i, ..GESTURE }.to_vec().unwrap();
        c.raw(&f).await;
        frames.push(f);
    }
    let seq = c.next_seq();
    c.send(Resign { seq, game }).await;
    let calls = rig.host_calls(1, |c| matches!(c, HostCall::Client { .. })).await;
    assert_eq!(seq, 8, "accepted after the 3 gestures dropped");
    assert!(matches!(calls[0], HostCall::Client { kind: MsgType::Resign, .. }));
    let relayed: Vec<Bytes> = rig
        .hosts
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            HostCall::Gesture { game: g, user: 1, frame } if g == game => Some(frame),
            _ => None,
        })
        .collect();
    assert_eq!(
        relayed,
        frames[..3].iter().map(|f| Bytes::from(f.clone())).collect::<Vec<_>>(),
        "byte for byte"
    );
    assert!(rig.anomalies.anomalies().is_empty());
    // No RateLimited, no Error: the next message is a heartbeat.
    assert_eq!(name(&c.msg().await), "S_Ping");
}

#[tokio::test(start_paused = true)]
async fn spends_no_message_token_on_gestures() {
    let rig = Rig::new(&[
        ("WS_MSG_RATE", "1"),
        ("WS_MSG_BURST", "3"),
        ("GESTURE_RATE", "60"),
        ("GESTURE_BURST", "120"),
    ])
    .await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    for _ in 0..40 {
        c.gesture(game).await;
    }
    for _ in 0..3 {
        c.queue_leave().await;
    }
    let mut acks = Vec::new();
    while acks.len() < 3 {
        match c.next().await {
            ServerMsg::Ack(a) => acks.push(a.r#ref),
            ServerMsg::Error(e) => panic!("{e:?}"),
            _ => {}
        }
    }
    assert_eq!(acks, [42, 43, 44]);
    assert_eq!(rig.host_calls(40, |c| matches!(c, HostCall::Gesture { .. })).await.len(), 40);
}

#[tokio::test(start_paused = true)]
async fn drops_every_gesture_at_rate_zero_and_those_for_a_game_not_attached() {
    let rig = Rig::new(&[("GESTURE_RATE", "0")]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    for _ in 0..5 {
        c.gesture(game).await;
    }
    let seq = c.queue_leave().await;
    assert_eq!((seq, ack_of(&c.until("Ack").await)), (7, Some(7)));
    let rig = Rig::new(&[]).await;
    rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    c.gesture(rig.hosts.next_id(1)).await;
    let seq = c.queue_leave().await;
    assert_eq!(ack_of(&c.until("Ack").await), Some(seq));
    assert!(!rig.hosts.calls().iter().any(|c| matches!(c, HostCall::Gesture { .. })));
    assert!(rig.anomalies.anomalies().is_empty());
}

#[tokio::test(start_paused = true)]
async fn closes_a_gross_gesture_flood_4301_and_a_malformed_gesture_4001() {
    let rig = Rig::new(&[("GESTURE_RATE", "1"), ("GESTURE_BURST", "2")]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    for _ in 0..60 {
        c.gesture(game).await;
    }
    let (e, code) = c.refused().await;
    assert_eq!((e.code, code), (ErrorCode::Flood, 4301));
    let a = rig.anomalies.anomalies();
    // max(50, 10 x 2, ceil(1 x 4.5 s)) drops allowed.
    assert_eq!((a[0].kind, a[0].detail.to_string()), ("flood", r#"{"gestureDrops":51}"#.into()));
    assert_eq!(rig.host_calls(2, |c| matches!(c, HostCall::Gesture { .. })).await.len(), 2);

    let rig = Rig::new(&[]).await;
    let game = rig.game_in_progress().await;
    let (mut c, _) = rig.login(ALICE).await;
    let mut bad = ClientGesture { seq: 2, game, ..GESTURE }.to_vec().unwrap();
    let flags = bad.len() - 10;
    bad[flags] = 0xff; // flags above the GestureFlag bits
    c.raw(&bad).await;
    let (e, code) = c.refused().await;
    assert_eq!((e.r#ref, e.code, code), (2, ErrorCode::Malformed, 4001));
    assert_eq!(rig.anomalies.kinds(), ["malformed"]);
}

// ---- token buckets over time ----------------------------------------------------------------------

/// A session fed messages at chosen times, as the socket would deliver them.
struct BucketRig {
    rig: Rig,
    clock: Arc<ManualClock>,
    session: super::session::Session,
    out: Arc<crate::realtime::Outbound>,
    game: GameId,
    seq: u32,
    _io: DuplexStream,
}

impl BucketRig {
    async fn new(overrides: &[(&str, &str)]) -> BucketRig {
        let clock = ManualClock::new(1_000_000.0, START);
        // The default heartbeat, which sets the gesture flood threshold.
        let mut all = vec![("HEARTBEAT_INTERVAL_MS", "10000"), ("HEARTBEAT_TIMEOUT_MS", "30000")];
        all.extend_from_slice(overrides);
        let rig = Rig::with_clock(&all, clock.clone()).await;
        let (io, server) = tokio::io::duplex(1 << 16);
        let ws = rig.ws(server);
        let (ep, out) = Endpoint::for_tests(ws.info.id(), 1);
        let (link, _cmds) = ConnLink::new(ep, "alice".into(), [0; 32]);
        let mut session = super::session::Session::new(rig.ctx.clone(), link, ws.info.clone());
        let game = rig.hosts.next_id(0);
        session.attach(game);
        BucketRig { rig, clock, session, out, game, seq: 1, _io: io }
    }

    fn gesture(&mut self, at: f64) {
        self.clock.set_mono(at);
        self.seq += 1;
        let f = ClientGesture { seq: self.seq, game: self.game, ..GESTURE }.to_bytes().unwrap();
        self.session.on_message(f);
    }

    fn relayed(&self) -> usize {
        self.rig.hosts.calls().iter().filter(|c| matches!(c, HostCall::Gesture { .. })).count()
    }

    /// Not closed, no anomaly.
    fn healthy(&self) -> bool {
        self.out.is_open() && self.rig.anomalies.anomalies().is_empty()
    }

    /// A client pacing its gestures at `rate` for `ms` from `t`; returns the end.
    fn paced(&mut self, rate: f64, mut t: f64, ms: f64) -> f64 {
        for _ in 0..(ms * rate / 1000.0) as usize {
            t += 1000.0 / rate;
            self.gesture(t);
        }
        t
    }

    /// The same client while nothing is read for `ms`: its gestures arrive together at the end.
    fn stalled(&mut self, rate: f64, t: f64, ms: f64) -> f64 {
        let t = t + ms;
        for _ in 0..(ms * rate / 1000.0) as usize {
            self.gesture(t);
        }
        t
    }
}

#[tokio::test]
async fn lets_a_client_pacing_its_gestures_live_through_a_stall_that_delivers_them_together() {
    for (rate, stall) in [(4.0, 35_000.0), (20.0, 5000.0), (20.0, 38_000.0), (60.0, 2500.0), (60.0, 40_000.0)]
    {
        let rate_s = format!("{rate}");
        let mut b = BucketRig::new(&[("GESTURE_RATE", &rate_s)]).await;
        let burst = b.rig.config().gesture_burst as usize;
        let per_s = rate as usize;
        let t = b.paced(rate, 1_000_000.0, 10_000.0);
        assert_eq!(b.relayed(), 10 * per_s, "rate {rate}: nothing dropped while paced");
        let t = b.stalled(rate, t, stall);
        assert_eq!(b.relayed(), 10 * per_s + burst, "rate {rate}: one bucket of the burst relayed");
        b.paced(rate, t, 20_000.0);
        assert!(b.healthy(), "rate {rate}, {stall} ms stall: no flood");
        assert_eq!(b.relayed(), 30 * per_s + burst, "rate {rate}, {stall} ms stall: the relay goes on");
    }
}

#[tokio::test]
async fn still_closes_a_gross_gesture_flood_whatever_the_rate() {
    for rate in [1.0, 4.0, 60.0] {
        let rate_s = format!("{rate}");
        let mut b = BucketRig::new(&[("GESTURE_RATE", &rate_s)]).await;
        let allowed = b.rig.ctx.settings.gesture_flood_drops as usize;
        let burst = b.rig.config().gesture_burst as usize;
        for _ in 0..burst + allowed + 1 {
            b.gesture(1_000_000.0);
        }
        assert_eq!(b.rig.anomalies.kinds(), ["flood"], "rate {rate}");
        assert_eq!(b.out.close_request().map(|c| c.code), Some(4301));
    }
}
