//! Password hashing with Argon2id (RFC 9106), stored as PHC strings:
//!
//! ```text
//! $argon2id$v=19$m=65536,t=3,p=4$<salt, standard base64 without padding>$<tag, same>
//! ```
//!
//! (64 MiB, 3 passes, 4 lanes, 16-byte salt, 32-byte tag, no secret and no associated data: RFC
//! 9106's second recommended option, the format of the former Node.js server). A stored hash with
//! weaker parameters is reported by [`PasswordHasher::verify`] as `needs_rehash`, and the login
//! upgrades it with the password it just checked. There are no legacy (scrypt) hashes.
//!
//! Unknown accounts: [`PasswordHasher::verify_dummy`] does the same work on a fixed dummy hash, so
//! that a login for an unknown user costs as much as one with a wrong password.
//!
//! The functions here are blocking (one hash is about half a second of CPU and 64 MiB):
//! they run on blocking threads behind the hash limiter (see [`super::LimitedHasher`]), never on a
//! runtime thread.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use argon2::{Algorithm, Argon2, Params, Version};
use parking_lot::Mutex;
use zeroize::Zeroizing;

use super::policy::normalize_password;
use crate::security::encoding::{b64_std, node_b64_decode, random_bytes};
use crate::security::keys::safe_eq;

/// The result of a verification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    /// The password matches the stored hash.
    pub ok: bool,
    /// The password matched a hash with weaker parameters than today's: hash it again.
    pub needs_rehash: bool,
}

/// The key derivation itself failed: parameters it refuses (a stored hash with `m < 8 p`, a salt
/// shorter than 8 bytes...), memory exhausted, or a test double's error. The request ends in an
/// internal error (the former server's KDF threw); nothing was compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashFailure(pub String);

impl fmt::Display for HashFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "password hashing failed: {}", self.0)
    }
}

impl std::error::Error for HashFailure {}

/// A password hasher: blocking functions, called on blocking threads through the hash limiter.
/// Implemented by [`Argon2Hasher`] and by the test doubles of the auth tests.
pub trait PasswordHasher: Send + Sync + 'static {
    /// The algorithm of new hashes (`"argon2id"`).
    fn algorithm(&self) -> &'static str;

    /// A new self-describing hash of `password` (NFC-normalised first), with a random salt.
    fn hash(&self, password: &str) -> Result<String, HashFailure>;

    /// Checks `password` against `stored`. A string that is not a hash this hasher can read costs
    /// the dummy work and does not match.
    fn verify(&self, stored: &str, password: &str) -> Result<Verified, HashFailure>;

    /// The work of a verification for an account that does not exist or has no password. Never
    /// matches.
    fn verify_dummy(&self, password: &str) -> Result<(), HashFailure>;

    /// Prepares the dummy hash and measures the slowest verification a failed login can cost, in
    /// milliseconds (the floor of the padding of failed checks). The default measures nothing.
    fn warm_up(&self) -> Result<f64, HashFailure> {
        Ok(0.0)
    }
}

/// Argon2id parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Argon2Params {
    /// Memory in KiB (`m`).
    pub memory_kib: u32,
    /// Passes (`t`).
    pub passes: u32,
    /// Lanes (`p`).
    pub lanes: u32,
    /// Tag length in bytes.
    pub tag_len: usize,
    /// Salt length in bytes.
    pub salt_len: usize,
}

impl Argon2Params {
    /// The parameters of new hashes: RFC 9106 second recommended option (64 MiB, 3 passes, 4
    /// lanes), 16-byte salt, 32-byte tag.
    pub const DEFAULT: Argon2Params =
        Argon2Params { memory_kib: 65536, passes: 3, lanes: 4, tag_len: 32, salt_len: 16 };
}

impl Default for Argon2Params {
    fn default() -> Self {
        Argon2Params::DEFAULT
    }
}

/// Largest memory cost a stored hash may ask for (2 GiB, RFC 9106's first recommended option).
/// Above it the hash is refused like parameters the KDF rejects, instead of trying to allocate
/// what a corrupted row asks for.
pub const MAX_MEMORY_KIB: u32 = 2 * 1024 * 1024;

