//! Base64 in the variants the server uses: base64url without padding (tokens, hashes in URLs),
//! standard base64 with padding (generated secrets, SMTP), and Node's lenient decoder, which the
//! configuration must reproduce bit for bit because every key derived from `SERVER_SECRET`
//! depends on the decoded bytes.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// base64url without padding (RFC 4648 section 5), Node's `toString('base64url')`.
pub fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Strict base64url without padding: `None` for any character outside the alphabet, padding, or
/// a length that cannot come from the encoder.
pub fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Standard base64 with padding, Node's `toString('base64')`.
pub fn base64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Strict standard base64 with padding.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

/// Decodes like Node's `Buffer.from(text, 'base64')` (and `'base64url'`, the same decoder).
///
/// It never fails: both alphabets are accepted (`+/` and `-_`), characters outside them are
/// skipped (spaces, quotes, line breaks), decoding stops at the first `=`, and a trailing group of
/// 2 or 3 characters gives 1 or 2 bytes (a lone character gives none). Node reads each UTF-16
/// code unit through its low byte, so `"Ł"` (U+0141) counts as `A` and any character whose low
/// byte is `=` (U+013D, every high surrogate of an emoji) ends the input.
pub fn base64_decode_lenient(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut count = 0;
    for unit in text.encode_utf16() {
        let byte = (unit & 0xff) as u8;
        if byte == b'=' {
            break;
        }
        let Some(sextet) = sextet(byte) else { continue };
        acc = (acc << 6) | u32::from(sextet);
        count += 1;
        if count == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            count = 0;
        }
    }
    match count {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    }
    out
}

fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trip_without_padding() {
        assert_eq!(base64url_encode(&[0xfb, 0xff, 0xbf]), "-_-_");
        assert_eq!(base64url_encode(b"ab"), "YWI");
        assert_eq!(base64url_decode("YWI").unwrap(), b"ab");
        assert!(base64url_decode("YWI=").is_none());
        assert!(base64url_decode("Y+I").is_none());
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
    }

    /// Vectors produced by Node 22's `Buffer.from(s, 'base64').toString('hex')`.
    #[test]
    fn lenient_decoder_matches_node() {
        let cases: &[(&str, &str)] = &[
            ("QUJD", "414243"),
            ("\"QUJD\"", "414243"),
            ("QU=JD", "41"),
            ("QUJD!!RUY", "4142434546"),
            ("QQ==QQ", "41"),
            ("Q", ""),
            ("QU", "41"),
            ("QUJ", "4142"),
            ("QUJDR", "414243"),
            ("QUJDRQ", "41424345"),
            ("QUJDRUY=", "4142434546"),
            ("QU JD", "414243"),
            ("QUJD\nRUY", "4142434546"),
            ("-_-_", "fbffbf"),
            ("+/+/", "fbffbf"),
            ("éQUJD", "414243"),
            ("QUJD=", "414243"),
            ("=QUJD", ""),
            ("QUJD==RUY", "414243"),
            ("QU\tJD", "414243"),
            ("", ""),
            ("====", ""),
            ("Q=", ""),
            ("QUI=RUY", "4142"),
            ("a.b.c.d", "69b71d"),
            ("QUJ\u{0}D", "414243"),
            ("//8", "ffff"),
            ("__8", "ffff"),
            ("QUJD%RUY", "4142434546"),
            ("Q\u{e9}U\u{4e2d}JD", "414f89"),
            ("QUJDR=UY", "414243"),
            ("QUJDRU=Y", "41424345"),
            ("QU\u{141}JD", "414009"),
            ("QUJ\u{144}", "414243"),
            ("QUJD\u{13d}RUY", "414243"),
            ("QUJD\u{bd}RUY", "4142434546"),
            ("Q😀UJD", ""),
            ("QUJD\u{100}RUY", "4142434546"),
        ];
        for (text, hex) in cases {
            assert_eq!(hex::encode(base64_decode_lenient(text)), *hex, "{text:?}");
        }
    }
}
