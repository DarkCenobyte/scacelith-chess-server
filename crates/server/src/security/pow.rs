//! Proof of work (DESIGN.md section 8).
//!
//! An endpoint that wants one answers HTTP 428
//! `{"error":"pow_required","pow":{"challenge":"<opaque ASCII>","bits":18,"expiresAt":ms}}` and
//! the client repeats the request with `"pow":{"challenge","nonce"}` in its JSON body, where the
//! nonce is a decimal ASCII string such that SHA-256(challenge + ":" + nonce) starts with `bits`
//! zero bits (most significant bit of the first byte first).
//!
//! Challenge format: `base64url(JSON payload) + "." + base64url(HMAC-SHA256(pow key, payload
//! part))`, payload `{"v":1,"h":<keyed hash of the client network>,"e":<endpoint>,"b":<bits>,
//! "x":<expiry ms>,"r":<random>}`. The server keeps no state until the answer comes back; single
//! use is enforced with a single-use key (the signature) for the challenge's remaining life.
//! Expiries are wall-clock times.

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::clock::SharedClock;
use crate::security::encoding::{b64_url, node_b64_decode, random_bytes};
use crate::security::keys::{AuthKeys, Key, hmac_sha256, safe_eq};
use crate::security::ratelimit::{LocalControl, ip_key};

/// Lifetime of a challenge, in ms.
pub const POW_TTL_MS: i64 = 120_000;

/// Allowance for the client's clock in the expiry check, in ms.
const EXPIRY_SLACK_MS: i64 = 1000;

/// Number of leading zero bits of `bytes`, most significant bit first.
pub fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut n = 0;
    for &b in bytes {
        if b != 0 {
            return n + b.leading_zeros();
        }
        n += 8;
    }
    n
}

/// True when SHA-256(`challenge` + ":" + `nonce`) has at least `bits` leading zero bits.
pub fn check_work(challenge: &str, nonce: &str, bits: u32) -> bool {
    let mut h = Sha256::new();
    h.update(challenge.as_bytes());
    h.update(b":");
    h.update(nonce.as_bytes());
    leading_zero_bits(&h.finalize()) >= bits
}

/// Finds a nonce for `challenge` (what the game client does; for tests and tools).
pub fn solve_pow(challenge: &str, bits: u32) -> String {
    (0u64..)
        .map(|n| n.to_string())
        .find(|n| check_work(challenge, n, bits))
        .expect("a nonce is eventually found")
}

/// A new challenge, as the 428 answer carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowChallenge {
    /// The opaque challenge text.
    pub challenge: String,
    /// The difficulty, in leading zero bits.
    pub bits: u32,
    /// Wall-clock expiry, ms since the Unix epoch.
    pub expires_at: i64,
}

impl PowChallenge {
    /// The `pow` object of the 428 answer: `{"challenge","bits","expiresAt"}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "challenge": self.challenge, "bits": self.bits, "expiresAt": self.expires_at })
    }
}

/// Why an answer was refused (the `reason` of the 428 answer and of the `pow_failed` event).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowRefusal {
    /// Not a challenge or not a decimal nonce.
    Malformed,
    /// The challenge was not issued by this server (or was altered).
    Signature,
    /// Issued for another endpoint.
    Endpoint,
    /// Issued to another client network.
    Network,
    /// Out of its lifetime.
    Expired,
    /// Issued with fewer bits than required now.
    Bits,
    /// The nonce does not do the work.
    Work,
    /// Already answered.
    Replayed,
}

impl PowRefusal {
    /// The wire name of the reason.
    pub fn as_str(self) -> &'static str {
        match self {
            PowRefusal::Malformed => "malformed",
            PowRefusal::Signature => "signature",
            PowRefusal::Endpoint => "endpoint",
            PowRefusal::Network => "network",
            PowRefusal::Expired => "expired",
            PowRefusal::Bits => "bits",
            PowRefusal::Work => "work",
            PowRefusal::Replayed => "replayed",
        }
    }
}

