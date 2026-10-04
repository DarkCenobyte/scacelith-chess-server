//! CORS for the web pages of `CORS_ORIGINS` (docs/API.md section 1.2): an allow-list, off by
//! default.
//!
//! With `CORS_ORIGINS` empty nothing here adds a header. Otherwise, every answer of the API for a
//! path under `/api/v1` (the health paths excepted) carries `Vary: Origin`, and when the request's
//! `Origin` is byte for byte one of the listed origins, also `Access-Control-Allow-Origin` with
//! that origin and `Access-Control-Expose-Headers` (`Retry-After`, `Content-Disposition`). The
//! `OPTIONS` answer of an existing path to a preflight (`Access-Control-Request-Method`) from a
//! listed origin also carries `Access-Control-Allow-Methods` (the `Allow` list),
//! `Access-Control-Allow-Headers` (`Authorization`, `Content-Type`) and
//! `Access-Control-Max-Age: 600`. No answer ever carries `Access-Control-Allow-Credentials` or
//! `*`: pages send the bearer token in `Authorization`, never a cookie.
//!
//! `Cross-Origin-Resource-Policy: same-origin` stays on every answer of the API: the Fetch
//! standard applies it only to `no-cors` requests (an `<img>` or a `<script>` of another site,
//! whose answers are opaque), never to the `cors` requests of a listed page.

use http::Method;
use http::header::{self, HeaderMap, HeaderValue};

use super::router::API_PREFIX;

/// `Access-Control-Expose-Headers`: what a page may read besides the safelisted headers
/// (`Content-Length` and `Content-Type` are among them).
pub const EXPOSE_HEADERS: &str = "Retry-After, Content-Disposition";
/// `Access-Control-Allow-Headers` of a preflight: the request headers a page may send.
pub const ALLOW_HEADERS: &str = "Authorization, Content-Type";
/// `Access-Control-Max-Age` of a preflight, in seconds: how long a browser may reuse it.
pub const MAX_AGE: &str = "600";

/// The origins of `CORS_ORIGINS`, checked when the configuration was loaded.
#[derive(Debug, Default, Clone)]
pub struct Cors {
    origins: Vec<String>,
}

impl Cors {
    /// The allow-list (empty: CORS off).
    pub fn new(origins: &[String]) -> Cors {
        Cors { origins: origins.to_vec() }
    }

    /// Whether any origin is listed.
    pub fn is_enabled(&self) -> bool {
        !self.origins.is_empty()
    }

    /// What CORS adds to the answer of a request for `path` (the path alone, without the query).
    pub fn check(&self, method: &Method, path: &str, headers: &HeaderMap) -> CorsCheck {
        if self.origins.is_empty() || !under_api(path) {
            return CorsCheck::default();
        }
        let mut sent = headers.get_all(header::ORIGIN).iter();
        let origin = match (sent.next(), sent.next()) {
            (Some(o), None) if self.origins.iter().any(|a| a.as_bytes() == o.as_bytes()) => Some(o.clone()),
            _ => None,
        };
        let preflight = origin.is_some()
            && method == Method::OPTIONS
            && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
        CorsCheck { applies: true, origin, preflight }
    }
}

/// A path of the API that CORS covers: `/api/v1` and below, but not the health endpoints, which
/// are for monitoring.
fn under_api(path: &str) -> bool {
    let below = path.strip_prefix(API_PREFIX).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    below && !matches!(path, "/api/v1/healthz" | "/api/v1/readyz")
}

/// The CORS headers of one request's answer (see [`Cors::check`]).
#[derive(Debug, Default, Clone)]
pub struct CorsCheck {
    applies: bool,
    origin: Option<HeaderValue>,
    preflight: bool,
}

impl CorsCheck {
    /// A preflight from a listed origin: an `OPTIONS` with `Access-Control-Request-Method`.
    pub fn is_preflight(&self) -> bool {
        self.preflight
    }

    /// The listed origin the request came from, if any.
    pub fn origin(&self) -> Option<&HeaderValue> {
        self.origin.as_ref()
    }

