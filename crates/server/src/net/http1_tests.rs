//! Tests of the API connections: the Node listener's answers, timers and counting, through an
//! in-memory stream (the TLS end-to-end tests live in `net::server`).

use std::sync::atomic::AtomicUsize;

use super::*;
use crate::clock::{Clock, ManualClock};
use crate::config::Config;
use crate::http::{Answer, ApiError, RouteOpts, Router};
use crate::log::Logger;
use crate::net::abuse::BlockOrder;
use crate::net::ip::{AddrKey, IpMatcher};
use crate::net::upgrade::WsEndpoint;
use crate::net::ws::tests::{frame, read_server_frame};
use crate::net::ws::{WsConnection, WsEvent, WsSettings};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// What the test routes share with the tests.
#[derive(Default)]
struct Probe {
    /// Requests that reached a handler.
    handled: AtomicUsize,
    /// Permits released by the test let the `/slow` handlers answer.
    release: Option<Arc<Semaphore>>,
}

fn routes(probe: Arc<Probe>) -> Router {
    let mut r = Router::new();
    let p = probe.clone();
    r.get("/echo-ip", RouteOpts::new(), move |ctx| {
        p.handled.fetch_add(1, Ordering::Relaxed);
        async move { Ok(Answer::json(json!({"ip": ctx.ip.to_string()}))) }
    });
    let p = probe.clone();
    r.get("/slow", RouteOpts::new(), move |_ctx| {
        p.handled.fetch_add(1, Ordering::Relaxed);
        let release = p.release.clone();
        async move {
            match release {
                Some(sem) => {
                    let permit = sem.acquire().await.map_err(ApiError::internal)?;
                    permit.forget();
                }
                None => tokio::time::sleep(Duration::from_millis(900)).await,
            }
            Ok(Answer::json(json!({})))
        }
    });
    r.get("/big", RouteOpts::new(), |_ctx| async { Ok(Answer::bytes(vec![b'x'; 4 << 20])) });
    r.post("/upload", RouteOpts::new().body_limit(64), |_ctx| async {
        Ok(Answer::json(json!({"ok": true})))
    });
    r
}

struct Setup {
    edge: Arc<HttpListener>,
    guard: Arc<IpGuard>,
    probe: Arc<Probe>,
}

struct Options {
    env: Box<dyn FnOnce(&mut Config)>,
    client: ClientAddress,
    upgrades: bool,
    timeouts: HttpTimeouts,
    release: bool,
    ready: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            env: Box::new(|_| {}),
            client: ClientAddress::direct(),
            upgrades: true,
            timeouts: HttpTimeouts::default(),
            release: false,
            ready: true,
        }
    }
}

fn echo_ws() -> Arc<dyn Fn(WsConnection) + Send + Sync> {
    Arc::new(|mut c: WsConnection| {
        tokio::spawn(async move {
            while let WsEvent::Message(m) = c.reader.next().await {
                if c.writer.send(&m).await.is_err() {
                    break;
                }
            }
        });
    })
}

fn setup(o: Options) -> Setup {
    let mut c = Config::for_tests();
    c.http_rate_per_ip = 20;
    (o.env)(&mut c);
    let clock = ManualClock::new(1e6, 0);
    let guard = Arc::new(IpGuard::new(&c, clock as Arc<dyn Clock>, Logger::root()).expect("a guard"));
    let probe = Arc::new(Probe {
        handled: AtomicUsize::new(0),
        release: o.release.then(|| Arc::new(Semaphore::new(0))),
    });
    let api = Api::builder(Arc::new(c.clone()), routes(probe.clone())).guard(guard.clone()).build();
    let readiness = Readiness::new();
    readiness.set(o.ready);
    let mut edge = HttpListener::new(api, readiness)
        .guard(guard.clone())
        .client_address(o.client.clone())
        .timeouts(o.timeouts);
    if o.upgrades {
        let settings = WsSettings {
            max_message_bytes: 512,
            close_timeout: Duration::from_millis(500),
            clock: crate::clock::system(),
            log: Logger::root().child("ws-test"),
        };
        let ws = WsEndpoint::new(&c, settings, echo_ws()).guard(guard.clone()).client_address(o.client);
        edge = edge.upgrades(Arc::new(ws));
    }
    Setup { edge: Arc::new(edge), guard, probe }
}

