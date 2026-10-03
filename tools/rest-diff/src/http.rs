//! A raw HTTP/1.1 client over TLS or plain TCP that sends exactly what a scenario asks for
//! (malformed requests included) and keeps everything of the answer: status line, headers in
//! their order and case, body bytes, and whether the server closed the connection after it.
//!
//! Each simulated client binds its own loopback source address (127.0.x.y), so that the
//! per-address limits of one scenario never spend those of another: the servers run with their
//! production limits.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::ClientConfig;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

/// Longest wait for a whole answer (an export or a GIF render may take tens of seconds).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);
/// A kept-alive connection idle for longer is replaced before use (both servers close idle
/// connections after 5 s).
const MAX_IDLE: Duration = Duration::from_millis(3500);
/// How long to wait for the server to close a connection it announced it would close.
const CLOSE_WAIT: Duration = Duration::from_millis(3000);
/// Largest answer head accepted.
const MAX_HEAD: usize = 1 << 20;

/// A request to send.
#[derive(Clone, Debug)]
pub struct Req {
    /// Method.
    pub method: String,
    /// Request target (path and query).
    pub target: String,
    /// Header lines after `Host` (and before the automatic `Content-Length`).
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
    /// Exact bytes to send instead of a request built from the fields above.
    pub raw: Option<Vec<u8>>,
    /// Send on a new connection (and close it after the answer).
    pub fresh: bool,
    /// Do not add `Content-Length` (the scenario writes its own framing headers).
    pub no_length: bool,
    /// After the answer, wait to see whether the server closes the connection.
    pub watch_close: bool,
}

impl Req {
    /// A request with no body.
    pub fn new(method: &str, target: impl Into<String>) -> Req {
        Req {
            method: method.to_string(),
            target: target.into(),
            headers: Vec::new(),
            body: Vec::new(),
            raw: None,
            fresh: false,
            no_length: false,
            watch_close: false,
        }
    }

    /// `GET target`.
    pub fn get(target: impl Into<String>) -> Req {
        Req::new("GET", target)
    }

    /// `POST target` (no body yet).
    pub fn post(target: impl Into<String>) -> Req {
        Req::new("POST", target)
    }

    /// Exact bytes, sent on a new connection.
    pub fn raw(bytes: impl Into<Vec<u8>>) -> Req {
        Req { raw: Some(bytes.into()), fresh: true, watch_close: true, ..Req::new("RAW", "") }
    }

    /// A JSON body (`Content-Type: application/json`).
    pub fn json(self, v: serde_json::Value) -> Req {
        self.body_bytes("application/json", v.to_string().into_bytes())
    }

    /// A body with its content type.
    pub fn body_bytes(mut self, content_type: &str, body: impl Into<Vec<u8>>) -> Req {
        self.headers.push(("Content-Type".into(), content_type.into()));
        self.body = body.into();
        self
    }

    /// A body without a `Content-Type` header.
    pub fn body_untyped(mut self, body: impl Into<Vec<u8>>) -> Req {
        self.body = body.into();
        self
    }

