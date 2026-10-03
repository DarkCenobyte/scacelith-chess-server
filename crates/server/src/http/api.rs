//! The API request pipeline (DESIGN 5.9, docs/API.md 1): request target checks, the health
//! paths, routing, authentication, the account budget, route rates, query and body validation,
//! the handler under its timeout, and the answer with its headers.
//!
//! The listener (`net`) admits each request first (blocks, per-address rates, in-flight slots)
//! and answers `GET`/`HEAD` of the health paths itself; [`Api::handle`] does everything after.

use std::borrow::Cow;
use std::fmt::Display;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use hyper::body::Body;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::answer::{Answer, ApiError, Payload};
use super::body::{BODY_TIMEOUT, parse_body, read_body};
use super::ctx::{AuthInfo, Authenticator, Ctx, DynAuthenticator, NoAuthenticator};
use super::json;
use super::rates::{RateLimits, UserBudget};
use super::router::{AuthMode, CloseHook, Route, RouteMatch, Router, allow_header};
use super::schema::Schema;
use super::url::parse_urlencoded;
use crate::clock::{self, SharedClock};
use crate::config::{Config, TlsMode};
use crate::log::Logger;
use crate::metrics::{self, CounterVec, Histogram};
use crate::net::guard::{AddressKeys, IpGuard};
use crate::net::health::Readiness;
use crate::net::ip::for_log;
use crate::net::limits::SharedLimits;
use crate::{log_debug, log_error};

/// The longest request target (bytes).
pub const MAX_URL: usize = 4096;
/// How long a handler may take unless its route says otherwise.
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);
/// The content security policy of JSON, text and binary answers.
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";
/// The content security policy of HTML pages.
pub const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
/// `Strict-Transport-Security` when the server terminates TLS itself.
pub const HSTS: &str = "max-age=31536000";

/// Methods whose body is read.
const BODY_METHODS: [Method; 4] = [Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

/// Renders the HTML error page of a page route: `(title, message) -> page`. Implemented by the
/// pages module ("Request refused" or "Server error", the error's message).
pub type PageRenderer = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

struct HttpMetrics {
    requests: CounterVec,
    duration: Histogram,
}

fn http_metrics() -> &'static HttpMetrics {
    static M: LazyLock<HttpMetrics> = LazyLock::new(|| HttpMetrics {
        requests: metrics::counter_vec("scacelith_http_requests_total", "API requests", &["route", "status"]),
        duration: metrics::histogram(
            "scacelith_http_request_duration_ms",
            "API request duration",
            &[2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0],
        ),
    });
    &M
}

/// Escapes text for HTML (`& < > " '`).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The page used until the pages module provides its layout.
fn fallback_page(title: &str, message: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>{}</title></head>\
         <body><main><h1>{}</h1><p class=\"error\">{}</p></main></body></html>\n",
        escape_html(title),
        escape_html(title),
        escape_html(message)
    )
}

/// Builds an [`Api`].
pub struct ApiBuilder {
    config: Arc<Config>,
    router: Router,
    clock: SharedClock,
    log: Logger,
    auth: Arc<dyn DynAuthenticator>,
    shared: Option<Arc<SharedLimits>>,
    guard: Option<Arc<IpGuard>>,
    ready: Readiness,
    page_renderer: PageRenderer,
    body_timeout: Duration,
    handler_timeout: Duration,
}

impl ApiBuilder {
    /// The clock of the rate limits and of `Ctx::now_ms` (default: the system clock).
    pub fn clock(mut self, clock: SharedClock) -> ApiBuilder {
        self.clock = clock;
        self
    }

    /// The logger (default: `http`).
    pub fn logger(mut self, log: Logger) -> ApiBuilder {
        self.log = log;
        self
    }

    /// The token validator (default: every token refused).
    pub fn authenticator(mut self, auth: impl Authenticator) -> ApiBuilder {
        self.auth = Arc::new(auth);
        self
    }

    /// The shared windows of the `shared` rates (default: a new set on the API's clock).
    pub fn shared_limits(mut self, shared: Arc<SharedLimits>) -> ApiBuilder {
        self.shared = Some(shared);
        self
    }

    /// The protection per address that counts the refusals of address-keyed limits.
    pub fn guard(mut self, guard: Arc<IpGuard>) -> ApiBuilder {
        self.guard = Some(guard);
        self
    }