struct Conn {
    io: DuplexStream,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

fn connect(edge: &Arc<HttpListener>, peer: &str) -> Conn {
    connect_with(edge, peer, 1 << 16)
}

fn connect_with(edge: &Arc<HttpListener>, peer: &str, buffer: usize) -> Conn {
    let (io, server) = tokio::io::duplex(buffer);
    let (stop, rx) = watch::channel(false);
    let edge = edge.clone();
    let peer: IpAddr = peer.parse().expect("an address");
    let task = tokio::spawn(async move { edge.serve(server, peer, rx).await });
    Conn { io, stop, task }
}

/// An answer as read from the wire.
#[derive(Debug)]
struct Wire {
    head: String,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Wire {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }

    /// The head with the date masked.
    fn masked(&self) -> String {
        match self.header("date") {
            Some(d) => self.head.replace(d, "<date>"),
            None => self.head.clone(),
        }
    }
}

async fn read_head(io: &mut (impl AsyncRead + Unpin)) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match io.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

/// Reads one answer; `None` at the end of the stream.
async fn read_answer(io: &mut (impl AsyncRead + Unpin), head_request: bool) -> Option<Wire> {
    let head = read_head(io).await?;
    let mut lines = head.trim_end().split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let mut w = Wire { head, status, headers, body: Vec::new() };
    if head_request || status == 101 || status == 204 || status == 100 {
        return Some(w);
    }
    if let Some(len) = w.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        let mut body = vec![0u8; len];
        io.read_exact(&mut body).await.ok()?;
        w.body = body;
    } else if w.header("transfer-encoding") == Some("chunked") {
        let mut end = [0u8; 5];
        io.read_exact(&mut end).await.ok()?;
        assert_eq!(&end, b"0\r\n\r\n", "only empty chunked bodies here");
    }
    Some(w)
}

async fn request(c: &mut Conn, text: &str) -> Wire {
    c.io.write_all(text.as_bytes()).await.expect("write");
    read_answer(&mut c.io, text.starts_with("HEAD ")).await.expect("an answer")
}

async fn get(edge: &Arc<HttpListener>, target: &str) -> Wire {
    let mut c = connect(edge, "127.0.0.1");
    request(&mut c, &format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n")).await
}

/// Reads until the end of the stream (the server closed); the bytes read.
async fn until_closed(io: &mut DuplexStream, within: Duration) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, io.read(&mut buf)).await {
            Err(_) => return None,
            Ok(Ok(0)) | Ok(Err(_)) => return Some(out),
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
        }
    }
}

fn local() -> AddrKey {
    AddrKey::of(IpAddr::from([127, 0, 0, 1]))
}

fn reports(guard: &IpGuard) -> Vec<(AddrKey, f64)> {
    guard.flush_reports().into_iter().map(|e| (e.k64, e.weight)).collect()
}

