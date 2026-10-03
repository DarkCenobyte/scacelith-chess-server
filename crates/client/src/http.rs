//! A small HTTP/1.1 client: one request at a time on a keep-alive connection, bodies with
//! `Content-Length`, chunked transfer coding or read to the end. Enough for the Scacelith API
//! and its downloads (PGN, GIF); no redirects, no compression, no pipelining.

use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{ApiError, ClientError, Result};
use crate::net::{Endpoint, Stream};

/// Largest response head (status line and headers).
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest response body.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// An HTTP request.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// Method (`GET`, `POST`...).
    pub method: &'a str,
    /// Request target: path and query (`/api/v1/leaderboard?category=3%2B2`).
    pub target: &'a str,
    /// Session token sent as `Authorization: Bearer <token>`.
    pub bearer: Option<&'a str>,
    /// `Content-Type` of the body (`None`: no header).
    pub content_type: Option<&'a str>,
    /// The body (may be empty).
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// A `GET` of `target`.
    pub fn get(target: &'a str) -> Request<'a> {
        Request { method: "GET", target, bearer: None, content_type: None, body: &[] }
    }

    /// A `POST` of a JSON body.
    pub fn post_json(target: &'a str, body: &'a [u8]) -> Request<'a> {
        Request { method: "POST", target, bearer: None, content_type: Some("application/json"), body }
    }

    /// The same request with a session token.
    pub fn with_bearer(self, token: &'a str) -> Request<'a> {
        Request { bearer: Some(token), ..self }
    }
}

/// An HTTP response.
#[derive(Clone, Debug)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Headers in the order received (names as sent).
    pub headers: Vec<(String, String)>,
    /// The body, transfer coding removed.
    pub body: Bytes,
}

impl Response {
    /// The first header with this name (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `Retry-After` in seconds.
    pub fn retry_after(&self) -> Option<u64> {
        self.header("retry-after")?.trim().parse().ok()
    }

    /// The body parsed as JSON.
    pub fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body).map_err(|e| ClientError::Http(format!("invalid JSON body: {e}")))
    }

    /// The answer as an [`ApiError`] (for a status the caller did not expect).
    pub fn api_error(&self) -> ApiError {
        let body = self.json().unwrap_or(Value::Null);
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
        ApiError {
            status: self.status,
            error: text("error").unwrap_or_else(|| format!("http_{}", self.status)),
            message: text("message").unwrap_or_default(),
            retry_after: self.retry_after(),
            body,
        }
    }
}

/// One HTTP/1.1 connection, reused for the next request while the server keeps it open.
#[derive(Debug)]
pub struct HttpConnection<S = Stream> {
    stream: S,
    buf: BytesMut,
    host: String,
    reusable: bool,
    served: u64,
    last_used: Instant,
}

