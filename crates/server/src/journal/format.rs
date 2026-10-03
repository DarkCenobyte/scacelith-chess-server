//! The on-disk format of the journal: records and segment names.
//!
//! A segment is a sequence of records, little-endian:
//!
//! ```text
//! u32 len      17 + payload length (the bytes between this field and the CRC)
//! u8  kind     RecordKind
//! u64 game     game id (below 2^53)
//! f64 at       epoch milliseconds, fractional allowed
//! ... payload  opaque bytes (the game module's encoding), at most MAX_PAYLOAD
//! u32 crc32c   CRC-32C of every byte of the record before it, `len` included
//! ```
//!
//! A reader stops at the first record whose length is impossible, which is cut short (torn
//! write) or whose CRC or kind is wrong; the records before it stand.

use super::crc::crc32c;
use crate::ids::GameId;

/// Largest payload of one record.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

/// Bytes of the length field.
const LEN_BYTES: usize = 4;
/// Bytes of kind + game + at.
const FIXED: usize = 17;
/// Bytes of the CRC trailer.
const CRC_BYTES: usize = 4;

/// Bytes of a record besides its payload.
pub const RECORD_OVERHEAD: usize = LEN_BYTES + FIXED + CRC_BYTES;

/// The kind of a record. The journal itself interprets only `Committed` (the game is in the
/// database: its records may go) and `Snapshot` (the whole state of the game: it supersedes the
/// game's earlier records).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// A game was created (its spec).
    Created = 1,
    /// A move.
    Move = 2,
    /// Any other event of a game (offers, presence, checkpoints).
    Event = 3,
    /// The game ended.
    Ended = 4,
    /// The game is committed to the database.
    Committed = 5,
    /// The whole state of a game.
    Snapshot = 6,
}

impl RecordKind {
    /// The kind of a byte read from disk.
    pub fn from_u8(b: u8) -> Option<RecordKind> {
        Some(match b {
            1 => RecordKind::Created,
            2 => RecordKind::Move,
            3 => RecordKind::Event,
            4 => RecordKind::Ended,
            5 => RecordKind::Committed,
            6 => RecordKind::Snapshot,
            _ => return None,
        })
    }