/// `^[A-Za-z0-9_-]{16,400}\.[A-Za-z0-9_-]{43}$`
fn is_challenge_shape(s: &str) -> bool {
    let b64url = |p: &str| p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
    match s.split_once('.') {
        Some((part, sig)) => {
            (16..=400).contains(&part.len()) && sig.len() == 43 && b64url(part) && b64url(sig)
        }
        None => false,
    }
}

/// `^[0-9]{1,20}$`
fn is_nonce_shape(s: &str) -> bool {
    (1..=20).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_digit())
}

/// The proof-of-work service: issues and checks challenges.
pub struct Pow {
    key: Key,
    ip_hash_key: Key,
    clock: SharedClock,
    control: Arc<LocalControl>,
    ttl_ms: i64,
}

impl std::fmt::Debug for Pow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pow").field("ttl_ms", &self.ttl_ms).finish_non_exhaustive()
    }
}

impl Pow {
    /// A service signing with `key` and hashing client networks with `ip_hash_key`; single use
    /// goes through `control`.
    pub fn new(key: &Key, ip_hash_key: &Key, control: Arc<LocalControl>, clock: SharedClock) -> Pow {
        Pow { key: key.clone(), ip_hash_key: ip_hash_key.clone(), clock, control, ttl_ms: POW_TTL_MS }
    }

    /// A service with the `pow` and `ip_hash` keys of the auth keys.
    pub fn from_keys(keys: &AuthKeys, control: Arc<LocalControl>, clock: SharedClock) -> Pow {
        Pow::new(&keys.pow, &keys.ip_hash, control, clock)
    }

    /// The keyed hash of the client's network (IPv4 address or IPv6 /64), 16 characters.
    fn net_hash(&self, ip: &str) -> String {
        let mut h = b64_url(&hmac_sha256(self.ip_hash_key.as_bytes(), ip_key(ip).as_bytes()));
        h.truncate(16);
        h
    }

    fn sign(&self, part: &str) -> String {
        b64_url(&hmac_sha256(self.key.as_bytes(), part.as_bytes()))
    }

