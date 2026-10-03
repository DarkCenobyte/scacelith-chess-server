//! Security primitives: password policy and hashing (Argon2id, legacy scrypt verification, hash
//! limiter, check floor), TOTP and recovery codes, secret box (AES-256-GCM), key derivation,
//! proof of work, rate-limit primitives (sliding windows, token buckets, single-use keys) and the
//! address abuse tracker. Owner: security. See docs/RUST-PORT.md.
