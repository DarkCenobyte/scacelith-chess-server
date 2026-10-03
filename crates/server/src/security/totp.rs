//! TOTP second factor (RFC 6238 over RFC 4226 HOTP: HMAC-SHA1, 6 digits, 30 s steps, one step
//! of tolerance either way), base32 secrets and the `otpauth://` URI authenticator apps read.
//!
//! Replay protection is the caller's job: [`verify_totp`] returns the matched time step and the
//! caller stores it atomically, refusing any step not newer than the last one used.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use zeroize::Zeroizing;

use crate::security::encoding::{encode_uri_component, fill_random, js_is_space};
use crate::security::keys::safe_eq;

/// Digits of a code.
pub const TOTP_DIGITS: u32 = 6;
/// Length of a time step, in seconds.
pub const TOTP_PERIOD_S: u32 = 30;
/// Length of a new secret, in bytes (32 base32 characters).
pub const TOTP_SECRET_BYTES: usize = 20;
/// Steps accepted on either side of the current one.
pub const TOTP_WINDOW: i64 = 1;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding (20 bytes give 32 characters).
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut value, mut bits) = (0u32, 0u32);
    for &b in bytes {
        value = (value << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            out.push(char::from(B32[((value >> (bits - 5)) & 31) as usize]));
            bits -= 5;
        }
        value &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(B32[((value << (5 - bits)) & 31) as usize]));
    }
    out
}

