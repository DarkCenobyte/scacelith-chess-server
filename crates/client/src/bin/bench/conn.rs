//! The two protocol adapters behind one interface: the scenarios send [`Cmd`]s and read
//! [`Event`]s, whatever the server speaks.
//!
//! * [`Target::Rust`]: protocol v1 (`scacelith.rt1`) through the SDK's [`Connection`];
//! * [`Target::Node`]: protocol 3 (`scacelith.v1`) through the SDK's WebSocket [`Session`] and
//!   the bench's own minimal codec ([`crate::proto3`]).
//!
//! The WebSocket, TLS and REST code is the same for both, so only the message codec differs.

use std::time::{Duration, Instant};

use scacelith_client::ws::{Session, SessionOptions};
use scacelith_client::{ClientError, ConnectOptions, Connection, Endpoint};
use scacelith_protocol::{
    Abort, ChallengeAccept, ChallengeCreate, ClientGesture, ClientMsg, Color, ColorPref, GameStatus, Move,
    QueueJoin, QueueLeave, Resign, Resync, ServerMsg,
};

use crate::proto3;

/// Which server, hence which protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The Rust server: protocol v1.
    Rust,
    /// The Node server: protocol 3.
    Node,
}

impl Target {
    /// The name used on the command line and in reports.
    pub fn name(self) -> &'static str {
        match self {
            Target::Rust => "rust",
            Target::Node => "node",
        }
    }

    /// Parses a command-line name.
    pub fn parse(s: &str) -> Option<Target> {
        match s {
            "rust" => Some(Target::Rust),
            "node" => Some(Target::Node),
            _ => None,
        }
    }
}

/// What the `Welcome` said that the scenarios use.
#[derive(Clone, Debug)]
pub struct WelcomeInfo {
    /// The account's name.
    pub username: String,
    /// A running game of the account (0 = none).
    pub active_game: u64,
    /// Gestures relayed per second (0 = no relay).
    pub gesture_rate: u16,
}

/// The parts of a `GameSnapshot` the scenarios use.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Game id.
    pub game: u64,
    /// Whether the receiver plays White.
    pub you_white: bool,
    /// Moves played, packed.
    pub moves: Vec<u16>,
    /// Whether the game has ended.
    pub over: bool,
}

impl Snapshot {
    /// The receiver's colour.
    pub fn you(&self) -> Color {
        if self.you_white { Color::White } else { Color::Black }
    }
}

/// A server message, reduced to what the scenarios look at.
#[derive(Clone, Debug)]
pub enum Event {
    /// A request was refused (`ref`: its `seq`; `code`: the ErrorCode).
    Error { r#ref: u32, code: u8 },
    /// A challenge arrived.
    ChallengeReceived { id: u32 },
    /// A game's full state (start, resynchronisation).
    Snapshot(Snapshot),
    /// A move was played.
    MoveMade { game: u64, ply: u16, mv: u16 },
    /// An own move intent was refused (`code`: the ErrorCode).
    MoveRejected { game: u64, code: u8 },
    /// A game ended.
    GameEnd { game: u64 },
    /// The opponent's gesture (only the yaw is read: it carries the sequence number).
    Gesture { game: u64, yaw: i32 },
    /// Anything else (acknowledgements, queue and challenge states, notices...).
    Other,
}

/// A client request.
#[derive(Clone, Debug)]
pub enum Cmd<'a> {
    /// Join a matchmaking queue.
    QueueJoin { category: &'a str, rated: bool },
    /// Leave the matchmaking queue.
    QueueLeave,
    /// Challenge a player (colour random).
    Challenge { target: &'a str, base_sec: u16, inc_sec: u8, rated: bool },
    /// Accept a challenge.
    Accept { id: u32 },
    /// A move intent.
    Move { game: u64, ply: u16, mv: u16, pos_hash: u32, think_ms: u32 },
    /// Resign.
    Resign { game: u64 },
    /// Abort before the first move.
    Abort { game: u64 },
    /// Ask for the game's snapshot.
    Resync { game: u64 },
    /// A head gesture.
    Gesture { game: u64, ply: u16, yaw: i32, pitch: i32, lean: u8 },
}

/// How long each step of a connection took.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    /// TCP connect, TLS handshake and WebSocket upgrade.
    pub handshake: Duration,
    /// `Hello` sent to `Welcome` read.
    pub hello: Duration,
}

/// A connection failure, by class (the report counts failures per class).
#[derive(Debug)]
pub struct ConnectFailure {
    /// Short class: `timeout`, `tls`, `io`, `upgrade_503`, `refused_6`, `closed_4006`...
    pub class: String,
    /// The full error.
    pub detail: String,
}

impl ConnectFailure {
    fn of(e: &ClientError) -> ConnectFailure {
        let class = match e {
            ClientError::Timeout(_) => "timeout".to_string(),
            ClientError::Tls(_) => "tls".to_string(),
            ClientError::Io(io) => format!("io_{:?}", io.kind()).to_lowercase(),
            ClientError::UpgradeRefused { status, .. } => format!("upgrade_{status}"),
            ClientError::Refused { code, .. } => format!("refused_{}", code.to_u8()),
            ClientError::Closed(info) => format!("closed_{}", info.code),
            ClientError::WebSocket(_) => "websocket".to_string(),
            _ => "other".to_string(),
        };
        ConnectFailure { class, detail: e.to_string() }
    }

