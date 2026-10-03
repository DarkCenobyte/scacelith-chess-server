//! Request bodies (docs/API.md 1.2): reading under a size limit and a deadline, media types, and
//! parsing JSON and HTML form bodies.

use std::fmt::Display;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::HeaderValue;
use http_body_util::BodyExt;
use hyper::body::Body;
use serde_json::{Map, Value};

use super::answer::ApiError;
use super::json;
use super::url::parse_urlencoded;

/// How long reading a body may take, from the start of the read.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(10);

fn too_large(limit: usize) -> ApiError {
    ApiError::new(413, "payload_too_large", format!("The body must not exceed {limit} bytes."))
        .close_connection()
}

/// Reads a whole body of at most `limit` bytes within `timeout`. Every refusal closes the
/// connection once answered: 400 for a malformed `Content-Length`, 413 announced or streamed
/// over the limit, 408 on the deadline, 400 when the body breaks off.
pub async fn read_body<B>(
    mut body: B,
    content_length: Option<&HeaderValue>,
    limit: usize,
    timeout: Duration,
) -> Result<Bytes, ApiError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Display,
{
    if let Some(cl) = content_length {
        let digits = cl.as_bytes();
        if digits.is_empty() || digits.len() > 15 || !digits.iter().all(u8::is_ascii_digit) {
            return Err(ApiError::invalid_request("Invalid Content-Length.").close_connection());
        }
        let announced: u64 = cl.to_str().ok().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
        if announced > limit as u64 {
            return Err(too_large(limit));
        }
    }
    let read = async {
        let mut buf = BytesMut::new();
        while let Some(frame) = body.frame().await {
            let frame = frame
                .map_err(|_| ApiError::invalid_request("The request was aborted.").close_connection())?;
            if let Ok(data) = frame.into_data() {
                if buf.len() + data.len() > limit {
                    return Err(too_large(limit));
                }
                buf.extend_from_slice(&data);
            }
        }
        Ok(buf.freeze())
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(r) => r,
        Err(_) => {
            Err(ApiError::new(408, "request_timeout", "The request body took too long.").close_connection())
        }
    }
}

/// Whitespace as JavaScript's `\s` and `trim()` see it.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// The `charset` of one media type parameter (`/^\s*charset\s*=\s*"?([^";\s]+)"?\s*$/i`).
fn charset_param(p: &str) -> Option<String> {
    let p = p.trim_start_matches(js_space);
    let name = p.get(..7).filter(|n| n.eq_ignore_ascii_case("charset"))?;
    let rest = p[name.len()..].trim_start_matches(js_space).strip_prefix('=')?.trim_start_matches(js_space);
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find(|c: char| c == '"' || c == ';' || js_space(c)).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let tail = &rest[end..];
    let tail = tail.strip_prefix('"').unwrap_or(tail);
    if !tail.trim_start_matches(js_space).is_empty() {
        return None;
    }
    Some(rest[..end].to_lowercase())
}

/// The media type of a `Content-Type` header (lower-case, empty without one) and its charset
/// (lower-case, the last valid parameter wins).
pub fn media_type(header: Option<&HeaderValue>) -> (String, Option<String>) {
    let Some(h) = header.filter(|h| !h.is_empty()) else {
        return (String::new(), None);
    };
    let text = String::from_utf8_lossy(h.as_bytes());
    let mut parts = text.split(';');
    let ty = parts.next().unwrap_or("").trim_matches(js_space).to_lowercase();
    let charset = parts.filter_map(charset_param).next_back();
    (ty, charset)
}