#[test]
fn imf_fixdate_formats_like_node() {
    assert_eq!(imf_fixdate(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(imf_fixdate(1_791_017_491), "Sat, 03 Oct 2026 08:51:31 GMT");
    assert_eq!(imf_fixdate(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    assert!(expects_continue(&HeaderValue::from_static("100-Continue")));
    assert!(!expects_continue(&HeaderValue::from_static("x100-continue")));
    assert!(!expects_continue(&HeaderValue::from_static("foo")));
}

#[tokio::test]
async fn health_answers_come_before_the_api_with_the_listener_headers() {
    let s = setup(Options::default());
    let w = get(&s.edge, "/api/v1/healthz").await;
    assert_eq!(
        w.masked(),
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nDate: <date>\r\n\
         Connection: keep-alive\r\nKeep-Alive: timeout=5, max=1000\r\n\r\n"
    );
    assert_eq!(w.body, br#"{"status":"ok"}"#);
    assert!(w.header("date").is_some_and(|d| d.ends_with(" GMT") && d.len() == 29));
    assert_eq!(get(&s.edge, "/readyz").await.json(), json!({"status": "ready"}));
    let mut c = connect(&s.edge, "127.0.0.1");
    let h = request(&mut c, "HEAD /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!((h.status, h.header("content-length")), (200, Some("15")));
    let after = request(&mut c, "GET /healthz?x=1 HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(after.body, br#"{"status":"ok"}"#, "no body after the HEAD answer");
    let not_ready = setup(Options { ready: false, ..Options::default() });
    let w = get(&not_ready.edge, "/api/v1/readyz").await;
    assert_eq!((w.status, w.json()), (503, json!({"status": "not_ready"})));
    let w = get(&s.edge, "/api/v1/nothing").await;
    assert_eq!(w.status, 404);
    assert_eq!(w.header("cross-origin-resource-policy"), Some("same-origin"), "an API answer");
}

#[tokio::test]
async fn proxy_mode_reads_the_forwarded_address() {
    let trusted = ClientAddress::behind(IpMatcher::new(&["127.0.0.1"]).expect("a list"));
    let s = setup(Options { client: trusted, ..Options::default() });
    let mut c = connect(&s.edge, "127.0.0.1");
    let w =
        request(&mut c, "GET /api/v1/echo-ip HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 198.51.100.7\r\n\r\n")
            .await;
    assert_eq!(w.json(), json!({"ip": "198.51.100.7"}));
}

#[tokio::test]
async fn every_request_takes_a_token_before_routing() {
    let s = setup(Options::default());
    let tokens = || s.guard.request_tokens(local()).expect("a bucket");
    assert_eq!(get(&s.edge, "/healthz").await.status, 200);
    assert_eq!(tokens(), 9.0);
    assert_eq!(get(&s.edge, "/api/v1/nothing").await.status, 404);
    let mut c = connect(&s.edge, "127.0.0.1");
    request(&mut c, "HEAD /api/v1/info HTTP/1.1\r\nHost: x\r\n\r\n").await;
    request(&mut c, "OPTIONS /api/v1/info HTTP/1.1\r\nHost: x\r\n\r\n").await;
    get(&s.edge, "/verify-email?token=x").await;
    assert_eq!(get(&s.edge, &format!("/{}", "a".repeat(5000))).await.status, 414);
    let up = upgrade_request("scacelith.rt1");
    let w = request(&mut connect(&s.edge, "127.0.0.1"), &up).await;
    assert_eq!(w.status, 101);
    assert_eq!(tokens(), 3.0);
    for _ in 0..3 {
        assert_eq!(get(&s.edge, "/healthz").await.status, 200);
    }
    let w = get(&s.edge, "/healthz").await;
    assert_eq!(w.status, 429);
    assert_eq!(
        w.json(),
        json!({"error": "rate_limited", "message": "Too many requests; try again later.", "retryAfter": 3})
    );
    assert_eq!((w.header("retry-after"), w.header("cache-control")), (Some("3"), Some("no-store")));
    assert_eq!(w.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(w.header("connection"), Some("keep-alive"), "not blocked: the connection stays");
    let w = request(&mut connect(&s.edge, "127.0.0.1"), &up).await;
    assert_eq!((w.status, w.header("retry-after")), (429, Some("3")), "the upgrade too");
    assert_eq!(w.json()["error"], "rate_limited");
}

#[tokio::test]
async fn a_blocked_address_gets_429_with_connection_close() {
    let s = setup(Options { env: Box::new(|c| c.http_rate_per_ip = 600), ..Options::default() });
    let mut c = connect(&s.edge, "127.0.0.1");
    let first = request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!((first.status, first.header("connection")), (200, Some("keep-alive")));
    s.guard.apply_blocks(&[BlockOrder::new(local(), 120_000.0, 2)]);
    let w = request(&mut c, "GET /api/v1/info HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(
        (w.status, w.header("connection"), w.header("retry-after")),
        (429, Some("close"), Some("120"))
    );
    assert_eq!(w.json()["retryAfter"], 120);
    assert!(
        w.masked().ends_with("Retry-After: 120\r\nConnection: close\r\nDate: <date>\r\n\r\n"),
        "{}",
        w.head
    );
    assert!(
        until_closed(&mut c.io, Duration::from_secs(3)).await.is_some(),
        "the server ended the connection"
    );
}

#[tokio::test]
async fn caps_the_requests_in_progress_of_one_address() {
    let s = setup(Options {
        env: Box::new(|c| {
            c.ip_max_inflight = 2;
            c.http_rate_per_ip = 600;
        }),
        release: true,
        ..Options::default()
    });
    let mut a = connect(&s.edge, "127.0.0.1");
    let mut b = connect(&s.edge, "127.0.0.1");
    a.io.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    b.io.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    while s.probe.handled.load(Ordering::Relaxed) < 2 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let w = get(&s.edge, "/api/v1/slow").await;
    assert_eq!((w.status, w.header("retry-after")), (429, Some("1")));
    s.probe.release.as_ref().expect("a semaphore").add_permits(2);
    assert_eq!(read_answer(&mut a.io, false).await.expect("a").status, 200);
    assert_eq!(read_answer(&mut b.io, false).await.expect("b").status, 200);
    while s.guard.inflight_total() > 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    s.probe.release.as_ref().expect("a semaphore").add_permits(1);
    assert_eq!(get(&s.edge, "/api/v1/slow").await.status, 200, "the slots came back");
}

#[tokio::test]
async fn a_request_in_its_handler_keeps_its_slot_after_the_client_left() {
    let s = setup(Options {
        env: Box::new(|c| {
            c.ip_max_inflight = 1;
            c.http_rate_per_ip = 600;
        }),
        release: true,
        ..Options::default()
    });
    let mut a = connect(&s.edge, "127.0.0.1");
    a.io.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    while s.probe.handled.load(Ordering::Relaxed) < 1 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    drop(a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(s.guard.inflight_total(), 1, "held while the handler works");
    assert_eq!(get(&s.edge, "/api/v1/slow").await.status, 429);
    s.probe.release.as_ref().expect("a semaphore").add_permits(1);
    while s.guard.inflight_total() > 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn slowloris_is_cut_at_the_head_timeout_and_counted() {
    let timeouts = HttpTimeouts { head: Duration::from_secs(1), ..HttpTimeouts::default() };
    let s = setup(Options { timeouts, ..Options::default() });
    let mut c = connect(&s.edge, "127.0.0.1");
    let t0 = Instant::now();
    c.io.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: x\r\n").await.expect("write");
    let writer = {
        let (mut r, mut w) = tokio::io::split(c.io);
        let task = tokio::spawn(async move {
            for i in 0..20 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if w.write_all(format!("X-Slow-{i}: 1\r\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            let _ = w.write_all(b"\r\n").await;
        });
        let mut answer = Vec::new();
        let _ = r.read_to_end(&mut answer).await;
        task.abort();
        answer
    };
    let ms = t0.elapsed().as_millis();
    assert_eq!(writer, ClientError::Timeout.raw_answer());
    assert!((1000..2300).contains(&ms), "closed after {ms} ms");
    assert_eq!(s.probe.handled.load(Ordering::Relaxed), 0, "never reaches the API");
    assert_eq!(s.edge.client_errors(ClientError::Timeout), 1);
    assert_eq!(reports(&s.guard), [(local(), 1.0)]);
    c.task.await.expect("served");
}

#[tokio::test(start_paused = true)]
async fn a_silent_connection_gets_408_at_ten_seconds() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    let t0 = Instant::now();
    let answer = until_closed(&mut c.io, Duration::from_secs(20)).await.expect("closed");
    assert_eq!(answer, ClientError::Timeout.raw_answer());
    assert_eq!(t0.elapsed().as_secs(), 10);
}

#[tokio::test]
async fn an_aborted_request_is_not_counted_and_a_malformed_one_counts_once() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    c.io.write_all(b"GET /api/v1/x HTTP/1.1\r\nHost: x\r\n").await.expect("write");
    c.io.shutdown().await.expect("FIN");
    assert_eq!(until_closed(&mut c.io, Duration::from_secs(3)).await, Some(Vec::new()));
    c.task.await.expect("served");
    assert_eq!(s.edge.client_errors(ClientError::Malformed), 0);
    assert!(reports(&s.guard).is_empty());
    let mut m = connect(&s.edge, "127.0.0.1");
    m.io.write_all(b"BLAH\r\n\r\n").await.expect("write");
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = m.io.write_all(b"more\r\n").await;
    }
    let answer = until_closed(&mut m.io, Duration::from_secs(3)).await.expect("closed");
    assert_eq!(answer, ClientError::Malformed.raw_answer());
    assert_eq!(s.edge.client_errors(ClientError::Malformed), 1);
    assert_eq!(reports(&s.guard), [(local(), 1.0)]);
}

#[tokio::test]
async fn malformed_http_counts_toward_a_block_of_the_client_not_of_a_trusted_proxy() {
    for (client, counted) in [
        (ClientAddress::direct(), true),
        (ClientAddress::behind(IpMatcher::new(&["127.0.0.1"]).expect("a list")), false),
    ] {
        let s = setup(Options { client, ..Options::default() });
        let mut c = connect(&s.edge, "127.0.0.1");
        c.io.write_all(b"BLAH\r\n\r\n").await.expect("write");
        let answer = until_closed(&mut c.io, Duration::from_secs(3)).await.expect("closed");
        assert_eq!(answer, ClientError::Malformed.raw_answer());
        assert_eq!(reports(&s.guard).len(), usize::from(counted));
    }
}

#[tokio::test]
async fn unknown_methods_and_heads_node_refuses_get_raw_answers() {
    let s = setup(Options::default());
    for (text, e) in [
        ("FOO / HTTP/1.1\r\nHost: x\r\n\r\n", ClientError::Malformed),
        ("get / HTTP/1.1\r\nHost: x\r\n\r\n", ClientError::Malformed),
        ("GET / HTTP/1.2\r\nHost: x\r\n\r\n", ClientError::Malformed),
    ] {
        let mut c = connect(&s.edge, "127.0.0.1");
        c.io.write_all(text.as_bytes()).await.expect("write");
        let answer = until_closed(&mut c.io, Duration::from_secs(3)).await.expect("closed");
        assert_eq!(String::from_utf8_lossy(&answer), String::from_utf8_lossy(e.raw_answer()), "{text:?}");
    }
    let mut c = connect(&s.edge, "127.0.0.1");
    let long = format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(8300));
    c.io.write_all(long.as_bytes()).await.expect("write");
    let answer = until_closed(&mut c.io, Duration::from_secs(3)).await.expect("closed");
    assert_eq!(answer, ClientError::TooLarge.raw_answer());
    let mut c = connect(&s.edge, "127.0.0.1");
    let headers: String = (0..90).map(|i| format!("X-{i:02}: {}\r\n", "v".repeat(95))).collect();
    c.io.write_all(format!("GET / HTTP/1.1\r\nHost: x\r\n{headers}\r\n").as_bytes()).await.expect("write");
    let answer = until_closed(&mut c.io, Duration::from_secs(3)).await.expect("closed");
    assert_eq!(answer, ClientError::TooLarge.raw_answer(), "names + values over 8192 bytes");
    assert_eq!(s.edge.client_errors(ClientError::TooLarge), 2);
}

#[tokio::test]
async fn a_head_of_more_than_64_header_lines_gets_the_raw_431() {
    let s = setup(Options::default());
    let head = |lines: usize, target: &str, extra: &str| {
        let more: String = (1..lines).map(|i| format!("Cookie: c{i}=1\r\n")).collect();
        format!("GET {target} HTTP/1.1\r\nHost: x\r\n{extra}{more}\r\n")
    };
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, &head(64, "/healthz", "")).await;
    assert_eq!(w.status, 200, "64 header lines, the Host line included");
    for (text, what) in [
        (head(65, "/healthz", ""), "65 header lines"),
        (head(100, "/api/v1/x", ""), "100 header lines (hyper's own limit is 100)"),
        (
            head(
                61,
                "/ws",
                "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
            ),
            "a WebSocket upgrade with 65 header lines",
        ),
    ] {
        let mut c = connect(&s.edge, "127.0.0.1");
        c.io.write_all(text.as_bytes()).await.expect("write");
        let answer = until_closed(&mut c.io, Duration::from_secs(3)).await.expect("closed");
        assert_eq!(
            String::from_utf8_lossy(&answer),
            String::from_utf8_lossy(ClientError::TooLarge.raw_answer()),
            "{what}"
        );
    }
    assert_eq!(s.edge.client_errors(ClientError::TooLarge), 3);
    assert_eq!(reports(&s.guard), [(local(), 3.0)], "each one counts toward a block");
}

#[tokio::test]
async fn node_answers_missing_host_and_unknown_expect_itself() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, "GET /healthz HTTP/1.1\r\n\r\n").await;
    assert_eq!(
        w.masked(),
        "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nDate: <date>\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    assert!(until_closed(&mut c.io, Duration::from_secs(3)).await.is_some());
    assert_eq!(s.edge.client_errors(ClientError::Malformed), 0, "not counted");
    assert_eq!(s.guard.request_tokens(local()), None, "before the protection per address");
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\nExpect: something\r\n\r\n").await;
    assert_eq!(
        w.masked(),
        "HTTP/1.1 417 Expectation Failed\r\nDate: <date>\r\nConnection: keep-alive\r\n\
         Keep-Alive: timeout=5, max=1000\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    let w = request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(w.status, 200, "the connection goes on");
    c.io.write_all(b"POST /api/v1/upload HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n")
        .await
        .expect("write");
    let cont = read_answer(&mut c.io, false).await.expect("100");
    assert_eq!(cont.head, "HTTP/1.1 100 Continue\r\n\r\n");
    c.io.write_all(b"{}").await.expect("write");
    assert_eq!(read_answer(&mut c.io, false).await.expect("answer").status, 200);
}

#[tokio::test]
async fn connect_closes_without_an_answer() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    c.io.write_all(b"CONNECT example.org:443 HTTP/1.1\r\nHost: example.org:443\r\n\r\n")
        .await
        .expect("write");
    assert_eq!(until_closed(&mut c.io, Duration::from_secs(3)).await, Some(Vec::new()));
}

#[tokio::test]
async fn the_thousandth_request_closes_the_connection() {
    let s = setup(Options { env: Box::new(|c| c.http_rate_per_ip = 100_000), ..Options::default() });
    let mut c = connect(&s.edge, "127.0.0.1");
    for i in 1..=1000 {
        let w = request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await;
        let expected = if i < 1000 { "keep-alive" } else { "close" };
        assert_eq!(w.header("connection"), Some(expected), "request {i}");
        assert_eq!(w.header("keep-alive").is_some(), i < 1000);
    }
    assert!(until_closed(&mut c.io, Duration::from_secs(3)).await.is_some());
}

#[tokio::test]
async fn keep_alive_follows_the_request() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert_eq!((w.header("connection"), w.header("keep-alive")), (Some("close"), None));
    assert!(until_closed(&mut c.io, Duration::from_secs(3)).await.is_some());
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, "GET /healthz HTTP/1.0\r\n\r\n").await;
    assert_eq!(w.header("connection"), Some("close"), "HTTP/1.0 closes by default");
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, "GET /healthz HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").await;
    assert_eq!(w.header("connection"), Some("keep-alive"));
    assert_eq!(request(&mut c, "GET /healthz HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").await.status, 200);
}

#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    c.io.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\nGET /api/v1/nothing HTTP/1.1\r\nHost: x\r\n\r\nGET /readyz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let statuses: Vec<u16> = [
        read_answer(&mut c.io, false).await,
        read_answer(&mut c.io, false).await,
        read_answer(&mut c.io, false).await,
    ]
    .into_iter()
    .map(|w| w.expect("an answer").status)
    .collect();
    assert_eq!(statuses, [200, 404, 200]);
}

#[tokio::test(start_paused = true)]
async fn an_idle_kept_alive_connection_closes_quietly_after_six_seconds() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    assert_eq!(request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await.status, 200);
    let t0 = Instant::now();
    assert_eq!(until_closed(&mut c.io, Duration::from_secs(20)).await, Some(Vec::new()));
    assert_eq!(t0.elapsed().as_secs(), 6);
    assert_eq!(s.edge.client_errors(ClientError::Timeout), 0);
}

