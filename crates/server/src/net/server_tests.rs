//! End-to-end tests of the server: real sockets, a real TLS client (tokio-rustls), test
//! certificates; the gate, the certificate reload, the HTTP answers and the WebSocket framing.

use std::sync::atomic::{AtomicBool, Ordering};

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::*;
use crate::config::TlsMinVersion;
use crate::http::{Answer, ApiError, RouteOpts, Router};
use crate::net::tls::ALPN_HTTP11;
use crate::net::tls::tests::{TempDir, TestCert, client_config, test_cert, test_client};
use crate::net::ws::tests::{frame, read_server_frame};
use crate::net::ws::{WsConnection, WsEvent, WsSettings};

const WAIT: Duration = Duration::from_secs(10);

fn routes(release: Arc<Semaphore>) -> Router {
    let mut r = Router::new();
    r.get("/hello", RouteOpts::new(), |ctx| async move {
        Ok(Answer::json(json!({"hello": "world", "ip": ctx.ip.to_string()})))
    });
    r.get("/slow", RouteOpts::new(), move |_ctx| {
        let release = release.clone();
        async move {
            release.acquire().await.map_err(ApiError::internal)?.forget();
            Ok(Answer::json(json!({"slow": true})))
        }
    });
    r
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

/// A configuration for the tests: loopback, ephemeral ports, generous rates.
fn base_config() -> Config {
    let mut c = Config::for_tests();
    c.bind_address = "127.0.0.1".to_string();
    c.api_port = 0;
    c.ws_port = 0;
    c.metrics_port = 0;
    c.ip_conn_rate = 100_000;
    c.http_rate_per_ip = 100_000;
    c.http_rate_per_prefix = 100_000;
    c.shutdown_grace_ms = 3000;
    c
}

/// A native-TLS configuration serving `cert` from files in `dir`.
fn native_config(dir: &TempDir, cert: &TestCert) -> Config {
    let mut c = base_config();
    c.tls_mode = TlsMode::Native;
    c.tls_cert_file = dir.write("server.crt", &cert.cert_pem).display().to_string();
    c.tls_key_file = dir.write("server.key", &cert.key_pem).display().to_string();
    c
}

/// A port nobody listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bound").local_addr().expect("address").port()
}

struct Running {
    addrs: Vec<(ListenerKind, SocketAddr)>,
    handle: ServerHandle,
    task: JoinHandle<()>,
    release: Arc<Semaphore>,
    readiness: Readiness,
}

impl Running {
    fn addr(&self, kind: ListenerKind) -> SocketAddr {
        self.addrs.iter().find(|(k, _)| *k == kind).map(|(_, a)| *a).expect("a listener of that kind")
    }

    fn api(&self) -> SocketAddr {
        self.addrs
            .iter()
            .find(|(k, _)| matches!(k, ListenerKind::Api | ListenerKind::ApiWs))
            .map(|(_, a)| *a)
            .expect("an API listener")
    }

    async fn stop(self) {
        self.handle.shutdown();
        tokio::time::timeout(WAIT, self.task).await.expect("drained").expect("no panic");
    }
}

fn parts(config: &Config, full: Option<FullSignal>) -> (ServerParts, Arc<Semaphore>) {
    let release = Arc::new(Semaphore::new(0));
    let guard = Arc::new(IpGuard::new(config, crate::clock::system(), Logger::root()).expect("a guard"));
    let api = Api::builder(Arc::new(config.clone()), routes(release.clone())).guard(guard.clone()).build();
    let settings = WsSettings {
        max_message_bytes: 1024,
        close_timeout: Duration::from_millis(500),
        clock: crate::clock::system(),
        log: Logger::root().child("ws-test"),
    };
    let readiness = Readiness::new();
    readiness.set(true);
    let ws = WsEndpoint::new(config, settings, echo_ws());
    (ServerParts { api, ws, guard, readiness, full }, release)
}

async fn start_with(config: &Config, full: Option<FullSignal>) -> Running {
    let (parts, release) = parts(config, full);
    let readiness = parts.readiness.clone();
    let server = Server::bind(config, parts, Logger::root().child("server-test")).await.expect("bound");
    let addrs = server.addresses();
    let handle = server.handle();
    let task = tokio::spawn(server.run());
    Running { addrs, handle, task, release, readiness }
}

async fn start(config: &Config) -> Running {
    start_with(config, None).await
}

async fn tls_connect(addr: SocketAddr, client: Arc<ClientConfig>) -> io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost").expect("a name");
    tokio::time::timeout(WAIT, TlsConnector::from(client).connect(name, tcp))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

