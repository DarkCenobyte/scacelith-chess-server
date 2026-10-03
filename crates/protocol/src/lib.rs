//! Scacelith realtime protocol, version 1: the message codec, the constants and the rules of the
//! frozen protocol spoken by the game client and the dedicated server over a WebSocket
//! (subprotocol [`SUBPROTOCOL`]). The specification is `docs/PROTOCOL.md`; the schema is
//! `protocol/scacelith-v1.json`, from which `protogen` (feature `gen`) generates this crate's
//! codec, the game's C++ codec, the tables of the specification and the golden vectors.
//!
//! # Messages
//!
//! Every message is a struct implementing [`Message`]; [`ClientMsg`] and [`ServerMsg`] gather
//! the messages of each direction. A message is one binary WebSocket frame: its type byte, then
//! its fields, little-endian, without padding.
//!
//! * Client messages (type 0x01-0x7F) decode **strictly**, as the server does: truncation,
//!   trailing bytes, unknown types, values outside their bounds, unknown enum values are all
//!   refused with a [`DecodeError`] whose `Display` form is a stable reason (`"ply above max"`).
//! * Server messages (type 0x80-0xFF) decode **leniently**, as a client must: an unknown type is
//!   ignored ([`ServerMsg::decode`] returns `Ok(None)`), trailing bytes (fields of a later minor)
//!   are ignored, and unknown values of open enums decode as `Unknown`.
//! * Encoding validates the same rules (an invalid value is an [`EncodeError`] and writes
//!   nothing) and writes into any [`bytes::BufMut`]. [`ServerMsg::to_bytes`] gives a cheaply
//!   clonable [`bytes::Bytes`]: a `MoveMade` is encoded once and sent to both players.
//!
//! ```
//! use scacelith_protocol::{ClientMsg, Message, Resign, ServerMsg, ServerPing};
//!
//! let frame = Resign { seq: 7, game: 42 }.to_vec().unwrap();
//! assert_eq!(scacelith_protocol::peek_seq(&frame), Some(7));
//! assert_eq!(ClientMsg::decode(&frame).unwrap(), ClientMsg::Resign(Resign { seq: 7, game: 42 }));
//!
//! let ping = ServerPing { nonce: 1, server_time: 1.79e12 }.to_vec().unwrap();
//! assert!(matches!(ServerMsg::decode(&ping), Ok(Some(ServerMsg::Ping(_)))));
//! assert_eq!(ServerMsg::decode(&[0xEE]), Ok(None)); // a type of a later minor
//! ```
//!
//! # Versions
//!
//! [`PROTOCOL_VERSION`] is 1 for the life of this protocol. A later minor ([`MINOR`]) only adds
//! message types, appends fields to messages, adds values to open enums and defines capability
//! bits ([`CAPS`]); the server reads the Hello of any minor ([`decode_hello`],
//! [`HelloPrefix`]). [`FINGERPRINT`] identifies the schema in logs and is never compared.

mod moves;
pub mod wire;

#[rustfmt::skip]
mod r#gen;

#[cfg(test)]
#[rustfmt::skip]
mod gen_json;

#[cfg(any(test, feature = "gen"))]
#[doc(hidden)]
pub mod codegen;

#[cfg(test)]
mod tests;

use bytes::{BufMut, Bytes, BytesMut};

pub use crate::r#gen::*;
pub use crate::moves::{fen_digest, fnv1a32, move_to_uci, pack_move, uci_to_move, unpack_move};
use crate::wire::Reader;
pub use crate::wire::{DecodeError, Defect, EncodeError};

/// A message of the protocol (implemented by every generated message struct).
pub trait Message: Sized {
    /// Type byte of the message.
    const TYPE: MsgType;
    /// Smallest encoded size, type byte included.
    const MIN_LEN: usize;
    /// Largest encoded size, type byte included.
    const MAX_LEN: usize;

    /// Encoded size of this value, type byte included.
    fn encoded_len(&self) -> usize;

    /// Checks that every field holds a value a sender may send.
    fn validate(&self) -> Result<(), EncodeError>;

    /// Writes the fields (validated) after the type byte.
    #[doc(hidden)]
    fn write_fields<B: BufMut>(&self, out: &mut B);

    /// Reads the fields after the type byte.
    #[doc(hidden)]
    fn read_fields(r: &mut Reader<'_>) -> Result<Self, DecodeError>;

    /// Validates the message, then appends its encoding to `out`; nothing is written on error.
    fn encode<B: BufMut>(&self, out: &mut B) -> Result<(), EncodeError> {
        self.validate()?;
        out.put_u8(Self::TYPE.to_u8());
        self.write_fields(out);
        Ok(())
    }

    /// The encoding in a buffer of the exact size.
    fn to_bytes(&self) -> Result<Bytes, EncodeError> {
        let mut out = BytesMut::with_capacity(self.encoded_len());
        self.encode(&mut out)?;
        Ok(out.freeze())
    }

    /// The encoding in a vector of the exact size.
    fn to_vec(&self) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode(&mut out)?;
        Ok(out)
    }

