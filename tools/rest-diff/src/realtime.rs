//! Plays short scripted games through each server's realtime protocol, to give both servers the
//! same game history: the Node server speaks protocol 3 (`scacelith.v1`, a minimal hand-written
//! codec below, layouts from the Node tree's `src/protocol/schema.js`, as the bench's
//! `proto3.rs`), the Rust server protocol v1 through the SDK's typed [`Connection`].
//!
//! A game is a challenge by name with a fixed colour (so both servers seat the same players on
//! the same sides), a list of UCI moves, and an ending (resignation, checkmate on the board,
//! agreed draw, abort).

use std::time::Duration;

use bytes::BufMut as _;
use scacelith_chess::ChessGame;
use scacelith_client::ws::{Session, SessionOptions};
use scacelith_client::{ConnectOptions, Connection, Endpoint};
use scacelith_protocol::{
    Abort, ChallengeAccept, ChallengeCreate, ColorPref, DrawAnswer, Move, Resign, ServerMsg,
};

use crate::servers::Kind;

/// Deadline of each wait for a server message.
const WAIT: Duration = Duration::from_secs(10);

/// What a scenario reads from the server messages.
#[derive(Clone, Debug)]
enum Ev {
    Error { code: u8 },
    ChallengeReceived { id: u32 },
    Snapshot { game: u64 },
    MoveMade { game: u64, ply: u16 },
    GameEnd { game: u64 },
    Other,
}

/// A client request.
#[derive(Clone, Debug)]
enum Cmd<'a> {
    Challenge { target: &'a str, base_sec: u16, inc_sec: u8, rated: bool },
    Accept { id: u32 },
    Move { game: u64, ply: u16, mv: u16, pos_hash: u32, draw_offer: bool },
    DrawAnswer { game: u64, accept: bool },
    Resign { game: u64 },
    Abort { game: u64 },
}

/// An authenticated realtime connection to either server.
pub struct Rt {
    inner: Inner,
}

enum Inner {
    V1(Box<Connection>),
    P3(Box<Session>),
}

// Protocol 3 message types (src/protocol/schema.js of the Node tree).
const P3_HELLO: u8 = 0x01;
const P3_PONG: u8 = 0x03;
const P3_CHALLENGE_CREATE: u8 = 0x12;
const P3_CHALLENGE_ACCEPT: u8 = 0x13;
const P3_MOVE: u8 = 0x20;
const P3_RESIGN: u8 = 0x21;
const P3_DRAW_ANSWER: u8 = 0x23;
const P3_ABORT: u8 = 0x25;
const P3_WELCOME: u8 = 0x80;
const P3_ERROR: u8 = 0x81;
const P3_PING: u8 = 0x82;
const P3_CHALLENGE_RECEIVED: u8 = 0x91;
const P3_SNAPSHOT: u8 = 0xA0;
const P3_MOVE_MADE: u8 = 0xA1;
const P3_GAME_END: u8 = 0xA4;
/// `Hello.schema` of protocol 3.
const P3_SCHEMA_HASH: u32 = 0xf782_5229;

fn p3_message(kind: u8) -> Vec<u8> {
    let mut out = vec![kind];
    out.put_u32_le(0);
    out
}

fn p3_str8(out: &mut Vec<u8>, s: &str) {
    let len = s.len().min(255);
    out.put_u8(len as u8);
    out.put_slice(&s.as_bytes()[..len]);
}

/// `Pong {seq, nonce}` for a protocol 3 `Ping {nonce u32, serverTime f64}`.
fn p3_pong(msg: &[u8]) -> Option<Vec<u8>> {
    if msg.len() != 13 || msg[0] != P3_PING {
        return None;
    }
    let mut out = p3_message(P3_PONG);
    out.put_slice(&msg[1..5]);
    Some(out)
}

