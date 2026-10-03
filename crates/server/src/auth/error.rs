//! Errors of the auth service.
//!
//! An [`AuthError`] is either an answer for the client (an HTTP status, a snake_case code, an
//! English message and extra JSON fields such as `retryAfter`, answered as
//! `{"error": code, "message": message, ...extra}`), or an internal failure (the store, the key
//! derivation) that the HTTP layer answers with its generic 500 and logs.

use std::borrow::Cow;
use std::fmt;

use serde_json::{Map, Value};

use crate::security::password::{BusyAnswer, HashError, HashFailure};
use crate::security::ratelimit::random_retry_after;
use crate::store::{ErrorKind, StoreError};

/// The range of the random `Retry-After` (seconds) of a 503 `server_busy` or a 429 `rate_limited`
/// answer of a refused password hash.
pub const BUSY_RETRY_AFTER_SEC: (u64, u64) = (5, 15);

/// An error of the auth service (module documentation).
pub struct AuthError(Box<Repr>);

enum Repr {
    Answer(Answer),
    Store(StoreError),
    Hash(HashFailure),
    Internal(String),
}

struct Answer {
    status: u16,
    code: &'static str,
    message: Cow<'static, str>,
    extra: Map<String, Value>,
    refund_rate: bool,
}

impl AuthError {
    /// An answer for the client.
    pub fn new(status: u16, code: &'static str, message: impl Into<Cow<'static, str>>) -> AuthError {
        AuthError(Box::new(Repr::Answer(Answer {
            status,
            code,
            message: message.into(),
            extra: Map::new(),
            refund_rate: false,
        })))
    }

    /// Adds an extra field to the answer (after the ones already there). No effect on an internal
    /// error.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> AuthError {
        if let Repr::Answer(a) = &mut *self.0 {
            a.extra.insert(key.to_owned(), value.into());
        }
        self
    }

    /// An internal failure (500), described for the logs only.
    pub fn internal(message: impl Into<String>) -> AuthError {
        AuthError(Box::new(Repr::Internal(message.into())))
    }

    /// 429 `too_many_attempts` with the delay rounded up to seconds (at least 1) as `retryAfter`.
    pub fn too_many_attempts(retry_after_ms: i64) -> AuthError {
        let secs = (retry_after_ms.max(0) as u64).div_ceil(1000).max(1);
        AuthError::new(429, "too_many_attempts", "Too many attempts; wait before trying again.")
            .with("retryAfter", secs)
    }

    /// 503 `server_busy`: no password hash can run now (queue full or wait expired), or the store
    /// is busy. `retry_after_sec` defaults to a random 5 to 15 s, so that the clients refused
    /// during one burst do not all come back together.
    pub fn server_busy(retry_after_sec: Option<u64>) -> AuthError {
        let secs = retry_after_sec
            .unwrap_or_else(|| random_retry_after(BUSY_RETRY_AFTER_SEC.0, BUSY_RETRY_AFTER_SEC.1));
        AuthError::new(503, "server_busy", "The server is busy; try again in a few seconds.")
            .with("retryAfter", secs)
    }

    /// 429 `rate_limited`, the answer of the HTTP rate limits: this client source already has as
    /// many password hashes waiting as it may. The HTTP layer gives the request's rate tokens back
    /// ([`AuthError::refund_rate`]): nothing was hashed.
    pub fn hash_rate_limited() -> AuthError {
        let secs = random_retry_after(BUSY_RETRY_AFTER_SEC.0, BUSY_RETRY_AFTER_SEC.1);
        let mut e = AuthError::new(429, "rate_limited", "Too many requests; try again later.")
            .with("retryAfter", secs);
        if let Repr::Answer(a) = &mut *e.0 {
            a.refund_rate = true;
        }
        e
    }

    /// The single answer of every failed password login (unknown account, wrong password, no
    /// password).
    pub fn invalid_credentials() -> AuthError {
        AuthError::new(401, "invalid_credentials", "Wrong user name, e-mail or password.")
    }

    /// True for an answer meant for the client; false for an internal failure.
    pub fn is_exposed(&self) -> bool {
        matches!(&*self.0, Repr::Answer(_))
    }

    /// The HTTP status (500 for an internal failure).
    pub fn status(&self) -> u16 {
        match &*self.0 {
            Repr::Answer(a) => a.status,
            _ => 500,
        }
    }

    /// The snake_case code (`internal` for an internal failure).
    pub fn code(&self) -> &str {
        match &*self.0 {
            Repr::Answer(a) => a.code,
            _ => "internal",
        }
    }

    /// The message of the answer (for an internal failure: its description, for the logs only).
    pub fn message(&self) -> Cow<'_, str> {
        match &*self.0 {
            Repr::Answer(a) => Cow::Borrowed(&a.message),
            Repr::Store(e) => Cow::Owned(format!("store: {} ({})", e.message(), e.code())),
            Repr::Hash(e) => Cow::Owned(e.to_string()),
            Repr::Internal(m) => Cow::Borrowed(m),
        }
    }

    /// The extra fields of the answer, in order (empty for an internal failure).
    pub fn extra(&self) -> Option<&Map<String, Value>> {
        match &*self.0 {
            Repr::Answer(a) => Some(&a.extra),
            _ => None,
        }
    }

    /// The `retryAfter` of the answer, in seconds, when it has one (the HTTP layer sends it as
    /// the `Retry-After` header too).
    pub fn retry_after(&self) -> Option<u64> {
        self.extra()?.get("retryAfter")?.as_u64().filter(|&s| s > 0)
    }

    /// True when the HTTP layer must give back the rate-limit tokens the request took (a hash
    /// refused for its client source: [`AuthError::hash_rate_limited`]).
    pub fn refund_rate(&self) -> bool {
        matches!(&*self.0, Repr::Answer(a) if a.refund_rate)
    }

    /// The store error behind an internal failure.
    pub fn store_error(&self) -> Option<&StoreError> {
        match &*self.0 {
            Repr::Store(e) => Some(e),
            _ => None,
        }
    }

    /// True for a store that stayed locked past its busy timeout (nothing changed).
    pub fn is_store_busy(&self) -> bool {
        self.store_error().is_some_and(|e| e.kind() == ErrorKind::Busy)
    }

    /// The JSON body of the answer: `{"error": code, "message": message, ...extra}` (for an
    /// internal failure, the generic answer of the HTTP layer).
    pub fn body(&self) -> Value {
        let mut out = Map::new();
        match &*self.0 {
            Repr::Answer(a) => {
                out.insert("error".into(), Value::from(a.code));
                out.insert("message".into(), Value::from(a.message.as_ref()));
                for (k, v) in &a.extra {
                    out.insert(k.clone(), v.clone());
                }
            }
            _ => {
                out.insert("error".into(), Value::from("internal"));
                out.insert("message".into(), Value::from("Internal server error."));
            }
        }
        Value::Object(out)
    }
}

