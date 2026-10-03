//! WebSocket frames (RFC 6455 section 5): encoding with or without a mask, and parsing with
//! the checks of the receiving side. The client side masks what it sends and refuses masked
//! frames; the server side (the in-test fake servers) does the opposite.

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Continuation frame.
pub const OP_CONTINUATION: u8 = 0x0;
/// Text frame.
pub const OP_TEXT: u8 = 0x1;
/// Binary frame.
pub const OP_BINARY: u8 = 0x2;
/// Close frame.
pub const OP_CLOSE: u8 = 0x8;
/// Ping frame.
pub const OP_PING: u8 = 0x9;
/// Pong frame.
pub const OP_PONG: u8 = 0xA;

/// Longest payload of a control frame.
pub const MAX_CONTROL_PAYLOAD: usize = 125;

/// One frame, as read (payload unmasked) or to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Final fragment of its message.
    pub fin: bool,
    /// Opcode (`OP_*`).
    pub opcode: u8,
    /// Payload, unmasked.
    pub payload: Bytes,
}

impl Frame {
    /// A final frame.
    pub fn new(opcode: u8, payload: impl Into<Bytes>) -> Frame {
        Frame { fin: true, opcode, payload: payload.into() }
    }

    /// Whether the opcode is a control opcode (close, ping, pong and the reserved 0xB-0xF).
    pub fn is_control(&self) -> bool {
        self.opcode & 0x8 != 0
    }
}

/// Which end reads the frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The client: frames from the server must not be masked.
    Client,
    /// The server: frames from the client must be masked.
    Server,
}

/// A frame the receiver refuses, with the close code it answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameError {
    /// Close code (1002 protocol error, 1009 too big).
    pub code: u16,
    /// What is wrong.
    pub reason: &'static str,
}

impl FrameError {
    fn protocol(reason: &'static str) -> FrameError {
        FrameError { code: 1002, reason }
    }
}

/// Appends a frame header for a payload of `len` bytes to `out`, with the masking key when
/// given (client frames); the payload, masked with the same key, must follow.
pub fn encode_header(out: &mut BytesMut, fin: bool, opcode: u8, len: usize, mask: Option<[u8; 4]>) {
    out.reserve(14 + len);
    out.put_u8(if fin { 0x80 } else { 0 } | (opcode & 0x0F));
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    if len < 126 {
        out.put_u8(mask_bit | len as u8);
    } else if let Ok(len16) = u16::try_from(len) {
        out.put_u8(mask_bit | 126);
        out.put_u16(len16);
    } else {
        out.put_u8(mask_bit | 127);
        out.put_u64(len as u64);
    }
    if let Some(key) = mask {
        out.put_slice(&key);
    }
}

/// Appends a frame to `out`, masked with `mask` when given (client frames).
pub fn encode_frame(out: &mut BytesMut, fin: bool, opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) {
    encode_header(out, fin, opcode, payload.len(), mask);
    let start = out.len();
    out.put_slice(payload);
    if let Some(key) = mask {
        apply_mask(&mut out[start..], key);
    }
}

/// XORs `data` with the masking key (masking and unmasking are the same operation).
pub fn apply_mask(data: &mut [u8], key: [u8; 4]) {
    let word = u32::from_ne_bytes(key);
    let wide = u64::from(word) | (u64::from(word) << 32);
    let (chunks, rest) = data.as_chunks_mut::<8>();
    for chunk in chunks {
        *chunk = (u64::from_ne_bytes(*chunk) ^ wide).to_ne_bytes();
    }
    for (i, b) in rest.iter_mut().enumerate() {
        *b ^= key[i % 4];
    }
}

/// Takes one complete frame off the front of `buf`: `Ok(None)` when more bytes are needed.
/// `max_payload` bounds one frame's payload (checked from the header, before it is buffered).
pub fn parse_frame(buf: &mut BytesMut, role: Role, max_payload: usize) -> Result<Option<Frame>, FrameError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    if b0 & 0x70 != 0 {
        return Err(FrameError::protocol("reserved bits set"));
    }
    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0F;
    let masked = b1 & 0x80 != 0;
    match (role, masked) {
        (Role::Client, true) => return Err(FrameError::protocol("masked frame from the server")),
        (Role::Server, false) => return Err(FrameError::protocol("unmasked frame from the client")),
        _ => {}
    }
    if !matches!(opcode, OP_CONTINUATION | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG) {
        return Err(FrameError::protocol("reserved opcode"));
    }
    let control = opcode & 0x8 != 0;
    let (len, header) = match b1 & 0x7F {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u64::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let len = u64::from_be_bytes(buf[2..10].try_into().expect("8 bytes"));
            if len >> 63 != 0 {
                return Err(FrameError::protocol("payload length with the top bit set"));
            }
            (len, 10)
        }
        n => (u64::from(n), 2),
    };
    if control && (!fin || len > MAX_CONTROL_PAYLOAD as u64) {
        return Err(FrameError::protocol("fragmented or oversized control frame"));
    }
    if len > max_payload as u64 {
        return Err(FrameError { code: 1009, reason: "message too big" });
    }
    let len = len as usize;
    let mask_len = if masked { 4 } else { 0 };
    let total = header + mask_len + len;
    if buf.len() < total {
        buf.reserve(total - buf.len());
        return Ok(None);
    }
    let key = masked.then(|| <[u8; 4]>::try_from(&buf[header..header + 4]).expect("4 bytes"));
    buf.advance(header + mask_len);
    let mut payload = buf.split_to(len);
    if let Some(key) = key {
        apply_mask(&mut payload, key);
    }
    Ok(Some(Frame { fin, opcode, payload: payload.freeze() }))
}

