//! Tests of the WebSocket session, the realtime connection, the bot and the API client against
//! fake servers written in the tests: a WebSocket server end (handshake, unmasked frames, checks
//! of the client's masked frames), a TLS server, and a scripted HTTP server.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use scacelith_protocol::{
    Ack, ClientMsg, Color, EndReason, ErrorCode, GameEnd, GameSnapshot, GameStatus, Message, MoveMade,
    PlayerInfo, QueueLeave, Resign, ServerMsg, ServerPing, Welcome, decode_hello,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::bot::{Bot, BotConfig};
use crate::http::find;
use crate::ws::frame::{self, Frame, OP_BINARY, OP_CLOSE, OP_CONTINUATION, OP_PING, OP_PONG, OP_TEXT, Role};
use crate::ws::{Session, SessionOptions, accept_key};
use crate::{
    ApiClient, ClientError, CloseInfo, Closer, ConnectOptions, Connection, Endpoint, Login, TlsConfig,
};

const TOKEN: &str = "sct_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const WAIT: Duration = Duration::from_secs(5);

/// The server end of one WebSocket connection.
struct FakeServer<S = TcpStream> {
    stream: S,
    buf: BytesMut,
    request: String,
}

async fn listen() -> (TcpListener, Endpoint) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint::plain(listener.local_addr().unwrap());
    (listener, endpoint)
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A standard `101` answer for `request`.
fn switching(request: &str) -> String {
    let key = header(request, "sec-websocket-key").unwrap();
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: scacelith.rt1\r\nScacelith-Server-Id: test-server\r\n\r\n",
        accept_key(key)
    )
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> FakeServer<S> {
    /// Reads the upgrade request and writes `answer(request)`.
    async fn accept_with(mut stream: S, answer: impl FnOnce(&str) -> String) -> FakeServer<S> {
        let mut buf = BytesMut::new();
        let end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i;
            }
            assert!(stream.read_buf(&mut buf).await.unwrap() > 0, "request head");
        };
        let head = buf.split_to(end + 4);
        let request = String::from_utf8(head.to_vec()).unwrap();
        stream.write_all(answer(&request).as_bytes()).await.unwrap();
        FakeServer { stream, buf, request }
    }

    /// The next client frame (masked, as the parser requires).
    async fn frame(&mut self) -> Frame {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(frame) = frame::parse_frame(&mut self.buf, Role::Server, 1 << 20).unwrap() {
                    return frame;
                }
                assert!(self.stream.read_buf(&mut self.buf).await.unwrap() > 0, "client closed the stream");
            }
        })
        .await
        .expect("a client frame in time")
    }

    /// The next client message, decoded strictly (as the server does).
    async fn msg(&mut self) -> ClientMsg {
        let frame = self.frame().await;
        assert_eq!((frame.fin, frame.opcode), (true, OP_BINARY));
        match frame.payload.first() {
            Some(0x01) => ClientMsg::Hello(decode_hello(&frame.payload).unwrap()),
            _ => ClientMsg::decode(&frame.payload).unwrap(),
        }
    }

    async fn send_frame(&mut self, fin: bool, opcode: u8, payload: &[u8]) {
        let mut out = BytesMut::new();
        frame::encode_frame(&mut out, fin, opcode, payload, None);
        self.stream.write_all(&out).await.unwrap();
    }

    async fn send(&mut self, msg: impl Into<ServerMsg>) {
        let bytes = msg.into().to_bytes().unwrap();
        self.send_frame(true, OP_BINARY, &bytes).await;
    }

    async fn close(&mut self, code: u16, reason: &str) {
        self.send_frame(true, OP_CLOSE, &frame::close_payload(code, reason)).await;
    }

    /// Reads the Hello and answers a Welcome for `username`.
    async fn welcome(&mut self, username: &str) {
        let ClientMsg::Hello(hello) = self.msg().await else { panic!("Hello first") };
        assert_eq!((hello.seq, hello.proto, hello.minor, hello.caps), (1, 1, 0, 0));
        assert_eq!(hello.token, TOKEN);
        self.send(welcome_msg(username)).await;
    }
}