/// A parsed `$argon2id$v=19$...` string.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedHash {
    /// Memory in KiB.
    pub memory_kib: u32,
    /// Passes.
    pub passes: u32,
    /// Lanes.
    pub lanes: u32,
    /// The salt.
    pub salt: Vec<u8>,
    /// The tag (derived key).
    pub tag: Vec<u8>,
}

impl fmt::Debug for ParsedHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedHash")
            .field("memory_kib", &self.memory_kib)
            .field("passes", &self.passes)
            .field("lanes", &self.lanes)
            .field("salt_len", &self.salt.len())
            .field("tag_len", &self.tag.len())
            .finish()
    }
}

/// Reads `[0-9]{1,max}` at the start of `s`; returns the value and the rest.
fn digits(s: &str, max: usize) -> Option<(u32, &str)> {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 || n > max {
        return None;
    }
    Some((s[..n].parse().ok()?, &s[n..]))
}

/// A base64 field of the PHC string: `[A-Za-z0-9+/]{11,}` (no padding), decoded leniently.
fn b64_field(s: &str) -> Option<Vec<u8>> {
    let valid = s.len() >= 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    valid.then(|| node_b64_decode(s))
}

/// Parses a stored hash exactly as the former server did, with the pattern
/// `^\$argon2id\$v=19\$m=(\d{1,8}),t=(\d{1,3}),p=(\d{1,3})\$([A-Za-z0-9+/]{11,})\$([A-Za-z0-9+/]{11,})$`.
/// Anything else (a disabled password `!...`, an empty string, another algorithm) is `None`.
pub fn parse_hash(stored: &str) -> Option<ParsedHash> {
    let rest = stored.strip_prefix("$argon2id$v=19$m=")?;
    let (memory_kib, rest) = digits(rest, 8)?;
    let (passes, rest) = digits(rest.strip_prefix(",t=")?, 3)?;
    let (lanes, rest) = digits(rest.strip_prefix(",p=")?, 3)?;
    let (salt, tag) = rest.strip_prefix('$')?.split_once('$')?;
    Some(ParsedHash { memory_kib, passes, lanes, salt: b64_field(salt)?, tag: b64_field(tag)? })
}

/// Formats a PHC string.
fn format_hash(memory_kib: u32, passes: u32, lanes: u32, salt: &[u8], tag: &[u8]) -> String {
    format!("$argon2id$v=19$m={memory_kib},t={passes},p={lanes}${}${}", b64_std(salt), b64_std(tag))
}

/// Runs Argon2id. Errors are the KDF's refusals (and the memory cap).
fn argon2id(
    password: &[u8],
    salt: &[u8],
    memory_kib: u32,
    passes: u32,
    lanes: u32,
    out: &mut [u8],
) -> Result<(), HashFailure> {
    if memory_kib > MAX_MEMORY_KIB {
        return Err(HashFailure(format!("argon2: memory cost {memory_kib} KiB above the limit")));
    }
    let params = Params::new(memory_kib, passes, lanes, Some(out.len()))
        .map_err(|e| HashFailure(format!("argon2: {e}")))?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password, salt, out)
        .map_err(|e| HashFailure(format!("argon2: {e}")))
}

/// The UTF-8 bytes of the NFC form of a password, wiped when dropped.
fn password_bytes(password: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(normalize_password(password).into_bytes())
}

/// The Argon2id hasher of the server.
pub struct Argon2Hasher {
    params: Argon2Params,
    /// The dummy hash, computed once. A failed computation is not kept: the next call computes
    /// it again, instead of every unknown-account login failing until a restart.
    dummy: Mutex<Option<Arc<ParsedHash>>>,
}

impl fmt::Debug for Argon2Hasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Argon2Hasher").field("params", &self.params).finish()
    }
}

impl Default for Argon2Hasher {
    fn default() -> Self {
        Argon2Hasher::new(Argon2Params::DEFAULT)
    }
}

