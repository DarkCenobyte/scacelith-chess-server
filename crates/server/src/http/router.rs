//! The router of the API (DESIGN 5.9, docs/API.md 1.2): registration in order, matching on the
//! raw path, literal segments over parameters, `Allow` lists, strictly decoded parameters.
//!
//! Paths: a path that starts with `/api/` is taken as it is; any other path is relative to
//! `/api/v1` (`/players/:username` is `/api/v1/players/:username`), except page routes
//! ([`Router::page`]), absolute paths outside `/api` (`/verify-email`). Parameters are `:name`
//! segments.
//!
//! ```ignore
//! router.get("/players/:username", RouteOpts::new().auth(AuthMode::Optional)
//!     .rate(RateSpec::new("public_read", 60.0, 60_000).by_user()), |ctx: Ctx| async move {
//!     Ok(Answer::json(json!({ "username": ctx.param("username") })))
//! });
//! ```

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use http::Method;

use super::answer::{Answer, ApiError};
use super::ctx::Ctx;
use super::rates::RateSpec;
use super::schema::Schema;
use super::url::decode_uri_component;

/// The prefix of the API paths.
pub const API_PREFIX: &str = "/api/v1";

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A route handler.
pub type Handler = Arc<dyn Fn(Ctx) -> BoxFuture<Result<Answer, ApiError>> + Send + Sync>;

/// Whether a route reads the `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    /// The header is not read at all.
    #[default]
    None,
    /// A valid session is used when present; an invalid one is refused.
    Optional,
    /// A valid session is required (401 `unauthorized` without one).
    Required,
}

/// The options of a route.
#[derive(Clone, Default, Debug)]
pub struct RouteOpts {
    /// Authentication mode.
    pub auth: AuthMode,
    /// Rates taken in order, all or none, after authentication.
    pub rates: Vec<RateSpec>,
    /// Body schema (none: only `{}` or an empty body passes).
    pub body: Option<Schema>,
    /// The parsed body (any JSON value) reaches the handler unchecked.
    pub own_body_validation: bool,
    /// Query schema (none: the query strings as they are).
    pub query: Option<Schema>,
    /// Body limit in bytes (none: `HTTP_BODY_LIMIT`).
    pub body_limit: Option<usize>,
    /// Handler timeout (none: 30 s).
    pub timeout: Option<Duration>,
    /// An HTML page outside `/api` (form bodies, HTML errors). Set by [`Router::page`].
    pub page: bool,
}

impl RouteOpts {
    /// No authentication, no rate, no body.
    pub fn new() -> RouteOpts {
        RouteOpts::default()
    }

    /// Sets the authentication mode.
    pub fn auth(mut self, mode: AuthMode) -> RouteOpts {
        self.auth = mode;
        self
    }

    /// Adds a rate.
    pub fn rate(mut self, rate: RateSpec) -> RouteOpts {
        self.rates.push(rate);
        self
    }

    /// Sets the body schema.
    pub fn body(mut self, schema: Schema) -> RouteOpts {
        self.body = Some(schema);
        self
    }

    /// The handler validates the parsed body itself.
    pub fn own_body_validation(mut self) -> RouteOpts {
        self.own_body_validation = true;
        self
    }

    /// Sets the query schema.
    pub fn query(mut self, schema: Schema) -> RouteOpts {
        self.query = Some(schema);
        self
    }

    /// Sets the body limit.
    pub fn body_limit(mut self, bytes: usize) -> RouteOpts {
        self.body_limit = Some(bytes);
        self
    }

    /// Sets the handler timeout.
    pub fn timeout(mut self, timeout: Duration) -> RouteOpts {
        self.timeout = Some(timeout);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Lit(String),
    Param(String),
}

/// A registered route.
pub struct Route {
    method: Method,
    path: String,
    label: String,
    segs: Vec<Seg>,
    opts: RouteOpts,
    handler: Handler,
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route").field("method", &self.method).field("path", &self.path).finish()
    }
}

impl Route {
    /// The method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The full path pattern (`/api/v1/players/:username`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The metric label: `"<METHOD> <path>"`.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The options.
    pub fn opts(&self) -> &RouteOpts {
        &self.opts
    }

    /// The handler.
    pub fn handler(&self) -> &Handler {
        &self.handler
    }
}

/// The result of [`Router::find`].
#[derive(Debug)]
pub enum RouteMatch<'r> {
    /// No route has this path.
    NotFound,
    /// The path exists for other methods only (registration order, no duplicates).
    OtherMethods(Vec<Method>),
    /// The route and its decoded parameters.
    Found(&'r Route, Vec<(String, String)>),
    /// A parameter is not valid URL encoding (400 "malformed path").
    MalformedPath,
}

pub(crate) type CloseHook = Box<dyn FnOnce() -> BoxFuture<()> + Send + Sync>;