impl FakeServer<TcpStream> {
    async fn accept(listener: &TcpListener) -> FakeServer<TcpStream> {
        let (stream, _) = listener.accept().await.unwrap();
        FakeServer::accept_with(stream, switching).await
    }
}

fn welcome_msg(username: &str) -> Welcome {
    Welcome {
        proto: 1,
        server_time: 1.79e12,
        user_id: 7,
        username: username.into(),
        server_name: "Fake".into(),
        heartbeat_ms: 10_000,
        max_msg_per_sec: 20,
        msg_burst: 40,
        gesture_rate: 20,
        gesture_burst: 40,
        ..Welcome::default()
    }
}

/// A connected client and its fake server, past the Welcome.
async fn connected() -> (Connection, FakeServer) {
    let (listener, endpoint) = listen().await;
    let server = tokio::spawn(async move {
        let mut s = FakeServer::accept(&listener).await;
        s.welcome("alice").await;
        s
    });
    let conn = Connection::connect(&endpoint, TOKEN, &ConnectOptions::default()).await.unwrap();
    (conn, server.await.unwrap())
}

#[tokio::test]
async fn handshake_hello_and_welcome() {
    let (conn, server) = connected().await;
    let r = &server.request;
    assert!(r.starts_with("GET /ws HTTP/1.1\r\n"), "{r}");
    assert_eq!(header(r, "sec-websocket-protocol"), Some("scacelith.rt1"));
    assert_eq!(header(r, "sec-websocket-version"), Some("13"));
    assert_eq!(header(r, "upgrade"), Some("websocket"));
    assert_eq!(header(r, "host").map(|h| h.starts_with("127.0.0.1:")), Some(true));
    assert!(header(r, "origin").is_none(), "no Origin header");
    assert_eq!((conn.username(), conn.user_id()), ("alice", 7));
    assert_eq!(conn.session().upgrade_header("scacelith-server-id"), Some("test-server"));
    assert!(conn.timings().hello > Duration::ZERO);
}

#[tokio::test]
async fn messages_are_numbered_and_pings_answered() {
    let (mut conn, mut server) = connected().await;
    assert_eq!(conn.send(QueueLeave { seq: 999 }).unwrap(), 2);
    assert_eq!(server.msg().await, ClientMsg::QueueLeave(QueueLeave { seq: 2 }));
    server.send(ServerPing { nonce: 77, server_time: 1.0 }).await;
    match server.msg().await {
        ClientMsg::Pong(p) => assert_eq!((p.seq, p.nonce), (3, 77)),
        other => panic!("expected the automatic Pong, got {other:?}"),
    }
    // The ping is delivered too.
    assert!(matches!(conn.recv_timeout(WAIT).await.unwrap(), ServerMsg::Ping(p) if p.nonce == 77));
    assert_eq!(conn.send(Resign { seq: 0, game: 5 }).unwrap(), 4);
    assert_eq!(server.msg().await, ClientMsg::Resign(Resign { seq: 4, game: 5 }));
    assert_eq!(conn.session().stats().auto_replies, 1);
}