    /// The byte written to disk.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Appends one record to `out`. The caller has checked `payload.len() <= MAX_PAYLOAD`.
pub fn encode_record(out: &mut Vec<u8>, kind: RecordKind, game: GameId, at: f64, payload: &[u8]) {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let start = out.len();
    out.reserve(RECORD_OVERHEAD + payload.len());
    out.extend_from_slice(&((FIXED + payload.len()) as u32).to_le_bytes());
    out.push(kind.as_u8());
    out.extend_from_slice(&game.to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
    out.extend_from_slice(payload);
    let crc = crc32c(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// A record inside a segment buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordRef<'a> {
    pub kind: RecordKind,
    pub game: GameId,
    pub at: f64,
    pub payload: &'a [u8],
}

/// Why a segment stops before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The last record is cut short (a write the process or the machine did not finish).
    Torn,
    /// A length no record can have (garbage, such as zero-filled blocks after a power loss).
    BadLength,
    /// The CRC does not match.
    Crc,
    /// A valid record of a kind this server does not know.
    BadKind,
}

impl ParseError {
    /// The name used in logs and stats.
    pub fn as_str(self) -> &'static str {
        match self {
            ParseError::Torn => "torn",
            ParseError::BadLength => "bad_length",
            ParseError::Crc => "crc",
            ParseError::BadKind => "bad_kind",
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What [`parse_segment`] read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOutcome {
    /// Bytes of valid records at the start of the buffer.
    pub end: usize,
    /// Why parsing stopped before the end of the buffer, if it did.
    pub error: Option<ParseError>,
}

fn u32_at(buf: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
}

fn u64_at(buf: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[o..o + 8]);
    u64::from_le_bytes(b)
}

/// Calls `on_record` for each record of a segment, in order, and stops at the first invalid one.
pub fn parse_segment<'a>(buf: &'a [u8], mut on_record: impl FnMut(RecordRef<'a>)) -> ParseOutcome {
    let mut o = 0;
    let stop = |end, error| ParseOutcome { end, error: Some(error) };
    while o < buf.len() {
        if buf.len() - o < LEN_BYTES {
            return stop(o, ParseError::Torn);
        }
        let len = u32_at(buf, o) as usize;
        if !(FIXED..=FIXED + MAX_PAYLOAD).contains(&len) {
            return stop(o, ParseError::BadLength);
        }
        let body = o + LEN_BYTES;
        let crc_at = body + len;
        if buf.len() - o < LEN_BYTES + len + CRC_BYTES {
            return stop(o, ParseError::Torn);
        }
        if crc32c(&buf[o..crc_at]) != u32_at(buf, crc_at) {
            return stop(o, ParseError::Crc);
        }
        let Some(kind) = RecordKind::from_u8(buf[body]) else {
            return stop(o, ParseError::BadKind);
        };
        let game = u64_at(buf, body + 1);
        let at = f64::from_bits(u64_at(buf, body + 9));
        on_record(RecordRef { kind, game, at, payload: &buf[body + FIXED..crc_at] });
        o = crc_at + CRC_BYTES;
    }
    ParseOutcome { end: o, error: None }
}

/// The file name of segment `seq`.
pub fn segment_name(seq: u64) -> String {
    format!("segment-{seq:010}.log")
}

/// The number of a segment file name (only the names [`segment_name`] makes).
pub fn segment_seq(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("segment-")?.strip_suffix(".log")?;
    if digits.len() < 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seq: u64 = digits.parse().ok()?;
    (segment_name(seq) == name).then_some(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Parsed = (RecordKind, GameId, f64, Vec<u8>);

    fn records(buf: &[u8]) -> (Vec<Parsed>, ParseOutcome) {
        let mut out = Vec::new();
        let res = parse_segment(buf, |r| out.push((r.kind, r.game, r.at, r.payload.to_vec())));
        (out, res)
    }

    #[test]
    fn records_round_trip_bit_exact() {
        let mut buf = Vec::new();
        let big = (1u64 << 53) - 1;
        encode_record(&mut buf, RecordKind::Created, 11, 1000.5, b"{\"white\":1}");
        encode_record(&mut buf, RecordKind::Move, big, -0.0, &[1, 2]);
        encode_record(&mut buf, RecordKind::Committed, 0, f64::MAX, &[]);
        assert_eq!(buf.len(), 3 * RECORD_OVERHEAD + 11 + 2);
        // The first record, byte by byte.
        assert_eq!(&buf[..4], &28u32.to_le_bytes());
        assert_eq!(buf[4], 1);
        assert_eq!(&buf[5..13], &11u64.to_le_bytes());
        assert_eq!(&buf[13..21], &1000.5f64.to_le_bytes());
        assert_eq!(&buf[21..32], b"{\"white\":1}");
        assert_eq!(&buf[32..36], &crc32c(&buf[..32]).to_le_bytes());
        let (recs, res) = records(&buf);
        assert_eq!(res, ParseOutcome { end: buf.len(), error: None });
        assert_eq!(recs[0], (RecordKind::Created, 11, 1000.5, b"{\"white\":1}".to_vec()));
        assert_eq!(recs[1].1, big);
        assert!(recs[1].2 == 0.0 && recs[1].2.is_sign_negative(), "at is kept bit for bit");
        assert_eq!(recs[2], (RecordKind::Committed, 0, f64::MAX, vec![]));
        assert_eq!(records(&[]).1, ParseOutcome { end: 0, error: None });
    }

    #[test]
    fn parsing_stops_at_the_first_invalid_record() {
        let mut buf = Vec::new();
        for i in 0..4u8 {
            encode_record(&mut buf, RecordKind::Move, 7, f64::from(i), &[i; 10]);
        }
        let rec = RECORD_OVERHEAD + 10;
        let at = |res: ParseOutcome| (res.end, res.error);
        // Torn inside the length field, inside the body, inside the CRC.
        for cut in [rec * 3 + 2, rec * 3 + 4, rec * 3 + 20, buf.len() - 1] {
            let (recs, res) = records(&buf[..cut]);
            assert_eq!((recs.len(), at(res)), (3, (rec * 3, Some(ParseError::Torn))), "cut at {cut}");
        }
        // A flipped bit anywhere in a record: its length, kind, game, at, payload or CRC.
        for offset in [0, 4, 6, 15, 25, rec - 1] {
            let mut bad = buf.clone();
            bad[rec * 2 + offset] ^= 0x10;
            let (recs, res) = records(&bad);
            assert_eq!(recs.len(), 2, "flip at {offset}");
            assert_eq!(res.end, rec * 2);
            let expected =
                if offset == 0 { [ParseError::Torn, ParseError::BadLength] } else { [ParseError::Crc; 2] };
            assert!(expected.contains(&res.error.unwrap()), "flip at {offset}: {:?}", res.error);
        }
        // Zero-filled blocks after valid records.
        let mut zeros = buf.clone();
        zeros.extend_from_slice(&[0; 100]);
        assert_eq!(at(records(&zeros).1), (buf.len(), Some(ParseError::BadLength)));
        // An over-long length.
        let mut long = buf[..rec].to_vec();
        long.extend_from_slice(&((17 + MAX_PAYLOAD + 1) as u32).to_le_bytes());
        long.extend_from_slice(&[0; 64]);
        assert_eq!(at(records(&long).1), (rec, Some(ParseError::BadLength)));
        // A valid CRC over an unknown kind.
        let mut unknown = buf[..rec].to_vec();
        let start = unknown.len();
        unknown.extend_from_slice(&17u32.to_le_bytes());
        unknown.push(9);
        unknown.extend_from_slice(&[0; 16]);
        let crc = crc32c(&unknown[start..]);
        unknown.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(at(records(&unknown).1), (rec, Some(ParseError::BadKind)));
    }

    #[test]
    fn segment_names() {
        assert_eq!(segment_name(1), "segment-0000000001.log");
        assert_eq!(segment_name(12_345_678_901), "segment-12345678901.log");
        assert_eq!(segment_seq("segment-0000000042.log"), Some(42));
        assert_eq!(segment_seq("segment-12345678901.log"), Some(12_345_678_901));
        for bad in [
            "segment-42.log",
            "segment-00000000042.log",
            "segment-000000004x.log",
            "segment-0000000042.tmp",
            "x",
        ] {
            assert_eq!(segment_seq(bad), None, "{bad}");
        }
    }
}
