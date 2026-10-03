//! A minimal codec of the Node server's realtime protocol 3 (subprotocol `scacelith.v1`), for the
//! `--target node` adapter only.
//!
//! It covers just the messages the scenarios exchange, and is derived by hand from the Node
//! tree's `src/protocol/schema.js` (`PROTOCOL_VERSION = 3`, `SCHEMA_HASH = 0xf7825229`). Nothing
//! else in the client crate depends on it: the SDK speaks protocol v1 only. Layout rules of that
//! schema: one message per binary WebSocket message, `u8 type` then the fields little-endian
//! without padding; every client message starts with `seq u32` (the session numbers it); `str8`
//! is a length byte and UTF-8; `id53` is a `u64`; `bool` and enums are one byte.

use bytes::BufMut;

use crate::conn::{Event, Snapshot, WelcomeInfo};

/// The WebSocket subprotocol of protocol 3.
pub const SUBPROTOCOL: &str = "scacelith.v1";
/// `Hello.proto`.
pub const PROTOCOL_VERSION: u16 = 3;
/// `Hello.schema`: the hash of the schema the codec was made from.
pub const SCHEMA_HASH: u32 = 0xf782_5229;
/// Largest message the Node server sends (its `MAX_SERVER_MESSAGE`).
pub const MAX_SERVER_MESSAGE: usize = 65536;

// Message types (identical numbers in protocol v1, the layouts of Hello and Welcome differ).
const HELLO: u8 = 0x01;
const PONG: u8 = 0x03;
const QUEUE_JOIN: u8 = 0x10;
const QUEUE_LEAVE: u8 = 0x11;
const CHALLENGE_CREATE: u8 = 0x12;
const CHALLENGE_ACCEPT: u8 = 0x13;
const MOVE: u8 = 0x20;
const RESIGN: u8 = 0x21;
const ABORT: u8 = 0x25;
const RESYNC: u8 = 0x26;
const GESTURE: u8 = 0x28;
const S_WELCOME: u8 = 0x80;
const S_ERROR: u8 = 0x81;
const S_PING: u8 = 0x82;
const S_CHALLENGE_RECEIVED: u8 = 0x91;
const S_GAME_SNAPSHOT: u8 = 0xA0;
const S_MOVE_MADE: u8 = 0xA1;
const S_MOVE_REJECTED: u8 = 0xA2;
const S_GAME_END: u8 = 0xA4;
const S_GESTURE: u8 = 0xA6;

/// `GameStatus.Ongoing`.
const ONGOING: u8 = 0;

/// A client message with its type and a zero `seq` (the session writes the real one).
fn message(kind: u8, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + len);
    out.put_u8(kind);
    out.put_u32_le(0);
    out
}

fn put_str8(out: &mut Vec<u8>, s: &str) {
    let len = s.len().min(255);
    out.put_u8(len as u8);
    out.put_slice(&s.as_bytes()[..len]);
}

/// `Hello {seq, proto u16, schema u32, client str8 (max 48), token str8 (16..=160)}`.
pub fn hello(client: &str, token: &str) -> Vec<u8> {
    let client = &client[..client.len().min(48)];
    let mut out = message(HELLO, 8 + client.len() + token.len());
    out.put_u16_le(PROTOCOL_VERSION);
    out.put_u32_le(SCHEMA_HASH);
    put_str8(&mut out, client);
    put_str8(&mut out, token);
    out
}

/// `QueueJoin {seq, category str8, rated bool}`.
pub fn queue_join(category: &str, rated: bool) -> Vec<u8> {
    let mut out = message(QUEUE_JOIN, 2 + category.len());
    put_str8(&mut out, category);
    out.put_u8(u8::from(rated));
    out
}

/// `QueueLeave {seq}`.
pub fn queue_leave() -> Vec<u8> {
    message(QUEUE_LEAVE, 0)
}

/// `ChallengeCreate {seq, target str8, baseSec u16, incSec u8, rated bool, color u8}` (colour
/// Random).
pub fn challenge_create(target: &str, base_sec: u16, inc_sec: u8, rated: bool) -> Vec<u8> {
    let mut out = message(CHALLENGE_CREATE, 6 + target.len());
    put_str8(&mut out, target);
    out.put_u16_le(base_sec);
    out.put_u8(inc_sec);
    out.put_u8(u8::from(rated));
    out.put_u8(0);
    out
}

/// `ChallengeAccept {seq, id u32}`.
pub fn challenge_accept(id: u32) -> Vec<u8> {
    let mut out = message(CHALLENGE_ACCEPT, 4);
    out.put_u32_le(id);
    out
}

