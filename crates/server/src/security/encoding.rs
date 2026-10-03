//! Encodings shared by the security code, with the exact semantics of the former Node.js server:
//! its lenient base64 decoder (secrets, sealed TOTP secrets, password hashes), the JavaScript
//! whitespace set (`trim()` and the regex `\s`), UTF-16 lengths and `encodeURIComponent`.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};

/// Fills a buffer with bytes of the operating system's random generator.
///
/// # Panics
/// When the operating system cannot provide random bytes (it never fails on a supported Linux;
/// the server must not go on without randomness).
pub fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the operating system's random generator failed");
}

/// `N` bytes of the operating system's random generator (see [`fill_random`]).
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    fill_random(&mut b);
    b
}

/// Standard base64 without padding (`Buffer.toString('base64')` with the `=` stripped).
pub fn b64_std(bytes: &[u8]) -> String {
    STANDARD_NO_PAD.encode(bytes)
}

/// base64url without padding (`Buffer.toString('base64url')`).
pub fn b64_url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64_value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    } as u32)
}

/// Node's lenient base64 decoder (`Buffer.from(s, 'base64')`, identical for `'base64url'`): both
/// alphabets are accepted, decoding stops at the first `=`, every other character outside the
/// alphabets is skipped, and leftover bits that do not make a whole byte are dropped
/// (`"QU*JD"` -> `ABC`, `"QU=JD"` -> `A`, `"Q"` -> nothing).
pub fn node_b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = b64_value(c) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// True for the characters of JavaScript's `\s` and `String.prototype.trim()` (which differ
/// from `char::is_whitespace`: U+FEFF is one, U+0085 is not).
pub fn js_is_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// JavaScript's `String.prototype.trim()`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_is_space)
}

/// JavaScript's `String.prototype.length`: the number of UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// JavaScript's `encodeURIComponent`: every byte of the UTF-8 form outside
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` becomes `%XX` (upper-case hex).
pub fn encode_uri_component(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
        {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_base64_leniency() {
        // Vectors verified on Node 22 (porting notes, section 13.1).
        assert_eq!(node_b64_decode("QU*JD"), b"ABC");
        assert_eq!(node_b64_decode("QU=JD"), b"A");
        assert_eq!(node_b64_decode("Q"), b"");
        assert_eq!(node_b64_decode("QR"), b"A");
        assert_eq!(node_b64_decode("QUJDQQ=x"), b"ABCA");
        assert_eq!(node_b64_decode("-_-_"), node_b64_decode("+/+/"));
        assert_eq!(node_b64_decode("QU JD\n"), b"ABC");
        assert_eq!(node_b64_decode("QUJD€"), b"ABC");
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(node_b64_decode(&b64_std(&bytes)), bytes);
        assert_eq!(node_b64_decode(&b64_url(&bytes)), bytes);
    }

    #[test]
    fn encoders_strip_padding() {
        assert_eq!(b64_std(&[0xfb, 0xff]), "+/8");
        assert_eq!(b64_url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn javascript_whitespace() {
        assert_eq!(js_trim("\u{FEFF}\u{00A0} a b \u{3000}\n"), "a b");
        assert_eq!(js_trim("\u{0085}x"), "\u{0085}x");
        assert!(js_is_space('\u{2028}') && !js_is_space('\u{200B}'));
    }

    #[test]
    fn utf16_lengths() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("é"), 1);
        assert_eq!(utf16_len("😀"), 2);
    }

    #[test]
    fn uri_component_encoding() {
        assert_eq!(encode_uri_component("Scacelith Test"), "Scacelith%20Test");
        assert_eq!(encode_uri_component("A&B"), "A%26B");
        assert_eq!(encode_uri_component("a-_.!~*'()z"), "a-_.!~*'()z");
        assert_eq!(encode_uri_component("é/:"), "%C3%A9%2F%3A");
    }

    #[test]
    fn random_bytes_differ() {
        assert_ne!(random_bytes::<16>(), random_bytes::<16>());
    }
}