#[tokio::test(start_paused = true)]
async fn a_request_on_a_kept_alive_connection_has_its_own_head_timer() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await.status, 200);
    tokio::time::sleep(Duration::from_secs(5)).await;
    c.io.write_all(b"GET /healthz HTTP/1.1\r\n").await.expect("write");
    let t0 = Instant::now();
    let answer = until_closed(&mut c.io, Duration::from_secs(20)).await.expect("closed");
    assert_eq!(answer, ClientError::Timeout.raw_answer());
    assert_eq!(t0.elapsed().as_secs(), 10, "10 s from the first byte of the request");
}

#[tokio::test(start_paused = true)]
async fn a_client_that_stops_reading_is_cut_and_a_slow_reader_meets_the_send_deadline() {
    let timeouts = HttpTimeouts {
        idle: Duration::from_millis(500),
        send: Duration::from_millis(1500),
        ..HttpTimeouts::default()
    };
    let s = setup(Options { env: Box::new(|c| c.http_rate_per_ip = 600), timeouts, ..Options::default() });
    // Never reads: nothing moves, the inactivity timeout ends it.
    let mut idle = connect_with(&s.edge, "127.0.0.1", 16 * 1024);
    idle.io.write_all(b"GET /api/v1/big HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    let t0 = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), idle.task).await.expect("cut").expect("served");
    let ms = t0.elapsed().as_millis();
    assert!((450..1000).contains(&ms), "idle reader cut after {ms} ms");
    // Reads a little every 50 ms: only the send deadline ends it.
    let mut slow = connect_with(&s.edge, "127.0.0.1", 16 * 1024);
    slow.io.write_all(b"GET /api/v1/big HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    let t0 = Instant::now();
    let mut got = 0usize;
    let mut buf = [0u8; 1024];
    loop {
        match slow.io.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ms = t0.elapsed().as_millis();
    assert!(ms >= 1400, "not before the deadline ({ms} ms)");
    assert!(got > 0 && got < 4 << 20, "cut before the end ({got} bytes)");
}

#[tokio::test(start_paused = true)]
async fn a_handler_at_work_keeps_its_socket_past_the_inactivity_timeout() {
    let timeouts = HttpTimeouts { idle: Duration::from_millis(300), ..HttpTimeouts::default() };
    let s = setup(Options { timeouts, ..Options::default() });
    let t0 = Instant::now();
    let w = get(&s.edge, "/api/v1/slow").await;
    assert_eq!((w.status, w.body.as_slice()), (200, b"{}".as_slice()));
    assert!(t0.elapsed() >= Duration::from_millis(850));
}

#[tokio::test]
async fn a_close_error_closes_after_its_answer_while_reading_the_rest() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    c.io.write_all(b"POST /api/v1/upload HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n")
        .await
        .expect("write");
    let w = read_answer(&mut c.io, false).await.expect("an answer");
    assert_eq!((w.status, w.header("connection")), (413, Some("close")));
    // The server keeps reading what the client still sends, then closes.
    for _ in 0..10 {
        if c.io.write_all(&[b'a'; 4096]).await.is_err() {
            break;
        }
    }
    assert!(until_closed(&mut c.io, Duration::from_secs(5)).await.is_some());
}

fn upgrade_request(protocol: &str) -> String {
    format!(
        "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: {protocol}\r\n\r\n"
    )
}

#[tokio::test]
async fn the_shared_port_upgrades_and_refuses_with_json() {
    let s = setup(Options::default());
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, &upgrade_request("scacelith.rt1")).await;
    assert_eq!(
        w.head,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-Websocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-Websocket-Protocol: scacelith.rt1\r\n\r\n"
    );
    c.io.write_all(&frame(2, &[1, 2, 3])).await.expect("write");
    assert_eq!(read_server_frame(&mut c.io).await, Some((2, vec![1, 2, 3])));
    let mut r = connect(&s.edge, "127.0.0.1");
    let w = request(&mut r, &upgrade_request("scacelith.v1")).await;
    assert_eq!(
        w.head,
        "HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\nCache-Control: no-store\r\n\
         Content-Type: application/json\r\nContent-Length: 62\r\n\r\n"
    );
    assert_eq!(w.json(), json!({"error": "unsupported_protocol", "supported": ["scacelith.rt1"]}));
    assert!(until_closed(&mut r.io, Duration::from_secs(3)).await.is_some());
}

#[tokio::test]
async fn an_api_only_port_answers_upgrades_as_api_requests() {
    let s = setup(Options { upgrades: false, ..Options::default() });
    let mut c = connect(&s.edge, "127.0.0.1");
    let w = request(&mut c, &upgrade_request("scacelith.rt1")).await;
    assert_eq!(w.status, 404);
}

#[tokio::test]
async fn proxy_mode_never_closes_the_proxy_connection_on_a_block() {
    let trusted = ClientAddress::behind(IpMatcher::new(&["127.0.0.1"]).expect("a list"));
    let s =
        setup(Options { env: Box::new(|c| c.http_rate_per_ip = 600), client: trusted, ..Options::default() });
    s.guard.apply_blocks(&[BlockOrder::new(AddrKey::of("198.51.100.7".parse().expect("ip")), 60_000.0, 1)]);
    let mut c = connect(&s.edge, "127.0.0.1");
    let w =
        request(&mut c, "GET /api/v1/x HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 198.51.100.7\r\n\r\n").await;
    assert_eq!((w.status, w.header("connection")), (429, Some("keep-alive")));
    let other =
        request(&mut c, "GET /healthz HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 198.51.100.8\r\n\r\n").await;
    assert_eq!(other.status, 200, "the same proxy connection serves the next client");
    let tokens = s.guard.request_tokens(AddrKey::of("198.51.100.8".parse().expect("ip")));
    assert_eq!(tokens, Some(299.0), "counted for the forwarded client");
}

#[tokio::test]
async fn a_shutdown_lets_the_request_in_progress_finish_and_closes() {
    let s = setup(Options { release: true, ..Options::default() });
    // An idle connection closes at once.
    let mut idle = connect(&s.edge, "127.0.0.1");
    assert_eq!(request(&mut idle, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await.status, 200);
    // A connection with a request in its handler gets its answer, then closes.
    let mut busy = connect(&s.edge, "127.0.0.1");
    busy.io.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    while s.probe.handled.load(Ordering::Relaxed) < 1 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    s.edge.set_draining();
    let _ = idle.stop.send(true);
    let _ = busy.stop.send(true);
    assert_eq!(until_closed(&mut idle.io, Duration::from_secs(3)).await, Some(Vec::new()));
    s.probe.release.as_ref().expect("a semaphore").add_permits(1);
    let w = read_answer(&mut busy.io, false).await.expect("the answer");
    assert_eq!((w.status, w.header("connection")), (200, Some("close")));
    assert!(until_closed(&mut busy.io, Duration::from_secs(3)).await.is_some());
}