    /// The readiness flag of `/readyz` (default: always ready).
    pub fn readiness(mut self, ready: Readiness) -> ApiBuilder {
        self.ready = ready;
        self
    }

    /// The HTML error page of page routes.
    pub fn page_renderer(
        mut self,
        render: impl Fn(&str, &str) -> String + Send + Sync + 'static,
    ) -> ApiBuilder {
        self.page_renderer = Arc::new(render);
        self
    }

    /// How long reading a body may take (default 10 s).
    pub fn body_timeout(mut self, timeout: Duration) -> ApiBuilder {
        self.body_timeout = timeout;
        self
    }

    /// How long a handler may take unless its route says otherwise (default 30 s).
    pub fn handler_timeout(mut self, timeout: Duration) -> ApiBuilder {
        self.handler_timeout = timeout;
        self
    }

    /// The API.
    pub fn build(mut self) -> Arc<Api> {
        let shared = self.shared.unwrap_or_else(|| Arc::new(SharedLimits::new(self.clock.clone())));
        let close_hooks = Mutex::new(self.router.take_close_hooks());
        Arc::new(Api {
            hsts: self.config.tls_mode == TlsMode::Native,
            rates: Arc::new(RateLimits::new(self.clock.clone(), shared)),
            budget: UserBudget::new(&self.config, self.clock.clone()),
            router: self.router,
            close_hooks,
            config: self.config,
            clock: self.clock,
            log: self.log,
            auth: self.auth,
            guard: self.guard,
            ready: self.ready,
            page_renderer: self.page_renderer,
            body_timeout: self.body_timeout,
            handler_timeout: self.handler_timeout,
        })
    }
}

/// The API: the routes and everything the pipeline needs. Shared by every connection.
pub struct Api {
    router: Router,
    close_hooks: Mutex<Vec<CloseHook>>,
    config: Arc<Config>,
    clock: SharedClock,
    log: Logger,
    auth: Arc<dyn DynAuthenticator>,
    rates: Arc<RateLimits>,
    budget: UserBudget,
    guard: Option<Arc<IpGuard>>,
    ready: Readiness,
    page_renderer: PageRenderer,
    body_timeout: Duration,
    handler_timeout: Duration,
    hsts: bool,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").field("routes", &self.router.routes().len()).finish()
    }
}

/// What the pipeline learnt before a failure: the metric label, whether errors render as HTML,
/// and the rate tokens to give back on a refunded error.
struct Progress {
    label: Cow<'static, str>,
    page: bool,
    taken: Option<Arc<Mutex<Vec<super::rates::Taken>>>>,
}

/// An answer before it becomes a response.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn into_response(self, head: bool) -> Response<Bytes> {
        let body = if head || self.status == StatusCode::NO_CONTENT { Bytes::new() } else { self.body };
        let mut res = Response::new(body);
        *res.status_mut() = self.status;
        *res.headers_mut() = self.headers;
        res
    }
}

fn header_pair(name: &str, value: &str) -> Option<(HeaderName, HeaderValue)> {
    Some((HeaderName::from_bytes(name.as_bytes()).ok()?, HeaderValue::from_str(value).ok()?))
}

/// `String(v)` for the `Retry-After` header.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map(json::js_number).unwrap_or_else(|| n.to_string()),
        other => json::stringify(other),
    }
}

/// JavaScript truthiness.
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The query parameters: every name once, its first value.
fn parse_query(search: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in parse_urlencoded(search) {
        out.entry(k).or_insert(Value::String(v));
    }
    out
}

impl Api {
    /// Starts building the API of `config` with `router`'s routes.
    pub fn builder(config: Arc<Config>, router: Router) -> ApiBuilder {
        ApiBuilder {
            config,
            router,
            clock: clock::system(),
            log: Logger::root().child("http"),
            auth: Arc::new(NoAuthenticator),
            shared: None,
            guard: None,
            ready: Readiness::ready(),
            page_renderer: Arc::new(fallback_page),
            body_timeout: BODY_TIMEOUT,
            handler_timeout: HANDLER_TIMEOUT,
        }
    }

    /// The routes.
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// The route limiter (tests, diagnostics).
    pub fn rates(&self) -> &Arc<RateLimits> {
        &self.rates
    }

