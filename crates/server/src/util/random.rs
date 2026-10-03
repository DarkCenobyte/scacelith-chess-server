//! Cryptographically secure randomness from the operating system (`getrandom`): bytes, tokens,
//! UUIDs and unbiased integers. A failing system generator is an unrecoverable environment error
//! (no secret could be made safely), so these functions panic in that case.

use super::encoding::base64url_encode;

const RNG_FAILED: &str = "the operating system's random number generator failed";

/// Fills `buf` with random bytes.
pub fn fill(buf: &mut [u8]) {
    getrandom::fill(buf).expect(RNG_FAILED);
}

/// `n` random bytes.
pub fn bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    fill(&mut v);
    v
}

/// A random array.
pub fn array<const N: usize>() -> [u8; N] {
    let mut a = [0u8; N];
    fill(&mut a);
    a
}

/// `prefix` followed by `n` random bytes in base64url without padding, the shape of the server's
/// tokens (`token("sct_", 32)` gives `sct_` and 43 characters).
pub fn token(prefix: &str, n: usize) -> String {
    let mut s = String::with_capacity(prefix.len() + n.div_ceil(3) * 4);
    s.push_str(prefix);
    s.push_str(&base64url_encode(&bytes(n)));
    s
}

/// `n` random bytes as lowercase hexadecimal (`2 n` characters).
pub fn hex(n: usize) -> String {
    hex::encode(bytes(n))
}

/// A random (version 4) UUID in its lowercase hyphenated form, as `crypto.randomUUID()`.
pub fn uuid_v4() -> String {
    let mut b: [u8; 16] = array();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// A uniformly distributed integer in `0..n`, without modulo bias (`crypto.randomInt(n)`).
///
/// # Panics
/// When `n` is 0.
pub fn below(n: u64) -> u64 {
    assert!(n > 0, "random::below needs a non-empty range");
    // Rejection sampling: draws beyond the largest multiple of n are retried.
    let zone = u64::MAX - (u64::MAX % n + 1) % n;
    loop {
        let v = getrandom::u64().expect(RNG_FAILED);
        if v <= zone {
            return v % n;
        }
    }
}

/// A uniformly distributed float in `[0, 1)` with 53 random bits (`Math.random()`, but from the
/// system generator).
pub fn unit_f64() -> f64 {
    (getrandom::u64().expect(RNG_FAILED) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(bytes(48).len(), 48);
        assert_ne!(bytes(16), bytes(16));
        let t = token("sct_", 32);
        assert!(t.starts_with("sct_") && t.len() == 4 + 43, "{t}");
        assert_eq!(hex(16).len(), 32);
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "{u}");
        assert_eq!(u.matches('-').count(), 4);
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut seen = [false; 6];
        for _ in 0..600 {
            let v = below(6);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
        assert_eq!(below(1), 0);
        for _ in 0..100 {
            let f = unit_f64();
            assert!((0.0..1.0).contains(&f));
        }
    }
}