impl Argon2Hasher {
    /// A hasher whose new hashes use `params` (tests use cheaper ones).
    pub fn new(params: Argon2Params) -> Argon2Hasher {
        Argon2Hasher { params, dummy: Mutex::new(None) }
    }

    /// The parameters of new hashes.
    pub fn params(&self) -> Argon2Params {
        self.params
    }

    fn hash_bytes(&self, password: &[u8]) -> Result<String, HashFailure> {
        let p = self.params;
        let mut salt = vec![0u8; p.salt_len];
        crate::security::encoding::fill_random(&mut salt);
        let mut tag = Zeroizing::new(vec![0u8; p.tag_len]);
        argon2id(password, &salt, p.memory_kib, p.passes, p.lanes, &mut tag)?;
        Ok(format_hash(p.memory_kib, p.passes, p.lanes, &salt, &tag))
    }

    /// True when a matching hash should be computed again with today's parameters.
    fn outdated(&self, h: &ParsedHash) -> bool {
        let p = self.params;
        h.memory_kib < p.memory_kib || h.passes < p.passes || h.lanes != p.lanes || h.tag.len() != p.tag_len
    }

    fn dummy_hash(&self) -> Result<Arc<ParsedHash>, HashFailure> {
        if let Some(d) = self.dummy.lock().as_ref() {
            return Ok(d.clone());
        }
        // Computed outside the lock: two first callers may both compute one; the first stored
        // wins, which costs the same as a verification each.
        let pw = b64_std(&random_bytes::<18>());
        let parsed = parse_hash(&self.hash_bytes(pw.as_bytes())?).expect("the hasher's own format parses");
        Ok(self.dummy.lock().get_or_insert_with(|| Arc::new(parsed)).clone())
    }

    fn dummy_work(&self, password: &[u8]) -> Result<(), HashFailure> {
        let d = self.dummy_hash()?;
        let mut out = Zeroizing::new(vec![0u8; d.tag.len()]);
        argon2id(password, &d.salt, d.memory_kib, d.passes, d.lanes, &mut out)
    }
}

