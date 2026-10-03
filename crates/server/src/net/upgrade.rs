//! WebSocket upgrades (DESIGN 5.8): the checks of an upgrade request in their order, the refusal
//! and `101` answers, the admission hook, and the strict head reader of the dedicated WebSocket
//! port (`WS_PORT != API_PORT`).
//!
//! On the shared port the request comes from hyper (an `Upgrade` request is never an API
//! request); on the dedicated port [`WsEndpoint::serve_dedicated`] reads the head itself: 8 KiB,
//! 64 header lines, CRLF only, every duplicate header joined.

use std::borrow::Cow;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Response, StatusCode, Version};
use serde_json::json;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::guard::IpGuard;
use super::ip::ClientAddress;
use super::ws::{AdmissionPermit, CLOSE_INTERNAL, WsConnection, WsIo, WsSettings};
use crate::config::Config;
use crate::http::json::stringify;
use crate::log::Logger;
use crate::log_error;
use crate::metrics::{self, Counter, CounterVec, Histogram};
use crate::security::ratelimit::random_retry_after;

/// The path of the upgrade.
pub const WS_PATH: &str = "/ws";
/// The subprotocol of the realtime protocol v1.
pub const SUBPROTOCOL: &str = "scacelith.rt1";
/// Largest request head on the dedicated port (bytes, blank line included).
pub const MAX_HEAD_BYTES: usize = 8192;
/// Most header lines on the dedicated port.
pub const MAX_HEADER_LINES: usize = 64;
/// How long a refused socket may take to read its answer.
pub const REFUSAL_LINGER: Duration = Duration::from_secs(1);
/// The range of the random `Retry-After` (seconds) of the 429 and 503 refusals that only time
/// ends (too many connections, a full or draining server). Short: the game waits up to half
/// more than it says, and a player with a game in progress must still come back within the
/// reconnection grace (at least 15 s), as with the game's own 8 s between attempts at most.
pub const BUSY_RETRY_AFTER_SEC: (u64, u64) = (2, 5);

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

struct UpgradeMetrics {
    accepted: Counter,
    rejected: CounterVec,
    handshake_ms: Histogram,
}

fn upgrade_metrics() -> &'static UpgradeMetrics {
    static M: LazyLock<UpgradeMetrics> = LazyLock::new(|| UpgradeMetrics {
        accepted: metrics::counter("scacelith_ws_handshakes_accepted_total", "WebSocket upgrades accepted"),
        rejected: metrics::counter_vec(
            "scacelith_ws_handshakes_rejected_total",
            "WebSocket upgrades refused",
            &["reason"],
        ),
        handshake_ms: metrics::histogram(
            "scacelith_ws_handshake_ms",
            "Upgrade request to 101 response (admission included)",
            &[0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0],
        ),
    });
    &M
}

/// `Sec-WebSocket-Accept` of a key.
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// Bytes read as ISO-8859-1 (every byte one character, as Node reads header bytes).
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// JavaScript whitespace among the ISO-8859-1 characters.
fn js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}')
}

