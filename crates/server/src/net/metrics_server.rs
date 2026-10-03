//! The metrics and health endpoint: plain HTTP on `METRICS_BIND:METRICS_PORT` (keep it private;
//! port 0 disables it). `GET /metrics` renders the registry (Prometheus text 0.0.4), `/healthz`
//! answers while the process runs, `/readyz` while the server is ready and not shutting down.
//! With `METRICS_TOKEN`, `/metrics` needs `Authorization: Bearer <token>` with that exact text; the
//! bearer and the token are compared as SHA-256 digests in constant time, whatever their lengths.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::health::Readiness;
use super::listener::{self, Acceptor, ListenError};
use crate::config::Config;
use crate::log::Logger;
use crate::log_error;

/// Time a client has to send a request head (Node's `headersTimeout` of this server).
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// Content type of the plain answers.
const TEXT: &str = "text/plain; charset=utf-8";
/// Content type of `/metrics`.
const PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Renders the metrics text.
pub type RenderFn = Arc<dyn Fn() -> String + Send + Sync>;

fn sha256(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// The token of a `Bearer` authorization (`^Bearer\s+(\S+)$`, case-insensitive scheme).
fn bearer_of(value: &HeaderValue) -> Option<&str> {
    let text = value.to_str().ok()?;
    let scheme = text.get(..6)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = &text[6..];
    let token = rest.trim_start_matches(char::is_whitespace);
    if token.len() == rest.len() || token.is_empty() || token.contains(char::is_whitespace) {
        return None;
    }
    Some(token)
}

fn text(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    answer(status, TEXT, Bytes::from_static(body.as_bytes()))
}

fn answer(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(Bytes::new()));
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    *res.body_mut() = Full::new(body);
    res
}

/// The answers of the endpoint.
pub struct MetricsEndpoint {
    token: Option<[u8; 32]>,
    readiness: Readiness,
    render: RenderFn,
    log: Logger,
}

impl std::fmt::Debug for MetricsEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsEndpoint").field("token", &self.token.is_some()).finish_non_exhaustive()
    }
}

impl MetricsEndpoint {
    /// The endpoint of `config` (`METRICS_TOKEN`, trimmed: a value from the environment keeps its
    /// spaces, and a bearer holds none), answering `/readyz` from `readiness` and `/metrics` from
    /// the global registry.
    pub fn new(config: &Config, readiness: Readiness, log: Logger) -> MetricsEndpoint {
        let token = config.metrics_token.as_ref().map(|t| t.as_str().trim()).filter(|t| !t.is_empty());
        MetricsEndpoint {
            token: token.map(sha256),
            readiness,
            render: Arc::new(|| crate::metrics::registry().render()),
            log,
        }
    }

    /// Renders `/metrics` with `render` instead of the global registry.
    pub fn render_with(mut self, render: impl Fn() -> String + Send + Sync + 'static) -> MetricsEndpoint {
        self.render = Arc::new(render);
        self
    }

    fn authorized(&self, authorization: Option<&HeaderValue>) -> bool {
        let Some(token) = &self.token else { return true };
        match authorization.and_then(bearer_of) {
            Some(bearer) => bool::from(sha256(bearer).ct_eq(token)),
            None => false,
        }
    }

    /// The answer to `method` on `uri` (the query is ignored; an absolute-form target matches no
    /// path, as in Node). Every answer has `Content-Type`, `Content-Length` and
    /// `Cache-Control: no-store`.
    pub fn answer(
        &self,
        method: &Method,
        uri: &Uri,
        authorization: Option<&HeaderValue>,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET && method != Method::HEAD {
            let mut res = text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
            res.headers_mut().insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return res;
        }
        let path = if uri.authority().is_some() { "" } else { uri.path() };
        match path {
            "/healthz" => text(StatusCode::OK, "ok\n"),
            "/readyz" if self.readiness.is_ready() => text(StatusCode::OK, "ready\n"),
            "/readyz" => text(StatusCode::SERVICE_UNAVAILABLE, "not ready\n"),
            "/metrics" if !self.authorized(authorization) => {
                let mut res = text(StatusCode::UNAUTHORIZED, "unauthorized\n");
                res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                res
            }
            "/metrics" => match std::panic::catch_unwind(AssertUnwindSafe(|| (self.render)())) {
                Ok(body) => answer(StatusCode::OK, PROMETHEUS, Bytes::from(body)),
                Err(_) => {
                    log_error!(self.log, "metrics collection failed");
                    text(StatusCode::INTERNAL_SERVER_ERROR, "metrics unavailable\n")
                }
            },
            _ => text(StatusCode::NOT_FOUND, "not found\n"),
        }
    }