/// `Move {seq, game id53, ply u16, move u16, posHash u32, thinkMs u32, drawOffer bool}`.
pub fn move_intent(game: u64, ply: u16, mv: u16, pos_hash: u32, think_ms: u32) -> Vec<u8> {
    let mut out = message(MOVE, 21);
    out.put_u64_le(game);
    out.put_u16_le(ply);
    out.put_u16_le(mv);
    out.put_u32_le(pos_hash);
    out.put_u32_le(think_ms);
    out.put_u8(0);
    out
}

fn game_only(kind: u8, game: u64) -> Vec<u8> {
    let mut out = message(kind, 8);
    out.put_u64_le(game);
    out
}

/// `Resign {seq, game id53}`.
pub fn resign(game: u64) -> Vec<u8> {
    game_only(RESIGN, game)
}

/// `Abort {seq, game id53}`.
pub fn abort(game: u64) -> Vec<u8> {
    game_only(ABORT, game)
}

/// `Resync {seq, game id53}`.
pub fn resync(game: u64) -> Vec<u8> {
    game_only(RESYNC, game)
}

/// `Gesture {seq, game id53, ply u16, touch u8, aim u8, placed u16, flags u8, yaw i32, pitch i32,
/// lean u8}`, the head only (nothing in hand).
pub fn gesture(game: u64, ply: u16, yaw: i32, pitch: i32, lean: u8) -> Vec<u8> {
    let mut out = message(GESTURE, 24);
    out.put_u64_le(game);
    out.put_u16_le(ply.min(1199));
    out.put_u8(64);
    out.put_u8(64);
    out.put_u16_le(0);
    out.put_u8(0);
    out.put_i32_le(yaw.clamp(-3142, 3142));
    out.put_i32_le(pitch.clamp(-1571, 1571));
    out.put_u8(lean.min(100));
    out
}

/// The automatic answer to the server's `Ping {nonce u32, serverTime f64}`: `Pong {seq, nonce}`.
pub fn pong_reply(msg: &[u8]) -> Option<Vec<u8>> {
    if msg.len() != 13 || msg[0] != S_PING {
        return None;
    }
    let mut out = message(PONG, 4);
    out.put_slice(&msg[1..5]);
    Some(out)
}

/// A cursor over a received message; every read fails past the end.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

type Decoded<T> = Result<T, &'static str>;

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Decoded<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or("truncated message")?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Decoded<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Decoded<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    fn u32(&mut self) -> Decoded<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn i32(&mut self) -> Decoded<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Decoded<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn skip(&mut self, n: usize) -> Decoded<()> {
        self.take(n).map(|_| ())
    }

    fn str8(&mut self) -> Decoded<String> {
        let len = usize::from(self.u8()?);
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes).map(str::to_string).map_err(|_| "string not UTF-8")
    }

    /// `PlayerInfo {userId u32, name str8, rating u16, provisional bool}`.
    fn skip_player(&mut self) -> Decoded<()> {
        self.skip(4)?;
        let len = usize::from(self.u8()?);
        self.skip(len + 3)
    }
}

/// `Welcome {proto u16, serverTime f64, userId u32, username str8, serverName str8, heartbeatMs
/// u32, clientPingMs u32, maxMsgPerSec u16, activeGame id53, gestureRate u16, gestureBurst u16}`;
/// `None` for another message.
pub fn decode_welcome(msg: &[u8]) -> Decoded<Option<WelcomeInfo>> {
    if msg.first() != Some(&S_WELCOME) {
        return Ok(None);
    }
    let mut r = Reader { buf: msg, pos: 1 };
    r.skip(2 + 8 + 4)?;
    let username = r.str8()?;
    let _server_name = r.str8()?;
    r.skip(4 + 4 + 2)?;
    let active_game = r.u64()?;
    let gesture_rate = r.u16()?;
    Ok(Some(WelcomeInfo { username, active_game, gesture_rate }))
}