/// Reads until the peer closes (an abrupt end counts as the end).
async fn read_all<R: AsyncRead + Unpin>(r: &mut R) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(WAIT, r.read(&mut buf)).await.expect("the peer answers") {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads one answer head (up to the blank line).
async fn read_head<R: AsyncRead + Unpin>(r: &mut R) -> String {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while !out.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(WAIT, r.read(&mut byte)).await.expect("a head").expect("read");
        assert_eq!(n, 1, "closed in the head: {:?}", String::from_utf8_lossy(&out));
        out.push(byte[0]);
    }
    String::from_utf8(out).expect("utf-8")
}

/// One request with `Connection: close`; the whole answer.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(io: &mut S, path: &str) -> String {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    io.write_all(req.as_bytes()).await.expect("write");
    read_all(io).await
}

fn body_json(answer: &str) -> Value {
    let (_, body) = answer.split_once("\r\n\r\n").expect("a head and a body");
    serde_json::from_str(body).expect("JSON")
}

fn upgrade_request() -> &'static str {
    "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
     Sec-WebSocket-Protocol: scacelith.rt1\r\n\r\n"
}

/// Whether the server reset the connection (or closed it) without a TLS handshake.
async fn refused_before_tls(addr: SocketAddr) -> bool {
    tls_connect(addr, test_client()).await.is_err()
}

#[tokio::test]
async fn https_serves_the_api_with_hsts_and_the_listener_headers() {
    let dir = TempDir::new("srv-https");
    let config = native_config(&dir, &test_cert("localhost"));
    let s = start(&config).await;
    let mut c = tls_connect(s.api(), test_client()).await.expect("handshake");
    assert_eq!(c.get_ref().1.alpn_protocol(), Some(ALPN_HTTP11));
    let answer = exchange(&mut c, "/api/v1/hello").await;
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.contains("\r\nStrict-Transport-Security: max-age=31536000\r\n"), "{answer}");
    assert!(answer.contains("\r\nConnection: close\r\n"), "{answer}");
    assert_eq!(body_json(&answer), json!({"hello": "world", "ip": "127.0.0.1"}));
    let mut c = tls_connect(s.api(), test_client()).await.expect("handshake");
    let answer = exchange(&mut c, "/api/v1/healthz").await;
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.contains("\r\nStrict-Transport-Security: max-age=31536000\r\n"), "{answer}");
    s.stop().await;
}

#[tokio::test]
async fn alpn_and_the_minimum_version_follow_the_configuration() {
    let dir = TempDir::new("srv-alpn");
    let mut config = native_config(&dir, &test_cert("localhost"));
    let s = start(&config).await;
    let both = [&rustls::version::TLS13, &rustls::version::TLS12];
    assert!(tls_connect(s.api(), client_config(&both, &[b"h2"])).await.is_err(), "h2 only: no protocol");
    let c = tls_connect(s.api(), client_config(&both, &[])).await.expect("no ALPN is fine");
    assert_eq!(c.get_ref().1.alpn_protocol(), None);
    let c = tls_connect(s.api(), client_config(&[&rustls::version::TLS12], &[ALPN_HTTP11])).await;
    assert_eq!(c.expect("TLS 1.2").get_ref().1.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_2));
    s.stop().await;

    config.tls_min_version = TlsMinVersion::Tls13;
    let s = start(&config).await;
    let old = client_config(&[&rustls::version::TLS12], &[ALPN_HTTP11]);
    assert!(tls_connect(s.api(), old).await.is_err(), "TLS 1.2 refused");
    let c = tls_connect(s.api(), test_client()).await.expect("TLS 1.3");
    assert_eq!(c.get_ref().1.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));
    s.stop().await;
}

#[tokio::test]
async fn a_certificate_reload_reaches_new_connections() {
    let dir = TempDir::new("srv-reload");
    let (first, second) = (test_cert("localhost"), test_cert("localhost"));
    let config = native_config(&dir, &first);
    let s = start(&config).await;
    let peer_cert =
        |c: &TlsStream<TcpStream>| c.get_ref().1.peer_certificates().expect("a chain")[0].to_vec();
    let mut old = tls_connect(s.api(), test_client()).await.expect("handshake");
    assert_eq!(peer_cert(&old), first.der);

    dir.write("server.crt", &second.cert_pem);
    dir.write("server.key", &first.key_pem);
    assert!(!s.handle.reload_certificates().await, "a key that does not match is refused");
    let c = tls_connect(s.api(), test_client()).await.expect("handshake");
    assert_eq!(peer_cert(&c), first.der, "the current certificate is kept");

    dir.write("server.key", &second.key_pem);
    assert!(s.handle.reload_certificates().await);
    let c = tls_connect(s.api(), test_client()).await.expect("handshake");
    assert_eq!(peer_cert(&c), second.der);
    let answer = exchange(&mut old, "/api/v1/hello").await;
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "an open connection goes on: {answer}");
    s.stop().await;
}

