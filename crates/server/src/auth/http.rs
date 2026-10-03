//! The auth service seen from the HTTP API: the Bearer hook ([`Authenticator`] on [`Auth`]), the
//! answer of an [`AuthError`], and the session of a request.

use serde_json::Value;

use super::sessions::hash_bytes;
use super::{Auth, AuthError, SessionInfo};
use crate::http::{ApiError, AuthInfo, Authenticator};

impl Authenticator for Auth {
    /// [`Auth::validate_token`]: the session, `None` for a token that opens none (401
    /// `invalid_token`); a store that cannot answer is 503 `server_busy`.
    async fn validate_token(&self, token: &str) -> Result<Option<AuthInfo>, ApiError> {
        let session = Auth::validate_token(self, token).await?;
        Ok(session.map(AuthInfo::from))
    }
}

impl From<SessionInfo> for AuthInfo {
    fn from(s: SessionInfo) -> AuthInfo {
        AuthInfo {
            user_id: s.user_id,
            username: s.username,
            session_id: s.session_id,
            email_verified: s.email_verified,
            token_hash: Some(hex::encode(s.token_hash)),
        }
    }
}

impl SessionInfo {
    /// The session of a request authenticated by [`Auth`] (`None` when the validator did not give
    /// the token's digest).
    pub fn from_auth_info(info: &AuthInfo) -> Option<SessionInfo> {
        Some(SessionInfo {
            user_id: info.user_id,
            username: info.username.clone(),
            session_id: info.session_id,
            email_verified: info.email_verified,
            token_hash: hash_bytes(info.token_hash.as_deref()?)?,
        })
    }
}

impl From<AuthError> for ApiError {
    /// An answer keeps its status, code, message, extra fields and rate refund; a store that
    /// stayed locked is 503 `server_busy` (`retryAfter` 1); any other failure is 500
    /// `internal_error` (its description logged by the HTTP layer, never sent).
    fn from(e: AuthError) -> ApiError {
        if e.is_exposed() {
            let mut out = ApiError::new(e.status(), e.code().to_owned(), e.message().into_owned());
            if let Some(extra) = e.extra() {
                out.extra = extra.clone();
            }
            out.refund_rate = e.refund_rate();
            return out;
        }
        if e.is_store_busy() {
            return ApiError::from(AuthError::server_busy(Some(1)));
        }
        ApiError::internal(e.message())
    }
}

/// The `retryAfter` field of an error as the `Retry-After` header writes it.
pub(crate) fn retry_after_header(e: &ApiError) -> Option<String> {
    match e.extra.get("retryAfter")? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ErrorKind, StoreError};

    #[test]
    fn errors_become_api_errors() {
        let e = ApiError::from(AuthError::too_many_attempts(4500));
        assert_eq!((e.status, e.code.as_ref()), (429, "too_many_attempts"));
        assert_eq!(e.extra.get("retryAfter"), Some(&Value::from(5)));
        assert_eq!(retry_after_header(&e).as_deref(), Some("5"));
        let e = ApiError::from(AuthError::hash_rate_limited());
        assert!(e.refund_rate && e.is_exposed());
        let e = ApiError::from(AuthError::from(StoreError::new(ErrorKind::Busy, "locked")));
        assert_eq!((e.status, e.code.as_ref()), (503, "server_busy"));
        assert_eq!(e.extra.get("retryAfter"), Some(&Value::from(1)));
        let e = ApiError::from(AuthError::internal("disk on fire"));
        assert_eq!((e.status, e.is_exposed()), (500, false));
    }

    #[test]
    fn sessions_and_auth_infos() {
        let s = SessionInfo {
            user_id: 7,
            username: "alice".into(),
            session_id: 3,
            email_verified: true,
            token_hash: [0xab; 32],
        };
        let info = AuthInfo::from(s.clone());
        assert_eq!(info.token_hash.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(SessionInfo::from_auth_info(&info), Some(s));
        assert_eq!(SessionInfo::from_auth_info(&AuthInfo { token_hash: None, ..info }), None);
    }
}