impl fmt::Debug for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.0 {
            Repr::Answer(a) => f
                .debug_struct("AuthError")
                .field("status", &a.status)
                .field("code", &a.code)
                .field("message", &a.message)
                .field("extra", &a.extra)
                .finish(),
            Repr::Store(e) => f.debug_tuple("AuthError::Store").field(e).finish(),
            Repr::Hash(e) => f.debug_tuple("AuthError::Hash").field(e).finish(),
            Repr::Internal(m) => f.debug_tuple("AuthError::Internal").field(m).finish(),
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.0 {
            Repr::Answer(a) => write!(f, "{} {}: {}", a.status, a.code, a.message),
            _ => f.write_str(&self.message()),
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &*self.0 {
            Repr::Store(e) => Some(e),
            Repr::Hash(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for AuthError {
    fn from(e: StoreError) -> AuthError {
        AuthError(Box::new(Repr::Store(e)))
    }
}

impl From<HashError> for AuthError {
    /// A refused hash: 429 `rate_limited` for a client source with too many hashes waiting, 503
    /// `server_busy` otherwise (the optional hashes that would not wait never get here: their
    /// callers skip them). A failed key derivation is an internal failure.
    fn from(e: HashError) -> AuthError {
        match e {
            HashError::Busy(reason) => match reason.answer() {
                BusyAnswer::RateLimited => AuthError::hash_rate_limited(),
                BusyAnswer::ServerBusy | BusyAnswer::Skip => AuthError::server_busy(None),
            },
            HashError::Failed(f) => AuthError(Box::new(Repr::Hash(f))),
        }
    }
}

/// `Result` of the auth service.
pub type AuthResult<T> = Result<T, AuthError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::password::BusyReason;

    #[test]
    fn answers_keep_the_order_of_their_fields() {
        let e = AuthError::new(428, "pow_required", "Proof of work required.")
            .with("reason", "required")
            .with("pow", serde_json::json!({ "challenge": "c", "bits": 18, "expiresAt": 1 }));
        assert_eq!(
            serde_json::to_string(&e.body()).unwrap(),
            r#"{"error":"pow_required","message":"Proof of work required.","reason":"required","pow":{"challenge":"c","bits":18,"expiresAt":1}}"#
        );
        assert_eq!(
            (e.status(), e.code(), e.retry_after(), e.refund_rate()),
            (428, "pow_required", None, false)
        );
    }

    #[test]
    fn too_many_attempts_rounds_the_delay_up_to_a_second_at_least() {
        for (ms, secs) in [(0, 1), (1, 1), (999, 1), (1000, 1), (1001, 2), (900_000, 900)] {
            let e = AuthError::too_many_attempts(ms);
            assert_eq!(
                (e.status(), e.code(), e.retry_after()),
                (429, "too_many_attempts", Some(secs)),
                "{ms}"
            );
        }
        assert_eq!(AuthError::too_many_attempts(1).message(), "Too many attempts; wait before trying again.");
    }

    #[test]
    fn busy_answers() {
        let e = AuthError::server_busy(Some(1));
        assert_eq!(
            serde_json::to_string(&e.body()).unwrap(),
            r#"{"error":"server_busy","message":"The server is busy; try again in a few seconds.","retryAfter":1}"#
        );
        for _ in 0..50 {
            let s = AuthError::server_busy(None).retry_after().unwrap();
            assert!((5..=15).contains(&s));
        }
        let e: AuthError = HashError::Busy(BusyReason::SourceLimit).into();
        assert_eq!((e.status(), e.code(), e.refund_rate()), (429, "rate_limited", true));
        assert_eq!(e.message(), "Too many requests; try again later.");
        assert!((5..=15).contains(&e.retry_after().unwrap()));
        for r in [BusyReason::QueueFull, BusyReason::Timeout] {
            let e: AuthError = HashError::Busy(r).into();
            assert_eq!((e.status(), e.code(), e.refund_rate()), (503, "server_busy", false));
        }
    }

    #[test]
    fn internal_failures_are_not_exposed() {
        let e: AuthError = StoreError::new(ErrorKind::Busy, "database is locked").into();
        assert!(!e.is_exposed() && e.is_store_busy());
        assert_eq!((e.status(), e.code(), e.extra()), (500, "internal", None));
        assert_eq!(e.body(), serde_json::json!({ "error": "internal", "message": "Internal server error." }));
        let e: AuthError = HashError::Failed(HashFailure("m < 8p".into())).into();
        assert!(!e.is_exposed() && !e.is_store_busy());
        assert_eq!(AuthError::invalid_credentials().status(), 401);
    }
}