    /// The configuration.
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// Runs the close hooks of the route modules once (the server's shutdown).
    pub async fn close(&self) {
        let hooks = std::mem::take(&mut *self.close_hooks.lock());
        for hook in hooks {
            hook().await;
        }
    }

    /// Answers one request from `client` (already admitted by the listener). The work runs to its
    /// end even when the caller stops polling: the caller spawns this future.
    pub async fn handle<B>(self: Arc<Self>, req: Request<B>, client: AddressKeys) -> Response<Bytes>
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Display,
    {
        let started = Instant::now();
        let head = req.method() == Method::HEAD;
        let mut progress = Progress { label: Cow::Borrowed("unmatched"), page: false, taken: None };
        let reply = match self.run(req, &client, &mut progress).await {
            Ok(reply) => reply,
            Err(err) => self.failure(err, &progress, &client),
        };
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let m = http_metrics();
        m.requests.with(&[&progress.label, reply.status.as_str()]).inc();
        m.duration.observe(ms);
        log_debug!(self.log, "request", {
            "route": progress.label, "status": reply.status.as_u16(), "ms": ms.round() as u64, "ip": for_log(client.ip),
        });
        reply.into_response(head)
    }

    fn base_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::with_capacity(12);
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        h.insert(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        );
        if self.hsts {
            h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static(HSTS));
        }
        h
    }

    /// The pipeline up to the answer; an error goes to [`Api::failure`].
    async fn run<B>(
        &self,
        req: Request<B>,
        client: &AddressKeys,
        progress: &mut Progress,
    ) -> Result<Reply, ApiError>
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Display,
    {
        let (parts, body) = req.into_parts();
        let method = parts.method;
        let raw: Cow<str> = match parts.uri.path_and_query() {
            Some(pq) if parts.uri.scheme().is_none() => Cow::Borrowed(pq.as_str()),
            _ => Cow::Owned(parts.uri.to_string()),
        };
        if raw.len() > MAX_URL {
            return Err(ApiError::new(414, "uri_too_long", "The URL is too long."));
        }
        if !raw.starts_with('/') || raw.starts_with("//") {
            return Err(ApiError::invalid_request("Invalid request target."));
        }
        let (pathname, search) = raw.split_once('?').unwrap_or((&raw, ""));

        let health = match pathname {
            "/healthz" | "/api/v1/healthz" => Some("healthz"),
            "/readyz" | "/api/v1/readyz" => Some("readyz"),
            _ => None,
        };
        if let Some(which) = health {
            progress.label = Cow::Borrowed(which);
            if method != Method::GET && method != Method::HEAD {
                return Err(method_not_allowed("GET, HEAD"));
            }
            let (status, body) = match which {
                "healthz" => (200, json!({"status": "ok"})),
                _ if self.ready.is_ready() => (200, json!({"status": "ready"})),
                _ => (503, json!({"status": "not_ready"})),
            };
            return self.answer(Answer::json(body).status(status));
        }

        let (route, params) = match self.router.find(&method, pathname) {
            RouteMatch::NotFound => return Err(ApiError::not_found("No such endpoint.")),
            RouteMatch::MalformedPath => return Err(ApiError::invalid_request("malformed path")),
            RouteMatch::OtherMethods(methods) => {
                let allow = allow_header(&methods);
                if method == Method::OPTIONS {
                    return self.answer(Answer::no_content().header("Allow", allow));
                }
                return Err(method_not_allowed(&allow));
            }
            RouteMatch::Found(route, params) => (route, params),
        };
        progress.label = Cow::Owned(route.label().to_string());
        progress.page = route.opts().page;
        let opts = route.opts();

        let auth = self.authenticate(&parts.headers, opts.auth).await?;
        if let Some(a) = &auth {
            self.budget.take(a.user_id)?;
        }
        let taken = Arc::new(Mutex::new(Vec::new()));
        progress.taken = Some(taken.clone());
        let user = auth.as_ref().map(|a| a.user_id);
        let route_taken = self.rates.check(&opts.rates, client, user)?;
        taken.lock().extend(route_taken);

        let mut query = parse_query(search);
        if let Some(schema) = &opts.query {
            match schema.validate(&Value::Object(query)) {
                Ok(valid) => query = if let Value::Object(m) = valid { m } else { Map::new() },
                Err(e) => {
                    let field = e.field.map(Value::String).unwrap_or(Value::Null);
                    return Err(ApiError::invalid_request(e.message).with_extra("field", field));
                }
            }
        }

        let mut body_value = Value::Object(Map::new());
        if BODY_METHODS.contains(&method) {
            let limit =
                opts.body_limit.unwrap_or_else(|| usize::try_from(self.config.http_body_limit).unwrap_or(0));
            let buf =
                read_body(body, parts.headers.get(header::CONTENT_LENGTH), limit, self.body_timeout).await?;
            let parsed = parse_body(&buf, parts.headers.get(header::CONTENT_TYPE), opts.page)?;
            body_value =
                if opts.own_body_validation { parsed } else { validate_body(opts.body.as_ref(), &parsed)? };
        } else {
            drop(body);
        }

        let ctx = Ctx {
            ip: client.ip,
            method: if method == Method::HEAD { Method::GET } else { method },
            headers: parts.headers,
            params,
            query,
            body: body_value,
            auth,
            now_ms: self.clock.wall_ms(),
            route: route.path().to_string(),
            keys: *client,
            rates: self.rates.clone(),
            taken: taken.clone(),
        };
        let answer = self.run_handler(route, ctx).await?;
        if answer.refund_rate {
            self.rates.give_back(&taken.lock());
        }
        self.answer(answer)
    }

    /// Reads the bearer token of a route that wants one.
    async fn authenticate(&self, headers: &HeaderMap, mode: AuthMode) -> Result<Option<AuthInfo>, ApiError> {
        if mode == AuthMode::None {
            return Ok(None);
        }
        let value = headers.get(header::AUTHORIZATION).map(HeaderValue::as_bytes).filter(|v| !v.is_empty());
        let Some(value) = value else {
            if mode == AuthMode::Required {
                return Err(ApiError::new(401, "unauthorized", "Log in first.")
                    .with_header("WWW-Authenticate", "Bearer realm=\"scacelith\""));
            }
            return Ok(None);
        };
        let token = value
            .strip_prefix(b"Bearer ")
            .filter(|t| (1..=512).contains(&t.len()) && t.iter().all(|&c| (0x21..=0x7e).contains(&c)))
            .and_then(|t| std::str::from_utf8(t).ok());
        let info = match token {
            Some(token) => self.auth.validate(token).await?,
            None => None,
        };
        info.map(Some).ok_or_else(|| {
            ApiError::new(401, "invalid_token", "The session is invalid or has expired; log in again.")
                .with_header("WWW-Authenticate", "Bearer realm=\"scacelith\", error=\"invalid_token\"")
        })
    }

    /// Runs the handler on its own task under the route's timeout. A late handler keeps running
    /// (its side effects happen) and its answer is dropped.
    async fn run_handler(&self, route: &Route, ctx: Ctx) -> Result<Answer, ApiError> {
        let timeout = route.opts().timeout.unwrap_or(self.handler_timeout);
        let task = tokio::spawn((route.handler())(ctx));
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(join)) => Err(ApiError::internal(format!("handler panicked: {join}"))),
            Err(_) => Err(ApiError::new(503, "timeout", "The server took too long to answer; try again.")),
        }
    }

    /// The answer of a handler, an OPTIONS or a health check.
    fn answer(&self, answer: Answer) -> Result<Reply, ApiError> {
        let mut headers = self.base_headers();
        let status = StatusCode::from_u16(answer.status)
            .map_err(|_| ApiError::internal(format!("invalid answer status {}", answer.status)))?;
        let no_content = status == StatusCode::NO_CONTENT;
        let csp = HeaderValue::from_static(API_CSP);
        let mut body = Bytes::new();
        let content_type = |ct: &Option<String>, default: &'static str| match ct {
            Some(ct) => HeaderValue::from_str(ct)
                .map_err(|_| ApiError::internal(format!("invalid content type {ct:?}"))),
            None => Ok(HeaderValue::from_static(default)),
        };
        match answer.payload {
            Payload::Html(html) => {
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_CSP));
                body = Bytes::from(html);
            }
            Payload::Text(text) => {
                headers.insert(header::CONTENT_SECURITY_POLICY, csp);
                if !no_content {
                    headers.insert(
                        header::CONTENT_TYPE,
                        content_type(&answer.content_type, "text/plain; charset=utf-8")?,
                    );
                    body = Bytes::from(text);
                }
            }
            Payload::Bytes(bytes) => {
                headers.insert(header::CONTENT_SECURITY_POLICY, csp);
                if !no_content {
                    headers.insert(
                        header::CONTENT_TYPE,
                        content_type(&answer.content_type, "application/octet-stream")?,
                    );
                    body = bytes;
                }
            }
            Payload::Json(value) => {
                headers.insert(header::CONTENT_SECURITY_POLICY, csp);
                if !no_content {
                    headers.insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json; charset=utf-8"),
                    );
                    body = Bytes::from(json::to_vec(&value));
                }
            }
            Payload::Empty | Payload::NoContent => {
                headers.insert(header::CONTENT_SECURITY_POLICY, csp);
            }
        }
        for (name, value) in &answer.headers {
            let (n, v) = header_pair(name, value)
                .ok_or_else(|| ApiError::internal(format!("invalid answer header {name:?}")))?;
            headers.insert(n, v);
        }
        if !no_content {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        }
        Ok(Reply { status, headers, body })
    }

    /// The answer of an error: the client's address is counted toward a block when the error
    /// says so, internal errors are logged and hidden, page routes get an HTML page.
    fn failure(&self, err: ApiError, progress: &Progress, client: &AddressKeys) -> Reply {
        if !err.is_exposed() {
            log_error!(self.log, "request failed", {
                "route": progress.label, "err": {"message": err.internal.as_deref().unwrap_or("")},
            });
        }
        if err.refund_rate
            && let Some(taken) = &progress.taken
        {
            self.rates.give_back(&taken.lock());
        }
        if err.abuse_weight > 0.0
            && let Some(guard) = &self.guard
        {
            guard.note_refusal(client, err.abuse_weight);
        }
        let close = err.close_connection;
        let err = if err.is_exposed() && StatusCode::from_u16(err.status).is_ok() {
            err
        } else {
            ApiError::new(500, "internal_error", "Internal server error.")
        };
        let mut headers: Vec<(String, String)> = err.headers.clone();
        if let Some(r) = err.extra.get("retryAfter").filter(|v| js_truthy(v)) {
            headers.push(("Retry-After".into(), js_string(r)));
        }
        if close {
            headers.push(("Connection".into(), "close".into()));
        }
        let mut answer = if progress.page {
            let title = if err.status >= 500 { "Server error" } else { "Request refused" };
            Answer::html((self.page_renderer)(title, &err.message))
        } else {
            Answer::json(err.body())
        };
        answer.status = err.status;
        answer.headers = headers.into_iter().filter(|(k, v)| header_pair(k, v).is_some()).collect();
        self.answer(answer).unwrap_or_else(|e| {
            log_error!(self.log, "error answer failed", {"route": progress.label, "err": {"message": e.to_string()}});
            Reply { status: StatusCode::INTERNAL_SERVER_ERROR, headers: self.base_headers(), body: Bytes::new() }
        })
    }
}

fn method_not_allowed(allow: &str) -> ApiError {
    ApiError::new(405, "method_not_allowed", "Method not allowed.").with_header("Allow", allow)
}

/// Validates a body against the route's schema (none: only `{}` passes).
fn validate_body(schema: Option<&Schema>, parsed: &Value) -> Result<Value, ApiError> {
    static EMPTY: LazyLock<Schema> = LazyLock::new(Schema::new);
    schema.unwrap_or(&EMPTY).validate(parsed).map_err(|e| {
        let err = ApiError::invalid_request(e.message);
        match e.field.filter(|f| !f.is_empty()) {
            Some(f) => err.with_extra("field", Value::String(f)),
            None => err,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            escape_html("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(js_string(&json!(6)), "6");
        assert_eq!(js_string(&json!(1.5)), "1.5");
        assert!(
            js_truthy(&json!(1))
                && !js_truthy(&json!(0))
                && !js_truthy(&json!(""))
                && !js_truthy(&Value::Null)
        );
        let q = parse_query("a=1&a=2&b");
        assert_eq!(Value::Object(q), json!({"a": "1", "b": ""}));
    }
}
