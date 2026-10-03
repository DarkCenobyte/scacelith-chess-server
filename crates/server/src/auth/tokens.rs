//! Single-use tokens kept in the store's `tokens` table (only their SHA-256 is stored):
//!
//! | kind | lifetime | given in | data |
//! |---|---|---|---|
//! | `email_verify` | 24 h | e-mail link | `{email}` |
//! | `email_change` | 24 h | e-mail link (new address) | `{email: new address, from: address at the request}` |
//! | `password_reset` | 1 h | e-mail link | `{email}` |
//! | `mfa_login` | 5 min | login answer (`mfa_...`) | `{attempts, clientLabel, method, link?, pwh?}` |
//! | `sso_attempt` | 10 min | SSO start answer (`sso_...`) | `{challenge, stateHash, nonce, verifier, redirectUri}` |
//! | `sso_ticket` | 10 min | SSO finish answer (`sso_...`) | `{sub, email}` |
//! | `sso_link` | 10 min | SSO finish answer (`sso_...`) | `{userId, sub, email, tries}` |
//!
//! `sso_attempt` is consumed by the SSO finish, `sso_ticket` by the SSO complete. An `sso_link`
//! row (user id = the account) waits for the account's password: a try is reserved before each
//! hash (at most 5), and the row is consumed by the right password, the 5th failure or a failed
//! re-check. The `link` of an `mfa_login` step (`{sub, email, pwh}`) is the Google link a correct
//! code stores. The Google `state` itself is never stored, only its SHA-256.

use serde_json::{Map, Value};

use crate::store::Token;

/// An account's e-mail confirmation link.
pub const EMAIL_VERIFY: &str = "email_verify";
/// The confirmation link of a change of e-mail address.
pub const EMAIL_CHANGE: &str = "email_change";
/// A password reset link.
pub const PASSWORD_RESET: &str = "password_reset";
/// The second step of a login with two-step verification.
pub const MFA_LOGIN: &str = "mfa_login";
/// A Google sign-in started.
pub const SSO_ATTEMPT: &str = "sso_attempt";
/// A Google identity waiting for the username of its new account.
pub const SSO_TICKET: &str = "sso_ticket";
/// A Google identity waiting for the password of the account it will be linked to.
pub const SSO_LINK: &str = "sso_link";

const HOUR_MS: i64 = 3_600_000;
const MINUTE_MS: i64 = 60_000;

/// Lifetime of an `email_verify` token.
pub const EMAIL_VERIFY_TTL_MS: i64 = 24 * HOUR_MS;
/// Lifetime of an `email_change` token.
pub const EMAIL_CHANGE_TTL_MS: i64 = 24 * HOUR_MS;
/// Lifetime of a `password_reset` token.
pub const PASSWORD_RESET_TTL_MS: i64 = HOUR_MS;
/// Lifetime of an `mfa_login` token.
pub const MFA_LOGIN_TTL_MS: i64 = 5 * MINUTE_MS;
/// Lifetime of the `sso_attempt`, `sso_ticket` and `sso_link` tokens.
pub const SSO_TTL_MS: i64 = 10 * MINUTE_MS;

/// Prefix of a session token.
pub const SESSION_PREFIX: &str = "sct_";
/// Prefix of an `mfa_login` token.
pub const MFA_PREFIX: &str = "mfa_";
/// Prefix of the SSO tokens.
pub const SSO_PREFIX: &str = "sso_";

/// True for 43 base64url characters (`^[A-Za-z0-9_-]{43}$`): 32 random bytes.
pub fn is_token_body(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// True for an e-mail link token (`^[A-Za-z0-9_-]{43}$`).
pub fn is_link_token(s: &str) -> bool {
    is_token_body(s)
}

/// True for `prefix` followed by a token body (`^sct_[A-Za-z0-9_-]{43}$` for sessions...).
pub fn is_prefixed_token(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(is_token_body)
}

/// The data object of a token row: `{}` when it has none, or when it is not an object (a JSON
/// text holding an object is read too).
pub fn data_of(row: &Token) -> Map<String, Value> {
    match &row.data {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::String(s)) => match serde_json::from_str(s) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        },
        _ => Map::new(),
    }
}

/// True when a token row exists, is not consumed and not expired at `now`.
pub fn is_live(row: Option<&Token>, now: i64) -> bool {
    row.is_some_and(|r| r.consumed_at.is_none() && r.expires_at > now)
}

/// A string field of a data object (`None` when absent or not a string).
pub fn str_field<'a>(data: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(data: Option<Value>, expires_at: i64, consumed_at: Option<i64>) -> Token {
        Token {
            id: 1,
            kind: MFA_LOGIN.into(),
            user_id: Some(1),
            data,
            created_at: 0,
            expires_at,
            consumed_at,
        }
    }

    #[test]
    fn shapes() {
        let body = "A".repeat(43);
        assert!(is_link_token(&body));
        assert!(!is_link_token(&"A".repeat(42)) && !is_link_token(&"A".repeat(44)));
        assert!(!is_link_token(&format!("{}=", "A".repeat(42))));
        assert!(is_prefixed_token(&format!("sct_{body}"), SESSION_PREFIX));
        assert!(!is_prefixed_token(&format!("mfa_{body}"), SESSION_PREFIX));
        assert!(!is_prefixed_token(&body, SESSION_PREFIX));
        assert!(is_prefixed_token(&format!("sso_{}", "-_9z".repeat(10) + "abc"), SSO_PREFIX));
    }

    #[test]
    fn data_and_liveness() {
        let obj = serde_json::json!({ "attempts": 2, "method": "password" });
        assert_eq!(data_of(&row(Some(obj.clone()), 10, None)), obj.as_object().unwrap().clone());
        assert_eq!(
            data_of(&row(Some(Value::String(obj.to_string())), 10, None)),
            obj.as_object().unwrap().clone()
        );
        assert!(data_of(&row(None, 10, None)).is_empty());
        assert!(data_of(&row(Some(Value::String("{".into())), 10, None)).is_empty());
        assert!(data_of(&row(Some(serde_json::json!([1])), 10, None)).is_empty());
        assert!(is_live(Some(&row(None, 10, None)), 9));
        assert!(!is_live(Some(&row(None, 10, None)), 10));
        assert!(!is_live(Some(&row(None, 10, Some(5))), 9));
        assert!(!is_live(None, 0));
    }
}
