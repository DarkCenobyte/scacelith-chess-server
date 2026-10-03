//! What a handler returns: an [`Answer`] (JSON, HTML, text, bytes or nothing) or an
//! [`ApiError`], answered `{ "error": code, "message": message, ...extra }` (docs/API.md 1.3).

use std::borrow::Cow;
use std::fmt;

use bytes::Bytes;
use serde_json::{Map, Value};

/// The payload of an answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// No answer at all (`null` in Node): 204 without headers of content.
    NoContent,
    /// A JSON body (`application/json; charset=utf-8`).
    Json(Value),
    /// No body, but `Content-Length: 0` (a JSON answer whose body is `undefined`).
    Empty,
    /// An HTML page (`text/html; charset=utf-8`, page CSP).
    Html(String),
    /// Text (`text/plain; charset=utf-8` unless a content type is given).
    Text(String),
    /// Binary data (`application/octet-stream` unless a content type is given).
    Bytes(Bytes),
}

/// A successful answer of a handler.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// HTTP status (200 by default).
    pub status: u16,
    /// The body.
    pub payload: Payload,
    /// The content type of a text or binary answer (`None`: the default of the payload).
    pub content_type: Option<String>,
    /// Extra headers, set after the content headers (they may replace them).
    pub headers: Vec<(String, String)>,
    /// Gives back every rate token the request took (the handler did none of the work the
    /// limits protect).
    pub refund_rate: bool,
}

impl Answer {
    fn of(payload: Payload) -> Answer {
        Answer { status: 200, payload, content_type: None, headers: Vec::new(), refund_rate: false }
    }

    /// A 200 JSON answer.
    pub fn json(body: Value) -> Answer {
        Answer::of(Payload::Json(body))
    }

    /// No answer: 204.
    pub fn no_content() -> Answer {
        Answer { status: 204, ..Answer::of(Payload::NoContent) }
    }

    /// An answer with no body and `Content-Length: 0`.
    pub fn empty() -> Answer {
        Answer::of(Payload::Empty)
    }

    /// A 200 HTML page.
    pub fn html(page: impl Into<String>) -> Answer {
        Answer::of(Payload::Html(page.into()))
    }

    /// A 200 text answer.
    pub fn text(text: impl Into<String>) -> Answer {
        Answer::of(Payload::Text(text.into()))
    }

    /// A 200 binary answer.
    pub fn bytes(bytes: impl Into<Bytes>) -> Answer {
        Answer::of(Payload::Bytes(bytes.into()))
    }

    /// Sets the status.
    pub fn status(mut self, status: u16) -> Answer {
        self.status = status;
        self
    }

    /// Sets the content type of a text or binary answer.
    pub fn content_type(mut self, content_type: impl Into<String>) -> Answer {
        self.content_type = Some(content_type.into());
        self
    }

    /// Adds a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Answer {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Gives back the request's rate tokens.
    pub fn refund_rate(mut self) -> Answer {
        self.refund_rate = true;
        self
    }
}

/// An error answer. Only an error built with [`ApiError::new`] (or a helper) is shown to the
/// client; [`ApiError::internal`] answers 500 `internal_error` and logs its detail. The fields
/// live behind one box (a `Result<_, ApiError>` stays a pointer wide) and read as
/// `err.status`, `err.code`...
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError(Box<ApiErrorData>);

impl std::ops::Deref for ApiError {
    type Target = ApiErrorData;

    fn deref(&self) -> &ApiErrorData {
        &self.0
    }
}

impl std::ops::DerefMut for ApiError {
    fn deref_mut(&mut self) -> &mut ApiErrorData {
        &mut self.0
    }
}

/// The fields of an [`ApiError`] (read and written through it).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiErrorData {
    /// HTTP status.
    pub status: u16,
    /// The snake_case `error` code.
    pub code: Cow<'static, str>,
    /// The English `message`.
    pub message: Cow<'static, str>,
    /// Fields added to the JSON answer after `error` and `message`, in order (`retryAfter` also
    /// sets the `Retry-After` header).
    pub extra: Map<String, Value>,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// Gives back every rate token the request took.
    pub refund_rate: bool,
    /// Answers with `Connection: close` and drops the connection once the answer is out.
    pub close_connection: bool,
    /// Weight of this refusal toward a block of the client's address (0: not counted).
    pub abuse_weight: f64,
    /// The detail of an internal error (logged, never sent).
    pub internal: Option<String>,
}