/// Comma-separated tokens, trimmed, lower-cased, empty ones dropped.
fn tokens(v: Option<&str>) -> Vec<String> {
    v.unwrap_or("")
        .split(',')
        .map(|s| s.trim_matches(js_space).to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `^[A-Za-z0-9+/]{21}[AQgw]==$`: the base64 of exactly 16 bytes.
fn valid_key(key: &str) -> bool {
    let b = key.as_bytes();
    b.len() == 24
        && b[..21].iter().all(|&c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
        && matches!(b[21], b'A' | b'Q' | b'g' | b'w')
        && &b[22..] == b"=="
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&c)
}

/// Request headers seen by Node once on the shared port: the first value kept.
const FIRST_WINS: [&str; 18] = [
    "host",
    "authorization",
    "content-type",
    "user-agent",
    "referer",
    "age",
    "content-length",
    "etag",
    "expires",
    "from",
    "if-modified-since",
    "if-unmodified-since",
    "last-modified",
    "location",
    "max-forwards",
    "proxy-authorization",
    "retry-after",
    "server",
];

/// An upgrade request as the checks read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadView {
    /// The method.
    pub method: String,
    /// `"1.1"`, `"1.0"`...
    pub version: String,
    /// The request target.
    pub target: String,
    headers: Vec<(String, String)>,
}

impl HeadView {
    /// A header value (lower-case name), duplicates already joined.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The view of a request parsed by hyper, with Node's duplicate rules (a first-wins list,
    /// `cookie` joined with `; `, every other header with `, `).
    pub fn from_request(method: &str, version: Version, target: &str, headers: &HeaderMap) -> HeadView {
        let mut out: Vec<(String, String)> = Vec::new();
        for (name, value) in headers {
            let k = name.as_str();
            let v = latin1(value.as_bytes()).trim_matches([' ', '\t']).to_string();
            match out.iter_mut().find(|(n, _)| n == k) {
                None => out.push((k.to_string(), v)),
                Some(_) if FIRST_WINS.contains(&k) => {}
                Some((_, prev)) => {
                    prev.push_str(if k == "cookie" { "; " } else { ", " });
                    prev.push_str(&v);
                }
            }
        }
        let version = match version {
            Version::HTTP_09 => "0.9",
            Version::HTTP_10 => "1.0",
            Version::HTTP_2 => "2.0",
            Version::HTTP_3 => "3.0",
            _ => "1.1",
        };
        HeadView {
            method: method.to_string(),
            version: version.to_string(),
            target: target.to_string(),
            headers: out,
        }
    }
}

/// Parses a request head (without its blank line) on the dedicated port. `None` when it is
/// malformed: more than 64 header lines, a request line other than
/// `^([A-Z]{1,16}) (\S{1,2048}) HTTP/(\d)\.(\d)$`, a header without a token name, or a value with
/// control characters. Every duplicate header is joined with `, `.
pub fn parse_request_head(head: &[u8]) -> Option<HeadView> {
    let text = latin1(head);
    let lines: Vec<&str> = text.split("\r\n").collect();
    if lines.len() > MAX_HEADER_LINES + 1 {
        return None;
    }
    let (method, rest) = lines[0].split_once(' ')?;
    if method.is_empty() || method.len() > 16 || !method.bytes().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let (target, version) = rest.split_once(' ')?;
    if target.is_empty() || target.chars().count() > 2048 || target.chars().any(js_space) {
        return None;
    }
    let v = version.strip_prefix("HTTP/")?.as_bytes();
    if v.len() != 3 || !v[0].is_ascii_digit() || v[1] != b'.' || !v[2].is_ascii_digit() {
        return None;
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in &lines[1..] {
        let c = line.find(':').filter(|&c| c > 0)?;
        let name = &line[..c];
        if !name.bytes().all(is_token_char) {
            return None;
        }
        let value = line[c + 1..].trim_matches([' ', '\t']);
        if value.chars().any(|ch| matches!(ch, '\0'..='\u{8}' | '\n'..='\u{1f}' | '\u{7f}')) {
            return None;
        }
        let k = name.to_ascii_lowercase();
        match headers.iter_mut().find(|(n, _)| *n == k) {
            Some((_, prev)) => {
                prev.push_str(", ");
                prev.push_str(value);
            }
            None => headers.push((k, value.to_string())),
        }
    }
    Some(HeadView {
        method: method.to_string(),
        version: format!("{}.{}", v[0] as char, v[2] as char),
        target: target.to_string(),
        headers,
    })
}

/// A refused upgrade: a small JSON answer, then the connection closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// HTTP status.
    pub status: u16,
    /// The reason (metric label, default body).
    pub reason: Cow<'static, str>,
    /// Extra headers, after `Content-Length`.
    pub extra: Vec<(&'static str, String)>,
    /// The JSON body.
    pub body: String,
}

impl Refusal {
    /// A refusal answered `{"error":"<reason>"}`.
    pub fn new(status: u16, reason: impl Into<Cow<'static, str>>) -> Refusal {
        let reason = reason.into();
        let body = stringify(&json!({ "error": reason.as_ref() }));
        Refusal { status, reason, extra: Vec::new(), body }
    }

    /// A 429 or 503 refusal that only time ends, answered `{"error":"<reason>","retryAfter":s}`
    /// with `Retry-After: s`, `s` drawn in [`BUSY_RETRY_AFTER_SEC`] so that the clients refused
    /// together do not all come back together.
    pub fn busy(status: u16, reason: impl Into<Cow<'static, str>>) -> Refusal {
        let mut r = Refusal::new(status, reason);
        let s = random_retry_after(BUSY_RETRY_AFTER_SEC.0, BUSY_RETRY_AFTER_SEC.1);
        r.body = stringify(&json!({ "error": r.reason.as_ref(), "retryAfter": s }));
        r.header("Retry-After", s.to_string())
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Refusal {
        self.extra.push((name, value.into()));
        self
    }

    fn reason_phrase(&self) -> &'static str {
        match self.status {
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            426 => "Upgrade Required",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            503 => "Service Unavailable",
            _ => "Error",
        }
    }

    /// The answer as raw bytes (dedicated port).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            self.status,
            self.reason_phrase(),
            self.body.len()
        );
        for (k, v) in &self.extra {
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
            s.push_str("\r\n");
        }
        s.push_str("\r\n");
        s.push_str(&self.body);
        s.into_bytes()
    }

    /// The answer through hyper (shared port): the same headers in the same order.
    pub fn to_response(&self) -> Response<Bytes> {
        let mut res = Response::new(Bytes::from(self.body.clone()));
        *res.status_mut() = StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_REQUEST);
        let h = res.headers_mut();
        h.insert(http::header::CONNECTION, HeaderValue::from_static("close"));
        h.insert(http::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(http::header::CONTENT_LENGTH, HeaderValue::from(self.body.len()));
        for (k, v) in &self.extra {
            if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                h.append(k, v);
            }
        }
        res
    }
}

/// Why the admission hook refused a connection (answered as a [`Refusal::busy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRefusal {
    /// HTTP status (429 for one address, 503 for the whole server).
    pub status: u16,
    /// The `error` of the answer and the metric label.
    pub error: Cow<'static, str>,
}

impl AdmissionRefusal {
    /// 429 `too_many_connections`: the address holds too many connections.
    pub fn too_many_connections() -> AdmissionRefusal {
        AdmissionRefusal { status: 429, error: Cow::Borrowed("too_many_connections") }
    }

    /// 503 `server_full`: the server holds `MAX_CONNECTIONS`.
    pub fn server_full() -> AdmissionRefusal {
        AdmissionRefusal { status: 503, error: Cow::Borrowed("server_full") }
    }
}

/// Admits WebSocket connections (implemented by the realtime module: per-address and global
/// connection counts). Called last, once every check passed; the permit is dropped when the
/// connection ends.
pub trait Admission: Send + Sync + 'static {
    /// Admits a connection from `ip`, or says why not.
    fn acquire(&self, ip: IpAddr) -> Result<AdmissionPermit, AdmissionRefusal>;
}

/// Receives every new connection (implemented by the realtime module, which spawns its task).
pub type OnConnection = Arc<dyn Fn(WsConnection) + Send + Sync>;

/// An accepted upgrade.
#[derive(Debug)]
pub struct Accepted {
    /// The `Sec-WebSocket-Accept` value.
    pub accept: String,
    /// The admission's permit.
    pub permit: AdmissionPermit,
    /// The client address.
    pub ip: IpAddr,
    /// When the checks started.
    pub started: Instant,
}

/// An invalid extra header of the `101` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUpgradeHeader(pub String);

impl std::fmt::Display for InvalidUpgradeHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid upgrade header {}", self.0)
    }
}

