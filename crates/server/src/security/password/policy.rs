//! Password policy (DESIGN.md section 8): length in characters and bytes, not containing the user
//! name or the e-mail's local part, not in the embedded list of common passwords.

use std::collections::HashSet;
use std::fmt;
use std::sync::LazyLock;

use unicode_normalization::UnicodeNormalization as _;

use crate::security::encoding::{js_trim, utf16_len};

/// The longest accepted password, in UTF-8 bytes after NFC normalisation (also published by
/// `GET /info` and the reset page).
pub const PASSWORD_MAX_BYTES: usize = 256;

static COMMON: LazyLock<HashSet<String>> = LazyLock::new(|| {
    include_str!("common-passwords.txt")
        .lines()
        .map(|l| js_trim(l).to_lowercase())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
});

/// The embedded list of common passwords, in lower case.
pub fn common_passwords() -> &'static HashSet<String> {
    &COMMON
}

/// True when the password, in lower case, is in the embedded list.
pub fn is_common_password(password: &str) -> bool {
    COMMON.contains(&password.to_lowercase())
}

/// Unicode NFC normalisation, applied to every password before it is hashed or checked, so that
/// the same password typed on different systems gives the same bytes.
pub fn normalize_password(password: &str) -> String {
    password.nfc().collect()
}

/// Why a password is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyReason {
    /// Fewer characters (code points) than the minimum.
    TooShort,
    /// More than [`PASSWORD_MAX_BYTES`] UTF-8 bytes.
    TooLong,
    /// Contains the user name (3 characters or more).
    ContainsUsername,
    /// Contains the local part of the e-mail address (3 characters or more).
    ContainsEmail,
    /// In the list of common passwords.
    TooCommon,
}

impl PolicyReason {
    /// The `reason` field of the 400 `weak_password` answer.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyReason::TooShort => "too_short",
            PolicyReason::TooLong => "too_long",
            PolicyReason::ContainsUsername => "contains_username",
            PolicyReason::ContainsEmail => "contains_email",
            PolicyReason::TooCommon => "too_common",
        }
    }
}

/// A refused password: the reason and the message the API answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyViolation {
    /// Machine-readable reason.
    pub reason: PolicyReason,
    /// The English message of the answer.
    pub message: String,
}

impl fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PolicyViolation {}

/// Checks the password policy on the NFC form of `password`, in this order: at least
/// `min_length` characters, at most 256 bytes, not containing `username` nor the local part of
/// `email` (each only when it has 3 characters or more; case-insensitive), not a common password.
pub fn check_password_policy(
    password: &str,
    min_length: usize,
    username: &str,
    email: &str,
) -> Result<(), PolicyViolation> {
    let pw = normalize_password(password);
    let refuse = |reason, message: String| Err(PolicyViolation { reason, message });
    if pw.chars().count() < min_length {
        return refuse(
            PolicyReason::TooShort,
            format!("The password must have at least {min_length} characters."),
        );
    }
    if pw.len() > PASSWORD_MAX_BYTES {
        return refuse(
            PolicyReason::TooLong,
            format!("The password must not exceed {PASSWORD_MAX_BYTES} bytes."),
        );
    }
    let lower = pw.to_lowercase();
    let user = username.to_lowercase();
    if utf16_len(&user) >= 3 && lower.contains(&user) {
        return refuse(PolicyReason::ContainsUsername, "The password must not contain the username.".into());
    }
    let email = email.to_lowercase();
    let local = email.split('@').next().unwrap_or("");
    if utf16_len(local) >= 3 && lower.contains(local) {
        return refuse(
            PolicyReason::ContainsEmail,
            "The password must not contain the e-mail address.".into(),
        );
    }
    if COMMON.contains(&lower) {
        return refuse(PolicyReason::TooCommon, "This password is too common; choose another one.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(pw: &str) -> Option<&'static str> {
        check_password_policy(pw, 10, "Magnus_C", "grandpatzer@example.com").err().map(|v| v.reason.as_str())
    }

    #[test]
    fn policy_length_bytes_username_email_common() {
        assert_eq!(reason("short"), Some("too_short"));
        assert_eq!(reason(&"é".repeat(9)), Some("too_short"));
        assert_eq!(reason(&"x".repeat(PASSWORD_MAX_BYTES + 1)), Some("too_long"));
        assert_eq!(reason(&"é".repeat(129)), Some("too_long"), "258 bytes");
        assert_eq!(reason("my magnus_c secret!"), Some("contains_username"));
        assert_eq!(reason("GRANDPATZER forever"), Some("contains_email"));
        assert_eq!(reason("qwertyuiop"), Some("too_common"));
        assert_eq!(reason("PASSWORD1234"), Some("too_common"));
        assert_eq!(reason("1q2w3e4r5t"), Some("too_common"));
        assert_eq!(reason("ivory rook takes e5"), None);
        assert_eq!(check_password_policy(&"x".repeat(PASSWORD_MAX_BYTES), 10, "", ""), Ok(()));
    }

    #[test]
    fn policy_messages() {
        let v = check_password_policy("short", 10, "", "").unwrap_err();
        assert_eq!(v.message, "The password must have at least 10 characters.");
        assert_eq!(
            check_password_policy(&"x".repeat(300), 10, "", "").unwrap_err().message,
            "The password must not exceed 256 bytes."
        );
        assert_eq!(
            check_password_policy("my magnus_c secret!", 10, "Magnus_C", "").unwrap_err().to_string(),
            "The password must not contain the username."
        );
        assert_eq!(
            check_password_policy("grandpatzer forever", 10, "", "grandpatzer@x.org").unwrap_err().message,
            "The password must not contain the e-mail address."
        );
        assert_eq!(
            check_password_policy("password1234", 10, "", "").unwrap_err().message,
            "This password is too common; choose another one."
        );
    }

    #[test]
    fn short_names_and_nfc() {
        // A user name or local part of fewer than 3 characters is not looked for.
        assert_eq!(check_password_policy("ab ivory rook takes", 10, "ab", "ab@example.org"), Ok(()));
        // The length counts characters after NFC: "e" + combining acute is one character.
        assert_eq!(normalize_password("cafe\u{301}"), "café");
        assert_eq!(check_password_policy(&"e\u{301}".repeat(10), 10, "", "").map_err(|v| v.reason), Ok(()));
        assert_eq!(
            check_password_policy(&"e\u{301}".repeat(9), 10, "", "").map_err(|v| v.reason),
            Err(PolicyReason::TooShort)
        );
    }

    #[test]
    fn the_embedded_list_is_large_and_lower_case() {
        let set = common_passwords();
        assert!(set.len() >= 1000, "size {}", set.len());
        assert!(set.iter().all(|p| *p == p.to_lowercase()));
        assert!(
            is_common_password("123456") && is_common_password("Password") && is_common_password("iloveyou")
        );
        assert!(!is_common_password("a very unusual passphrase 42"));
    }
}