#[tokio::test]
async fn automatic_answers_never_overtake_queued_messages() {
    let (conn, mut server) = connected().await;
    let conn = Arc::new(conn);
    let sender = {
        let conn = conn.clone();
        tokio::spawn(async move {
            for i in 0..300u64 {
                conn.send(Resign { seq: 0, game: i }).unwrap();
                if i % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        })
    };
    for nonce in 0..30 {
        server.send(ServerPing { nonce, server_time: 1.0 }).await;
    }
    let mut expected = 2;
    let (mut resigns, mut pongs) = (0, 0);
    while resigns < 300 || pongs < 30 {
        let msg = server.msg().await;
        assert_eq!(msg.seq(), expected, "consecutive seqs on the wire");
        expected += 1;
        match msg {
            ClientMsg::Resign(r) => {
                assert_eq!(r.game, resigns);
                resigns += 1;
            }
            ClientMsg::Pong(p) => {
                assert_eq!(p.nonce, pongs);
                pongs += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    sender.await.unwrap();
}

#[tokio::test]
async fn fragmented_messages_and_websocket_pings() {
    let (mut conn, mut server) = connected().await;
    let end = GameEnd {
        game: 42,
        gseq: 3,
        status: GameStatus::Draw,
        reason: EndReason::Agreement,
        white_ms: 1000,
        black_ms: 2000,
        server_time: 5.5,
    };
    let bytes = end.to_vec().unwrap();
    server.send_frame(false, OP_BINARY, &bytes[..10]).await;
    server.send_frame(true, OP_PING, b"are you there").await;
    server.send_frame(false, OP_CONTINUATION, &bytes[10..20]).await;
    server.send_frame(true, OP_CONTINUATION, &bytes[20..]).await;
    assert_eq!(conn.recv_timeout(WAIT).await.unwrap(), ServerMsg::GameEnd(end));
    let pong = server.frame().await;
    assert_eq!((pong.opcode, &pong.payload[..]), (OP_PONG, &b"are you there"[..]));
}

#[tokio::test]
async fn server_close_after_a_fatal_error() {
    let (mut conn, mut server) = connected().await;
    let error = scacelith_protocol::Error { r#ref: 0, code: ErrorCode::Replaced, fatal: true, game: 0 };
    server.send(error.clone()).await;
    server.close(4007, "replaced").await;
    assert_eq!(conn.recv_timeout(WAIT).await.unwrap(), ServerMsg::Error(error));
    let Err(ClientError::Closed(info)) = conn.recv_timeout(WAIT).await else { panic!("closed") };
    assert_eq!(info, CloseInfo { code: 4007, reason: "replaced".into(), closer: Closer::Server });
    assert_eq!(info.error_code(), Some(ErrorCode::Replaced));
    let echo = server.frame().await;
    assert_eq!((echo.opcode, frame::parse_close(&echo.payload).unwrap().0), (OP_CLOSE, 4007));
    assert!(conn.send(QueueLeave { seq: 0 }).is_err(), "nothing is sent after the end");
}

#[tokio::test]
async fn client_close_handshake() {
    let (mut conn, mut server) = connected().await;
    conn.send(QueueLeave { seq: 0 }).unwrap();
    conn.close();
    assert!(matches!(server.msg().await, ClientMsg::QueueLeave(_)), "queued messages go first");
    let close = server.frame().await;
    assert_eq!(frame::parse_close(&close.payload).unwrap(), (1000, String::new()));
    server.close(1000, "").await;
    let info = tokio::time::timeout(WAIT, conn.wait_closed()).await.unwrap();
    assert_eq!((info.code, info.closer), (1000, Closer::Client));
}

#[tokio::test]
async fn an_aborted_connection_ends_without_a_close_frame() {
    let (mut conn, mut server) = connected().await;
    conn.send(QueueLeave { seq: 0 }).unwrap();
    conn.abort();
    assert!(matches!(server.msg().await, ClientMsg::QueueLeave(_)), "queued messages go first");
    // The stream ends: no close frame, no more bytes.
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, server.stream.read_to_end(&mut rest)).await.unwrap();
    assert!(read.is_err() || rest.is_empty(), "nothing after the last message: {rest:?}");
    let info = tokio::time::timeout(WAIT, conn.wait_closed()).await.unwrap();
    assert_eq!((info.code, info.closer), (CloseInfo::ABNORMAL, Closer::Transport));
}

#[tokio::test]
async fn hello_refused() {
    let (listener, endpoint) = listen().await;
    tokio::spawn(async move {
        let mut s = FakeServer::accept(&listener).await;
        assert!(matches!(s.msg().await, ClientMsg::Hello(_)));
        s.send(scacelith_protocol::Error { r#ref: 1, code: ErrorCode::Unauthorized, fatal: true, game: 0 })
            .await;
        s.close(4003, "unauthorized").await;
        let _ = s.frame().await;
    });
    let err = Connection::connect(&endpoint, TOKEN, &ConnectOptions::default()).await.unwrap_err();
    let ClientError::Refused { code, close } = err else { panic!("{err}") };
    assert_eq!(code, ErrorCode::Unauthorized);
    assert_eq!(close.map(|c| c.code), Some(4003));
}

#[tokio::test]
async fn upgrade_refusals_and_bad_answers() {
    async fn try_answer(answer: fn(&str) -> String) -> ClientError {
        let (listener, endpoint) = listen().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut s = FakeServer::accept_with(stream, answer).await;
            let _ = s.stream.read_buf(&mut s.buf).await;
        });
        Session::connect(&endpoint, &SessionOptions::new("scacelith.rt1")).await.unwrap_err()
    }
    let err = try_answer(|_| {
        let body = r#"{"error":"unsupported_protocol","supported":["scacelith.rt1"]}"#;
        format!("HTTP/1.1 426 Upgrade Required\r\nContent-Length: {}\r\n\r\n{body}", body.len())
    })
    .await;
    let ClientError::UpgradeRefused { status, body, .. } = err else { panic!("{err}") };
    assert_eq!(status, 426);
    assert!(body.starts_with(b"{\"error\":\"unsupported_protocol\""));

    let err = try_answer(|_| {
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Length: 0\r\n\r\n".into()
    })
    .await;
    assert!(matches!(err, ClientError::UpgradeRefused { status: 429, retry_after: Some(3), .. }), "{err}");

    let err = try_answer(|r| switching(r).replace("Sec-WebSocket-Accept: ", "Sec-WebSocket-Accept: x")).await;
    assert!(matches!(&err, ClientError::WebSocket(e) if e.contains("Accept")), "{err}");
    let err = try_answer(|r| switching(r).replace("Sec-WebSocket-Protocol: scacelith.rt1\r\n", "")).await;
    assert!(matches!(&err, ClientError::WebSocket(e) if e.contains("subprotocol")), "{err}");
    let err = try_answer(|r| {
        switching(r).replace("\r\n\r\n", "\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n")
    })
    .await;
    assert!(matches!(&err, ClientError::WebSocket(e) if e.contains("extension")), "{err}");
}

/// A session whose fake server sends `bytes` raw after the handshake; returns how it ended and
/// the client's close frame, if any.
async fn session_end(bytes: Vec<u8>, max_message: usize) -> (CloseInfo, Option<(u16, String)>) {
    let (listener, endpoint) = listen().await;
    let server = tokio::spawn(async move {
        let mut s = FakeServer::accept(&listener).await;
        s.stream.write_all(&bytes).await.unwrap();
        if bytes.is_empty() {
            return None; // drop the TCP connection without a close frame
        }
        let close = s.frame().await;
        assert_eq!(close.opcode, OP_CLOSE);
        Some(frame::parse_close(&close.payload).unwrap())
    });
    let mut opts = SessionOptions::new("scacelith.rt1");
    opts.max_message = max_message;
    let mut session = Session::connect(&endpoint, &opts).await.unwrap();
    let info = tokio::time::timeout(WAIT, session.wait_closed()).await.unwrap();
    (info, server.await.unwrap())
}

#[tokio::test]
async fn protocol_errors_close_the_connection() {
    let mut masked = BytesMut::new();
    frame::encode_frame(&mut masked, true, OP_BINARY, b"x", Some([1, 2, 3, 4]));
    let (info, sent) = session_end(masked.to_vec(), 1024).await;
    assert_eq!((info.code, info.closer), (1002, Closer::Client));
    assert_eq!(sent.map(|s| s.0), Some(1002));

    let (info, _) = session_end(vec![0x81, 0x02, b'h', b'i'], 1024).await;
    assert_eq!(info.code, 1003, "text message");
    let (info, _) = session_end(vec![0x82, 126, 0x04, 0x01], 1024).await;
    assert_eq!(info.code, 1009, "message too big");
    let (info, _) = session_end(vec![0x80, 0x01, 0x00], 1024).await;
    assert_eq!(info.code, 1002, "continuation without a message");
    let (info, _) = session_end(vec![0xA2, 0x00], 1024).await;
    assert_eq!(info.code, 1002, "reserved bit");
    let mut too_long = Vec::new();
    for part in [(false, OP_BINARY), (true, OP_CONTINUATION)] {
        let mut out = BytesMut::new();
        frame::encode_frame(&mut out, part.0, part.1, &[0; 600], None);
        too_long.extend_from_slice(&out);
    }
    let (info, _) = session_end(too_long, 1000).await;
    assert_eq!(info.code, 1009, "fragments over the limit");
    let (info, sent) = session_end(Vec::new(), 1024).await;
    assert_eq!((info.code, info.closer, sent), (1006, Closer::Transport, None));
}

#[tokio::test]
async fn unknown_and_undecodable_messages_are_skipped() {
    let (mut conn, mut server) = connected().await;
    server.send_frame(true, OP_BINARY, &[0xEE, 1, 2, 3]).await;
    server.send_frame(true, OP_BINARY, &[0x84, 1]).await; // a truncated Ack
    server.send(Ack { r#ref: 9 }).await;
    assert_eq!(conn.recv_timeout(WAIT).await.unwrap(), ServerMsg::Ack(Ack { r#ref: 9 }));
    assert_eq!(conn.ignored_messages(), 2);
    assert!(conn.last_decode_error().is_some());
}

#[tokio::test]
async fn raw_frames_for_negative_tests() {
    let (conn, mut server) = connected().await;
    let session = conn.session();
    session.send_unsequenced(&[0x11, 9, 9, 9, 9]).unwrap();
    session.send_frame(&Frame::new(OP_TEXT, Bytes::from_static(b"text")), true).unwrap();
    session.send_frame(&Frame::new(OP_BINARY, Bytes::from_static(b"plain")), false).unwrap();
    assert_eq!(server.frame().await.payload, Bytes::from_static(&[0x11, 9, 9, 9, 9]));
    assert_eq!(server.frame().await.opcode, OP_TEXT);
    // The unmasked frame is refused by the server-side parser.
    let refusal = loop {
        match frame::parse_frame(&mut server.buf, Role::Server, 1024) {
            Err(e) => break e,
            Ok(None) => assert!(server.stream.read_buf(&mut server.buf).await.unwrap() > 0),
            Ok(Some(f)) => panic!("unexpected frame {f:?}"),
        }
    };
    assert_eq!(refusal.code, 1002);
    assert_eq!(session.next_seq(), 2, "raw frames take no seq");
    assert!(session.send(&[1, 2]).is_err(), "a numbered message has a seq");
}

fn snapshot(game: u64, you: Color) -> GameSnapshot {
    let player =
        |id, name: &str| PlayerInfo { user_id: id, name: name.into(), rating: 1500, provisional: true };
    GameSnapshot {
        game,
        category: "3+2".into(),
        base_ms: 180_000,
        inc_ms: 2000,
        white: player(7, "alice"),
        black: player(8, "bob"),
        you,
        running: Color::None,
        draw_offer: Color::None,
        rematch: Color::None,
        white_ms: 180_000,
        black_ms: 180_000,
        white_connected: true,
        black_connected: true,
        first_move_ms: 30_000,
        ..GameSnapshot::default()
    }
}

#[tokio::test]
async fn bot_finds_a_game_and_plays_legal_moves() {
    let (conn, mut server) = connected().await;
    let script = tokio::spawn(async move {
        let ClientMsg::QueueJoin(join) = server.msg().await else { panic!("QueueJoin") };
        assert_eq!((join.category.as_str(), join.rated), ("3+2", false));
        server.send(Ack { r#ref: join.seq }).await;
        server.send(snapshot(4242, Color::White)).await;
        let mut game = crate::bot::GameTracker::new(4242, Color::White, []).unwrap();
        let mut gestures = 0;
        while game.ply() < 6 {
            match server.msg().await {
                ClientMsg::Move(m) => {
                    assert!(game.is_my_turn(), "the bot moves only on its turn");
                    assert_eq!((m.game, m.ply, m.pos_hash), (4242, game.ply(), game.position().digest()));
                    assert!(game.position().is_legal(m.r#move));
                    for ply_move in [m.r#move] {
                        let mm = MoveMade {
                            game: 4242,
                            gseq: u32::from(m.ply) + 1,
                            ply: m.ply,
                            r#move: ply_move,
                            ..MoveMade::default()
                        };
                        game.apply(m.ply, ply_move);
                        server.send(mm).await;
                    }
                    // The opponent answers with its own random move.
                    let reply = game.random_move(&mut crate::bot::Rng::new(u64::from(m.ply))).unwrap();
                    let ply = game.ply();
                    game.apply(ply, reply);
                    server
                        .send(MoveMade {
                            game: 4242,
                            gseq: u32::from(ply) + 1,
                            ply,
                            r#move: reply,
                            ..MoveMade::default()
                        })
                        .await;
                }
                ClientMsg::Gesture(g) => {
                    assert_eq!((g.game, g.touch, g.aim), (4242, 64, 64));
                    gestures += 1;
                }
                other => panic!("{other:?}"),
            }
        }
        server
            .send(GameEnd {
                game: 4242,
                gseq: 7,
                status: GameStatus::WhiteWins,
                reason: EndReason::Resignation,
                ..GameEnd::default()
            })
            .await;
        gestures
    });
    let cfg = BotConfig {
        move_delay: Duration::from_millis(30),
        gesture_hz: 100.0,
        seed: Some(1),
        ..BotConfig::default()
    };
    let mut bot = Bot::new(conn, cfg);
    let game = bot.join_queue("3+2", false).await.unwrap();
    assert!(game.is_my_turn());
    let result = tokio::time::timeout(WAIT, bot.play(game)).await.unwrap().unwrap();
    assert_eq!((result.end.reason, result.plies, result.moves_sent), (EndReason::Resignation, 6, 3));
    let gestures = script.await.unwrap();
    assert!(gestures > 0 && result.gestures_sent >= gestures);
}

#[tokio::test]
async fn tls_trust_settings() {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_der = certified.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let server_config =
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert_der.clone())], key)
            .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    let mut s = FakeServer::accept_with(tls, switching).await;
                    s.welcome("tls-user").await;
                    let _ = s.frame().await;
                }
            });
        }
    });
    let opts = ConnectOptions::default();
    let pinned =
        Endpoint::tls(addr, "localhost", TlsConfig::with_root_der(&cert_der).unwrap().without_resumption());
    let conn = Connection::connect(&pinned, TOKEN, &opts).await.unwrap();
    assert_eq!(conn.username(), "tls-user");
    assert!(conn.timings().tls > Duration::ZERO);

    let any = Endpoint::tls(addr, "other.name", TlsConfig::dangerous_accept_any_certificate());
    assert!(any.tls_config().unwrap().accepts_any_certificate());
    assert_eq!(Connection::connect(&any, TOKEN, &opts).await.unwrap().username(), "tls-user");

    let public = Endpoint::tls(addr, "localhost", TlsConfig::webpki_roots());
    assert!(matches!(Connection::connect(&public, TOKEN, &opts).await, Err(ClientError::Io(_))));
    let wrong_name = Endpoint::tls(addr, "example.org", TlsConfig::with_root_der(&cert_der).unwrap());
    assert!(Connection::connect(&wrong_name, TOKEN, &opts).await.is_err());
    assert!(TlsConfig::with_root_pem(b"not a certificate").is_err());
}

/// A scripted HTTP server: answers each request with the next of `answers`; returns the requests
/// it read (head and body), and how many TCP connections they came on.
async fn http_server(answers: Vec<String>) -> (Endpoint, tokio::task::JoinHandle<(Vec<String>, usize)>) {
    let (listener, endpoint) = listen().await;
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut connections = 0;
        let mut answers = answers.into_iter();
        'conns: while answers.len() > 0 {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await
            else {
                break;
            };
            connections += 1;
            let mut buf = BytesMut::new();
            loop {
                let end = loop {
                    if let Some(i) = find(&buf, b"\r\n\r\n") {
                        break i;
                    }
                    if stream.read_buf(&mut buf).await.unwrap() == 0 {
                        continue 'conns;
                    }
                };
                let head = String::from_utf8(buf.split_to(end + 4).to_vec()).unwrap();
                let len: usize = header(&head, "content-length").map_or(0, |v| v.parse().unwrap());
                while buf.len() < len {
                    stream.read_buf(&mut buf).await.unwrap();
                }
                let body = String::from_utf8(buf.split_to(len).to_vec()).unwrap();
                requests.push(format!("{head}{body}"));
                let Some(answer) = answers.next() else { break 'conns };
                stream.write_all(answer.as_bytes()).await.unwrap();
                if answers.len() == 0 {
                    break 'conns;
                }
            }
        }
        (requests, connections)
    });
    (endpoint, task)
}

fn json_answer(status: &str, body: serde_json::Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn api_client_flows() {
    let session =
        json!({"token": TOKEN, "expiresAt": 1_800_000_000_000i64, "user": {"id": 1, "username": "alice"}});
    let (endpoint, server) = http_server(vec![
        json_answer("200 OK", json!({"name": "Fake", "protocol": {"min": 1, "max": 1}})),
        json_answer("201 Created", json!({"status": "ready"})),
        json_answer("200 OK", json!({"mfaRequired": true, "mfaToken": "mfa_x", "expiresIn": 300})),
        json_answer("200 OK", session.clone()),
        json_answer("401 Unauthorized", json!({"error": "invalid_credentials", "message": "Wrong."})),
        json_answer("200 OK", json!({"status": "logged_out"})),
    ])
    .await;
    let api = ApiClient::new(endpoint);
    assert_eq!(api.info().await.unwrap()["protocol"]["max"], 1);
    assert_eq!(
        api.register("alice", "a@example.org", "correct horse battery").await.unwrap()["status"],
        "ready"
    );
    let Login::MfaRequired { mfa_token, expires_in } =
        api.login("alice", "pw-pw-pw-pw", Some("tests")).await.unwrap()
    else {
        panic!("second step")
    };
    assert_eq!((mfa_token.as_str(), expires_in), ("mfa_x", 300));
    let s = api.login_mfa(&mfa_token, "123456").await.unwrap();
    assert_eq!((s.token.as_str(), s.user["username"].as_str()), (TOKEN, Some("alice")));
    let err = api.login("alice", "wrong-password", None).await.unwrap_err();
    let ClientError::Api(e) = err else { panic!("{err}") };
    assert_eq!((e.status, e.error.as_str(), e.message.as_str()), (401, "invalid_credentials", "Wrong."));
    api.logout(TOKEN).await.unwrap();

    let (requests, connections) = server.await.unwrap();
    assert_eq!(connections, 1, "one keep-alive connection");
    assert!(requests[0].starts_with("GET /api/v1/info HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("POST /api/v1/auth/register HTTP/1.1\r\n"));
    assert!(requests[1].contains("Content-Type: application/json\r\n"));
    assert!(
        requests[1]
            .ends_with(r#"{"username":"alice","email":"a@example.org","password":"correct horse battery"}"#)
    );
    assert!(requests[2].ends_with(r#"{"login":"alice","password":"pw-pw-pw-pw","clientLabel":"tests"}"#));
    assert!(requests[3].ends_with(r#"{"mfaToken":"mfa_x","code":"123456"}"#));
    assert!(requests[5].starts_with("POST /api/v1/auth/logout HTTP/1.1\r\n"));
    assert!(requests[5].contains(&format!("Authorization: Bearer {TOKEN}\r\n")));
}

#[tokio::test]
async fn connections_open_from_the_chosen_local_address() {
    let (listener, endpoint) = listen().await;
    let from = "127.0.0.2".parse().unwrap();
    let endpoint = endpoint.with_local_addr(from);
    assert_eq!(endpoint.local_addr(), Some(from));
    let (connected, accepted) = tokio::join!(endpoint.connect(), listener.accept());
    let (stream, _) = connected.unwrap();
    let (_, peer) = accepted.unwrap();
    assert_eq!(peer.ip(), from);
    assert_eq!(stream.tcp().local_addr().unwrap().ip(), from);
}