impl PasswordHasher for Argon2Hasher {
    fn algorithm(&self) -> &'static str {
        "argon2id"
    }

    fn hash(&self, password: &str) -> Result<String, HashFailure> {
        self.hash_bytes(&password_bytes(password))
    }

    fn verify(&self, stored: &str, password: &str) -> Result<Verified, HashFailure> {
        let pw = password_bytes(password);
        let Some(h) = parse_hash(stored) else {
            self.dummy_work(&pw)?;
            return Ok(Verified::default());
        };
        let mut key = Zeroizing::new(vec![0u8; h.tag.len()]);
        argon2id(&pw, &h.salt, h.memory_kib, h.passes, h.lanes, &mut key)?;
        let ok = safe_eq(&key, &h.tag);
        Ok(Verified { ok, needs_rehash: ok && self.outdated(&h) })
    }

    fn verify_dummy(&self, password: &str) -> Result<(), HashFailure> {
        self.dummy_work(&password_bytes(password))
    }

    /// Computes the dummy hash when it does not exist yet (that is the work of one verification),
    /// else times one dummy verification. There is only one kind of stored hash, so this is the
    /// slowest verification a failed login can cost.
    fn warm_up(&self) -> Result<f64, HashFailure> {
        let t0 = Instant::now();
        if self.dummy.lock().is_some() {
            let pw = b64_std(&random_bytes::<18>());
            self.dummy_work(pw.as_bytes())?;
        } else {
            self.dummy_hash()?;
        }
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters for the unit tests (1 MiB, 1 pass, 1 lane).
    pub(crate) const FAST: Argon2Params =
        Argon2Params { memory_kib: 1024, passes: 1, lanes: 1, tag_len: 32, salt_len: 16 };

    #[test]
    fn reference_vector_of_the_argon2_reference_implementation() {
        // phc-winner-argon2 test vector: argon2id v=19, t=2, m=65536, p=1, "password"/"somesalt".
        let mut out = [0u8; 32];
        argon2id(b"password", b"somesalt", 65536, 2, 1, &mut out).unwrap();
        assert_eq!(
            format_hash(65536, 2, 1, b"somesalt", &out),
            "$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc"
        );
        // And the verification of that string with the reference parameters.
        let h = Argon2Hasher::new(Argon2Params {
            memory_kib: 65536,
            passes: 2,
            lanes: 1,
            ..Argon2Params::DEFAULT
        });
        let stored = "$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc";
        assert_eq!(h.verify(stored, "password").unwrap(), Verified { ok: true, needs_rehash: false });
        assert!(!h.verify(stored, "Password").unwrap().ok);
    }

    #[test]
    fn the_default_format_round_trips() {
        let h = Argon2Hasher::default();
        let s = h.hash("a sufficiently long passphrase").unwrap();
        let p = parse_hash(&s).unwrap();
        assert!(s.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"), "{s}");
        assert_eq!((p.salt.len(), p.tag.len()), (16, 32));
        assert_eq!(s.split('$').nth(4).unwrap().len(), 22);
        assert_eq!(s.split('$').nth(5).unwrap().len(), 43);
        assert_eq!(
            h.verify(&s, "a sufficiently long passphrase").unwrap(),
            Verified { ok: true, needs_rehash: false }
        );
    }

    #[test]
    fn hash_and_verify_wrong_passwords_and_garbage() {
        let h = Argon2Hasher::new(FAST);
        let s = h.hash("correct horse battery").unwrap();
        assert!(s.starts_with("$argon2id$v=19$m=1024,t=1,p=1$"));
        assert_eq!(
            h.verify(&s, "correct horse battery").unwrap(),
            Verified { ok: true, needs_rehash: false }
        );
        assert!(!h.verify(&s, "correct horse batterY").unwrap().ok);
        assert_eq!(h.verify("not a hash", "x").unwrap(), Verified::default());
        assert_eq!(h.verify("", "x").unwrap(), Verified::default());
        assert_eq!(h.verify("!disabled", "x").unwrap(), Verified::default());
        assert_ne!(h.hash("same").unwrap(), h.hash("same").unwrap(), "random salt");
        h.verify_dummy("x").unwrap();
    }

    #[test]
    fn passwords_are_compared_after_nfc_normalisation() {
        let h = Argon2Hasher::new(FAST);
        let s = h.hash("caf\u{e9} au lait noir").unwrap();
        assert!(h.verify(&s, "cafe\u{301} au lait noir").unwrap().ok);
    }

    #[test]
    fn weaker_parameters_are_reported_after_a_successful_check_only() {
        let old = Argon2Hasher::new(FAST).hash("passphrase one two").unwrap();
        let stronger = Argon2Hasher::new(Argon2Params { memory_kib: 2048, ..FAST });
        assert_eq!(
            stronger.verify(&old, "passphrase one two").unwrap(),
            Verified { ok: true, needs_rehash: true }
        );
        assert_eq!(stronger.verify(&old, "wrong").unwrap(), Verified::default());
        let more_passes = Argon2Hasher::new(Argon2Params { passes: 2, ..FAST });
        assert!(more_passes.verify(&old, "passphrase one two").unwrap().needs_rehash);
        let other_lanes = Argon2Hasher::new(Argon2Params { lanes: 2, ..FAST });
        assert!(other_lanes.verify(&old, "passphrase one two").unwrap().needs_rehash);
        let longer_tag = Argon2Hasher::new(Argon2Params { tag_len: 64, ..FAST });
        assert!(longer_tag.verify(&old, "passphrase one two").unwrap().needs_rehash);
        // A stronger stored hash than today's parameters is not downgraded.
        let weaker = Argon2Hasher::new(Argon2Params { memory_kib: 512, ..FAST });
        assert!(!weaker.verify(&old, "passphrase one two").unwrap().needs_rehash);
    }

    #[test]
    fn parse_follows_the_former_pattern() {
        let salt = b64_std(&[1u8; 16]);
        let tag = b64_std(&[2u8; 32]);
        let ok = format!("$argon2id$v=19$m=65536,t=3,p=4${salt}${tag}");
        let p = parse_hash(&ok).unwrap();
        assert_eq!((p.memory_kib, p.passes, p.lanes), (65536, 3, 4));
        assert_eq!((p.salt, p.tag), (vec![1u8; 16], vec![2u8; 32]));
        assert!(
            parse_hash(&format!("$argon2id$v=19$m=065536,t=03,p=4${salt}${tag}")).is_some(),
            "leading zeros"
        );
        for bad in [
            format!("$argon2id$v=19$m=123456789,t=3,p=4${salt}${tag}"),
            format!("$argon2id$v=19$m=65536,t=1000,p=4${salt}${tag}"),
            format!("$argon2id$v=19$m=65536,t=3,p=4${salt}=${tag}"),
            format!("$argon2id$v=19$m=65536,t=3,p=4${salt}${tag}="),
            format!("$argon2id$v=19$m=65536,t=3,p=4$AAAAAAAAAA${tag}"),
            format!("$argon2id$v=19$m=65536,t=3,p=4${salt}${tag}$"),
            format!("$argon2id$v=19$m=65536,t=3,p=4${salt}"),
            format!("$argon2i$v=19$m=65536,t=3,p=4${salt}${tag}"),
            format!("$argon2id$v=16$m=65536,t=3,p=4${salt}${tag}"),
            format!("$argon2id$v=19$m=,t=3,p=4${salt}${tag}"),
            format!("$argon2id$v=19$m=65536,t=3,p=4${}${tag}", salt.replace('A', "-")),
            format!("scrypt$17$8$1${salt}${tag}"),
            String::new(),
        ] {
            assert!(parse_hash(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parameters_the_kdf_refuses_are_errors_not_mismatches() {
        let h = Argon2Hasher::new(FAST);
        let salt = b64_std(&[1u8; 16]);
        let tag = b64_std(&[2u8; 32]);
        // m < 8 p
        assert!(h.verify(&format!("$argon2id$v=19$m=16,t=1,p=4${salt}${tag}"), "x").is_err());
        // t = 0
        assert!(h.verify(&format!("$argon2id$v=19$m=1024,t=0,p=1${salt}${tag}"), "x").is_err());
        // memory above the cap
        assert!(h.verify(&format!("$argon2id$v=19$m=99999999,t=1,p=1${salt}${tag}"), "x").is_err());
        let e = h.verify(&format!("$argon2id$v=19$m=1024,t=1,p=0${salt}${tag}"), "x").unwrap_err();
        assert!(e.to_string().starts_with("password hashing failed: argon2"), "{e}");
    }

    #[test]
    fn warm_up_computes_the_dummy_then_times_a_dummy_check() {
        let h = Argon2Hasher::new(FAST);
        assert!(h.dummy.lock().is_none());
        let first = h.warm_up().unwrap();
        assert!(first > 0.0);
        let d = h.dummy.lock().clone().unwrap();
        let again = h.warm_up().unwrap();
        assert!(again > 0.0);
        assert!(Arc::ptr_eq(&d, h.dummy.lock().as_ref().unwrap()), "the dummy is computed once");
    }

    #[test]
    fn unknown_accounts_cost_the_same_work() {
        let h = Argon2Hasher::new(Argon2Params { memory_kib: 8192, ..FAST });
        h.warm_up().unwrap();
        let stored = h.hash("whatever it is").unwrap();
        let time = |f: &dyn Fn()| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64()
        };
        let mut real = Vec::new();
        let mut dummy = Vec::new();
        for _ in 0..5 {
            real.push(time(&|| assert!(!h.verify(&stored, "wrong password here").unwrap().ok)));
            dummy.push(time(&|| h.verify_dummy("wrong password here").unwrap()));
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[2]
        };
        let ratio = med(&mut real) / med(&mut dummy);
        assert!(ratio > 0.5 && ratio < 2.0, "ratio {ratio}");
    }
}