    /// An `application/x-www-form-urlencoded` body.
    pub fn form(self, fields: &[(&str, &str)]) -> Req {
        let body =
            fields.iter().map(|(k, v)| format!("{}={}", form_escape(k), form_escape(v))).collect::<Vec<_>>();
        self.body_bytes("application/x-www-form-urlencoded", body.join("&").into_bytes())
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(self, token: &str) -> Req {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    /// One more header line.
    pub fn header(mut self, name: &str, value: &str) -> Req {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// On a new connection, watching whether the server closes it after the answer.
    pub fn fresh(mut self) -> Req {
        self.fresh = true;
        self.watch_close = true;
        self
    }

    /// Without the automatic `Content-Length`.
    pub fn no_length(mut self) -> Req {
        self.no_length = true;
        self
    }

    /// A one-line summary for the report.
    pub fn summary(&self) -> String {
        if let Some(raw) = &self.raw {
            let first = raw.split(|&b| b == b'\n').next().unwrap_or_default();
            let mut s = String::from_utf8_lossy(first).trim_end().to_string();
            if s.len() > 120 {
                s.truncate(120);
                s.push_str("...");
            }
            return format!("raw {} bytes: {s}", raw.len());
        }
        let mut s = format!("{} {}", self.method, self.target);
        if s.len() > 160 {
            s.truncate(160);
            s.push_str("...");
        }
        if !self.body.is_empty() {
            let body = String::from_utf8_lossy(&self.body);
            if body.len() <= 200 {
                s.push_str(&format!(" {body}"));
            } else {
                s.push_str(&format!(" <{} bytes>", self.body.len()));
            }
        }
        s
    }

    fn bytes(&self, host: &str) -> Vec<u8> {
        if let Some(raw) = &self.raw {
            return raw.replace_host(host);
        }
        let mut head = format!("{} {} HTTP/1.1\r\nHost: {host}\r\n", self.method, self.target);
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        let body_method = matches!(self.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
        if !self.no_length && (!self.body.is_empty() || body_method) {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Raw requests name the host as `{host}`, replaced by the server's.
trait ReplaceHost {
    fn replace_host(&self, host: &str) -> Vec<u8>;
}

impl ReplaceHost for Vec<u8> {
    fn replace_host(&self, host: &str) -> Vec<u8> {
        let marker = b"{host}";
        let mut out = Vec::with_capacity(self.len());
        let mut i = 0;
        while i < self.len() {
            if self[i..].starts_with(marker) {
                out.extend_from_slice(host.as_bytes());
                i += marker.len();
            } else {
                out.push(self[i]);
                i += 1;
            }
        }
        out
    }
}

fn form_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// An answer (or the failure to get one).
#[derive(Clone, Debug, Default)]
pub struct Resp {
    /// Status code (0 when no answer came).
    pub status: u16,
    /// Reason phrase of the status line.
    pub reason: String,
    /// Protocol of the status line (`HTTP/1.1`).
    pub version: String,
    /// Headers in their order and case.
    pub headers: Vec<(String, String)>,
    /// Body (transfer coding removed).
    pub body: Vec<u8>,
    /// Whether the body came chunked.
    pub chunked: bool,
    /// Whether the server closed the connection after the answer (`None`: not watched).
    pub closed: Option<bool>,
    /// The transport failure when no complete answer came.
    pub error: Option<String>,
    /// Wall time of the answer (ms since the epoch).
    pub at_ms: f64,
}

impl Resp {
    /// The first header with this name (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The body as JSON.
    pub fn json(&self) -> Option<crate::json::J> {
        crate::json::parse(&self.body).ok()
    }

    /// A value of the JSON body by path, as text.
    pub fn text_at(&self, path: &str) -> Option<String> {
        self.json()?.at(path)?.scalar_text()
    }
}

/// Where a server listens and how to reach it.
#[derive(Clone)]
pub struct Target {
    /// Address of the listener.
    pub addr: SocketAddr,
    /// TLS client settings (`None`: plain HTTP).
    pub tls: Option<Arc<ClientConfig>>,
    /// The `Host` header value.
    pub host: String,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

struct Conn {
    io: Box<dyn Io>,
    buf: Vec<u8>,
    last_used: Instant,
}

/// One simulated client: a source address and one kept-alive connection.
pub struct Client {
    target: Target,
    src: IpAddr,
    conn: Option<Conn>,
}

impl Client {
    /// A client of `target` from the source address `src`.
    pub fn new(target: Target, src: IpAddr) -> Client {
        Client { target, src, conn: None }
    }

    async fn connect(&self) -> Result<Conn, String> {
        let socket = match self.src {
            IpAddr::V4(_) => TcpSocket::new_v4(),
            IpAddr::V6(_) => TcpSocket::new_v6(),
        }
        .map_err(|e| format!("socket: {e}"))?;
        socket.bind(SocketAddr::new(self.src, 0)).map_err(|e| format!("bind {}: {e}", self.src))?;
        let tcp: TcpStream = tokio::time::timeout(Duration::from_secs(10), socket.connect(self.target.addr))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| format!("connect: {e}"))?;
        tcp.set_nodelay(true).ok();
        let io: Box<dyn Io> = match &self.target.tls {
            None => Box::new(tcp),
            Some(cfg) => {
                let name = ServerName::try_from("localhost".to_string()).expect("a valid server name");
                let tls = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio_rustls::TlsConnector::from(cfg.clone()).connect(name, tcp),
                )
                .await
                .map_err(|_| "TLS handshake timeout".to_string())?
                .map_err(|e| format!("TLS: {e}"))?;
                Box::new(tls)
            }
        };
        Ok(Conn { io, buf: Vec::new(), last_used: Instant::now() })
    }

    /// Sends a request and reads its answer. A kept-alive connection that the server closed
    /// while idle is replaced once.
    pub async fn send(&mut self, req: &Req) -> Resp {
        let bytes = req.bytes(&self.target.host);
        if req.fresh {
            let mut conn = match self.connect().await {
                Ok(c) => c,
                Err(e) => return failure(e),
            };
            return exchange(&mut conn, &bytes, req).await.0;
        }
        for attempt in 0..2 {
            let stale = self.conn.as_ref().is_none_or(|c| c.last_used.elapsed() > MAX_IDLE);
            if stale {
                self.conn = match self.connect().await {
                    Ok(c) => Some(c),
                    Err(e) => return failure(e),
                };
            }
            let conn = self.conn.as_mut().expect("a connection was just opened");
            let (resp, reusable, nothing_read) = exchange(conn, &bytes, req).await;
            if !reusable {
                self.conn = None;
            } else if let Some(c) = self.conn.as_mut() {
                c.last_used = Instant::now();
            }
            // A connection closed before any byte of an answer on a reused connection: the
            // server closed it while idle; the request was not read.
            if nothing_read && !stale && attempt == 0 {
                continue;
            }
            return resp;
        }
        failure("no answer".into())
    }
}

/// A failed exchange: no complete answer.
pub fn failure(e: String) -> Resp {
    Resp { error: Some(e), at_ms: now_ms(), ..Resp::default() }
}

/// Wall time in ms since the epoch.
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Writes the request and reads one answer: (answer, connection reusable, closed before any
/// answer byte).
async fn exchange(conn: &mut Conn, bytes: &[u8], req: &Req) -> (Resp, bool, bool) {
    if let Err(e) = conn.io.write_all(bytes).await {
        return (failure(format!("write: {e}")), false, conn.buf.is_empty());
    }
    let _ = conn.io.flush().await;
    let head_only = req.method == "HEAD";
    match tokio::time::timeout(ANSWER_TIMEOUT, read_answer(conn, head_only)).await {
        Err(_) => (failure("no answer within the deadline".into()), false, false),
        Ok(Err((e, nothing))) => (failure(e), false, nothing),
        Ok(Ok(mut resp)) => {
            let announced_close = resp
                .header("connection")
                .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")));
            let to_end = resp.error.is_none() && resp.header("content-length").is_none() && !resp.chunked;
            let mut reusable = !announced_close && !req.fresh && !to_end && resp.version == "HTTP/1.1";
            if req.watch_close || announced_close {
                let closed = wait_closed(conn).await;
                resp.closed = Some(closed);
                if closed {
                    reusable = false;
                }
            }
            (resp, reusable, false)
        }
    }
}

async fn wait_closed(conn: &mut Conn) -> bool {
    let deadline = Instant::now() + CLOSE_WAIT;
    let mut scratch = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, conn.io.read(&mut scratch)).await {
            Err(_) => return false,
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
        }
    }
}

async fn fill(conn: &mut Conn) -> Result<usize, String> {
    let mut chunk = [0u8; 16384];
    match conn.io.read(&mut chunk).await {
        Ok(n) => {
            conn.buf.extend_from_slice(&chunk[..n]);
            Ok(n)
        }
        // A TLS close without close_notify is how Node ends some connections: treat as EOF.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
        Err(e) => Err(format!("read: {e}")),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn read_answer(conn: &mut Conn, head_only: bool) -> Result<Resp, (String, bool)> {
    let end = loop {
        if let Some(i) = find(&conn.buf, b"\r\n\r\n") {
            break i;
        }
        if conn.buf.len() > MAX_HEAD {
            return Err(("answer head too large".into(), false));
        }
        let nothing = conn.buf.is_empty();
        match fill(conn).await {
            Ok(0) => {
                let what = if nothing {
                    "connection closed without an answer".to_string()
                } else {
                    format!("connection closed in the answer head: {:?}", String::from_utf8_lossy(&conn.buf))
                };
                return Err((what, nothing));
            }
            Ok(_) => {}
            Err(e) => return Err((e, nothing)),
        }
    };
    let head: Vec<u8> = conn.buf.drain(..end + 4).collect();
    let text = String::from_utf8_lossy(&head[..end]).to_string();
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_string();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default().to_string();
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let reason = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.to_string(), v.trim().to_string()));
        } else {
            headers.push((line.to_string(), String::new()));
        }
    }
    let mut resp = Resp { status, reason, version, headers, at_ms: now_ms(), ..Resp::default() };
    if (100..200).contains(&status) && status != 101 {
        // An interim answer: the real one follows.
        return Box::pin(read_answer(conn, head_only)).await;
    }
    if head_only || status == 204 || status == 304 {
        return Ok(resp);
    }
    let chunked =
        resp.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        resp.chunked = true;
        match read_chunked(conn).await {
            Ok(body) => resp.body = body,
            Err(e) => resp.error = Some(e),
        }
    } else if let Some(len) = resp.header("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        while conn.buf.len() < len {
            match fill(conn).await {
                Ok(0) => {
                    resp.error = Some(format!("body cut at {} of {len} bytes", conn.buf.len()));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    resp.error = Some(e);
                    break;
                }
            }
        }
        let n = len.min(conn.buf.len());
        resp.body = conn.buf.drain(..n).collect();
    } else {
        loop {
            match fill(conn).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        resp.body = std::mem::take(&mut conn.buf);
    }
    Ok(resp)
}

async fn read_line(conn: &mut Conn) -> Result<String, String> {
    loop {
        if let Some(i) = find(&conn.buf, b"\r\n") {
            let line: Vec<u8> = conn.buf.drain(..i + 2).collect();
            return Ok(String::from_utf8_lossy(&line[..i]).to_string());
        }
        if fill(conn).await? == 0 {
            return Err("connection closed in a chunked body".into());
        }
    }
}

async fn read_chunked(conn: &mut Conn) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line = read_line(conn).await?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or_default().trim(), 16)
            .map_err(|_| format!("invalid chunk size {line:?}"))?;
        if size == 0 {
            while !read_line(conn).await?.is_empty() {}
            return Ok(body);
        }
        while conn.buf.len() < size + 2 {
            if fill(conn).await? == 0 {
                return Err("connection closed in a chunk".into());
            }
        }
        body.extend(conn.buf.drain(..size));
        conn.buf.drain(..2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_requests() {
        let r = Req::post("/api/v1/x").json(serde_json::json!({"a": 1})).bearer("sct_t");
        let bytes = String::from_utf8(r.bytes("localhost:1")).unwrap();
        assert_eq!(
            bytes,
            "POST /api/v1/x HTTP/1.1\r\nHost: localhost:1\r\nContent-Type: application/json\r\n\
             Authorization: Bearer sct_t\r\nContent-Length: 7\r\n\r\n{\"a\":1}"
        );
        let raw = Req::raw(b"GET / HTTP/1.1\r\nHost: {host}\r\n\r\n".to_vec());
        assert_eq!(raw.bytes("h:2"), b"GET / HTTP/1.1\r\nHost: h:2\r\n\r\n");
        assert_eq!(form_escape("a b&c=é"), "a+b%26c%3D%C3%A9");
        assert_eq!(
            String::from_utf8(Req::post("/p").bytes("h")).unwrap(),
            "POST /p HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n"
        );
    }
}