/// The routes, in registration order.
#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
    close_hooks: Vec<CloseHook>,
}

impl fmt::Debug for Router {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router").field("routes", &self.routes).finish()
    }
}

impl Router {
    /// An empty router.
    pub fn new() -> Router {
        Router::default()
    }

    /// Registers a route. Panics when the path does not start with `/` or the route exists
    /// already (programming errors, found at start-up).
    pub fn route<F, Fut>(&mut self, method: Method, path: &str, opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        assert!(path.starts_with('/'), "route {path}: path must start with /");
        let full = if opts.page || path.starts_with("/api/") || path == "/api" {
            path.to_string()
        } else if path == "/" {
            API_PREFIX.to_string()
        } else {
            format!("{API_PREFIX}{path}")
        };
        assert!(
            !self.routes.iter().any(|r| r.method == method && r.path == full),
            "route {method} {full} registered twice"
        );
        let segs = full
            .split('/')
            .skip(1)
            .map(|s| match s.strip_prefix(':') {
                Some(p) => Seg::Param(p.to_string()),
                None => Seg::Lit(s.to_string()),
            })
            .collect();
        let handler: Handler = Arc::new(move |ctx| Box::pin(handler(ctx)));
        self.routes.push(Route {
            label: format!("{method} {full}"),
            method,
            path: full,
            segs,
            opts,
            handler,
        });
        self
    }

    /// Registers a GET route.
    pub fn get<F, Fut>(&mut self, path: &str, opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        self.route(Method::GET, path, opts, handler)
    }

    /// Registers a POST route.
    pub fn post<F, Fut>(&mut self, path: &str, opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        self.route(Method::POST, path, opts, handler)
    }

    /// Registers a PUT route.
    pub fn put<F, Fut>(&mut self, path: &str, opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        self.route(Method::PUT, path, opts, handler)
    }

    /// Registers a PATCH route.
    pub fn patch<F, Fut>(&mut self, path: &str, opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        self.route(Method::PATCH, path, opts, handler)
    }

    /// Registers a DELETE route.
    pub fn delete<F, Fut>(&mut self, path: &str, opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        self.route(Method::DELETE, path, opts, handler)
    }

    /// Registers an HTML page outside `/api` (absolute path, form bodies, HTML errors).
    pub fn page<F, Fut>(&mut self, method: Method, path: &str, mut opts: RouteOpts, handler: F) -> &mut Router
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
    {
        opts.page = true;
        self.route(method, path, opts, handler)
    }

    /// Registers a hook run once when the API server closes (route modules release what they
    /// hold, such as rendering threads).
    pub fn on_close<F, Fut>(&mut self, hook: F) -> &mut Router
    where
        F: FnOnce() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.close_hooks.push(Box::new(move || Box::pin(hook())));
        self
    }

    pub(crate) fn take_close_hooks(&mut self) -> Vec<CloseHook> {
        std::mem::take(&mut self.close_hooks)
    }

    /// The routes, in registration order.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Finds the route of `method` (HEAD matches GET) and the raw (still percent-encoded)
    /// `pathname`. One trailing slash is ignored.
    pub fn find(&self, method: &Method, pathname: &str) -> RouteMatch<'_> {
        let p = if pathname.len() > 1 && pathname.ends_with('/') {
            &pathname[..pathname.len() - 1]
        } else {
            pathname
        };
        let parts: Vec<&str> = p.split('/').skip(1).collect();
        let m = if method == Method::HEAD { &Method::GET } else { method };
        let mut best: Option<&Route> = None;
        let mut methods: Vec<Method> = Vec::new();
        for r in &self.routes {
            if r.segs.len() != parts.len() {
                continue;
            }
            let fits = r.segs.iter().zip(&parts).all(|(s, part)| match s {
                Seg::Lit(l) => l == part,
                Seg::Param(_) => !part.is_empty(),
            });
            if !fits {
                continue;
            }
            if !methods.contains(&r.method) {
                methods.push(r.method.clone());
            }
            if r.method != m {
                continue;
            }
            if best.is_none_or(|b| more_specific(r, b)) {
                best = Some(r);
            }
        }
        let Some(route) = best else {
            return if methods.is_empty() { RouteMatch::NotFound } else { RouteMatch::OtherMethods(methods) };
        };
        let mut params = Vec::new();
        for (s, part) in route.segs.iter().zip(&parts) {
            if let Seg::Param(name) = s {
                match decode_uri_component(part) {
                    Some(v) => params.push((name.clone(), v)),
                    None => return RouteMatch::MalformedPath,
                }
            }
        }
        RouteMatch::Found(route, params)
    }
}