    /// Serves the endpoint on `listener` until `shutdown` turns true, then lets the requests in
    /// progress finish and returns.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, mut shutdown: watch::Receiver<bool>) {
        let mut acceptor = Acceptor::new(listener, self.log.clone());
        let mut conns = JoinSet::new();
        loop {
            let (stream, _) = tokio::select! {
                accepted = acceptor.accept() => accepted,
                _ = shutdown.wait_for(|stop| *stop) => break,
            };
            while conns.try_join_next().is_some() {}
            let endpoint = self.clone();
            let mut stop = shutdown.clone();
            conns.spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let res =
                        endpoint.answer(req.method(), req.uri(), req.headers().get(header::AUTHORIZATION));
                    async move { Ok::<_, Infallible>(res) }
                });
                let conn = http1::Builder::new()
                    .title_case_headers(true)
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT)
                    .serve_connection(TokioIo::new(stream), service);
                tokio::pin!(conn);
                tokio::select! {
                    _ = conn.as_mut() => return,
                    _ = stop.wait_for(|stop| *stop) => {}
                }
                conn.as_mut().graceful_shutdown();
                let _ = conn.await;
            });
        }
        drop(acceptor);
        while conns.join_next().await.is_some() {}
    }
}

/// Opens the listening socket of the endpoint: `None` when `METRICS_PORT` is 0 (disabled).
pub fn bind(config: &Config) -> Result<Option<TcpListener>, ListenError> {
    if config.metrics_port == 0 {
        return Ok(None);
    }
    let text = config.metrics_bind.trim();
    let bare = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
    let port = config.metrics_port;
    let ip: IpAddr = bare.parse().map_err(|_| {
        let e = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("METRICS_BIND is not an IP address: {text}"),
        );
        ListenError::new(SocketAddr::from(([0, 0, 0, 0], port)), e)
    })?;
    listener::bind(ip, port, listener::backlog_of(config)).map(Some)
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;
    use crate::config::SecretText;

    fn endpoint(token: Option<&str>) -> MetricsEndpoint {
        let mut c = Config::for_tests();
        c.metrics_token = token.map(|t| SecretText::new(t.to_string()));
        MetricsEndpoint::new(&c, Readiness::ready(), Logger::root()).render_with(|| "# metrics\n".to_string())
    }

    fn status(e: &MetricsEndpoint, method: &str, path: &str, bearer: Option<&str>) -> u16 {
        let auth = bearer.map(|b| HeaderValue::from_str(&format!("Bearer {b}")).expect("header"));
        let method = Method::from_bytes(method.as_bytes()).expect("method");
        e.answer(&method, &path.parse().expect("uri"), auth.as_ref()).status().as_u16()
    }

    async fn body(res: Response<Full<Bytes>>) -> String {
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[test]
    fn only_the_exact_token_text_is_accepted() {
        let e = endpoint(Some("hunter2"));
        assert_eq!(status(&e, "GET", "/metrics", Some("hunter2")), 200);
        assert_eq!(status(&e, "HEAD", "/metrics", Some("hunter2")), 200);
        for b in ["hunter2!", "hunter2?", "hunter3", "hunter"] {
            assert_eq!(status(&e, "GET", "/metrics", Some(b)), 401, "{b}");
        }
        assert_eq!(status(&e, "GET", "/metrics", None), 401);
        assert_eq!(status(&e, "GET", "/healthz", None), 200, "health needs no token");
    }

    #[test]
    fn a_hex_token_is_compared_as_written() {
        let hex = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        let e = endpoint(Some(hex));
        assert_eq!(status(&e, "GET", "/metrics", Some(hex)), 200);
        assert_eq!(
            status(&e, "GET", "/metrics", Some("obLD1OX2BxgpOktcbX6PkA==")),
            401,
            "the bytes in base64"
        );
        assert_eq!(status(&e, "GET", "/metrics", Some(&hex.to_uppercase())), 401);
    }

    #[test]
    fn a_token_without_base64_characters_does_not_open_the_endpoint() {
        let e = endpoint(Some("!!!!!!!!"));
        assert_eq!(status(&e, "GET", "/metrics", Some(".")), 401);
        assert_eq!(status(&e, "GET", "/metrics", Some("!!!!!!!!")), 200);
    }

    #[test]
    fn the_token_is_trimmed_and_the_scheme_is_case_insensitive() {
        let e = endpoint(Some("  s3cret \n"));
        let get = |value: &str| {
            let v = HeaderValue::from_str(value).expect("header");
            e.answer(&Method::GET, &Uri::from_static("/metrics"), Some(&v)).status().as_u16()
        };
        assert_eq!(get("Bearer s3cret"), 200);
        assert_eq!(get("bearer \t s3cret"), 200);
        assert_eq!(get("BEARER s3cret"), 200);
        assert_eq!(get("Bearers3cret"), 401);
        assert_eq!(get("Bearer s3cret extra"), 401);
        assert_eq!(get("Basic s3cret"), 401);
        assert_eq!(get("Bearer"), 401);
        assert_eq!(
            endpoint(Some("   ")).answer(&Method::GET, &Uri::from_static("/metrics"), None).status(),
            200
        );
    }

    #[tokio::test]
    async fn without_a_token_metrics_are_open_and_other_methods_get_405() {
        let e = endpoint(None);
        let res = e.answer(&Method::GET, &Uri::from_static("/metrics?x=1"), None);
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], PROMETHEUS);
        assert_eq!(body(res).await, "# metrics\n");
        let res = e.answer(&Method::POST, &Uri::from_static("/metrics"), None);
        assert_eq!(res.status(), 405);
        assert_eq!(res.headers()["allow"], "GET, HEAD");
        assert_eq!(body(res).await, "method not allowed\n");
    }

    #[tokio::test]
    async fn health_readiness_and_unknown_paths() {
        let mut c = Config::for_tests();
        c.metrics_token = None;
        let ready = Readiness::new();
        let e = MetricsEndpoint::new(&c, ready.clone(), Logger::root());
        let get = |path: &'static str| e.answer(&Method::GET, &Uri::from_static(path), None);
        assert_eq!(body(get("/healthz")).await, "ok\n");
        let res = get("/readyz");
        assert_eq!(res.status(), 503);
        assert_eq!(body(res).await, "not ready\n");
        ready.set(true);
        assert_eq!(body(get("/readyz")).await, "ready\n");
        let res = get("/metrics/");
        assert_eq!(res.status(), 404);
        assert_eq!(body(res).await, "not found\n");
        assert_eq!(get("http://x/metrics").status(), 404, "absolute form");
        let res = get("/metrics");
        assert_eq!(res.status(), 200);
        let names: Vec<&str> = res.headers().keys().map(|k| k.as_str()).collect();
        assert_eq!(names, ["content-type", "content-length", "cache-control"]);
        assert_eq!(res.headers()["cache-control"], "no-store");
    }

    #[tokio::test]
    async fn a_failed_collection_answers_500() {
        let e = endpoint(None).render_with(|| panic!("no registry"));
        let res = e.answer(&Method::GET, &Uri::from_static("/metrics"), None);
        assert_eq!(res.status(), 500);
        assert_eq!(body(res).await, "metrics unavailable\n");
    }

    #[test]
    fn port_zero_disables_the_endpoint() {
        let mut c = Config::for_tests();
        c.metrics_port = 0;
        assert!(bind(&c).expect("no error").is_none());
    }

    async fn exchange(at: SocketAddr, request: &str) -> String {
        let mut s = TcpStream::connect(at).await.expect("connected");
        s.write_all(request.as_bytes()).await.expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("read");
        String::from_utf8(out).expect("utf-8")
    }

    #[tokio::test]
    async fn serves_over_tcp_and_stops_on_shutdown() {
        let listener = listener::bind(IpAddr::from([127, 0, 0, 1]), 0, 16).expect("bound");
        let at = listener.local_addr().expect("address");
        let (stop, rx) = watch::channel(false);
        let server = tokio::spawn(Arc::new(endpoint(Some("t0k"))).serve(listener, rx));
        let answer = exchange(at, "GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
        assert!(answer.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{answer}");
        assert!(answer.contains("\r\nWww-Authenticate: Bearer\r\n"), "{answer}");
        assert!(answer.ends_with("\r\n\r\nunauthorized\n"), "{answer}");
        let answer = exchange(
            at,
            "HEAD /metrics HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t0k\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.contains("\r\nContent-Length: 10\r\n"), "{answer}");
        assert!(answer.ends_with("\r\n\r\n"), "no body for HEAD: {answer}");
        let mut idle = TcpStream::connect(at).await.expect("connected");
        idle.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
        let mut buf = [0u8; 512];
        let n = idle.read(&mut buf).await.expect("read");
        assert!(buf[..n].ends_with(b"ok\n"));
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), server).await.expect("stopped").expect("no panic");
        assert_eq!(idle.read(&mut buf).await.expect("eof"), 0, "the idle connection is closed");
        assert!(TcpStream::connect(at).await.is_err(), "no longer listening");
    }
}