impl ApiError {
    /// An error shown to the client.
    pub fn new(
        status: u16,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) -> ApiError {
        ApiError(Box::new(ApiErrorData {
            status,
            code: code.into(),
            message: message.into(),
            extra: Map::new(),
            headers: Vec::new(),
            refund_rate: false,
            close_connection: false,
            abuse_weight: 0.0,
            internal: None,
        }))
    }

    /// An unexpected failure: 500 `internal_error` "Internal server error.", `detail` logged.
    pub fn internal(detail: impl fmt::Display) -> ApiError {
        let mut e = ApiError::new(500, "internal_error", "Internal server error.");
        e.internal = Some(detail.to_string());
        e
    }

    /// 400 `invalid_request`.
    pub fn invalid_request(message: impl Into<Cow<'static, str>>) -> ApiError {
        ApiError::new(400, "invalid_request", message)
    }

    /// 404 `not_found`.
    pub fn not_found(message: impl Into<Cow<'static, str>>) -> ApiError {
        ApiError::new(404, "not_found", message)
    }

    /// 429 `rate_limited` "Too many requests; try again later." with `retryAfter` (seconds,
    /// `max(1, ceil(ms / 1000))`) and the `Retry-After` header.
    pub fn rate_limited(retry_after_ms: f64) -> ApiError {
        ApiError::new(429, "rate_limited", "Too many requests; try again later.")
            .with_extra("retryAfter", Value::from(retry_after_secs(retry_after_ms)))
    }

    /// Adds a field to the JSON answer.
    pub fn with_extra(mut self, key: impl Into<String>, value: Value) -> ApiError {
        self.extra.insert(key.into(), value);
        self
    }

    /// Adds a header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> ApiError {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Gives back the request's rate tokens.
    pub fn refund_rate(mut self) -> ApiError {
        self.refund_rate = true;
        self
    }

    /// Closes the connection after the answer.
    pub fn close_connection(mut self) -> ApiError {
        self.close_connection = true;
        self
    }

    /// Counts this refusal toward a block of the client's address with `weight`.
    pub fn abuse_weight(mut self, weight: f64) -> ApiError {
        self.abuse_weight = weight;
        self
    }

    /// Whether the error is shown to the client as it is (not an internal error).
    pub fn is_exposed(&self) -> bool {
        self.internal.is_none()
    }

    /// The JSON body of the error: `{ error, message, ...extra }`.
    pub fn body(&self) -> Value {
        let mut m = Map::new();
        m.insert("error".into(), Value::String(self.code.to_string()));
        m.insert("message".into(), Value::String(self.message.to_string()));
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.internal {
            Some(detail) => write!(f, "internal error: {detail}"),
            None => write!(f, "{} {}: {}", self.status, self.code, self.message),
        }
    }
}

impl std::error::Error for ApiError {}

/// `max(1, ceil(ms / 1000))`: the seconds of a `Retry-After`.
pub fn retry_after_secs(ms: f64) -> u64 {
    let s = (ms / 1000.0).ceil();
    if s.is_nan() || s < 1.0 { 1 } else { s as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_body_keeps_the_extra_order() {
        let e = ApiError::new(418, "teapot", "I am a teapot.")
            .with_extra("hint", json!(42))
            .with_extra("a", json!(1));
        assert_eq!(
            crate::http::json::stringify(&e.body()),
            r#"{"error":"teapot","message":"I am a teapot.","hint":42,"a":1}"#
        );
        assert!(e.is_exposed());
        let i = ApiError::internal("disk on fire");
        assert_eq!((i.status, i.code.as_ref(), i.is_exposed()), (500, "internal_error", false));
    }

    #[test]
    fn rate_limited_rounds_up_to_a_second() {
        assert_eq!(ApiError::rate_limited(6000.0).extra["retryAfter"], json!(6));
        assert_eq!(ApiError::rate_limited(1.0).extra["retryAfter"], json!(1));
        assert_eq!(ApiError::rate_limited(0.0).extra["retryAfter"], json!(1));
        assert_eq!(ApiError::rate_limited(62001.0).extra["retryAfter"], json!(63));
    }
}