/// Parses a body read for a route: `{}` when empty (whatever the content type), JSON on every
/// route, form data on pages. `Content-Type` charsets other than UTF-8 are refused first.
pub fn parse_body(buf: &[u8], content_type: Option<&HeaderValue>, page: bool) -> Result<Value, ApiError> {
    if buf.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let (ty, charset) = media_type(content_type);
    if charset.is_some_and(|c| c != "utf-8" && c != "utf8") {
        return Err(ApiError::new(415, "unsupported_media_type", "Only UTF-8 is accepted."));
    }
    let text = String::from_utf8_lossy(buf);
    if page && ty == "application/x-www-form-urlencoded" {
        let mut out = Map::new();
        for (k, v) in parse_urlencoded(&text) {
            if out.contains_key(&k) {
                return Err(ApiError::invalid_request(format!("field \"{k}\" given twice")));
            }
            out.insert(k, Value::String(v));
        }
        return Ok(Value::Object(out));
    }
    if ty == "application/json" {
        return json::parse(&text)
            .map_err(|_| ApiError::new(400, "invalid_json", "The body is not valid JSON."));
    }
    let message = if page { "Form data expected." } else { "Content-Type must be application/json." };
    Err(ApiError::new(415, "unsupported_media_type", message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::TestBody;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).expect("a header value")
    }

    #[test]
    fn media_types_and_charsets() {
        assert_eq!(media_type(None), (String::new(), None));
        assert_eq!(media_type(Some(&hv("Application/JSON"))), ("application/json".into(), None));
        assert_eq!(
            media_type(Some(&hv("application/json; Charset=\"UTF-8\""))),
            ("application/json".into(), Some("utf-8".into()))
        );
        assert_eq!(
            media_type(Some(&hv("text/plain; charset=latin1; charset=utf8"))).1.as_deref(),
            Some("utf8")
        );
        assert_eq!(media_type(Some(&hv("text/plain; charset=utf 8"))).1, None, "not a valid parameter");
        assert_eq!(media_type(Some(&hv("text/plain; charsetx=latin1"))).1, None);
        assert_eq!(media_type(Some(&hv("text/plain; charset=\"latin1"))).1.as_deref(), Some("latin1"));
    }

    #[test]
    fn parsing_by_route_kind() {
        let json_ct = hv("application/json");
        assert_eq!(parse_body(b"", Some(&hv("text/plain")), false).expect("empty"), serde_json::json!({}));
        assert_eq!(parse_body(b"[1]", Some(&json_ct), false).expect("json"), serde_json::json!([1]));
        let e = parse_body(b"{", Some(&json_ct), false).unwrap_err();
        assert_eq!((e.status, e.code.as_ref()), (400, "invalid_json"));
        let e = parse_body(b"{}", Some(&hv("application/json; charset=latin1")), false).unwrap_err();
        assert_eq!((e.status, e.message.as_ref()), (415, "Only UTF-8 is accepted."));
        let e = parse_body(b"a=1", Some(&hv("application/x-www-form-urlencoded")), false).unwrap_err();
        assert_eq!((e.status, e.message.as_ref()), (415, "Content-Type must be application/json."));
        let form = hv("application/x-www-form-urlencoded");
        assert_eq!(
            parse_body(b"token=a%20b&x=", Some(&form), true).expect("form"),
            serde_json::json!({"token": "a b", "x": ""})
        );
        let e = parse_body(b"token=a&token=b", Some(&form), true).unwrap_err();
        assert_eq!(e.message, "field \"token\" given twice");
        let e = parse_body(b"x", None, true).unwrap_err();
        assert_eq!(e.message, "Form data expected.");
        assert_eq!(
            parse_body(b"{\"a\":1}", Some(&json_ct), true).expect("json on a page"),
            serde_json::json!({"a": 1})
        );
    }

    #[tokio::test]
    async fn reading_limits() {
        let ok = read_body(TestBody::full("hello"), Some(&hv("5")), 5, BODY_TIMEOUT).await;
        assert_eq!(ok.expect("fits"), Bytes::from_static(b"hello"));
        for bad in ["-1", "1e3", "", "1234567890123456", "0x10"] {
            let e = read_body(TestBody::empty(), Some(&hv(bad)), 5, BODY_TIMEOUT).await.unwrap_err();
            assert_eq!(
                (e.status, e.message.as_ref(), e.close_connection),
                (400, "Invalid Content-Length.", true),
                "{bad}"
            );
        }
        let e =
            read_body(TestBody::empty(), Some(&hv("000000000000006")), 5, BODY_TIMEOUT).await.unwrap_err();
        assert_eq!((e.status, e.message.as_ref()), (413, "The body must not exceed 5 bytes."));
        let e = read_body(TestBody::chunks(&["abc", "def"]), None, 5, BODY_TIMEOUT).await.unwrap_err();
        assert_eq!((e.status, e.close_connection), (413, true), "a streamed body over the limit");
        let (tx, body) = TestBody::channel();
        tx.send("ab");
        tx.abort();
        let e = read_body(body, None, 5, BODY_TIMEOUT).await.unwrap_err();
        assert_eq!(
            (e.status, e.message.as_ref(), e.close_connection),
            (400, "The request was aborted.", true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_body_times_out() {
        let (tx, body) = TestBody::channel();
        tx.send("{");
        let e = read_body(body, None, 5, BODY_TIMEOUT).await.unwrap_err();
        assert_eq!((e.status, e.code.as_ref(), e.close_connection), (408, "request_timeout", true));
        drop(tx);
    }
}
