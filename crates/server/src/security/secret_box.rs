//! AES-256-GCM box for small secrets at rest (the TOTP secrets). The associated data binds a
//! ciphertext to its owner: a secret copied to another account does not open.
//!
//! Format: `"v1." + base64url(iv 12 | ciphertext | tag 16)`, a fresh random IV per seal.

use std::fmt;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Nonce, Payload};
use zeroize::Zeroizing;

use crate::security::encoding::{b64_url, node_b64_decode, random_bytes};
use crate::security::keys::Key;

const PREFIX: &str = "v1.";
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// The associated data of an account's enabled TOTP secret.
pub fn mfa_aad(user_id: i64) -> String {
    format!("mfa:{user_id}")
}

/// The associated data of an account's pending (not yet confirmed) TOTP secret.
pub fn mfa_pending_aad(user_id: i64) -> String {
    format!("mfa-pending:{user_id}")
}

/// The key of a secret box is not 32 bytes long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretBoxKeyError;

impl fmt::Display for SecretBoxKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("secret box key must be 32 bytes")
    }
}

impl std::error::Error for SecretBoxKeyError {}

/// An AES-256-GCM box (see the module documentation).
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBox(<redacted>)")
    }
}

impl SecretBox {
    /// A box under a 32-byte key.
    pub fn new(key: &[u8]) -> Result<SecretBox, SecretBoxKeyError> {
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SecretBoxKeyError)?;
        Ok(SecretBox { cipher })
    }

    /// A box under a derived key (the `mfa` key of the auth keys).
    pub fn from_key(key: &Key) -> SecretBox {
        SecretBox::new(key.as_bytes()).expect("derived keys are 32 bytes")
    }

    /// Encrypts `plain` bound to `aad`, with a fresh random IV.
    pub fn seal(&self, plain: &[u8], aad: &str) -> String {
        self.seal_with_iv(plain, aad, random_bytes::<IV_LEN>())
    }

    fn seal_with_iv(&self, plain: &[u8], aad: &str, iv: [u8; IV_LEN]) -> String {
        let nonce = Nonce::<Aes256Gcm>::from(iv);
        let sealed = self
            .cipher
            .encrypt(&nonce, Payload { msg: plain, aad: aad.as_bytes() })
            .expect("small secrets are far below the AES-GCM length limit");
        let mut raw = Vec::with_capacity(IV_LEN + sealed.len());
        raw.extend_from_slice(&iv);
        raw.extend_from_slice(&sealed);
        format!("{PREFIX}{}", b64_url(&raw))
    }

    /// Decrypts a sealed value bound to `aad`. `None` when it is not a sealed value, was sealed
    /// under another key or another `aad`, or was altered.
    pub fn open(&self, sealed: &str, aad: &str) -> Option<Zeroizing<Vec<u8>>> {
        let raw = node_b64_decode(sealed.strip_prefix(PREFIX)?);
        if raw.len() < IV_LEN + TAG_LEN + 1 {
            return None;
        }
        let (iv, body) = raw.split_at(IV_LEN);
        let nonce = Nonce::<Aes256Gcm>::try_from(iv).ok()?;
        self.cipher.decrypt(&nonce, Payload { msg: body, aad: aad.as_bytes() }).ok().map(Zeroizing::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::keys::AuthKeys;
    use crate::security::totp::generate_totp_secret;

    fn random_box() -> SecretBox {
        SecretBox::new(&random_bytes::<32>()).unwrap()
    }

    #[test]
    fn round_trip_bound_to_its_associated_data_and_tamper_evident() {
        let b = random_box();
        let secret = generate_totp_secret();
        let sealed = b.seal(secret.as_ref(), "mfa:1");
        assert!(sealed.starts_with("v1."));
        assert!(sealed[3..].bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_eq!(b.open(&sealed, "mfa:1").unwrap().as_slice(), secret.as_ref());
        assert!(b.open(&sealed, "mfa:2").is_none());
        let mut raw = node_b64_decode(&sealed[3..]);
        raw[14] ^= 1;
        assert!(b.open(&format!("v1.{}", b64_url(&raw)), "mfa:1").is_none());
        assert!(random_box().open(&sealed, "mfa:1").is_none());
        assert!(b.open("garbage", "mfa:1").is_none());
        assert!(b.open("v1.", "mfa:1").is_none());
        assert!(b.open("v1.AAAA", "mfa:1").is_none());
        assert_ne!(b.seal(secret.as_ref(), "mfa:1"), sealed, "random IV");
        assert_eq!(SecretBox::new(&[0u8; 16]).unwrap_err().to_string(), "secret box key must be 32 bytes");
        assert_eq!(format!("{b:?}"), "SecretBox(<redacted>)");
    }

    #[test]
    fn sealed_values_match_the_former_server() {
        let keys = AuthKeys::derive(&[7u8; 48], None).unwrap();
        let b = SecretBox::from_key(&keys.mfa);
        let plain: Vec<u8> = (1..=20).collect();
        let iv: [u8; 12] = std::array::from_fn(|i| i as u8);
        let sealed = b.seal_with_iv(&plain, &mfa_aad(42), iv);
        assert_eq!(sealed, "v1.AAECAwQFBgcICQoLevkXTD2YnYjB2Bnw6GTHPZUTIQuP1sqe5vmD9jk71x26MkXH");
        assert_eq!(b.open(&sealed, "mfa:42").unwrap().as_slice(), plain.as_slice());
        assert!(b.open(&sealed, &mfa_pending_aad(42)).is_none());
        assert_eq!(mfa_pending_aad(42), "mfa-pending:42");
    }
}
