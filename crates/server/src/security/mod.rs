//! Security primitives of the auth service:
//!
//! * [`keys`]: every key derived from `SERVER_SECRET` (HKDF-SHA256), token helpers and
//!   constant-time comparison;
//! * [`password`]: password policy, Argon2id hashing on a bounded blocking pool (hash limiter,
//!   per-request budget, padding floor of failed checks);
//! * [`totp`], [`secret_box`] and [`recovery`]: the second factor (RFC 6238 codes, AES-256-GCM
//!   encryption of the secrets at rest, single-use recovery codes);
//! * [`pow`]: proof-of-work challenges;
//! * [`ratelimit`]: address keys, token buckets, failure counters, the whole-server control
//!   counters and single-use keys, and the global login failure detector;
//! * [`encoding`]: the encodings of the former server (lenient base64, JavaScript whitespace,
//!   `encodeURIComponent`).
//!
//! Owner: security. See docs/RUST-PORT.md.

pub mod encoding;
pub mod keys;
pub mod password;
pub mod pow;
pub mod ratelimit;
pub mod recovery;
pub mod secret_box;
pub mod totp;