impl HttpConnection<Stream> {
    /// Connects to the endpoint (TLS when configured).
    pub async fn open(endpoint: &Endpoint) -> Result<HttpConnection<Stream>> {
        let (stream, _) = endpoint.connect().await?;
        Ok(HttpConnection::new(stream, endpoint.host_header()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> HttpConnection<S> {
    /// A connection over an open stream; `host` is the Host header value.
    pub fn new(stream: S, host: String) -> HttpConnection<S> {
        HttpConnection {
            stream,
            buf: BytesMut::with_capacity(8192),
            host,
            reusable: true,
            served: 0,
            last_used: Instant::now(),
        }
    }

    /// Whether another request may be sent on this connection (the last response allowed it and
    /// nothing failed).
    pub fn is_reusable(&self) -> bool {
        self.reusable
    }

    /// Requests answered on this connection.
    pub fn requests_served(&self) -> u64 {
        self.served
    }

    /// Time since the last response ended (or since the connection opened).
    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Sends a request and reads its response. After any error the connection is not reusable.
    pub async fn send(&mut self, req: &Request<'_>) -> Result<Response> {
        if !self.reusable {
            return Err(ClientError::Http("the connection is not reusable".into()));
        }
        self.reusable = false;
        let mut head =
            format!("{} {} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\n", req.method, req.target, self.host);
        if let Some(token) = req.bearer {
            head.push_str("Authorization: Bearer ");
            head.push_str(token);
            head.push_str("\r\n");
        }
        if let Some(ct) = req.content_type {
            head.push_str("Content-Type: ");
            head.push_str(ct);
            head.push_str("\r\n");
        }
        let has_body_method = !matches!(req.method, "GET" | "HEAD" | "OPTIONS");
        if !req.body.is_empty() || has_body_method {
            head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
        }
        head.push_str("\r\n");
        let mut out = Vec::with_capacity(head.len() + req.body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(req.body);
        self.stream.write_all(&out).await?;
        self.stream.flush().await?;

        let (status, headers, keep_alive) = loop {
            let parsed = self.read_head().await?;
            // Interim answers (100 Continue...) precede the real one.
            if !(100..200).contains(&parsed.0) || parsed.0 == 101 {
                break parsed;
            }
        };
        let header =
            |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
        let no_body = req.method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status);
        let chunked = header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let mut to_end = false;
        let body = if no_body {
            Bytes::new()
        } else if chunked {
            self.read_chunked().await?
        } else if let Some(len) = header("content-length") {
            let len: usize = len
                .trim()
                .parse()
                .map_err(|_| ClientError::Http(format!("invalid Content-Length {len:?}")))?;
            if len > MAX_BODY_BYTES {
                return Err(ClientError::Http(format!("body of {len} bytes is too large")));
            }
            self.read_exact_body(len).await?
        } else {
            to_end = true;
            self.read_to_end().await?
        };
        self.served += 1;
        self.last_used = Instant::now();
        self.reusable = keep_alive && !to_end;
        Ok(Response { status, headers, body })
    }

    async fn fill(&mut self) -> Result<usize> {
        let n = self.stream.read_buf(&mut self.buf).await?;
        Ok(n)
    }

    async fn fill_or_fail(&mut self, what: &str) -> Result<()> {
        if self.fill().await? == 0 {
            return Err(ClientError::Http(format!("connection closed in the {what}")));
        }
        Ok(())
    }

    /// Reads a response head: status, headers, whether the connection stays open.
    async fn read_head(&mut self) -> Result<(u16, Vec<(String, String)>, bool)> {
        let end = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i;
            }
            if self.buf.len() > MAX_HEAD_BYTES {
                return Err(ClientError::Http("response head too large".into()));
            }
            self.fill_or_fail("response head").await?;
        };
        let head = self.buf.split_to(end + 4);
        let text = std::str::from_utf8(&head[..end])
            .map_err(|_| ClientError::Http("response head is not UTF-8".into()))?;
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .filter(|s| (100..1000).contains(s))
            .ok_or_else(|| ClientError::Http(format!("invalid status line {status_line:?}")))?;
        if !version.starts_with("HTTP/1.") {
            return Err(ClientError::Http(format!("invalid status line {status_line:?}")));
        }
        let mut headers = Vec::new();
        for line in lines {
            let (k, v) =
                line.split_once(':').ok_or_else(|| ClientError::Http(format!("invalid header {line:?}")))?;
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        let connection = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(",");
        let keep_alive = if version == "HTTP/1.0" {
            connection.contains("keep-alive")
        } else {
            !connection.split(',').any(|t| t.trim() == "close")
        };
        Ok((status, headers, keep_alive))
    }

    async fn read_exact_body(&mut self, len: usize) -> Result<Bytes> {
        while self.buf.len() < len {
            self.buf.reserve(len - self.buf.len());
            self.fill_or_fail("response body").await?;
        }
        Ok(self.buf.split_to(len).freeze())
    }

    async fn read_to_end(&mut self) -> Result<Bytes> {
        while self.fill().await? > 0 {
            if self.buf.len() > MAX_BODY_BYTES {
                return Err(ClientError::Http("body too large".into()));
            }
        }
        Ok(self.buf.split().freeze())
    }

    async fn read_line(&mut self) -> Result<String> {
        let end = loop {
            if let Some(i) = find(&self.buf, b"\r\n") {
                break i;
            }
            if self.buf.len() > MAX_HEAD_BYTES {
                return Err(ClientError::Http("chunk line too long".into()));
            }
            self.fill_or_fail("chunked body").await?;
        };
        let line = self.buf.split_to(end + 2);
        String::from_utf8(line[..end].to_vec())
            .map_err(|_| ClientError::Http("chunk line is not UTF-8".into()))
    }

    async fn read_chunked(&mut self) -> Result<Bytes> {
        let mut body = BytesMut::new();
        loop {
            let line = self.read_line().await?;
            let size_text = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size_text, 16)
                .map_err(|_| ClientError::Http(format!("invalid chunk size {line:?}")))?;
            if size == 0 {
                // Trailers, up to the blank line.
                while !self.read_line().await?.is_empty() {}
                return Ok(body.freeze());
            }
            if body.len() + size > MAX_BODY_BYTES {
                return Err(ClientError::Http("body too large".into()));
            }
            while self.buf.len() < size + 2 {
                self.fill_or_fail("chunked body").await?;
            }
            body.extend_from_slice(&self.buf[..size]);
            if &self.buf[size..size + 2] != b"\r\n" {
                return Err(ClientError::Http("chunk not followed by CRLF".into()));
            }
            self.buf.advance(size + 2);
        }
    }
}

/// Position of `needle` in `haystack`.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader, duplex};

    async fn serve(answers: Vec<&'static [u8]>) -> HttpConnection<tokio::io::DuplexStream> {
        let (client, server) = duplex(1 << 16);
        tokio::spawn(async move {
            let mut server = BufReader::new(server);
            for answer in answers {
                // Read one request head (and its body, by Content-Length).
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if server.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; len];
                server.read_exact(&mut body).await.unwrap();
                server.get_mut().write_all(answer).await.unwrap();
            }
        });
        HttpConnection::new(client, "example.test".into())
    }

