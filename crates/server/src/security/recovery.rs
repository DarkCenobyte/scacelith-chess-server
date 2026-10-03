//! Single-use recovery codes of the second factor: 10 symbols of Crockford's alphabet in lower
//! case (no `i`, `l`, `o`, `u`; 50 bits), shown as `xxxx-xxxx-xx` and stored as an HMAC-SHA256
//! under the derived recovery pepper, bound to the account.

use crate::security::encoding::{fill_random, js_is_space, utf16_len};
use crate::security::keys::{hmac_sha256, safe_eq};

/// Codes per generation.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// The alphabet of the codes: 32 symbols, 5 bits each.
pub const RECOVERY_CODE_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Symbols per code.
const CODE_LEN: usize = 10;

/// Longest typed code considered, in UTF-16 units (separators and spaces included).
const TYPED_MAX: usize = 32;

/// `n` new codes formatted `xxxx-xxxx-xx`.
pub fn generate_recovery_codes(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let mut b = [0u8; CODE_LEN];
            fill_random(&mut b);
            let mut s = String::with_capacity(CODE_LEN + 2);
            for (j, &byte) in b.iter().enumerate() {
                if j == 4 || j == 8 {
                    s.push('-');
                }
                s.push(char::from(RECOVERY_CODE_ALPHABET[usize::from(byte & 31)]));
            }
            s
        })
        .collect()
}

/// The canonical form of a typed code (lower case, whitespace and `-` removed, `o` read as `0`,
/// `i` and `l` as `1`), or `None` when it cannot be a code.
pub fn normalize_recovery_code(s: &str) -> Option<String> {
    if utf16_len(s) > TYPED_MAX {
        return None;
    }
    let mut out = String::with_capacity(CODE_LEN);
    for c in s.to_lowercase().chars() {
        if c == '-' || js_is_space(c) {
            continue;
        }
        let c = match c {
            'o' => '0',
            'i' | 'l' => '1',
            c => c,
        };
        if !c.is_ascii() || !RECOVERY_CODE_ALPHABET.contains(&(c as u8)) || out.len() == CODE_LEN {
            return None;
        }
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// The stored form of a code: hex HMAC-SHA256 under `pepper` of `"<user id>:<normalized>"`.
pub fn hash_recovery_code(pepper: &[u8], user_id: i64, normalized: &str) -> String {
    hex::encode(hmac_sha256(pepper, format!("{user_id}:{normalized}").as_bytes()))
}

/// The stored form of a typed code, `None` when it cannot be a code.
pub fn hash_typed_recovery_code(pepper: &[u8], user_id: i64, typed: &str) -> Option<String> {
    normalize_recovery_code(typed).map(|n| hash_recovery_code(pepper, user_id, &n))
}

/// True when the typed code is the one stored as `stored_hash` (hex, see
/// [`hash_recovery_code`]); compared in constant time. The store normally finds and consumes a
/// code by its hash in one statement; this is for a hash already at hand.
pub fn recovery_code_matches(pepper: &[u8], user_id: i64, typed: &str, stored_hash: &str) -> bool {
    hash_typed_recovery_code(pepper, user_id, typed)
        .is_some_and(|h| safe_eq(h.as_bytes(), stored_hash.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::security::encoding::random_bytes;
    use crate::security::keys::AuthKeys;

    fn is_code(c: &str) -> bool {
        let b = c.as_bytes();
        b.len() == 12
            && b[4] == b'-'
            && b[9] == b'-'
            && b.iter().enumerate().all(|(i, x)| i == 4 || i == 9 || RECOVERY_CODE_ALPHABET.contains(x))
    }

    #[test]
    fn format_alphabet_uniqueness_normalisation() {
        let codes = generate_recovery_codes(RECOVERY_CODE_COUNT);
        assert_eq!(codes.len(), 10);
        assert_eq!(codes.iter().collect::<HashSet<_>>().len(), 10);
        for c in &codes {
            assert!(is_code(c), "{c}");
            let bare = c.replace('-', "");
            assert_eq!(normalize_recovery_code(c).as_deref(), Some(bare.as_str()));
            assert_eq!(
                normalize_recovery_code(&c.to_uppercase().replace('-', " ")).as_deref(),
                Some(bare.as_str())
            );
        }
        assert_eq!(normalize_recovery_code("abcd-efgh-jk").as_deref(), Some("abcdefghjk"));
        assert_eq!(normalize_recovery_code("ABCD-EFGH-JK").as_deref(), Some("abcdefghjk"));
        assert_eq!(normalize_recovery_code("OOOO-IIII-LL").as_deref(), Some("0000111111"));
        assert_eq!(normalize_recovery_code("o0il-o0il-o0").as_deref(), Some("0011001100"));
        assert_eq!(normalize_recovery_code("abcd-efgh-j"), None);
        assert_eq!(normalize_recovery_code("abcd-efgh-ju"), None, "u is not in the alphabet");
        assert_eq!(normalize_recovery_code("abcd-efgh-jkk"), None);
        assert_eq!(
            normalize_recovery_code("\u{feff}abcd\u{3000}efgh\u{a0}jk").as_deref(),
            Some("abcdefghjk")
        );
        assert_eq!(normalize_recovery_code(&format!("abcdefghjk{}", " ".repeat(23))), None, "33 units");
        assert_eq!(normalize_recovery_code("abcdéfghjk"), None);
        // 10 symbols of a 32-symbol alphabet.
        let symbols: HashSet<char> =
            generate_recovery_codes(200).concat().chars().filter(|&c| c != '-').collect();
        assert!((28..=32).contains(&symbols.len()));
    }

    #[test]
    fn hashes_are_peppered_and_bound_to_the_user() {
        let pepper = random_bytes::<32>();
        let n = normalize_recovery_code("abcd-efgh-jk").unwrap();
        let h1 = hash_recovery_code(&pepper, 1, &n);
        assert!(h1.len() == 64 && h1.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(hash_recovery_code(&pepper, 1, &n), h1);
        assert_ne!(hash_recovery_code(&pepper, 2, &n), h1);
        assert_ne!(hash_recovery_code(&random_bytes::<32>(), 1, &n), h1);
        assert_eq!(hash_typed_recovery_code(&pepper, 1, "ABCD EFGH JK"), Some(h1.clone()));
        assert_eq!(hash_typed_recovery_code(&pepper, 1, "nope"), None);
        assert!(recovery_code_matches(&pepper, 1, "abcd efgh jk", &h1));
        assert!(!recovery_code_matches(&pepper, 2, "abcd-efgh-jk", &h1));
        assert!(!recovery_code_matches(&pepper, 1, "abcd-efgh-jm", &h1));
        assert!(!recovery_code_matches(&pepper, 1, "nope", &h1));
    }

    #[test]
    fn hashes_match_the_former_server() {
        let keys = AuthKeys::derive(&[7u8; 48], None).unwrap();
        assert_eq!(
            hash_recovery_code(keys.recovery.as_bytes(), 42, "abcdefghjk"),
            "0c05d09ea60a7b405651427f9a47ae156a46149444bd2a74e11f39ff83615b4b"
        );
    }
}