/// Whether `a` is more specific than `b`: at the first segment where one is a literal and the
/// other a parameter, the literal wins.
fn more_specific(a: &Route, b: &Route) -> bool {
    for (sa, sb) in a.segs.iter().zip(&b.segs) {
        let (la, lb) = (matches!(sa, Seg::Lit(_)), matches!(sb, Seg::Lit(_)));
        if la != lb {
            return la;
        }
    }
    false
}

/// The `Allow` header of a path: its methods, `HEAD` after them when `GET` is one, then `OPTIONS`.
pub fn allow_header(methods: &[Method]) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(methods.len() + 2);
    for m in methods {
        if !out.contains(&m.as_str()) {
            out.push(m.as_str());
        }
    }
    if methods.contains(&Method::GET) && !out.contains(&"HEAD") {
        out.push("HEAD");
    }
    if !out.contains(&"OPTIONS") {
        out.push("OPTIONS");
    }
    out.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ok(_ctx: Ctx) -> Result<Answer, ApiError> {
        Ok(Answer::no_content())
    }

    fn router() -> Router {
        let mut r = Router::new();
        r.get("/echo/:name", RouteOpts::new(), ok);
        r.get("/echo/special", RouteOpts::new(), ok);
        r.get("/api/v1/absolute", RouteOpts::new(), ok);
        r.post("/body", RouteOpts::new(), ok);
        r.page(Method::GET, "/page", RouteOpts::new(), ok);
        r.page(Method::POST, "/page", RouteOpts::new(), ok);
        r
    }

    fn found<'a>(m: RouteMatch<'a>) -> (&'a str, Vec<(String, String)>) {
        match m {
            RouteMatch::Found(r, p) => (r.path(), p),
            other => panic!("not found: {other:?}"),
        }
    }

    #[test]
    fn matches_params_literals_prefix_and_pages() {
        let r = router();
        let (path, params) = found(r.find(&Method::GET, "/api/v1/echo/b%C3%A9b%C3%A9"));
        assert_eq!((path, params), ("/api/v1/echo/:name", vec![("name".to_string(), "bébé".to_string())]));
        assert_eq!(found(r.find(&Method::GET, "/api/v1/echo/special")).0, "/api/v1/echo/special");
        assert_eq!(found(r.find(&Method::GET, "/api/v1/echo/special/")).0, "/api/v1/echo/special");
        assert_eq!(found(r.find(&Method::HEAD, "/api/v1/absolute")).0, "/api/v1/absolute");
        assert_eq!(found(r.find(&Method::GET, "/page")).0, "/page");
        assert!(matches!(r.find(&Method::GET, "/echo/x"), RouteMatch::NotFound));
        assert!(matches!(r.find(&Method::GET, "/api/v1/echo/special//"), RouteMatch::NotFound));
        assert!(matches!(r.find(&Method::GET, "/api/v1/%65cho/x"), RouteMatch::NotFound), "literals are raw");
        assert!(matches!(r.find(&Method::GET, "/api/v1/echo/%E0%A4%A"), RouteMatch::MalformedPath));
        assert!(
            matches!(r.find(&Method::GET, "/api/v1/echo/"), RouteMatch::NotFound),
            "a parameter is never empty"
        );
    }

    #[test]
    fn other_methods_and_allow() {
        let r = router();
        let RouteMatch::OtherMethods(m) = r.find(&Method::DELETE, "/api/v1/echo/x") else { panic!("405") };
        assert_eq!(allow_header(&m), "GET, HEAD, OPTIONS");
        let RouteMatch::OtherMethods(m) = r.find(&Method::OPTIONS, "/api/v1/body") else { panic!("405") };
        assert_eq!(allow_header(&m), "POST, OPTIONS");
        let RouteMatch::OtherMethods(m) = r.find(&Method::PUT, "/page") else { panic!("405") };
        assert_eq!(allow_header(&m), "GET, POST, HEAD, OPTIONS");
        let RouteMatch::OtherMethods(m) = r.find(&Method::HEAD, "/api/v1/body") else { panic!("405") };
        assert_eq!(m, [Method::POST]);
    }

    #[test]
    fn relative_and_absolute_registrations_are_the_same_route() {
        let mut r = Router::new();
        r.get("/players/:username", RouteOpts::new(), ok);
        assert!(matches!(r.find(&Method::GET, "/api/v1/players/bob"), RouteMatch::Found(..)));
        assert!(
            matches!(r.find(&Method::POST, "/api/v1/players/bob"), RouteMatch::OtherMethods(ref m) if m == &[Method::GET])
        );
        assert!(matches!(r.find(&Method::GET, "/players/bob"), RouteMatch::NotFound));
        assert_eq!(r.routes()[0].label(), "GET /api/v1/players/:username");
        let dup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            r.get("/api/v1/players/:username", RouteOpts::new(), ok);
        }));
        let msg = dup.expect_err("a duplicate panics");
        let text = msg.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(text.contains("registered twice"), "{text}");
    }
}