/// The bench event of a server message. Types the scenarios do not use become [`Event::Other`].
pub fn decode(msg: &[u8]) -> Decoded<Event> {
    let kind = *msg.first().ok_or("empty message")?;
    let mut r = Reader { buf: msg, pos: 1 };
    let event = match kind {
        // Error {ref u32, code u8, fatal bool, game id53}
        S_ERROR => Event::Error { r#ref: r.u32()?, code: r.u8()? },
        // ChallengeReceived {id u32, from PlayerInfo, ...}
        S_CHALLENGE_RECEIVED => Event::ChallengeReceived { id: r.u32()? },
        S_GAME_SNAPSHOT => Event::Snapshot(snapshot(&mut r)?),
        // MoveMade {game id53, gseq u32, ply u16, move u16, ...}
        S_MOVE_MADE => {
            let game = r.u64()?;
            r.skip(4)?;
            Event::MoveMade { game, ply: r.u16()?, mv: r.u16()? }
        }
        // MoveRejected {game id53, ply u16, move u16, code u8}
        S_MOVE_REJECTED => {
            let game = r.u64()?;
            r.skip(4)?;
            Event::MoveRejected { game, code: r.u8()? }
        }
        // GameEnd {game id53, ...}
        S_GAME_END => Event::GameEnd { game: r.u64()? },
        // Gesture {game id53, ply u16, touch u8, aim u8, placed u16, flags u8, yaw i32, ...}
        S_GESTURE => {
            let game = r.u64()?;
            r.skip(2 + 1 + 1 + 2 + 1)?;
            Event::Gesture { game, yaw: r.i32()? }
        }
        _ => Event::Other,
    };
    Ok(event)
}

/// `GameSnapshot {game id53, gseq u32, category str8, baseMs u32, incMs u32, rated bool, white
/// PlayerInfo, black PlayerInfo, you u8, moves list16 of {move u16, spentMs u32, clockMs u32},
/// running u8, whiteMs u32, blackMs u32, serverTime f64, drawOffer u8, status u8, ...}`.
fn snapshot(r: &mut Reader<'_>) -> Decoded<Snapshot> {
    let game = r.u64()?;
    r.skip(4)?;
    let len = usize::from(r.u8()?);
    r.skip(len + 4 + 4 + 1)?;
    r.skip_player()?;
    r.skip_player()?;
    let you = r.u8()?;
    let count = usize::from(r.u16()?);
    let mut moves = Vec::with_capacity(count);
    for _ in 0..count {
        moves.push(r.u16()?);
        r.skip(8)?;
    }
    r.skip(1 + 4 + 4 + 8 + 1)?;
    let status = r.u8()?;
    Ok(Snapshot { game, you_white: you == 0, moves, over: status != ONGOING })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_follow_the_schema() {
        let h = hello("bench", "sct_0123456789abcdef");
        assert_eq!(h[0], HELLO);
        assert_eq!(&h[5..7], &3u16.to_le_bytes());
        assert_eq!(&h[7..11], &SCHEMA_HASH.to_le_bytes());
        assert_eq!(h.len(), 1 + 4 + 2 + 4 + 1 + 5 + 1 + 20);
        assert_eq!(move_intent(7, 0, 796, 0x3706_291C, 10).len(), 26, "Move is 26 bytes");
        assert_eq!(gesture(7, 3, 9999, -9999, 200).len(), 29, "Gesture is 29 bytes");
        assert_eq!(challenge_create("bob", 180, 2, true).len(), 1 + 4 + 1 + 3 + 2 + 1 + 1 + 1);
        let mut ping = vec![S_PING, 1, 2, 3, 4];
        ping.extend_from_slice(&0f64.to_le_bytes());
        assert_eq!(pong_reply(&ping), Some(vec![PONG, 0, 0, 0, 0, 1, 2, 3, 4]));
        assert_eq!(pong_reply(&ping[..12]), None);
    }

    #[test]
    fn decodes_what_the_scenarios_read() {
        let mut w = vec![S_WELCOME];
        w.extend_from_slice(&3u16.to_le_bytes());
        w.extend_from_slice(&0f64.to_le_bytes());
        w.extend_from_slice(&42u32.to_le_bytes());
        w.extend_from_slice(&[3, b'b', b'o', b'b', 2, b'S', b'1']);
        w.extend_from_slice(&[0; 10]);
        w.extend_from_slice(&9u64.to_le_bytes());
        w.extend_from_slice(&20u16.to_le_bytes());
        w.extend_from_slice(&40u16.to_le_bytes());
        let info = decode_welcome(&w).unwrap().unwrap();
        assert_eq!((info.username.as_str(), info.active_game, info.gesture_rate), ("bob", 9, 20));

        let mut s = vec![S_GAME_SNAPSHOT];
        s.extend_from_slice(&5u64.to_le_bytes());
        s.extend_from_slice(&1u32.to_le_bytes());
        s.extend_from_slice(&[3, b'3', b'+', b'2']);
        s.extend_from_slice(&[0; 9]);
        for name in [&b"ann"[..], &b"bob"[..]] {
            s.extend_from_slice(&[1, 0, 0, 0, 3]);
            s.extend_from_slice(name);
            s.extend_from_slice(&[0xDC, 0x05, 1]);
        }
        s.push(1);
        s.extend_from_slice(&1u16.to_le_bytes());
        s.extend_from_slice(&796u16.to_le_bytes());
        s.extend_from_slice(&[0; 8]);
        s.extend_from_slice(&[0; 18]);
        s.push(0);
        s.extend_from_slice(&[0; 25]);
        match decode(&s).unwrap() {
            Event::Snapshot(snap) => {
                assert_eq!((snap.game, snap.you_white, snap.moves, snap.over), (5, false, vec![796], false));
            }
            other => panic!("{other:?}"),
        }
        assert!(decode(&s[..20]).is_err());
        assert!(matches!(decode(&[0xEE]), Ok(Event::Other)));
    }
}
