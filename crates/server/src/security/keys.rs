//! Keys derived from the server's master secret (HKDF-SHA256, DESIGN.md section 8), token
//! helpers and constant-time comparison.
//!
//! Every purpose gets its own key, so that a key that signs proof-of-work challenges can never be
//! confused with the pepper of the recovery codes or the TOTP encryption key. Changing
//! `SERVER_SECRET` invalidates outstanding challenges and recovery codes and makes the TOTP
//! secrets unreadable, unless `MFA_ENCRYPTION_KEY` is set (it then encrypts them on its own).
//!
//! The derivation is the one of the former Node.js server (`hkdfSync('sha256', secret, <empty
//! salt>, label, 32)`), so derived values follow the same scheme.

use std::fmt;

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use zeroize::Zeroize as _;

use crate::config::Config;

use super::encoding::{b64_url, fill_random};

/// Length of every derived key, in bytes.
pub const KEY_LEN: usize = 32;

/// HKDF label of the proof-of-work signing key.
pub const LABEL_POW: &str = "scacelith/pow";
/// HKDF label of the recovery-code pepper.
pub const LABEL_RECOVERY: &str = "scacelith/recovery-codes";
/// HKDF label of the TOTP secret encryption key.
pub const LABEL_MFA: &str = "scacelith/mfa";
/// HKDF label of the key that hashes client networks (proof-of-work binding).
pub const LABEL_IP_HASH: &str = "scacelith/ip-hash";
/// HKDF label of the key that hashes e-mail addresses for the mail throttles.
pub const LABEL_MAIL_THROTTLE: &str = "scacelith/mail-throttle";

/// A 32-byte key. Wiped from memory when dropped; never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; KEY_LEN]);

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Key {
    /// Wraps raw key bytes.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Key {
        Key(bytes)
    }

    /// The key bytes.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// HKDF-SHA256 of `secret` for one purpose: empty salt, `info` = the UTF-8 label, 32 bytes.
pub fn derive_key(secret: &[u8], info: &str) -> Key {
    let mut okm = [0u8; KEY_LEN];
    Hkdf::<Sha256>::new(None, secret)
        .expand(info.as_bytes(), &mut okm)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    Key(okm)
}

/// `SERVER_SECRET` is too short to derive keys from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError;

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SERVER_SECRET must hold at least 32 bytes")
    }
}

impl std::error::Error for KeyError {}

/// The keys of the auth module, derived once at start-up.
#[derive(Clone, Debug)]
pub struct AuthKeys {
    /// Signs proof-of-work challenges.
    pub pow: Key,
    /// Pepper of the stored recovery-code hashes.
    pub recovery: Key,
    /// Encrypts the TOTP secrets at rest (secret box).
    pub mfa: Key,
    /// Hashes the client network bound into a proof-of-work challenge.
    pub ip_hash: Key,
    /// Hashes the e-mail addresses of the mail throttle keys.
    pub mail_throttle: Key,
}

impl AuthKeys {
    /// Derives the keys from the master secret. The TOTP key is `mfa_encryption_key` itself when
    /// it holds exactly 32 bytes, HKDF of it when it holds another non-zero length, and HKDF of
    /// the master secret when it is absent or empty.
    pub fn derive(server_secret: &[u8], mfa_encryption_key: Option<&[u8]>) -> Result<AuthKeys, KeyError> {
        if server_secret.len() < KEY_LEN {
            return Err(KeyError);
        }
        let mfa = match mfa_encryption_key {
            Some(mek) if mek.len() == KEY_LEN => {
                let mut k = [0u8; KEY_LEN];
                k.copy_from_slice(mek);
                Key(k)
            }
            Some(mek) if !mek.is_empty() => derive_key(mek, LABEL_MFA),
            _ => derive_key(server_secret, LABEL_MFA),
        };
        Ok(AuthKeys {
            pow: derive_key(server_secret, LABEL_POW),
            recovery: derive_key(server_secret, LABEL_RECOVERY),
            mfa,
            ip_hash: derive_key(server_secret, LABEL_IP_HASH),
            mail_throttle: derive_key(server_secret, LABEL_MAIL_THROTTLE),
        })
    }

    /// [`AuthKeys::derive`] from `SERVER_SECRET` and `MFA_ENCRYPTION_KEY`.
    pub fn from_config(config: &Config) -> Result<AuthKeys, KeyError> {
        AuthKeys::derive(config.server_secret.bytes(), config.mfa_encryption_key.as_ref().map(|k| k.bytes()))
    }
}