/// Payload of a close frame: the code (none for 1005) and the reason, cut to fit a control frame.
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    if code == 1005 {
        return Vec::new();
    }
    let mut cut = reason.len().min(MAX_CONTROL_PAYLOAD - 2);
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = Vec::with_capacity(2 + cut);
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&reason.as_bytes()[..cut]);
    out
}

/// Code and reason of a received close payload (1005 when empty); `Err` for a payload of one
/// byte, a reason that is not UTF-8 or a code a peer may not send.
pub fn parse_close(payload: &[u8]) -> Result<(u16, String), FrameError> {
    match payload {
        [] => Ok((1005, String::new())),
        [_] => Err(FrameError::protocol("close payload of one byte")),
        [a, b, reason @ ..] => {
            let code = u16::from_be_bytes([*a, *b]);
            if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
                return Err(FrameError::protocol("invalid close code"));
            }
            let reason = std::str::from_utf8(reason)
                .map_err(|_| FrameError { code: 1007, reason: "close reason not UTF-8" })?;
            Ok((code, reason.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(len: usize, mask: Option<[u8; 4]>, role: Role) {
        let payload: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
        let mut buf = BytesMut::new();
        encode_frame(&mut buf, true, OP_BINARY, &payload, mask);
        let header = match len {
            0..126 => 2,
            126..65536 => 4,
            _ => 10,
        } + if mask.is_some() { 4 } else { 0 };
        assert_eq!(buf.len(), header + len);
        // Every prefix is incomplete, the whole buffer is one frame.
        for cut in [0, 1, header - 1, header + len - 1] {
            let mut part = BytesMut::from(&buf[..cut]);
            assert_eq!(parse_frame(&mut part, role, 1 << 20), Ok(None), "len {len} cut {cut}");
        }
        let frame = parse_frame(&mut buf, role, 1 << 20).unwrap().unwrap();
        assert_eq!((frame.fin, frame.opcode, &frame.payload[..]), (true, OP_BINARY, &payload[..]));
        assert!(buf.is_empty());
    }

    #[test]
    fn lengths_and_masks() {
        for len in [0, 1, 125, 126, 1000, 65535, 65536, 70000] {
            round_trip(len, None, Role::Client);
            round_trip(len, Some([0x12, 0x34, 0x56, 0x78]), Role::Server);
        }
    }

    #[test]
    fn mask_is_rfc_example() {
        // RFC 6455 5.7: a masked "Hello".
        let mut buf = BytesMut::new();
        encode_frame(&mut buf, true, OP_TEXT, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d]));
        assert_eq!(&buf[..], &[0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]);
    }

    #[test]
    fn refusals() {
        let check = |bytes: &[u8], role: Role, max: usize| parse_frame(&mut BytesMut::from(bytes), role, max);
        assert_eq!(check(&[0x82, 0x81, 1, 2, 3, 4, 5], Role::Client, 100).unwrap_err().code, 1002);
        assert_eq!(check(&[0x82, 0x01, 5], Role::Server, 100).unwrap_err().code, 1002);
        assert_eq!(check(&[0xC2, 0x00], Role::Client, 100).unwrap_err().reason, "reserved bits set");
        assert_eq!(check(&[0x83, 0x00], Role::Client, 100).unwrap_err().reason, "reserved opcode");
        assert_eq!(check(&[0x09, 0x00], Role::Client, 100).unwrap_err().code, 1002, "fragmented ping");
        assert_eq!(check(&[0x89, 126, 0, 126], Role::Client, 1000).unwrap_err().code, 1002, "ping over 125");
        assert_eq!(check(&[0x82, 126, 0x01, 0x00], Role::Client, 255).unwrap_err().code, 1009);
        let mut huge = vec![0x82, 127, 0x80];
        huge.extend_from_slice(&[0; 7]);
        assert_eq!(check(&huge, Role::Client, 100).unwrap_err().code, 1002);
    }

    #[test]
    fn close_payloads() {
        assert_eq!(parse_close(&close_payload(4003, "unauthorized")), Ok((4003, "unauthorized".into())));
        assert_eq!(parse_close(&close_payload(1005, "ignored")), Ok((1005, String::new())));
        let long = "é".repeat(100);
        let payload = close_payload(1000, &long);
        assert!(payload.len() <= MAX_CONTROL_PAYLOAD);
        assert!(parse_close(&payload).is_ok(), "cut on a character boundary");
        assert_eq!(parse_close(&[3]).unwrap_err().code, 1002);
        assert_eq!(parse_close(&[0x03, 0xEE]).unwrap_err().code, 1002, "1006 may not be sent");
        assert_eq!(parse_close(&[0x03, 0xE8, 0xFF]).unwrap_err().code, 1007);
    }
}