#[tokio::test]
async fn tls_sessions_resume() {
    let dir = TempDir::new("srv-resume");
    let config = native_config(&dir, &test_cert("localhost"));
    let s = start(&config).await;
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let client = client_config(&[version], &[ALPN_HTTP11]);
        let mut c = tls_connect(s.api(), client.clone()).await.expect("handshake");
        assert_eq!(c.get_ref().1.handshake_kind(), Some(rustls::HandshakeKind::Full));
        exchange(&mut c, "/api/v1/hello").await;
        let c = tls_connect(s.api(), client).await.expect("handshake");
        assert_eq!(c.get_ref().1.handshake_kind(), Some(rustls::HandshakeKind::Resumed), "{version:?}");
    }
    s.stop().await;
}

#[tokio::test]
async fn websocket_frames_cross_tls_on_the_shared_port() {
    let dir = TempDir::new("srv-ws");
    let config = native_config(&dir, &test_cert("localhost"));
    let s = start(&config).await;
    assert_eq!(s.addrs.len(), 1, "one port for the API and the upgrade");
    let mut c = tls_connect(s.addr(ListenerKind::ApiWs), test_client()).await.expect("handshake");
    c.write_all(upgrade_request().as_bytes()).await.expect("write");
    let head = read_head(&mut c).await;
    assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{head}");
    assert!(head.contains("\r\nSec-Websocket-Protocol: scacelith.rt1\r\n"), "{head}");
    c.write_all(&frame(2, &[1, 2, 3])).await.expect("write");
    assert_eq!(read_server_frame(&mut c).await, Some((2, vec![1, 2, 3])));
    let big: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    c.write_all(&frame(2, &big)).await.expect("write");
    assert_eq!(read_server_frame(&mut c).await, Some((2, big)));
    c.write_all(&frame(8, &1000u16.to_be_bytes())).await.expect("write");
    assert_eq!(read_server_frame(&mut c).await.map(|f| f.0), Some(8), "the close is answered");
    s.stop().await;
}

#[tokio::test]
async fn the_dedicated_port_is_shed_while_full_and_the_api_port_never() {
    let dir = TempDir::new("srv-shed");
    let mut config = native_config(&dir, &test_cert("localhost"));
    config.ws_port = free_port();
    config.max_pending_handshakes = 2; // one new connection per second while full
    let full = Arc::new(AtomicBool::new(false));
    let signal = full.clone();
    let s = start_with(&config, Some(Arc::new(move || signal.load(Ordering::Relaxed)))).await;
    let (api, ws) = (s.addr(ListenerKind::Api), s.addr(ListenerKind::Ws));

    let mut c = tls_connect(ws, test_client()).await.expect("handshake");
    c.write_all(upgrade_request().as_bytes()).await.expect("write");
    let head = read_head(&mut c).await;
    assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{head}");
    c.write_all(&frame(2, b"dedicated")).await.expect("write");
    assert_eq!(read_server_frame(&mut c).await, Some((2, b"dedicated".to_vec())));
    let mut c = tls_connect(api, test_client()).await.expect("handshake");
    c.write_all(upgrade_request().as_bytes()).await.expect("write");
    assert_eq!(
        read_head(&mut c).await.split("\r\n").next(),
        Some("HTTP/1.1 404 Not Found"),
        "no upgrade there"
    );

    full.store(true, Ordering::Relaxed);
    let mut shed = 0;
    for _ in 0..4 {
        if refused_before_tls(ws).await {
            shed += 1;
        }
    }
    assert!(shed >= 2, "shed {shed} of 4");
    for _ in 0..6 {
        let mut c = tls_connect(api, test_client()).await.expect("the API port is never shed");
        assert!(exchange(&mut c, "/api/v1/hello").await.starts_with("HTTP/1.1 200 OK\r\n"));
    }
    s.stop().await;
}