    fn new(class: &str, detail: impl Into<String>) -> ConnectFailure {
        ConnectFailure { class: class.to_string(), detail: detail.into() }
    }
}

enum Inner {
    V1(Box<Connection>),
    P3(Box<Session>),
}

/// An authenticated realtime connection to either server.
pub struct Conn {
    inner: Inner,
    welcome: WelcomeInfo,
    timings: Timings,
}

/// The `Hello.client` of the bench.
const CLIENT_NAME: &str = concat!("scacelith-bench/", env!("CARGO_PKG_VERSION"));

impl Conn {
    /// Connects, upgrades and runs the Hello with `token`, all within `limit`.
    pub async fn connect(
        target: Target,
        endpoint: &Endpoint,
        token: &str,
        limit: Duration,
    ) -> Result<Conn, ConnectFailure> {
        match tokio::time::timeout(limit, Self::open(target, endpoint, token, limit)).await {
            Ok(result) => result,
            Err(_) => Err(ConnectFailure::new("timeout", "connection and Hello deadline")),
        }
    }

    async fn open(
        target: Target,
        endpoint: &Endpoint,
        token: &str,
        limit: Duration,
    ) -> Result<Conn, ConnectFailure> {
        match target {
            Target::Rust => {
                let mut opts =
                    ConnectOptions { client_name: CLIENT_NAME.to_string(), ..ConnectOptions::default() };
                opts.hello_timeout = limit;
                opts.session.handshake_timeout = limit;
                let conn =
                    Connection::connect(endpoint, token, &opts).await.map_err(|e| ConnectFailure::of(&e))?;
                let t = conn.timings();
                let w = conn.welcome();
                Ok(Conn {
                    welcome: WelcomeInfo {
                        username: w.username.clone(),
                        active_game: w.active_game,
                        gesture_rate: w.gesture_rate,
                    },
                    timings: Timings { handshake: t.tcp + t.tls + t.upgrade, hello: t.hello },
                    inner: Inner::V1(Box::new(conn)),
                })
            }
            Target::Node => {
                let mut opts = SessionOptions::new(proto3::SUBPROTOCOL);
                opts.max_message = proto3::MAX_SERVER_MESSAGE;
                opts.handshake_timeout = limit;
                opts.auto_reply = Some(proto3::pong_reply);
                let mut session =
                    Session::connect(endpoint, &opts).await.map_err(|e| ConnectFailure::of(&e))?;
                let t = session.timings();
                let started = Instant::now();
                session.send(&proto3::hello(CLIENT_NAME, token)).map_err(|e| ConnectFailure::of(&e))?;
                let welcome = loop {
                    let incoming = session.recv().await.map_err(|e| ConnectFailure::of(&e))?;
                    match proto3::decode_welcome(&incoming.payload) {
                        Ok(Some(w)) => break w,
                        Ok(None) => {
                            if let Ok(Event::Error { code, .. }) = proto3::decode(&incoming.payload) {
                                return Err(ConnectFailure::new(&format!("refused_{code}"), "Hello refused"));
                            }
                        }
                        Err(e) => return Err(ConnectFailure::new("websocket", format!("bad Welcome: {e}"))),
                    }
                };
                Ok(Conn {
                    welcome,
                    timings: Timings { handshake: t.tcp + t.tls + t.upgrade, hello: started.elapsed() },
                    inner: Inner::P3(Box::new(session)),
                })
            }
        }
    }

    /// What the `Welcome` said.
    pub fn welcome(&self) -> &WelcomeInfo {
        &self.welcome
    }

    /// How long the connection took.
    pub fn timings(&self) -> Timings {
        self.timings
    }

    /// Sends a request; returns its `seq`.
    pub fn send(&self, cmd: Cmd<'_>) -> Result<u32, ClientError> {
        match &self.inner {
            Inner::V1(conn) => conn.send(v1_message(cmd)),
            Inner::P3(session) => session.send(&p3_message(cmd)),
        }
    }