impl std::error::Error for InvalidUpgradeHeader {}

/// The WebSocket endpoint: the checks, the hooks, and the state shared by both ports.
pub struct WsEndpoint {
    allow_origins: HashSet<String>,
    upgrade_headers: Vec<(String, String)>,
    admission: Option<Arc<dyn Admission>>,
    on_connection: OnConnection,
    settings: WsSettings,
    accepting: AtomicBool,
    guard: Option<Arc<IpGuard>>,
    client: ClientAddress,
    head_timeout: Duration,
    log: Logger,
}

impl std::fmt::Debug for WsEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsEndpoint").field("accepting", &self.is_accepting()).finish()
    }
}

impl WsEndpoint {
    /// The endpoint of `config` (`WS_ALLOWED_ORIGINS`, `WS_HELLO_TIMEOUT_MS`), handing connections
    /// to `on_connection`.
    pub fn new(config: &Config, settings: WsSettings, on_connection: OnConnection) -> WsEndpoint {
        let hello = u64::try_from(config.ws_hello_timeout_ms).unwrap_or(10_000).min(10_000);
        WsEndpoint {
            allow_origins: config
                .ws_allowed_origins
                .iter()
                .map(|o| o.trim_matches(js_space).to_lowercase())
                .filter(|o| !o.is_empty())
                .collect(),
            upgrade_headers: Vec::new(),
            admission: None,
            on_connection,
            log: settings.log.clone(),
            settings,
            accepting: AtomicBool::new(true),
            guard: None,
            client: ClientAddress::direct(),
            head_timeout: Duration::from_millis(hello),
        }
    }

    /// Admits connections through `admission` (none: every connection is admitted).
    pub fn admission(mut self, admission: impl Admission) -> WsEndpoint {
        self.admission = Some(Arc::new(admission));
        self
    }

    /// Takes the request budget of the address first (429 `rate_limited`).
    pub fn guard(mut self, guard: Arc<IpGuard>) -> WsEndpoint {
        self.guard = Some(guard);
        self
    }

    /// How client addresses are found (proxy mode).
    pub fn client_address(mut self, client: ClientAddress) -> WsEndpoint {
        self.client = client;
        self
    }

    /// Adds a header to every `101` answer (`Scacelith-Server-Id`). The name must be a token and
    /// the value free of control characters.
    pub fn upgrade_header(mut self, name: &str, value: &str) -> Result<WsEndpoint, InvalidUpgradeHeader> {
        let bad_value = value.chars().any(|c| matches!(c, '\0'..='\u{8}' | '\n'..='\u{1f}' | '\u{7f}'));
        if name.is_empty()
            || !name.bytes().all(is_token_char)
            || bad_value
            || HeaderValue::from_str(value).is_err()
        {
            return Err(InvalidUpgradeHeader(name.to_string()));
        }
        self.upgrade_headers.push((name.to_string(), value.to_string()));
        Ok(self)
    }

    /// Stops (or resumes) accepting upgrades: new ones get 503 `shutting_down`.
    pub fn set_accepting(&self, accepting: bool) {
        self.accepting.store(accepting, Ordering::Release);
    }

    /// Whether upgrades are accepted.
    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    /// The connection settings.
    pub fn settings(&self) -> &WsSettings {
        &self.settings
    }