#[tokio::test]
async fn a_peer_over_its_connection_cap_is_refused_before_tls() {
    let dir = TempDir::new("srv-cap");
    let mut config = native_config(&dir, &test_cert("localhost"));
    config.ip_max_connections = 1;
    let s = start(&config).await;
    let mut held = tls_connect(s.api(), test_client()).await.expect("handshake");
    assert!(refused_before_tls(s.api()).await, "a second connection of the address");
    held.write_all(upgrade_request().as_bytes()).await.expect("write");
    assert!(read_head(&mut held).await.starts_with("HTTP/1.1 101 "));
    assert!(refused_before_tls(s.api()).await, "the WebSocket still counts");
    drop(held);
    let mut ok = false;
    for _ in 0..50 {
        if let Ok(mut c) = tls_connect(s.api(), test_client()).await {
            ok = exchange(&mut c, "/api/v1/hello").await.starts_with("HTTP/1.1 200 OK\r\n");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "the place is given back when the connection ends");
    s.stop().await;
}

#[tokio::test]
async fn shutdown_drains_requests_in_progress_and_closes_the_ports() {
    let dir = TempDir::new("srv-drain");
    let mut config = native_config(&dir, &test_cert("localhost"));
    config.metrics_port = free_port();
    let s = start(&config).await;
    let (api, metrics) = (s.api(), s.addr(ListenerKind::Metrics));
    let mut slow = tls_connect(api, test_client()).await.expect("handshake");
    slow.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: localhost\r\n\r\n").await.expect("write");
    let mut idle = tls_connect(api, test_client()).await.expect("handshake");
    idle.write_all(b"GET /api/v1/hello HTTP/1.1\r\nHost: localhost\r\n\r\n").await.expect("write");
    assert!(read_head(&mut idle).await.contains("\r\nConnection: keep-alive\r\n"));
    tokio::time::sleep(Duration::from_millis(100)).await;

    s.handle.shutdown();
    assert!(s.handle.is_shutting_down());
    assert!(!s.readiness.is_ready());
    let mut closed = false;
    for _ in 0..50 {
        if TcpStream::connect(api).await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "the API port no longer accepts");
    let mut m = TcpStream::connect(metrics).await.expect("metrics still up while draining");
    let answer = exchange(&mut m, "/readyz").await;
    assert!(answer.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{answer}");
    let rest = read_all(&mut idle).await;
    assert!(rest.ends_with("\"127.0.0.1\"}"), "the idle connection closes after its answer: {rest}");

    s.release.add_permits(1);
    let answer = read_all(&mut slow).await;
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.contains("\r\nConnection: close\r\n"), "{answer}");
    tokio::time::timeout(WAIT, s.task).await.expect("drained").expect("no panic");
    assert!(TcpStream::connect(metrics).await.is_err(), "metrics stop after the drain");
}

#[tokio::test]
async fn the_grace_period_bounds_the_drain() {
    let mut config = base_config();
    config.shutdown_grace_ms = 300;
    let s = start(&config).await;
    let mut slow = TcpStream::connect(s.api()).await.expect("connected");
    slow.write_all(b"GET /api/v1/slow HTTP/1.1\r\nHost: localhost\r\n\r\n").await.expect("write");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = std::time::Instant::now();
    s.handle.shutdown();
    tokio::time::timeout(WAIT, s.task).await.expect("drained").expect("no panic");
    let ms = t0.elapsed().as_millis();
    assert!((250..2000).contains(&ms), "drained after {ms} ms");
    assert_eq!(read_all(&mut slow).await, "", "dropped without an answer");
}

#[tokio::test]
async fn plain_mode_serves_http_and_a_dedicated_websocket_port() {
    let mut config = base_config();
    config.ws_port = free_port();
    let s = start(&config).await;
    let mut c = TcpStream::connect(s.addr(ListenerKind::Api)).await.expect("connected");
    let answer = exchange(&mut c, "/api/v1/hello").await;
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(!answer.contains("Strict-Transport-Security"), "{answer}");
    let mut w = TcpStream::connect(s.addr(ListenerKind::Ws)).await.expect("connected");
    w.write_all(upgrade_request().as_bytes()).await.expect("write");
    let head = read_head(&mut w).await;
    assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{head}");
    w.write_all(&frame(2, b"plain")).await.expect("write");
    assert_eq!(read_server_frame(&mut w).await, Some((2, b"plain".to_vec())));
    s.handle.shutdown();
    let mut late = TcpStream::connect(s.addr(ListenerKind::Ws)).await;
    if let Ok(late) = late.as_mut() {
        // Accepted before the port closed: the refusal of a draining server.
        late.write_all(upgrade_request().as_bytes()).await.expect("write");
        let answer = read_all(late).await;
        assert!(answer.is_empty() || answer.starts_with("HTTP/1.1 503 "), "{answer}");
    }
    tokio::time::timeout(WAIT, s.task).await.expect("drained").expect("no panic");
}

#[tokio::test]
async fn bind_reports_a_taken_port_and_a_broken_certificate() {
    let mut config = base_config();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
    config.api_port = taken.local_addr().expect("address").port();
    config.ws_port = config.api_port;
    let (p, _) = parts(&config, None);
    let err = Server::bind(&config, p, Logger::root()).await.expect_err("the port is taken");
    assert!(matches!(&err, ServerError::Listen(e) if e.code == Some("EADDRINUSE")), "{err}");

    let dir = TempDir::new("srv-broken");
    let mut config = native_config(&dir, &test_cert("localhost"));
    config.tls_key_file = dir.0.join("missing.key").display().to_string();
    let (p, _) = parts(&config, None);
    let err = Server::bind(&config, p, Logger::root()).await.expect_err("no key");
    assert!(matches!(err, ServerError::Certificate(_)), "{err}");
    assert!(err.to_string().starts_with("TLS certificate: "), "{err}");
}