fn p3_encode(cmd: &Cmd<'_>) -> Vec<u8> {
    match cmd {
        Cmd::Challenge { target, base_sec, inc_sec, rated } => {
            let mut out = p3_message(P3_CHALLENGE_CREATE);
            p3_str8(&mut out, target);
            out.put_u16_le(*base_sec);
            out.put_u8(*inc_sec);
            out.put_u8(u8::from(*rated));
            out.put_u8(1); // ColorPref.White: the challenger plays White
            out
        }
        Cmd::Accept { id } => {
            let mut out = p3_message(P3_CHALLENGE_ACCEPT);
            out.put_u32_le(*id);
            out
        }
        Cmd::Move { game, ply, mv, pos_hash, draw_offer } => {
            let mut out = p3_message(P3_MOVE);
            out.put_u64_le(*game);
            out.put_u16_le(*ply);
            out.put_u16_le(*mv);
            out.put_u32_le(*pos_hash);
            out.put_u32_le(100);
            out.put_u8(u8::from(*draw_offer));
            out
        }
        Cmd::DrawAnswer { game, accept } => {
            let mut out = p3_message(P3_DRAW_ANSWER);
            out.put_u64_le(*game);
            out.put_u8(u8::from(*accept));
            out
        }
        Cmd::Resign { game } => {
            let mut out = p3_message(P3_RESIGN);
            out.put_u64_le(*game);
            out
        }
        Cmd::Abort { game } => {
            let mut out = p3_message(P3_ABORT);
            out.put_u64_le(*game);
            out
        }
    }
}

fn p3_u64(msg: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(msg.get(at..at + 8)?.try_into().ok()?))
}

fn p3_decode(msg: &[u8]) -> Ev {
    let u16_at = |at: usize| msg.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    match msg.first() {
        // Error {ref u32, code u8, fatal bool, game id53}
        Some(&P3_ERROR) => Ev::Error { code: msg.get(5).copied().unwrap_or(0) },
        // ChallengeReceived {id u32, ...}
        Some(&P3_CHALLENGE_RECEIVED) => match msg.get(1..5) {
            Some(b) => Ev::ChallengeReceived { id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]) },
            None => Ev::Other,
        },
        // GameSnapshot {game id53, ...}
        Some(&P3_SNAPSHOT) => p3_u64(msg, 1).map_or(Ev::Other, |game| Ev::Snapshot { game }),
        // MoveMade {game id53, gseq u32, ply u16, ...}
        Some(&P3_MOVE_MADE) => match (p3_u64(msg, 1), u16_at(13)) {
            (Some(game), Some(ply)) => Ev::MoveMade { game, ply },
            _ => Ev::Other,
        },
        // GameEnd {game id53, ...}
        Some(&P3_GAME_END) => p3_u64(msg, 1).map_or(Ev::Other, |game| Ev::GameEnd { game }),
        _ => Ev::Other,
    }
}