/// Decodes base32: case-insensitive, whitespace and `=` ignored, leftover bits dropped. `None`
/// when another character is present.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut value, mut bits) = (0u32, 0u32);
    for c in s.to_uppercase().chars().filter(|&c| c != '=' && !js_is_space(c)) {
        let v = B32.iter().position(|&b| char::from(b) == c)? as u32;
        value = ((value << 5) | v) & 0xffff;
        bits += 5;
        if bits >= 8 {
            out.push((value >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// The HOTP value (RFC 4226) of `counter`: HMAC-SHA1 over the 8-byte big-endian counter, dynamic
/// truncation, the last `digits` (1 to 10) decimal digits, zero-padded.
pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> String {
    let digits = digits.clamp(1, 10);
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret).expect("HMAC takes keys of any length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = usize::from(h[h.len() - 1] & 0x0f);
    let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    format!("{:0width$}", u64::from(bin) % 10u64.pow(digits), width = digits as usize)
}

/// The time step of `now_ms` (wall clock, ms since the Unix epoch).
pub fn totp_step(now_ms: i64) -> i64 {
    now_ms.div_euclid(i64::from(TOTP_PERIOD_S) * 1000)
}

/// The 6-digit code at `now_ms`.
pub fn totp(secret: &[u8], now_ms: i64) -> String {
    // A step before 1970 wraps like the former server's two 32-bit halves.
    hotp(secret, totp_step(now_ms) as u64, TOTP_DIGITS)
}

/// True when `s` has the shape of a code: exactly 6 ASCII digits.
pub fn is_totp_code(s: &str) -> bool {
    s.len() == TOTP_DIGITS as usize && s.bytes().all(|b| b.is_ascii_digit())
}

/// Checks `code` against the steps around `now_ms` ([`TOTP_WINDOW`] either way), skipping the
/// steps not newer than `last_step` (already used; -1 when none). Every candidate is computed and
/// compared in constant time. Returns the lowest matching step.
pub fn verify_totp(secret: &[u8], code: &str, now_ms: i64, last_step: i64) -> Option<i64> {
    if !is_totp_code(code) {
        return None;
    }
    let current = totp_step(now_ms);
    let mut matched = None;
    for step in current - TOTP_WINDOW..=current + TOTP_WINDOW {
        let candidate = Zeroizing::new(hotp(secret, step as u64, TOTP_DIGITS));
        let ok = safe_eq(candidate.as_bytes(), code.as_bytes());
        if ok && step > last_step && matched.is_none() {
            matched = Some(step);
        }
    }
    matched
}

/// A new random secret of [`TOTP_SECRET_BYTES`].
pub fn generate_totp_secret() -> Zeroizing<[u8; TOTP_SECRET_BYTES]> {
    let mut s = Zeroizing::new([0u8; TOTP_SECRET_BYTES]);
    fill_random(s.as_mut());
    s
}

/// The `otpauth://` URI authenticator apps read (usually from a QR code). `issuer` is the server
/// name, `account` the username.
pub fn otpauth_uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    let iss = encode_uri_component(issuer);
    format!(
        "otpauth://totp/{iss}:{}?secret={}&issuer={iss}&algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_PERIOD_S}",
        encode_uri_component(account),
        base32_encode(secret),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1 column (seed "12345678901234567890", 8 digits).
    const RFC_SECRET: &[u8] = b"12345678901234567890";
    const RFC_VECTORS: &[(i64, &str)] = &[
        (59, "94287082"),
        (1111111109, "07081804"),
        (1111111111, "14050471"),
        (1234567890, "89005924"),
        (2000000000, "69279037"),
        (20000000000, "65353130"),
    ];

    /// 2026-09-28T12:00:10Z.
    const NOW: i64 = 1_790_596_810_000;

    fn random_secret() -> Vec<u8> {
        generate_totp_secret().to_vec()
    }

    #[test]
    fn rfc_6238_appendix_b_vectors() {
        for &(t, code) in RFC_VECTORS {
            assert_eq!(hotp(RFC_SECRET, totp_step(t * 1000) as u64, 8), code, "T={t}");
            assert_eq!(totp(RFC_SECRET, t * 1000), code[2..], "6 digits at T={t}");
        }
    }

    #[test]
    fn rfc_4226_appendix_d_values() {
        let expect = [
            "755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583", "399871",
            "520489",
        ];
        for (i, c) in expect.iter().enumerate() {
            assert_eq!(hotp(RFC_SECRET, i as u64, 6), *c);
        }
        assert_eq!(totp(RFC_SECRET, 59_000), "287082");
        assert_eq!(totp(RFC_SECRET, 1_111_111_109_000), "081804");
    }

    #[test]
    fn base32_round_trip_and_rfc_4648_vectors() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(base32_decode("MZXW 6YTB OI======").unwrap(), b"foobar");
        assert_eq!(base32_decode("MZ1"), None);
        for n in 0..50 {
            let mut b = vec![0u8; n];
            fill_random(&mut b);
            assert_eq!(base32_decode(&base32_encode(&b)).unwrap(), b);
        }
        let s = generate_totp_secret();
        assert_eq!(s.len(), 20);
        assert_eq!(base32_encode(s.as_ref()).len(), 32);
        let bytes: Vec<u8> = (1..=20).collect();
        assert_eq!(base32_encode(&bytes), "AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU");
    }

    #[test]
    fn verification_accepts_the_current_step_and_one_either_way() {
        let secret = random_secret();
        let step = totp_step(NOW);
        for d in [-1, 0, 1] {
            assert_eq!(verify_totp(&secret, &hotp(&secret, (step + d) as u64, 6), NOW, -1), Some(step + d));
        }
        for d in [-2, 2] {
            assert_eq!(verify_totp(&secret, &hotp(&secret, (step + d) as u64, 6), NOW, -1), None);
        }
    }

    #[test]
    fn verification_refuses_used_steps_and_malformed_codes() {
        let secret = random_secret();
        let step = totp_step(NOW);
        let code = hotp(&secret, step as u64, 6);
        assert_eq!(verify_totp(&secret, &code, NOW, step), None, "replay");
        assert_eq!(verify_totp(&secret, &code, NOW, step - 1), Some(step));
        assert_eq!(verify_totp(&secret, &hotp(&secret, (step - 1) as u64, 6), NOW, step), None);
        for bad in ["", "12345", "1234567", "abcdef", " 123456", "١٢٣٤٥٦"] {
            assert_eq!(verify_totp(&secret, bad, NOW, -1), None, "{bad:?}");
        }
        assert!(is_totp_code("012345") && !is_totp_code("01234a"));
    }

    #[test]
    fn otpauth_uri_carries_the_url_encoded_issuer() {
        let uri = otpauth_uri("Scacelith Community Server", "alice", RFC_SECRET);
        assert_eq!(
            uri,
            "otpauth://totp/Scacelith%20Community%20Server:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ\
             &issuer=Scacelith%20Community%20Server&algorithm=SHA1&digits=6&period=30"
        );
        let other = otpauth_uri("A&B", "bob", RFC_SECRET);
        assert!(other.starts_with("otpauth://totp/A%26B:bob?") && other.contains("issuer=A%26B&"));
        let bytes: Vec<u8> = (1..=20).collect();
        assert_eq!(
            otpauth_uri("Scacelith Test", "alice", &bytes),
            "otpauth://totp/Scacelith%20Test:alice?secret=AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU\
             &issuer=Scacelith%20Test&algorithm=SHA1&digits=6&period=30"
        );
    }
}
