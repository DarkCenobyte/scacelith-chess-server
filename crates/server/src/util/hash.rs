//! SHA-256, HMAC-SHA-256 and constant-time comparison.

use hmac::{KeyInit as _, Mac as _};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// HMAC-SHA-256 of `data` under `key` (any key length).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag);
    out
}

/// Compares two byte strings in time independent of their contents. Strings of different lengths
/// are unequal at once, so the length is not hidden: compare digests ([`ct_eq_hashed`]) when it
/// must be.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Compares the SHA-256 digests of two byte strings in constant time, which hides their lengths
/// as well (the metrics bearer token, as the Node server did with `timingSafeEqual`).
pub fn ct_eq_hashed(a: &[u8], b: &[u8]) -> bool {
    ct_eq(&sha256(a), &sha256(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(
            hex::encode(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // RFC 4231 test case 2.
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn constant_time_comparisons() {
        assert!(ct_eq(b"token", b"token"));
        assert!(!ct_eq(b"token", b"tokeN"));
        assert!(!ct_eq(b"token", b"token2"));
        assert!(ct_eq_hashed(b"token", b"token"));
        assert!(!ct_eq_hashed(b"token", b"token "));
    }
}