/// HMAC-SHA256 of `msg` under `key`.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// SHA-256 of a string as lower-case hex: the stored form of every token (prefix included).
pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Constant-time comparison of two byte strings. Different lengths compare unequal; the length is
/// then the only thing the time reveals.
pub fn safe_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Spend the time of a comparison anyway.
        std::hint::black_box(a.ct_eq(a));
        return false;
    }
    a.ct_eq(b).into()
}

/// A new random token: `prefix` + base64url of 32 random bytes (43 characters).
pub fn random_token(prefix: &str) -> String {
    random_token_of(prefix, 32)
}

/// A random token of `bytes` random bytes: `prefix` + their base64url.
pub fn random_token_of(prefix: &str, bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    fill_random(&mut raw);
    let mut out = String::with_capacity(prefix.len() + bytes.div_ceil(3) * 4);
    out.push_str(prefix);
    out.push_str(&b64_url(&raw));
    raw.zeroize();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret48() -> Vec<u8> {
        vec![7u8; 48]
    }

    #[test]
    fn derived_keys_match_the_node_server() {
        // Porting notes section 15: SERVER_SECRET = 48 x 0x07.
        let k = AuthKeys::derive(&secret48(), None).unwrap();
        assert_eq!(
            hex::encode(k.pow.as_bytes()),
            "2267f0ed984acaf3890ca5871c9dcce4f8c43fe9f9abf24479fe26c31fb8f531"
        );
        assert_eq!(
            hex::encode(k.recovery.as_bytes()),
            "86a73ba443ed0b68d5f1353c24f0a4e0f8a6d1f3be64e1d4790523a652853c84"
        );
        assert_eq!(
            hex::encode(k.mfa.as_bytes()),
            "f996ca21bc332d0fe395ad364e1f4d6a0d84b28882b7a0e01005bbac0a6a05be"
        );
        assert_eq!(
            hex::encode(k.ip_hash.as_bytes()),
            "0993f922877f6c136c14f130308a54be7f3d2ef7ad0a81ca70165f3324b506bd"
        );
        assert_eq!(
            hex::encode(k.mail_throttle.as_bytes()),
            "fdad1873f08de6f24e5d012c07cf7c2c71c581ccbd4b336105b6f59e1294fae2"
        );
    }

    #[test]
    fn the_mfa_key_follows_mfa_encryption_key() {
        let raw = AuthKeys::derive(&secret48(), Some(&[9u8; 32])).unwrap();
        assert_eq!(raw.mfa.as_bytes(), &[9u8; 32], "32 bytes are used as they are");
        let derived = AuthKeys::derive(&secret48(), Some(&[9u8; 40])).unwrap();
        assert_eq!(
            hex::encode(derived.mfa.as_bytes()),
            "871e884bc7f21aefe416d1abead97e4c1ed3d82184153577dae19cd3d03b4578"
        );
        let empty = AuthKeys::derive(&secret48(), Some(&[])).unwrap();
        assert_eq!(empty.mfa, AuthKeys::derive(&secret48(), None).unwrap().mfa);
        assert_eq!(raw.pow, derived.pow, "the other keys still come from SERVER_SECRET");
    }

    #[test]
    fn a_short_master_secret_is_refused() {
        assert_eq!(AuthKeys::derive(&[1u8; 31], None).unwrap_err(), KeyError);
        assert_eq!(KeyError.to_string(), "SERVER_SECRET must hold at least 32 bytes");
        assert!(AuthKeys::derive(&[1u8; 32], None).is_ok());
        let cfg = Config::for_tests();
        assert_eq!(
            AuthKeys::from_config(&cfg).unwrap().pow,
            AuthKeys::derive(&secret48(), None).unwrap().pow
        );
    }

    #[test]
    fn keys_are_never_printed() {
        let k = AuthKeys::derive(&secret48(), None).unwrap();
        assert!(!format!("{k:?}").contains("2267f0ed"));
        assert!(format!("{:?}", k.pow).contains("redacted"));
    }

    #[test]
    fn token_hash_and_comparison() {
        assert_eq!(
            sha256_hex(&format!("sct_{}", "A".repeat(43))),
            "28850eae27ffb41d02b578a483d8256fa7fa1d07354b0db47b80a232fda66d4a"
        );
        assert!(safe_eq(b"abc", b"abc"));
        assert!(!safe_eq(b"abc", b"abd"));
        assert!(!safe_eq(b"abc", b"abcd"));
        assert!(safe_eq(b"", b""));
    }

    #[test]
    fn random_tokens() {
        let t = random_token("sct_");
        assert_eq!(t.len(), 4 + 43);
        assert!(t.starts_with("sct_"));
        assert!(t[4..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(random_token(""), random_token(""));
        assert_eq!(random_token_of("x", 12).len(), 17);
    }

    #[test]
    fn hmac_vector() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
