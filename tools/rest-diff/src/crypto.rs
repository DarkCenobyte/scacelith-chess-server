//! What a client computes: authenticator codes (RFC 6238) and proofs of work (API.md 1.6).

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Decodes RFC 4648 base32 (upper or lower case, no padding needed).
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| *c != '=') {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u32 - 'A' as u32,
            c @ '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The 6-digit code of `secret` (base32) for the 30-second step `counter`.
pub fn totp(secret_b32: &str, counter: u64) -> String {
    let key = base32_decode(secret_b32).unwrap_or_default();
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&key).expect("HMAC takes keys of any length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = usize::from(h[h.len() - 1] & 0x0f);
    let bin = (u32::from(h[off] & 0x7f) << 24)
        | (u32::from(h[off + 1]) << 16)
        | (u32::from(h[off + 2]) << 8)
        | u32::from(h[off + 3]);
    format!("{:06}", bin % 1_000_000)
}

/// The current 30-second step.
pub fn totp_step() -> u64 {
    (crate::http::now_ms() / 30_000.0) as u64
}

/// A nonce such that SHA-256(`challenge` ":" nonce) starts with `bits` zero bits.
pub fn solve_pow(challenge: &str, bits: u32) -> String {
    let mut nonce: u64 = 0;
    loop {
        let text = nonce.to_string();
        let mut h = Sha256::new();
        h.update(challenge.as_bytes());
        h.update(b":");
        h.update(text.as_bytes());
        if leading_zero_bits(&h.finalize()) >= bits {
            return text;
        }
        nonce += 1;
    }
}

/// A nonce that does *not* solve the challenge (for the `work` refusal).
pub fn wrong_pow(challenge: &str, bits: u32) -> String {
    let mut nonce: u64 = 0;
    loop {
        let text = nonce.to_string();
        let mut h = Sha256::new();
        h.update(challenge.as_bytes());
        h.update(b":");
        h.update(text.as_bytes());
        if leading_zero_bits(&h.finalize()) < bits {
            return text;
        }
        nonce += 1;
    }
}

fn leading_zero_bits(h: &[u8]) -> u32 {
    let mut n = 0;
    for &b in h {
        if b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vector() {
        // RFC 6238 appendix B, SHA-1, T = 59 s: 94287082 (8 digits) -> 287082.
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        assert_eq!(base32_decode(secret).unwrap(), b"12345678901234567890");
        assert_eq!(totp(secret, 1), "287082");
    }

    #[test]
    fn pow_solutions() {
        let n = solve_pow("abc", 8);
        let mut h = Sha256::new();
        h.update(format!("abc:{n}").as_bytes());
        assert_eq!(h.finalize()[0], 0);
        let w = wrong_pow("abc", 8);
        let mut h = Sha256::new();
        h.update(format!("abc:{w}").as_bytes());
        assert_ne!(h.finalize()[0], 0);
    }
}