    /// Runs the checks of an upgrade request from `peer`, in order (DESIGN 5.8).
    pub fn check(&self, req: &HeadView, peer: IpAddr) -> Result<Accepted, Refusal> {
        let started = Instant::now();
        let ip = self.client.resolve(peer, req.header("x-forwarded-for"));
        let refused = |r: Refusal| {
            upgrade_metrics().rejected.with(&[&r.reason]).inc();
            Err(r)
        };
        if let Some(guard) = &self.guard
            && let Err(r) = guard.request(&guard.keys(ip))
        {
            let s = crate::http::answer::retry_after_secs(r.retry_after_ms);
            let mut refusal = Refusal::new(429, "rate_limited").header("Retry-After", s.to_string());
            refusal.body = stringify(&json!({
                "error": "rate_limited", "message": "Too many requests; try again later.", "retryAfter": s,
            }));
            return refused(refusal);
        }
        if !self.is_accepting() {
            return refused(Refusal::busy(503, "shutting_down"));
        }
        if req.method != "GET" {
            return refused(Refusal::new(405, "method_not_allowed").header("Allow", "GET"));
        }
        if req.version != "1.1" {
            return refused(Refusal::new(400, "http_version"));
        }
        let path = req.target.split_once('?').map_or(req.target.as_str(), |(p, _)| p);
        if path != WS_PATH {
            return refused(Refusal::new(404, "not_found"));
        }
        if req.header("host").is_none_or(str::is_empty) {
            return refused(Refusal::new(400, "bad_upgrade"));
        }
        let websocket = tokens(req.header("upgrade")).iter().any(|t| t == "websocket");
        let upgrade = tokens(req.header("connection")).iter().any(|t| t == "upgrade");
        if !websocket || !upgrade {
            return refused(Refusal::new(400, "bad_upgrade"));
        }
        if req.header("sec-websocket-version") != Some("13") {
            return refused(Refusal::new(426, "unsupported_version").header("Sec-WebSocket-Version", "13"));
        }
        let Some(key) = req.header("sec-websocket-key").filter(|k| valid_key(k)) else {
            return refused(Refusal::new(400, "bad_key"));
        };
        let origin = req.header("origin").or_else(|| req.header("sec-websocket-origin"));
        if let Some(o) = origin
            && !self.allow_origins.contains(&o.trim_matches(js_space).to_lowercase())
        {
            return refused(Refusal::new(403, "origin_forbidden"));
        }
        if !tokens(req.header("sec-websocket-protocol")).iter().any(|t| t == SUBPROTOCOL) {
            let mut r = Refusal::new(426, "unsupported_protocol");
            r.body = stringify(&json!({"error": "unsupported_protocol", "supported": [SUBPROTOCOL]}));
            return refused(r);
        }
        let permit = match &self.admission {
            None => AdmissionPermit::none(),
            Some(adm) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| adm.acquire(ip))) {
                Ok(Ok(permit)) => permit,
                Ok(Err(r)) => return refused(Refusal::busy(r.status, r.error)),
                Err(_) => {
                    log_error!(self.log, "admission failed", {"err": {"message": "the admission hook panicked"}});
                    return refused(Refusal::busy(503, "admission_error"));
                }
            },
        };
        Ok(Accepted { accept: accept_key(key), permit, ip, started })
    }

    /// The headers of the `101` answer, in order.
    pub fn switching_headers(&self, accept: &str) -> Vec<(String, String)> {
        let mut h = vec![
            ("Upgrade".to_string(), "websocket".to_string()),
            ("Connection".to_string(), "Upgrade".to_string()),
            ("Sec-WebSocket-Accept".to_string(), accept.to_string()),
            ("Sec-WebSocket-Protocol".to_string(), SUBPROTOCOL.to_string()),
        ];
        h.extend(self.upgrade_headers.iter().cloned());
        h
    }

    /// The `101` answer through hyper (shared port).
    pub fn switching_response(&self, accept: &str) -> Response<Bytes> {
        let mut res = Response::new(Bytes::new());
        *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        for (k, v) in self.switching_headers(accept) {
            if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
                res.headers_mut().append(k, v);
            }
        }
        res
    }

    /// Opens the connection on the upgraded stream and hands it to the realtime module.
    pub fn open(&self, io: Box<dyn WsIo>, leftover: Bytes, accepted: Accepted) {
        let conn = WsConnection::new(io, leftover, accepted.ip, accepted.permit, &self.settings);
        let m = upgrade_metrics();
        m.accepted.inc();
        m.handshake_ms.observe(accepted.started.elapsed().as_secs_f64() * 1000.0);
        let info = conn.info.clone();
        let hook = self.on_connection.clone();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || hook(conn))).is_err() {
            log_error!(self.log, "onConnection failed", {"connId": info.id()});
            info.close(CLOSE_INTERNAL, "internal error");
        }
    }

    /// Counts a head the dedicated port cannot read toward a block of the peer (not of a trusted
    /// proxy), then refuses it.
    fn refuse_head(&self, peer: IpAddr, status: u16, reason: &'static str) -> Refusal {
        if let Some(guard) = &self.guard
            && !self.client.is_trusted_peer(peer)
        {
            guard.note_refusal(&guard.keys(peer), 1.0);
        }
        upgrade_metrics().rejected.with(&[reason]).inc();
        Refusal::new(status, reason)
    }

    /// Serves one connection of the dedicated WebSocket port (after TLS): reads the head within
    /// `min(10 s, WS_HELLO_TIMEOUT_MS)`, checks it, then answers `101` and opens the connection,
    /// or answers the refusal and closes.
    pub async fn serve_dedicated(&self, mut io: Box<dyn WsIo>, peer: IpAddr) {
        let mut buf = BytesMut::with_capacity(1024);
        let deadline = tokio::time::Instant::now() + self.head_timeout;
        let end = loop {
            let mut chunk = [0u8; 2048];
            let n = match tokio::time::timeout_at(deadline, io.read(&mut chunk)).await {
                Err(_) => return refuse(io, self.refuse_head(peer, 408, "timeout")).await,
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(n)) => n,
            };
            let from = buf.len().saturating_sub(3);
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
                break from + i;
            }
            if buf.len() > MAX_HEAD_BYTES {
                return refuse(io, self.refuse_head(peer, 431, "headers_too_large")).await;
            }
        };
        if end + 4 > MAX_HEAD_BYTES {
            return refuse(io, self.refuse_head(peer, 431, "headers_too_large")).await;
        }
        let Some(req) = parse_request_head(&buf[..end]) else {
            return refuse(io, self.refuse_head(peer, 400, "bad_request")).await;
        };
        let leftover = buf.split_off(end + 4).freeze();
        match self.check(&req, peer) {
            Err(r) => refuse(io, r).await,
            Ok(accepted) => {
                let mut head = String::from("HTTP/1.1 101 Switching Protocols\r\n");
                for (k, v) in self.switching_headers(&accepted.accept) {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                if io.write_all(head.as_bytes()).await.is_err() || io.flush().await.is_err() {
                    return;
                }
                self.open(io, leftover, accepted);
            }
        }
    }
}