    /// Adds the headers of a preflight's answer (`allow`: the path's `Allow` list), when the
    /// request is one ([`CorsCheck::is_preflight`]). Returns whether it is.
    pub fn add_preflight(&self, headers: &mut HeaderMap, allow: &str) -> bool {
        if !self.preflight {
            return false;
        }
        if let Ok(methods) = HeaderValue::from_str(allow) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, methods);
        }
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static(ALLOW_HEADERS));
        headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE));
        true
    }

    /// Adds `Access-Control-Allow-Origin` (a listed origin only), `Access-Control-Expose-Headers`
    /// (not on a preflight's answer, where it means nothing) and `Vary: Origin`.
    pub fn finish(&self, headers: &mut HeaderMap, preflight_answer: bool) {
        if !self.applies {
            return;
        }
        if let Some(origin) = &self.origin {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
            if !preflight_answer {
                headers
                    .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(EXPOSE_HEADERS));
            }
        }
        headers.append(header::VARY, HeaderValue::from_static("Origin"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors() -> Cors {
        Cors::new(&["https://scacelith.com".to_string(), "http://localhost:8080".to_string()])
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn listed_origins_only_byte_for_byte_and_under_the_api_only() {
        let c = cors();
        let from = |o: &'static str| headers(&[("origin", o)]);
        let check = c.check(&Method::GET, "/api/v1/info", &from("https://scacelith.com"));
        assert_eq!(check.origin().map(HeaderValue::as_bytes), Some(&b"https://scacelith.com"[..]));
        assert!(!check.is_preflight());
        assert!(c.check(&Method::GET, "/api/v1", &from("http://localhost:8080")).origin().is_some());
        for other in ["https://Scacelith.com", "https://scacelith.com/", "https://evil.example", "null", "*"]
        {
            let check = c.check(&Method::GET, "/api/v1/info", &from(other));
            assert!(check.origin().is_none(), "{other}");
            assert!(check.applies, "{other}: still Vary");
        }
        let twice = headers(&[("origin", "https://scacelith.com"), ("origin", "https://scacelith.com")]);
        assert!(c.check(&Method::GET, "/api/v1/info", &twice).origin().is_none(), "one Origin only");
        for path in
            ["/verify-email", "/healthz", "/api/v1/healthz", "/api/v1/readyz", "/api/v10/x", "/api", "/"]
        {
            let check = c.check(&Method::GET, path, &from("https://scacelith.com"));
            assert!(!check.applies && check.origin().is_none(), "{path}");
        }
        let off = Cors::new(&[]);
        assert!(!off.is_enabled());
        assert!(!off.check(&Method::GET, "/api/v1/info", &from("https://scacelith.com")).applies);
    }

    #[test]
    fn preflights_and_the_headers() {
        let c = cors();
        let pre = headers(&[("origin", "https://scacelith.com"), ("access-control-request-method", "POST")]);
        let check = c.check(&Method::OPTIONS, "/api/v1/auth/login", &pre);
        assert!(check.is_preflight());
        let mut h = HeaderMap::new();
        assert!(check.add_preflight(&mut h, "POST, OPTIONS"));
        check.finish(&mut h, true);
        let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok());
        assert_eq!(get("access-control-allow-origin"), Some("https://scacelith.com"));
        assert_eq!(get("access-control-allow-methods"), Some("POST, OPTIONS"));
        assert_eq!(get("access-control-allow-headers"), Some("Authorization, Content-Type"));
        assert_eq!(get("access-control-max-age"), Some("600"));
        assert_eq!(get("vary"), Some("Origin"));
        assert_eq!(get("access-control-expose-headers"), None);
        assert_eq!(get("access-control-allow-credentials"), None);
        let plain = headers(&[("origin", "https://scacelith.com")]);
        assert!(!c.check(&Method::OPTIONS, "/api/v1/auth/login", &plain).is_preflight(), "no request method");
        let evil = headers(&[("origin", "https://evil.example"), ("access-control-request-method", "POST")]);
        let check = c.check(&Method::OPTIONS, "/api/v1/auth/login", &evil);
        let mut h = HeaderMap::new();
        assert!(!check.add_preflight(&mut h, "POST, OPTIONS"));
        check.finish(&mut h, false);
        assert_eq!(h.keys().map(|k| k.as_str()).collect::<Vec<_>>(), ["vary"]);
    }
}
