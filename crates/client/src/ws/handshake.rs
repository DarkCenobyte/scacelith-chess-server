//! The client side of the WebSocket opening handshake (RFC 6455 section 4.1).

use base64::Engine as _;
use bytes::BytesMut;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{ClientError, Result};
use crate::http::find;

/// The GUID of RFC 6455 appended to the key.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Largest `101` (or refusal) head read.
const MAX_HEAD: usize = 8192;
/// Largest refusal body read.
const MAX_REFUSAL_BODY: usize = 64 * 1024;

/// `Sec-WebSocket-Accept` of a key.
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// A fresh `Sec-WebSocket-Key`: 16 random bytes in base64.
pub fn new_key() -> String {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).expect("the operating system's random source is available");
    base64::engine::general_purpose::STANDARD.encode(nonce)
}

/// What the upgrade request carries.
#[derive(Clone, Debug)]
pub struct UpgradeRequest<'a> {
    /// Host header value.
    pub host: &'a str,
    /// Request target (`/ws`).
    pub path: &'a str,
    /// The one subprotocol asked for.
    pub subprotocol: &'a str,
    /// More header lines (`Origin`, `X-Forwarded-For` in tests).
    pub extra_headers: &'a [(String, String)],
}

/// Sends the upgrade request on `stream` and reads the answer. On `101` with every check passed,
/// returns the answer's headers and the bytes read after them (the first frames).
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    req: &UpgradeRequest<'_>,
) -> Result<(Vec<(String, String)>, BytesMut)> {
    let key = new_key();
    let mut head = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {}\r\n",
        req.path, req.host, req.subprotocol
    );
    for (k, v) in req.extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = BytesMut::with_capacity(1024);
    let end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(ClientError::WebSocket("upgrade answer head too large".into()));
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(ClientError::WebSocket("connection closed during the upgrade".into()));
        }
    };
    let head = buf.split_to(end + 4);
    let text = std::str::from_utf8(&head[..end])
        .map_err(|_| ClientError::WebSocket("upgrade answer is not UTF-8".into()))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| ClientError::WebSocket(format!("invalid status line {status_line:?}")))?;
    let mut headers = Vec::new();
    for line in lines {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| ClientError::WebSocket(format!("invalid header line {line:?}")))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let header =
        |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());

    if status != 101 {
        let retry_after = header("retry-after").and_then(|v| v.trim().parse().ok());
        let len = header("content-length").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
        let len = len.min(MAX_REFUSAL_BODY);
        while buf.len() < len {
            if stream.read_buf(&mut buf).await? == 0 {
                break;
            }
        }
        let body = buf.split_to(len.min(buf.len())).freeze();
        return Err(ClientError::UpgradeRefused { status, retry_after, body });
    }
    let has_token = |name: &str, token: &str| {
        header(name).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    if !has_token("upgrade", "websocket") || !has_token("connection", "upgrade") {
        return Err(ClientError::WebSocket("101 without Upgrade: websocket and Connection: Upgrade".into()));
    }
    if header("sec-websocket-accept") != Some(accept_key(&key).as_str()) {
        return Err(ClientError::WebSocket("wrong Sec-WebSocket-Accept".into()));
    }
    if header("sec-websocket-protocol") != Some(req.subprotocol) {
        return Err(ClientError::WebSocket(format!(
            "the server did not select the subprotocol {}",
            req.subprotocol
        )));
    }
    if header("sec-websocket-extensions").is_some() {
        return Err(ClientError::WebSocket("the server selected an extension nobody asked for".into()));
    }
    Ok((headers, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_is_rfc_example() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(new_key().len(), 24);
        assert_ne!(new_key(), new_key());
    }
}