    /// Decodes a frame holding this message: strictly for a client message, leniently (trailing
    /// bytes and unknown values of open enums accepted) for a server message.
    fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        decode_as(buf, Self::TYPE.is_client())
    }

    /// Decodes a frame holding exactly this message of this minor (strict in both directions).
    fn decode_exact(buf: &[u8]) -> Result<Self, DecodeError> {
        decode_as(buf, true)
    }
}

fn decode_as<M: Message>(buf: &[u8], strict: bool) -> Result<M, DecodeError> {
    let first = buf.first().copied();
    if first != Some(M::TYPE.to_u8()) {
        return Err(DecodeError::new(type_defect(first, M::TYPE.is_client()), ""));
    }
    let mut r = Reader::new(buf, strict);
    let msg = M::read_fields(&mut r)?;
    r.finish()?;
    Ok(msg)
}

/// Size of the [`HelloPrefix`]: type byte, `seq`, `proto`, `minor`, `caps`.
pub const HELLO_PREFIX_LEN: usize = 17;

/// The start of every Hello, frozen for all versions: a server reads it before anything else
/// to refuse an unsupported `proto` with the right error, whatever follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloPrefix {
    /// `seq` of the Hello (1).
    pub seq: u32,
    /// Major protocol version of the client.
    pub proto: u16,
    /// Minor version of the client.
    pub minor: u16,
    /// Capability bits of the client.
    pub caps: u64,
}

impl HelloPrefix {
    /// The prefix of a frame starting with a Hello type byte and at least
    /// [`HELLO_PREFIX_LEN`] bytes.
    pub fn read(buf: &[u8]) -> Option<Self> {
        let p: &[u8; HELLO_PREFIX_LEN] = buf.get(..HELLO_PREFIX_LEN)?.try_into().ok()?;
        if p[0] != MsgType::Hello.to_u8() {
            return None;
        }
        Some(Self {
            seq: u32::from_le_bytes([p[1], p[2], p[3], p[4]]),
            proto: u16::from_le_bytes([p[5], p[6]]),
            minor: u16::from_le_bytes([p[7], p[8]]),
            caps: u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]),
        })
    }
}

/// Decodes a Hello as the server does: strictly, except that the Hello of a later minor than
/// [`MINOR`] may carry fields this codec does not know (trailing bytes), which are ignored.
pub fn decode_hello(buf: &[u8]) -> Result<Hello, DecodeError> {
    let first = buf.first().copied();
    if first != Some(MsgType::Hello.to_u8()) {
        return Err(DecodeError::new(type_defect(first, true), ""));
    }
    let mut r = Reader::new(buf, true);
    let hello = Hello::read_fields(&mut r)?;
    let later_minor = hello.minor > MINOR;
    if !later_minor {
        r.finish()?;
    }
    Ok(hello)
}

/// Type of a frame, from its first byte (`None` when empty or unknown).
pub fn peek_type(buf: &[u8]) -> Option<MsgType> {
    MsgType::from_u8(*buf.first()?)
}

/// `seq` of a client frame (bytes 1..=4), read without decoding the frame: the `ref` of the
/// `Error` that answers a frame which does not decode. `None` when the frame is shorter than
/// 5 bytes or its type byte is not in the client range.
pub fn peek_seq(buf: &[u8]) -> Option<u32> {
    match buf {
        [0x01..=0x7f, a, b, c, d, ..] => Some(u32::from_le_bytes([*a, *b, *c, *d])),
        _ => None,
    }
}

/// Close code that follows a fatal `Error` with this code: `4000 + c` for c in 1..=99,
/// `4300 + (c - 240)` for c in 240..=255, `None` for codes that are never fatal (100..=239).
pub const fn close_code_for(code: ErrorCode) -> Option<u16> {
    match code.to_u8() {
        c @ 1..=99 => Some(4000 + c as u16),
        c @ 240..=255 => Some(4300 + (c - 240) as u16),
        _ => None,
    }
}

/// The error code of a close code of the rule of [`close_code_for`] (`Unknown` for a code
/// this minor does not define, such as [`close::SLOW_CONSUMER`]), or `None` for other codes.
pub const fn error_code_for_close(code: u16) -> Option<ErrorCode> {
    match code {
        4001..=4099 => Some(ErrorCode::from_u8((code - 4000) as u8)),
        4300..=4315 => Some(ErrorCode::from_u8((code - 4300) as u8 + 240)),
        _ => None,
    }
}