/// Writes a refusal, half-closes, and gives the client a second to read it.
async fn refuse(mut io: Box<dyn WsIo>, r: Refusal) {
    let linger = async {
        if io.write_all(&r.to_bytes()).await.is_err() {
            return;
        }
        let _ = io.shutdown().await;
        let mut sink = [0u8; 1024];
        while matches!(io.read(&mut sink).await, Ok(n) if n > 0) {}
    };
    let _ = tokio::time::timeout(REFUSAL_LINGER, linger).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, ManualClock};
    use crate::net::ip::AddrKey;
    use crate::net::ws::tests::{frame, read_server_frame};
    use crate::net::ws::{CloseInfo, WsEvent};
    use parking_lot::Mutex;

    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

    fn settings() -> WsSettings {
        WsSettings {
            max_message_bytes: 512,
            close_timeout: Duration::from_millis(300),
            clock: crate::clock::system(),
            log: Logger::root().child("upgrade-test"),
        }
    }

    type Opened = Arc<Mutex<Vec<WsConnection>>>;

    fn endpoint(config: &Config) -> (WsEndpoint, Opened) {
        let opened: Opened = Arc::default();
        let sink = opened.clone();
        let ep = WsEndpoint::new(config, settings(), Arc::new(move |c| sink.lock().push(c)));
        (ep, opened)
    }

    fn request(extra: &[(&str, &str)]) -> HeadView {
        let mut text = String::from(
            "GET /ws HTTP/1.1\r\nHost: example.org\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: scacelith.rt1",
        );
        for (k, v) in extra {
            text.push_str(&format!("\r\n{k}: {v}"));
        }
        parse_request_head(text.as_bytes()).expect("a valid head")
    }

    fn without(name: &str) -> HeadView {
        let mut r = request(&[]);
        r.headers.retain(|(k, _)| k != name);
        r
    }

    fn with(name: &str, value: &str) -> HeadView {
        let mut r = without(name);
        r.headers.push((name.to_string(), value.to_string()));
        r
    }

    fn peer() -> IpAddr {
        IpAddr::from([198, 51, 100, 20])
    }

    #[test]
    fn the_accept_key_of_rfc_6455() {
        assert_eq!(accept_key(KEY), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert!(valid_key(KEY));
        for bad in [
            "dGhlIHNhbXBsZSBub25jZQ=",
            "dGhlIHNhbXBsZSBub25jZR==",
            "dGhlIHNhbXBsZSBub25jZQ==, x",
            "AAAAAAAAAAAAAAAAAAAAAB==",
            "",
        ] {
            assert!(!valid_key(bad), "{bad}");
        }
    }

    #[test]
    fn the_checks_in_their_order() {
        let (ep, _) = endpoint(&Config::for_tests());
        let a = ep.check(&request(&[]), peer()).expect("accepted");
        assert_eq!(a.accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let cases: Vec<(HeadView, u16, &str)> = vec![
            (
                {
                    let mut r = request(&[]);
                    r.method = "POST".into();
                    r
                },
                405,
                "method_not_allowed",
            ),
            (
                {
                    let mut r = request(&[]);
                    r.version = "1.0".into();
                    r
                },
                400,
                "http_version",
            ),
            (
                {
                    let mut r = request(&[]);
                    r.target = "/wsx".into();
                    r
                },
                404,
                "not_found",
            ),
            (without("host"), 400, "bad_upgrade"),
            (with("host", ""), 400, "bad_upgrade"),
            (with("upgrade", "h2c"), 400, "bad_upgrade"),
            (with("connection", "keep-alive"), 400, "bad_upgrade"),
            (with("sec-websocket-version", "8"), 426, "unsupported_version"),
            (with("sec-websocket-key", "short"), 400, "bad_key"),
            (with("sec-websocket-key", &format!("{KEY}, {KEY}")), 400, "bad_key"),
            (with("origin", "https://evil.example"), 403, "origin_forbidden"),
            (with("origin", ""), 403, "origin_forbidden"),
            (with("sec-websocket-protocol", "scacelith.v1"), 426, "unsupported_protocol"),
            (without("sec-websocket-protocol"), 426, "unsupported_protocol"),
        ];
        for (req, status, reason) in cases {
            let r = ep.check(&req, peer()).expect_err(reason);
            assert_eq!((r.status, r.reason.as_ref()), (status, reason));
        }
        let mut q = request(&[]);
        q.target = "/ws?x=1".into();
        assert!(ep.check(&q, peer()).is_ok(), "a query is allowed");
        let r = ep.check(&with("sec-websocket-protocol", "scacelith.v1"), peer()).unwrap_err();
        assert_eq!(r.body, r#"{"error":"unsupported_protocol","supported":["scacelith.rt1"]}"#);
        assert!(
            ep.check(&with("sec-websocket-protocol", "x, SCACELITH.RT1"), peer()).is_ok(),
            "tokens are lower-cased"
        );
        assert!(ep.check(&with("connection", "keep-alive, Upgrade"), peer()).is_ok());
        let r = ep.check(&with("sec-websocket-version", "8"), peer()).unwrap_err();
        assert_eq!(r.extra, [("Sec-WebSocket-Version", "13".to_string())]);
        ep.set_accepting(false);
        let r = ep.check(&request(&[]), peer()).unwrap_err();
        assert_eq!((r.status, r.reason.as_ref()), (503, "shutting_down"));
        busy_secs(&r);
        assert_eq!(
            ep.check(&with("upgrade", "h2c"), peer()).unwrap_err().reason,
            "shutting_down",
            "before the others"
        );
    }

    #[test]
    fn origins_from_the_allowlist() {
        let mut c = Config::for_tests();
        c.ws_allowed_origins = vec![" https://Game.Example ".into(), "".into()];
        let (ep, _) = endpoint(&c);
        assert!(ep.check(&with("origin", "https://game.example"), peer()).is_ok());
        assert!(ep.check(&with("sec-websocket-origin", " HTTPS://GAME.EXAMPLE"), peer()).is_ok());
        assert_eq!(ep.check(&with("origin", "https://other.example"), peer()).unwrap_err().status, 403);
    }

    struct Limit(Mutex<u32>);

    struct Slot(Arc<Limit>);

    impl Drop for Slot {
        fn drop(&mut self) {
            *self.0.0.lock() -= 1;
        }
    }

    impl Admission for Arc<Limit> {
        fn acquire(&self, _ip: IpAddr) -> Result<AdmissionPermit, AdmissionRefusal> {
            let mut n = self.0.lock();
            if *n >= 1 {
                return Err(AdmissionRefusal::too_many_connections());
            }
            *n += 1;
            Ok(AdmissionPermit::new(Slot(self.clone())))
        }
    }

    #[test]
    fn admission_runs_last_and_its_permit_lives_with_the_connection() {
        let limit = Arc::new(Limit(Mutex::new(0)));
        let (ep, _) = endpoint(&Config::for_tests());
        let ep = ep.admission(limit.clone());
        let first = ep.check(&request(&[]), peer()).expect("admitted");
        assert_eq!(*limit.0.lock(), 1);
        let r = ep.check(&request(&[]), peer()).unwrap_err();
        assert_eq!((r.status, r.reason.as_ref()), (429, "too_many_connections"));
        busy_secs(&r);
        assert_eq!(ep.check(&with("upgrade", "x"), peer()).unwrap_err().reason, "bad_upgrade", "never asked");
        drop(first);
        assert_eq!(*limit.0.lock(), 0, "released once");
    }

    /// The `Retry-After` of a [`Refusal::busy`], in its range and the same in the body.
    fn busy_secs(r: &Refusal) -> u64 {
        let [("Retry-After", s)] = r.extra.as_slice() else { panic!("one Retry-After: {:?}", r.extra) };
        let secs: u64 = s.parse().expect("seconds");
        assert!((BUSY_RETRY_AFTER_SEC.0..=BUSY_RETRY_AFTER_SEC.1).contains(&secs), "{secs}");
        assert_eq!(r.body, format!(r#"{{"error":"{}","retryAfter":{secs}}}"#, r.reason));
        secs
    }

    struct Refuse(AdmissionRefusal);

    impl Admission for Refuse {
        fn acquire(&self, _ip: IpAddr) -> Result<AdmissionPermit, AdmissionRefusal> {
            Err(self.0.clone())
        }
    }

    struct Broken;

    impl Admission for Broken {
        fn acquire(&self, _ip: IpAddr) -> Result<AdmissionPermit, AdmissionRefusal> {
            panic!("an admission bug")
        }
    }

    #[test]
    fn refusals_that_only_time_ends_carry_a_retry_after() {
        let refusal = |ep: WsEndpoint| ep.check(&request(&[]), peer()).unwrap_err();
        let (ep, _) = endpoint(&Config::for_tests());
        let r = refusal(ep.admission(Refuse(AdmissionRefusal::too_many_connections())));
        assert_eq!((r.status, r.reason.as_ref()), (429, "too_many_connections"));
        let secs = busy_secs(&r);
        let text = String::from_utf8(r.to_bytes()).expect("ascii");
        assert!(text.starts_with("HTTP/1.1 429 Too Many Requests\r\n"), "{text}");
        assert!(text.contains(&format!("\r\nRetry-After: {secs}\r\n\r\n")), "{text}");
        let res = r.to_response();
        assert_eq!(res.headers()["retry-after"], secs.to_string().as_str());
        let (ep, _) = endpoint(&Config::for_tests());
        let r = refusal(ep.admission(Refuse(AdmissionRefusal::server_full())));
        assert_eq!((r.status, r.reason.as_ref()), (503, "server_full"));
        busy_secs(&r);
        let (ep, _) = endpoint(&Config::for_tests());
        let r = refusal(ep.admission(Broken));
        assert_eq!((r.status, r.reason.as_ref()), (503, "admission_error"));
        busy_secs(&r);
        let drawn: HashSet<u64> = (0..200).map(|_| busy_secs(&Refusal::busy(503, "server_full"))).collect();
        assert!(drawn.len() > 1, "drawn at random: {drawn:?}");
        // The other refusals end with a change of the request, not with time.
        let (ep, _) = endpoint(&Config::for_tests());
        assert!(ep.check(&with("upgrade", "x"), peer()).unwrap_err().extra.is_empty());
    }

    #[test]
    fn the_guard_comes_first() {
        let mut c = Config::for_tests();
        c.http_rate_per_ip = 10;
        c.http_rate_per_prefix = 1000;
        let clock = ManualClock::new(0.0, 0);
        let guard =
            Arc::new(IpGuard::new(&c, clock.clone() as Arc<dyn Clock>, Logger::root()).expect("guard"));
        let (ep, _) = endpoint(&c);
        let ep = ep.guard(guard);
        for _ in 0..5 {
            let _ = ep.check(&with("upgrade", "x"), peer());
        }
        ep.set_accepting(false);
        let r = ep.check(&request(&[]), peer()).unwrap_err();
        assert_eq!((r.status, r.reason.as_ref()), (429, "rate_limited"));
        assert_eq!(r.extra, [("Retry-After", "6".to_string())]);
        assert_eq!(
            r.body,
            r#"{"error":"rate_limited","message":"Too many requests; try again later.","retryAfter":6}"#
        );
    }

    #[test]
    fn refusal_bytes() {
        let r = Refusal::new(426, "unsupported_version").header("Sec-WebSocket-Version", "13");
        let text = String::from_utf8(r.to_bytes()).expect("ascii");
        assert_eq!(
            text,
            "HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\nCache-Control: no-store\r\n\
             Content-Type: application/json\r\nContent-Length: 31\r\nSec-WebSocket-Version: 13\r\n\r\n\
             {\"error\":\"unsupported_version\"}"
        );
        let res = r.to_response();
        let names: Vec<&str> = res.headers().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            ["connection", "cache-control", "content-type", "content-length", "sec-websocket-version"]
        );
        assert_eq!(Refusal::new(418, "x").reason_phrase(), "Error");
    }

    #[test]
    fn the_strict_head_parser() {
        let ok = parse_request_head(b"GET /ws?a=1 HTTP/1.1\r\nHost: a\r\nX-A: 1\r\nx-a:  2 \r\nHost: b")
            .expect("valid");
        assert_eq!((ok.method.as_str(), ok.target.as_str(), ok.version.as_str()), ("GET", "/ws?a=1", "1.1"));
        assert_eq!(ok.header("x-a"), Some("1, 2"));
        assert_eq!(ok.header("host"), Some("a, b"), "every duplicate is joined, Host too");
        let bad: [&[u8]; 10] = [
            b"get /ws HTTP/1.1",
            b"GET /ws HTTP/1.1 ",
            b"GET  /ws HTTP/1.1",
            b"GET /ws HTTP/11",
            b"GET /ws HTTP/1.1\r\n Host: a",
            b"GET /ws HTTP/1.1\r\nHost : a",
            b"GET /ws HTTP/1.1\r\n: a",
            b"GET /ws HTTP/1.1\r\nHost a",
            b"GET /ws HTTP/1.1\r\nX: a\x01b",
            b"GETTINGTOOLONGMETHOD /ws HTTP/1.1",
        ];
        for b in bad {
            assert!(parse_request_head(b).is_none(), "{}", String::from_utf8_lossy(b));
        }
        let mut many = String::from("GET /ws HTTP/1.1");
        for i in 0..64 {
            many.push_str(&format!("\r\nX-{i}: v"));
        }
        assert!(parse_request_head(many.as_bytes()).is_some(), "64 header lines");
        many.push_str("\r\nX-64: v");
        assert!(parse_request_head(many.as_bytes()).is_none(), "65 header lines");
        assert!(parse_request_head(b"GET /ws\xa0x HTTP/1.1").is_none(), "U+00A0 is a space for \\S");
        assert!(parse_request_head(b"GET /ws HTTP/1.1\r\nX: caf\xe9").is_some(), "bytes over 0x7f are fine");
        let long = format!("GET /{} HTTP/1.1", "a".repeat(2047));
        assert!(parse_request_head(long.as_bytes()).is_some());
        let long = format!("GET /{} HTTP/1.1", "a".repeat(2048));
        assert!(parse_request_head(long.as_bytes()).is_none());
    }

    #[test]
    fn shared_port_duplicates_follow_node() {
        let mut h = HeaderMap::new();
        h.append("host", HeaderValue::from_static("a"));
        h.append("host", HeaderValue::from_static("b"));
        h.append("origin", HeaderValue::from_static("x"));
        h.append("origin", HeaderValue::from_static("y"));
        h.append("cookie", HeaderValue::from_static("c=1"));
        h.append("cookie", HeaderValue::from_static("d=2"));
        let v = HeadView::from_request("GET", Version::HTTP_11, "/ws", &h);
        assert_eq!(
            (v.header("host"), v.header("origin"), v.header("cookie")),
            (Some("a"), Some("x, y"), Some("c=1; d=2"))
        );
        assert_eq!(v.version, "1.1");
    }

    #[test]
    fn upgrade_headers_are_validated() {
        let (ep, _) = endpoint(&Config::for_tests());
        let ep = ep.upgrade_header("Scacelith-Server-Id", "abc").expect("valid");
        assert!(ep.switching_headers("k").contains(&("Scacelith-Server-Id".into(), "abc".into())));
        let (ep2, _) = endpoint(&Config::for_tests());
        assert!(ep2.upgrade_header("Bad Name", "x").is_err());
        let (ep3, _) = endpoint(&Config::for_tests());
        assert!(ep3.upgrade_header("X", "a\nb").is_err());
    }

    async fn dedicated(ep: Arc<WsEndpoint>, client_bytes: &[u8]) -> (Vec<u8>, tokio::io::DuplexStream) {
        dedicated_from(ep, peer(), client_bytes).await
    }

    /// Serves `client_bytes` from `from` on the dedicated port; returns the answer head.
    async fn dedicated_from(
        ep: Arc<WsEndpoint>,
        from: IpAddr,
        client_bytes: &[u8],
    ) -> (Vec<u8>, tokio::io::DuplexStream) {
        let (server, mut client) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(async move { ep.serve_dedicated(Box::new(server), from).await });
        client.write_all(client_bytes).await.expect("write");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if client.read(&mut byte).await.expect("read") == 0 {
                break;
            }
            head.push(byte[0]);
        }
        if !head.starts_with(b"HTTP/1.1 101 ") {
            let _ = client.shutdown().await;
        }
        let _ = task.await;
        (head, client)
    }

    #[tokio::test]
    async fn the_dedicated_port_answers_101_and_feeds_the_leftover_bytes() {
        let (ep, opened) = endpoint(&Config::for_tests());
        let ep = Arc::new(ep.upgrade_header("Scacelith-Server-Id", "srv-1").expect("valid"));
        let mut bytes = b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
            Sec-WebSocket-Protocol: scacelith.rt1\r\n\r\n"
            .to_vec();
        bytes.extend(frame(2, &[1, 2, 3]));
        let (head, mut client) = dedicated(ep, &bytes).await;
        assert_eq!(
            String::from_utf8(head).expect("ascii"),
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-WebSocket-Protocol: scacelith.rt1\r\n\
             Scacelith-Server-Id: srv-1\r\n\r\n"
        );
        let mut conn = opened.lock().pop().expect("a connection");
        assert_eq!(conn.reader.next().await, WsEvent::Message(Bytes::from_static(&[1, 2, 3])));
        conn.writer.send(&[9]).await.expect("send");
        assert_eq!(read_server_frame(&mut client).await, Some((2, vec![9])));
        drop(client);
        assert_eq!(
            conn.reader.next().await,
            WsEvent::Closed(CloseInfo { code: 1006, reason: String::new() })
        );
    }

    #[tokio::test]
    async fn the_dedicated_port_refuses_bad_heads() {
        let (ep, _) = endpoint(&Config::for_tests());
        let ep = Arc::new(ep);
        let (head, _) = dedicated(ep.clone(), b"GET /ws HTTP/1.1\r\nHost a\r\n\r\n").await;
        assert!(
            head.starts_with(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        let big = format!("GET /ws HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(8200));
        let (head, _) = dedicated(ep.clone(), big.as_bytes()).await;
        assert!(head.starts_with(b"HTTP/1.1 431 Request Header Fields Too Large\r\n"));
        let (head, _) = dedicated(ep, b"POST /ws HTTP/1.1\r\nHost: h\r\n\r\n").await;
        assert!(head.starts_with(b"HTTP/1.1 405 Method Not Allowed\r\n"));
        assert!(String::from_utf8_lossy(&head).contains("Allow: GET\r\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_dedicated_connection_gets_408() {
        let (ep, _) = endpoint(&Config::for_tests());
        let (head, _) = dedicated(Arc::new(ep), b"GET /ws HTTP/1.1\r\n").await;
        assert!(head.starts_with(b"HTTP/1.1 408 Request Timeout\r\n"), "{}", String::from_utf8_lossy(&head));
    }

    /// The refusals of unreadable heads counted toward a block of 127.0.0.1, seen through
    /// `client`; a client leaving before the end of its head is neither refused nor counted.
    async fn counted(client: ClientAddress) -> Vec<(AddrKey, Option<AddrKey>, f64)> {
        let mut c = Config::for_tests();
        c.abuse_block_refusals_per_min = 100_000;
        let guard = Arc::new(IpGuard::new(&c, crate::clock::system(), Logger::root()).expect("guard"));
        let (ep, _) = endpoint(&c);
        let ep = Arc::new(ep.guard(guard.clone()).client_address(client));
        let local = IpAddr::from([127, 0, 0, 1]);
        let (h, _) = dedicated_from(ep.clone(), local, b"GET /ws HTTP/1.1\r\nBad Header\r\n\r\n").await;
        assert!(h.starts_with(b"HTTP/1.1 400 "));
        let big = format!("GET /ws HTTP/1.1\r\nX-Pad: {}\r\n\r\n", "a".repeat(9000));
        let (h, _) = dedicated_from(ep.clone(), local, big.as_bytes()).await;
        assert!(h.starts_with(b"HTTP/1.1 431 "));
        let (h, _) = dedicated_from(ep.clone(), local, b"GET /ws HTTP/1.1\r\nHost: x\r\n").await;
        assert!(h.starts_with(b"HTTP/1.1 408 "));
        let (server, mut client) = tokio::io::duplex(1024);
        let task = tokio::spawn({
            let ep = ep.clone();
            async move { ep.serve_dedicated(Box::new(server), local).await }
        });
        client.write_all(b"GET /ws HTTP/1.1\r\nHo").await.expect("write");
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(client);
        task.await.expect("served");
        guard.flush_reports().into_iter().map(|e| (e.k64, e.k48, e.weight)).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn unreadable_heads_count_toward_a_block_unless_a_trusted_proxy_sent_them() {
        let three = vec![(AddrKey::of(IpAddr::from([127, 0, 0, 1])), None, 3.0)];
        assert_eq!(counted(ClientAddress::direct()).await, three);
        let trusted =
            |list: &[&str]| ClientAddress::behind(crate::net::ip::IpMatcher::new(list).expect("list"));
        assert_eq!(counted(trusted(&["127.0.0.1"])).await, []);
        assert_eq!(counted(trusted(&["10.0.0.1"])).await, three);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_leaving_mid_head_is_no_timeout() {
        let (ep, _) = endpoint(&Config::for_tests());
        let (server, mut client) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move { ep.serve_dedicated(Box::new(server), peer()).await });
        client.write_all(b"GET /ws HTTP/1.1\r\nHo").await.expect("write");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.shutdown().await.expect("shutdown");
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.expect("read");
        assert!(out.is_empty(), "nothing written");
        task.await.expect("served");
    }

    /// What the dedicated port writes and delivers for `pieces` written one by one.
    async fn run_pieces(pieces: &[&[u8]]) -> (String, Vec<Vec<u8>>) {
        let msgs: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = msgs.clone();
        let on_connection: OnConnection = Arc::new(move |mut c: WsConnection| {
            let sink = sink.clone();
            tokio::spawn(async move {
                while let WsEvent::Message(m) = c.reader.next().await {
                    sink.lock().push(m.to_vec());
                }
            });
        });
        let ep = Arc::new(WsEndpoint::new(&Config::for_tests(), settings(), on_connection));
        let (server, mut client) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(async move { ep.serve_dedicated(Box::new(server), peer()).await });
        for p in pieces {
            let _ = client.write_all(p).await;
            tokio::task::yield_now().await;
        }
        let _ = client.shutdown().await;
        let mut out = Vec::new();
        let _ = client.read_to_end(&mut out).await;
        task.await.expect("served");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let delivered = msgs.lock().clone();
        (latin1(&out), delivered)
    }

    #[tokio::test]
    async fn a_head_in_any_pieces_gets_the_same_answer_and_frames() {
        let mut seed: u32 = 7;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff;
            seed % n
        };
        let keys = ["dGhlIHNhbXBsZSBub25jZQ==", "bad", "AAAAAAAAAAAAAAAAAAAAAA=="];
        for t in 0..300 {
            let pad_len = if rnd(4) == 0 { 7900 + rnd(400) } else { rnd(50) };
            let path = if rnd(5) > 0 { "/ws" } else { "/x" };
            let minor = if rnd(6) > 0 { 1 } else { 0 };
            let key = keys[rnd(3) as usize];
            let bad_line = if rnd(10) > 0 { "" } else { "Bad Line\r\n" };
            let mut all = format!(
                "GET {path} HTTP/1.{minor}\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: scacelith.rt1\r\n\
                 X-P: {}\r\n{bad_line}\r\n",
                "p".repeat(pad_len as usize)
            )
            .into_bytes();
            for _ in 0..rnd(3) {
                let payload = [rnd(256) as u8, rnd(256) as u8, rnd(256) as u8];
                all.extend([0x82, 0x83, 1, 2, 3, 4, payload[0] ^ 1, payload[1] ^ 2, payload[2] ^ 3]);
            }
            let mut pieces: Vec<&[u8]> = Vec::new();
            let mut o = 0;
            while o < all.len() {
                let bound = if rnd(2) > 0 { 5 } else { 200 };
                let n = 1 + rnd(bound) as usize;
                pieces.push(&all[o..(o + n).min(all.len())]);
                o += n;
            }
            assert_eq!(run_pieces(&pieces).await, run_pieces(&[&all]).await, "head {t}");
        }
    }
}