    fn challenge_for(&self, net: &str, endpoint: &str, bits: u32, expires_at: i64, random: &str) -> String {
        let endpoint = serde_json::to_string(endpoint).expect("strings serialize");
        let payload =
            format!(r#"{{"v":1,"h":"{net}","e":{endpoint},"b":{bits},"x":{expires_at},"r":"{random}"}}"#);
        let part = b64_url(payload.as_bytes());
        let sig = self.sign(&part);
        format!("{part}.{sig}")
    }

    /// A new challenge bound to the client network and the endpoint.
    pub fn issue(&self, ip: &str, endpoint: &str, bits: u32) -> PowChallenge {
        let expires_at = self.clock.wall_ms() + self.ttl_ms;
        let random = b64_url(&random_bytes::<12>());
        let challenge = self.challenge_for(&self.net_hash(ip), endpoint, bits, expires_at, &random);
        PowChallenge { challenge, bits, expires_at }
    }

    /// Checks an answer. `bits` is the difficulty required now (a challenge issued with fewer
    /// bits is refused). A successful check consumes the challenge.
    pub fn verify(
        &self,
        ip: &str,
        endpoint: &str,
        bits: u32,
        challenge: &str,
        nonce: &str,
    ) -> Result<(), PowRefusal> {
        if !is_challenge_shape(challenge) || !is_nonce_shape(nonce) {
            return Err(PowRefusal::Malformed);
        }
        let (part, given_sig) = challenge.split_once('.').expect("checked shape");
        // The signature text itself, in constant time: decoding it would accept 4 spellings of
        // one signature (base64url ignores the low 2 bits of its 43rd character), and single use
        // is keyed by that text.
        let sig = self.sign(part);
        if !safe_eq(given_sig.as_bytes(), sig.as_bytes()) {
            return Err(PowRefusal::Signature);
        }
        let payload: serde_json::Value =
            serde_json::from_slice(&node_b64_decode(part)).map_err(|_| PowRefusal::Malformed)?;
        if payload.get("v").and_then(serde_json::Value::as_f64) != Some(1.0) {
            return Err(PowRefusal::Malformed);
        }
        if payload.get("e").and_then(serde_json::Value::as_str) != Some(endpoint) {
            return Err(PowRefusal::Endpoint);
        }
        if payload.get("h").and_then(serde_json::Value::as_str) != Some(self.net_hash(ip).as_str()) {
            return Err(PowRefusal::Network);
        }
        let t = self.clock.wall_ms();
        let expires_at = payload.get("x").and_then(serde_json::Value::as_i64).ok_or(PowRefusal::Expired)?;
        if expires_at <= t || expires_at > t + self.ttl_ms + EXPIRY_SLACK_MS {
            return Err(PowRefusal::Expired);
        }
        let issued_bits = payload.get("b").and_then(serde_json::Value::as_f64).ok_or(PowRefusal::Bits)?;
        if issued_bits < f64::from(bits) {
            return Err(PowRefusal::Bits);
        }
        // The signed payload holds an integer from issue(); clamp anything else.
        if !check_work(challenge, nonce, issued_bits.clamp(0.0, 256.0) as u32) {
            return Err(PowRefusal::Work);
        }
        if !self.control.consume_once(&format!("pow:{sig}"), expires_at - t + EXPIRY_SLACK_MS) {
            return Err(PowRefusal::Replayed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    /// 2026-09-28T12:00:00Z.
    const T0: i64 = 1_790_596_800_000;

    fn setup() -> (Arc<ManualClock>, Pow) {
        let clock = ManualClock::new(T0 as f64, T0);
        let control = Arc::new(LocalControl::new(clock.clone()));
        let pow = Pow::new(
            &Key::from_bytes(random_bytes::<32>()),
            &Key::from_bytes(random_bytes::<32>()),
            control,
            clock.clone(),
        );
        (clock, pow)
    }

    #[test]
    fn leading_zero_bits_counts_from_the_most_significant_bit() {
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x3f]), 18);
        assert_eq!(leading_zero_bits(&[0x00, 0x00]), 16);
        assert_eq!(leading_zero_bits(&[0x00, 0x0f]), 12);
    }

    #[test]
    fn the_answer_is_sha256_of_challenge_colon_decimal_nonce() {
        let nonce = solve_pow("abc", 10);
        assert!(nonce.bytes().all(|b| b.is_ascii_digit()));
        let h = Sha256::digest(format!("abc:{nonce}").as_bytes());
        assert!(leading_zero_bits(&h) >= 10);
        assert!(check_work("abc", &nonce, 10));
    }

    #[test]
    fn a_solved_challenge_is_accepted_once() {
        let (_, pow) = setup();
        let c = pow.issue("203.0.113.5", "register", 10);
        assert_eq!(c.bits, 10);
        assert_eq!(c.expires_at, T0 + POW_TTL_MS);
        assert!(is_challenge_shape(&c.challenge));
        let nonce = solve_pow(&c.challenge, 10);
        assert_eq!(pow.verify("203.0.113.5", "register", 10, &c.challenge, &nonce), Ok(()));
        assert_eq!(
            pow.verify("203.0.113.5", "register", 10, &c.challenge, &nonce),
            Err(PowRefusal::Replayed)
        );
        assert_eq!(
            c.to_json(),
            serde_json::json!({ "challenge": c.challenge, "bits": 10, "expiresAt": T0 + POW_TTL_MS })
        );
    }

    #[test]
    fn single_use_whatever_the_spelling_of_the_signature() {
        let (_, pow) = setup();
        let c = pow.issue("203.0.113.5", "register", 4);
        // The 43rd character of the signature carries 4 bits; its 2 low bits are ignored by a
        // decoder, so 3 other characters decode to the same 32 bytes.
        const B64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let last = *c.challenge.as_bytes().last().unwrap();
        let i = B64URL.iter().position(|&b| b == last).unwrap();
        let variants: Vec<String> = (0..4)
            .map(|k| B64URL[(i & !3) | k])
            .filter(|&ch| ch != last)
            .map(|ch| format!("{}{}", &c.challenge[..c.challenge.len() - 1], char::from(ch)))
            .collect();
        assert_eq!(variants.len(), 3);
        let sig_bytes = |s: &str| node_b64_decode(s.split_once('.').unwrap().1);
        for v in &variants {
            assert_eq!(sig_bytes(v), sig_bytes(&c.challenge));
            let nonce = solve_pow(v, 4);
            assert_eq!(pow.verify("203.0.113.5", "register", 4, v, &nonce), Err(PowRefusal::Signature));
        }
        let nonce = solve_pow(&c.challenge, 4);
        assert_eq!(pow.verify("203.0.113.5", "register", 4, &c.challenge, &nonce), Ok(()));
    }

    #[test]
    fn refusals_in_order() {
        let (clock, pow) = setup();
        let ip = "203.0.113.5";
        let c = pow.issue(ip, "register", 8);
        let nonce = solve_pow(&c.challenge, 8);
        assert_eq!(pow.verify("198.51.100.7", "register", 8, &c.challenge, &nonce), Err(PowRefusal::Network));
        assert_eq!(pow.verify(ip, "login", 8, &c.challenge, &nonce), Err(PowRefusal::Endpoint));
        assert_eq!(pow.verify(ip, "register", 12, &c.challenge, &nonce), Err(PowRefusal::Bits));
        let (part, sig) = c.challenge.split_once('.').unwrap();
        let mut forged: serde_json::Value = serde_json::from_slice(&node_b64_decode(part)).unwrap();
        forged["b"] = 1.into();
        let forged_part = b64_url(forged.to_string().as_bytes());
        let forged = format!("{forged_part}.{sig}");
        assert_eq!(pow.verify(ip, "register", 8, &forged, &nonce), Err(PowRefusal::Signature));
        assert_eq!(pow.verify(ip, "register", 8, &c.challenge, "12x"), Err(PowRefusal::Malformed));
        assert_eq!(pow.verify(ip, "register", 8, &c.challenge, ""), Err(PowRefusal::Malformed));
        assert_eq!(pow.verify(ip, "register", 8, &c.challenge, &"1".repeat(21)), Err(PowRefusal::Malformed));
        assert_eq!(pow.verify(ip, "register", 8, "short", &nonce), Err(PowRefusal::Malformed));
        let bad = (0u64..).map(|n| n.to_string()).find(|n| !check_work(&c.challenge, n, 8)).unwrap();
        assert_eq!(pow.verify(ip, "register", 8, &c.challenge, &bad), Err(PowRefusal::Work));
        clock.advance((POW_TTL_MS + 1) as f64);
        assert_eq!(pow.verify(ip, "register", 8, &c.challenge, &nonce), Err(PowRefusal::Expired));
        assert_eq!(PowRefusal::Replayed.as_str(), "replayed");
    }

    #[test]
    fn the_network_binding_is_per_ipv6_64_and_accepts_ipv4_mapped_addresses() {
        let (_, pow) = setup();
        let c = pow.issue("2001:db8:1:2::10", "login", 4);
        let nonce = solve_pow(&c.challenge, 4);
        assert_eq!(pow.verify("2001:db8:1:2:ffff::1", "login", 4, &c.challenge, &nonce), Ok(()));
        let d = pow.issue("192.0.2.1", "login", 4);
        let nonce = solve_pow(&d.challenge, 4);
        assert_eq!(pow.verify("::ffff:192.0.2.1", "login", 4, &d.challenge, &nonce), Ok(()));
    }

    #[test]
    fn challenges_match_the_former_server() {
        let keys = AuthKeys::derive(&[7u8; 48], None).unwrap();
        let clock = ManualClock::new(T0 as f64, T0);
        let pow = Pow::from_keys(&keys, Arc::new(LocalControl::new(clock.clone())), clock);
        assert_eq!(pow.net_hash("203.0.113.7"), "l6NPFUezOhyVkRUI");
        assert_eq!(
            pow.challenge_for("l6NPFUezOhyVkRUI", "register", 18, 1_790_882_991_200, "BQUFBQUFBQUFBQUF"),
            "eyJ2IjoxLCJoIjoibDZOUEZVZXpPaHlWa1JVSSIsImUiOiJyZWdpc3RlciIsImIiOjE4LCJ4IjoxNzkwODgyOTkxMjAwLCJyIjoiQlFVRkJRVUZCUVVGQlFVRiJ9\
             .Fws0O8Bgmr3stqjC9qw2Os8sMnbOGoVnSjIgMaOtEAs"
        );
    }
}
