//! Encoding of the server messages the realtime layer sends itself.

use bytes::Bytes;
use scacelith_protocol::{Ack, Error, ErrorCode, Message, Notice, NoticeCode};

use crate::ids::GameId;

/// The encoding of `msg`, or `None` when a field is out of its bounds (a bug: the caller skips
/// the frame rather than sending something a client cannot decode).
pub(crate) fn encode(msg: &impl Message) -> Option<Bytes> {
    let frame = msg.to_bytes();
    debug_assert!(frame.is_ok(), "an invalid server message: {frame:?}");
    frame.ok()
}

/// `Error{ref, code, fatal, game}` (game 0 when the id is not a valid one).
pub(crate) fn error(r#ref: u32, code: ErrorCode, fatal: bool, game: GameId) -> Bytes {
    let game = if crate::ids::is_game_id(game) { game } else { 0 };
    Error { r#ref, code, fatal, game }.to_bytes().unwrap_or_else(|_| {
        // Unreachable: every field is in its bounds.
        Error { r#ref, code: ErrorCode::Internal, fatal, game: 0 }.to_bytes().unwrap_or_default()
    })
}

/// `Notice{code, arg}` (arg 0 when it is not finite).
pub(crate) fn notice(code: NoticeCode, arg: f64) -> Bytes {
    let arg = if arg.is_finite() { arg } else { 0.0 };
    Notice { code, arg }.to_bytes().unwrap_or_default()
}

/// `Ack{ref}`.
pub(crate) fn ack(r#ref: u32) -> Bytes {
    Ack { r#ref }.to_bytes().unwrap_or_default()
}

/// The fatal `Error` of a code and the close code that follows it (`close_code_for`).
pub(crate) fn fatal(r#ref: u32, code: ErrorCode) -> (Bytes, u16) {
    let close = scacelith_protocol::close_code_for(code).unwrap_or(crate::net::ws::CLOSE_INTERNAL);
    (error(r#ref, code, true, 0), close)
}