    /// The next event and the instant its message was read from the socket (messages that do
    /// not decode are skipped). Cancel-safe. `Err` once the connection is closed.
    pub async fn recv(&mut self) -> Result<(Event, Instant), ClientError> {
        match &mut self.inner {
            Inner::V1(conn) => {
                let (msg, at) = conn.recv_timed().await?;
                Ok((v1_event(msg), at))
            }
            Inner::P3(session) => loop {
                let incoming = session.recv().await?;
                if let Ok(event) = proto3::decode(&incoming.payload) {
                    return Ok((event, incoming.at));
                }
            },
        }
    }

    /// Starts the close handshake (code 1000).
    pub fn close(&self) {
        match &self.inner {
            Inner::V1(conn) => conn.close(),
            Inner::P3(session) => session.close(1000, ""),
        }
    }

    /// Waits until the connection is closed, at most `limit`.
    pub async fn wait_closed(&mut self, limit: Duration) {
        let fut = async {
            match &mut self.inner {
                Inner::V1(conn) => conn.wait_closed().await,
                Inner::P3(session) => session.wait_closed().await,
            }
        };
        let _ = tokio::time::timeout(limit, fut).await;
    }

    /// The close code once closed by either side.
    pub fn close_code(&self) -> Option<u16> {
        match &self.inner {
            Inner::V1(conn) => conn.close_info(),
            Inner::P3(session) => session.close_info(),
        }
        .map(|c| c.code)
    }
}

fn v1_message(cmd: Cmd<'_>) -> ClientMsg {
    match cmd {
        Cmd::QueueJoin { category, rated } => {
            QueueJoin { seq: 0, category: category.to_string(), rated }.into()
        }
        Cmd::QueueLeave => QueueLeave { seq: 0 }.into(),
        Cmd::Challenge { target, base_sec, inc_sec, rated } => ChallengeCreate {
            seq: 0,
            target: target.to_string(),
            base_sec,
            inc_sec,
            rated,
            color: ColorPref::Random,
        }
        .into(),
        Cmd::Accept { id } => ChallengeAccept { seq: 0, id }.into(),
        Cmd::Move { game, ply, mv, pos_hash, think_ms } => {
            Move { seq: 0, game, ply, r#move: mv, pos_hash, think_ms, draw_offer: false }.into()
        }
        Cmd::Resign { game } => Resign { seq: 0, game }.into(),
        Cmd::Abort { game } => Abort { seq: 0, game }.into(),
        Cmd::Resync { game } => Resync { seq: 0, game }.into(),
        Cmd::Gesture { game, ply, yaw, pitch, lean } => ClientGesture {
            seq: 0,
            game,
            ply: ply.min(1199),
            touch: 64,
            aim: 64,
            placed: 0,
            flags: 0,
            yaw: yaw.clamp(-3142, 3142),
            pitch: pitch.clamp(-1571, 1571),
            lean: lean.min(100),
        }
        .into(),
    }
}

fn p3_message(cmd: Cmd<'_>) -> Vec<u8> {
    match cmd {
        Cmd::QueueJoin { category, rated } => proto3::queue_join(category, rated),
        Cmd::QueueLeave => proto3::queue_leave(),
        Cmd::Challenge { target, base_sec, inc_sec, rated } => {
            proto3::challenge_create(target, base_sec, inc_sec, rated)
        }
        Cmd::Accept { id } => proto3::challenge_accept(id),
        Cmd::Move { game, ply, mv, pos_hash, think_ms } => {
            proto3::move_intent(game, ply, mv, pos_hash, think_ms)
        }
        Cmd::Resign { game } => proto3::resign(game),
        Cmd::Abort { game } => proto3::abort(game),
        Cmd::Resync { game } => proto3::resync(game),
        Cmd::Gesture { game, ply, yaw, pitch, lean } => proto3::gesture(game, ply, yaw, pitch, lean),
    }
}

fn v1_event(msg: ServerMsg) -> Event {
    match msg {
        ServerMsg::Error(e) => Event::Error { r#ref: e.r#ref, code: e.code.to_u8() },
        ServerMsg::ChallengeReceived(c) => Event::ChallengeReceived { id: c.id },
        ServerMsg::GameSnapshot(s) => Event::Snapshot(Snapshot {
            game: s.game,
            you_white: s.you == Color::White,
            moves: s.moves.iter().map(|m| m.r#move).collect(),
            over: s.status != GameStatus::Ongoing,
        }),
        ServerMsg::MoveMade(m) => Event::MoveMade { game: m.game, ply: m.ply, mv: m.r#move },
        ServerMsg::MoveRejected(m) => Event::MoveRejected { game: m.game, code: m.code.to_u8() },
        ServerMsg::GameEnd(g) => Event::GameEnd { game: g.game },
        ServerMsg::Gesture(g) => Event::Gesture { game: g.game, yaw: g.yaw },
        _ => Event::Other,
    }
}