    #[tokio::test]
    async fn content_length_chunked_and_keep_alive() {
        let mut conn = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"a\":[1,2]}",
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 429 Too Many\r\nRetry-After: 7\r\nTransfer-Encoding: chunked\r\n\r\n4;x=y\r\n{\"er\r\n14\r\nror\":\"rate_limited\"}\r\n0\r\nTrailer: 1\r\n\r\n",
            b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
        ])
        .await;
        let r = conn.send(&Request::get("/api/v1/info")).await.unwrap();
        assert_eq!((r.status, r.json().unwrap()), (200, serde_json::json!({"a": [1, 2]})));
        assert!(conn.is_reusable());
        let r = conn.send(&Request::post_json("/x", b"{}").with_bearer("sct_x")).await.unwrap();
        assert_eq!((r.status, r.retry_after()), (429, Some(7)));
        let e = r.api_error();
        assert_eq!((e.error.as_str(), e.retry_after), ("rate_limited", Some(7)));
        let r = conn.send(&Request { method: "DELETE", ..Request::get("/y") }).await.unwrap();
        assert_eq!((r.status, r.body.len()), (204, 0));
        assert!(!conn.is_reusable(), "Connection: close");
        assert_eq!(conn.requests_served(), 3);
        assert!(conn.send(&Request::get("/z")).await.is_err());
    }

    #[tokio::test]
    async fn body_to_the_end_and_cut_short() {
        let mut conn = serve(vec![b"HTTP/1.0 200 OK\r\n\r\nplain text"]).await;
        // The server task ends after its answer, which closes the stream.
        let r = conn.send(&Request::get("/")).await.unwrap();
        assert_eq!(&r.body[..], b"plain text");
        assert!(!conn.is_reusable());

        let mut conn = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort"]).await;
        assert!(matches!(conn.send(&Request::get("/")).await, Err(ClientError::Http(_))));
        let mut conn = serve(vec![b"SMTP/1.1 200 OK\r\n\r\n"]).await;
        assert!(matches!(conn.send(&Request::get("/")).await, Err(ClientError::Http(_))));
    }
}