fn v1_message(cmd: &Cmd<'_>) -> scacelith_protocol::ClientMsg {
    match *cmd {
        Cmd::Challenge { target, base_sec, inc_sec, rated } => ChallengeCreate {
            seq: 0,
            target: target.to_string(),
            base_sec,
            inc_sec,
            rated,
            color: ColorPref::White,
        }
        .into(),
        Cmd::Accept { id } => ChallengeAccept { seq: 0, id }.into(),
        Cmd::Move { game, ply, mv, pos_hash, draw_offer } => {
            Move { seq: 0, game, ply, r#move: mv, pos_hash, think_ms: 100, draw_offer }.into()
        }
        Cmd::DrawAnswer { game, accept } => DrawAnswer { seq: 0, game, accept }.into(),
        Cmd::Resign { game } => Resign { seq: 0, game }.into(),
        Cmd::Abort { game } => Abort { seq: 0, game }.into(),
    }
}

fn v1_event(msg: ServerMsg) -> Ev {
    match msg {
        ServerMsg::Error(e) => Ev::Error { code: e.code.to_u8() },
        ServerMsg::ChallengeReceived(c) => Ev::ChallengeReceived { id: c.id },
        ServerMsg::GameSnapshot(s) => Ev::Snapshot { game: s.game },
        ServerMsg::MoveMade(m) => Ev::MoveMade { game: m.game, ply: m.ply },
        ServerMsg::GameEnd(g) => Ev::GameEnd { game: g.game },
        _ => Ev::Other,
    }
}

impl Rt {
    /// Connects to the server of `kind` and signs in with `token`.
    pub async fn connect(kind: Kind, endpoint: &Endpoint, token: &str) -> Result<Rt, String> {
        match kind {
            Kind::Rust => {
                let opts = ConnectOptions { client_name: "rest-diff".into(), ..ConnectOptions::default() };
                let conn = Connection::connect(endpoint, token, &opts).await.map_err(|e| format!("v1 connect: {e}"))?;
                Ok(Rt { inner: Inner::V1(Box::new(conn)) })
            }
            Kind::Node => {
                let mut opts = SessionOptions::new("scacelith.v1");
                opts.auto_reply = Some(p3_pong);
                let mut session =
                    Session::connect(endpoint, &opts).await.map_err(|e| format!("protocol 3 connect: {e}"))?;
                let mut hello = p3_message(P3_HELLO);
                hello.put_u16_le(3);
                hello.put_u32_le(P3_SCHEMA_HASH);
                p3_str8(&mut hello, "rest-diff");
                p3_str8(&mut hello, token);
                session.send(&hello).map_err(|e| format!("protocol 3 Hello: {e}"))?;
                loop {
                    let msg = tokio::time::timeout(WAIT, session.recv())
                        .await
                        .map_err(|_| "no Welcome".to_string())?
                        .map_err(|e| format!("protocol 3 Welcome: {e}"))?;
                    match msg.payload.first() {
                        Some(&P3_WELCOME) => break,
                        Some(&P3_ERROR) => return Err(format!("Hello refused: {:?}", p3_decode(&msg.payload))),
                        _ => {}
                    }
                }
                Ok(Rt { inner: Inner::P3(Box::new(session)) })
            }
        }
    }

    fn send(&self, cmd: Cmd<'_>) -> Result<(), String> {
        match &self.inner {
            Inner::V1(c) => c.send(v1_message(&cmd)).map(drop).map_err(|e| e.to_string()),
            Inner::P3(s) => s.send(&p3_encode(&cmd)).map(drop).map_err(|e| e.to_string()),
        }
    }

    async fn recv(&mut self) -> Result<Ev, String> {
        let fut = async {
            match &mut self.inner {
                Inner::V1(c) => c.recv().await.map(v1_event).map_err(|e| e.to_string()),
                Inner::P3(s) => s.recv().await.map(|m| p3_decode(&m.payload)).map_err(|e| e.to_string()),
            }
        };
        tokio::time::timeout(WAIT, fut).await.map_err(|_| "no server message within 10 s".to_string())?
    }

    /// Waits for the first event `pick` accepts; an `Error` fails the wait.
    async fn wait<T>(&mut self, what: &str, mut pick: impl FnMut(&Ev) -> Option<T>) -> Result<T, String> {
        loop {
            let ev = self.recv().await.map_err(|e| format!("waiting for {what}: {e}"))?;
            if let Some(v) = pick(&ev) {
                return Ok(v);
            }
            if let Ev::Error { code } = ev {
                return Err(format!("waiting for {what}: Error code {code}"));
            }
        }
    }

    /// Closes the connection.
    pub async fn close(mut self) {
        match &mut self.inner {
            Inner::V1(c) => {
                c.close();
                let _ = tokio::time::timeout(Duration::from_secs(2), c.wait_closed()).await;
            }
            Inner::P3(s) => {
                s.close(1000, "");
                let _ = tokio::time::timeout(Duration::from_secs(2), s.wait_closed()).await;
            }
        }
    }
}

/// How a scripted game ends after its moves.
#[derive(Clone, Copy, Debug)]
pub enum End {
    /// The given side resigns (`true`: White).
    Resign { white: bool },
    /// The last move ends the game on the board (checkmate, stalemate).
    OnBoard,
    /// The last move carries a draw offer, which the other side accepts.
    DrawAgreed,
    /// The given side aborts.
    Abort { white: bool },
}

/// A scripted game.
#[derive(Clone, Debug)]
pub struct GameScript {
    /// Time control: base seconds.
    pub base_sec: u16,
    /// Time control: increment seconds.
    pub inc_sec: u8,
    /// Rated.
    pub rated: bool,
    /// UCI moves.
    pub moves: Vec<&'static str>,
    /// How it ends.
    pub end: End,
}

/// Plays `script` between two signed-in players (White challenges Black by name); returns the
/// game id once both players saw the end.
pub async fn play(white: &mut Rt, black: &mut Rt, black_name: &str, script: &GameScript) -> Result<u64, String> {
    white.send(Cmd::Challenge {
        target: black_name,
        base_sec: script.base_sec,
        inc_sec: script.inc_sec,
        rated: script.rated,
    })?;
    let id = black
        .wait("ChallengeReceived", |e| match e {
            Ev::ChallengeReceived { id } => Some(*id),
            _ => None,
        })
        .await?;
    black.send(Cmd::Accept { id })?;
    let snap = |e: &Ev| match e {
        Ev::Snapshot { game } => Some(*game),
        _ => None,
    };
    let game = white.wait("GameSnapshot", snap).await?;
    let game_b = black.wait("GameSnapshot", snap).await?;
    if game != game_b {
        return Err(format!("the players got different games ({game} and {game_b})"));
    }
    let mut chess = ChessGame::new(None).map_err(|e| format!("{e:?}"))?;
    let last = script.moves.len().saturating_sub(1);
    for (ply, uci) in script.moves.iter().enumerate() {
        let mv = chess.position().parse_uci(uci).ok_or_else(|| format!("illegal scripted move {uci}"))?;
        let pos_hash = chess.position().digest();
        let draw_offer = matches!(script.end, End::DrawAgreed) && ply == last;
        let ply = ply as u16;
        let mover: &mut Rt = if ply % 2 == 0 { &mut *white } else { &mut *black };
        mover.send(Cmd::Move { game, ply, mv, pos_hash, draw_offer })?;
        for p in [&mut *white, &mut *black] {
            p.wait("MoveMade", |e| match e {
                Ev::MoveMade { game: g, ply: p } if *g == game && *p == ply => Some(()),
                _ => None,
            })
            .await?;
        }
        chess.play(mv).map_err(|e| format!("{e:?}"))?;
    }
    match script.end {
        End::Resign { white: w } => {
            let p: &mut Rt = if w { &mut *white } else { &mut *black };
            p.send(Cmd::Resign { game })?;
        }
        End::Abort { white: w } => {
            let p: &mut Rt = if w { &mut *white } else { &mut *black };
            p.send(Cmd::Abort { game })?;
        }
        End::DrawAgreed => {
            // The offer came with the last move; the other side answers.
            let p: &mut Rt = if last % 2 == 0 { &mut *black } else { &mut *white };
            p.send(Cmd::DrawAnswer { game, accept: true })?;
        }
        End::OnBoard => {}
    }
    for p in [&mut *white, &mut *black] {
        p.wait("GameEnd", |e| match e {
            Ev::GameEnd { game: g } if *g == game => Some(()),
            _ => None,
        })
        .await?;
    }
    Ok(game)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol3_layouts() {
        let c = p3_encode(&Cmd::Challenge { target: "bob", base_sec: 180, inc_sec: 2, rated: true });
        assert_eq!(c.len(), 1 + 4 + 1 + 3 + 2 + 1 + 1 + 1);
        assert_eq!(*c.last().unwrap(), 1);
        let m = p3_encode(&Cmd::Move { game: 7, ply: 0, mv: 796, pos_hash: 1, draw_offer: true });
        assert_eq!(m.len(), 26);
        assert_eq!(p3_encode(&Cmd::DrawAnswer { game: 7, accept: true }).len(), 14);
        let mut ping = vec![P3_PING, 1, 2, 3, 4];
        ping.extend_from_slice(&0f64.to_le_bytes());
        assert_eq!(p3_pong(&ping), Some(vec![P3_PONG, 0, 0, 0, 0, 1, 2, 3, 4]));
        let mut mm = vec![P3_MOVE_MADE];
        mm.extend_from_slice(&9u64.to_le_bytes());
        mm.extend_from_slice(&1u32.to_le_bytes());
        mm.extend_from_slice(&3u16.to_le_bytes());
        assert!(matches!(p3_decode(&mm), Ev::MoveMade { game: 9, ply: 3 }));
    }
}
