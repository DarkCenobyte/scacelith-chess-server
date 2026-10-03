//! OpenID Connect client for Google sign-in (authorization code flow, PKCE S256): an installed-app
//! ("Desktop app") client, a loopback redirect per attempt (RFC 8252 section 7.3), the client
//! secret held only by this server. It builds the authorization URL, exchanges the code at the
//! token endpoint and verifies the ID token (RS256 signature with the provider's keys, cached as
//! their `Cache-Control` says; `iss`, `aud`, `azp`, `exp` and `iat` with a 2-minute skew, `nonce`).
//!
//! The redirect URI is given per call: `http://127.0.0.1:<the game's port>/oauth2/google/<origin
//! tag>`, the game's own listener. Both calls refuse any other form (`bad_redirect_uri`), so the
//! URI the exchange sends is always one the authorization request could carry.
//!
//! Requests go over HTTPS only (plain HTTP only when allowed, for tests against a local fake
//! provider), without following redirects, within 10 seconds, and answers are read up to 1 MiB.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use http::{Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use rustls_pki_types::ServerName;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::clock::SharedClock;
use crate::mail::smtp::default_tls_config;
use crate::security::encoding::{b64_url, node_b64_decode, utf16_len};
use crate::security::keys::safe_eq;

/// Time limit of one request to the provider.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest answer read from the provider.
pub const MAX_ANSWER_BYTES: usize = 1 << 20;
/// Clock skew allowed on `exp` and `iat`, in ms.
pub const SKEW_MS: i64 = 120_000;
/// Longest ID token accepted, in characters.
const MAX_TOKEN_LEN: usize = 16_384;
/// A key id unknown to the cached keys refetches them at most this often, in ms.
const UNKNOWN_KID_REFETCH_MS: i64 = 60_000;

/// The provider's endpoints and issuers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcEndpoints {
    /// The consent page.
    pub authorization: String,
    /// The code exchange.
    pub token: String,
    /// The signing keys (JWKS).
    pub jwks: String,
    /// The accepted `iss` values.
    pub issuers: Vec<String>,
}

impl OidcEndpoints {
    /// Google's endpoints.
    pub fn google() -> OidcEndpoints {
        OidcEndpoints {
            authorization: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            jwks: "https://www.googleapis.com/oauth2/v3/certs".into(),
            issuers: vec!["accounts.google.com".into(), "https://accounts.google.com".into()],
        }
    }
}

/// Why a step of the sign-in failed: the `reason` goes to the logs and the security event, never
/// to the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcError {
    /// Short code (`bad_signature`, `token_exchange_failed`, `timeout`...).
    pub reason: &'static str,
    /// Description for the logs (never holds a code, a token or a secret).
    pub message: String,
}

impl OidcError {
    fn new(reason: &'static str, message: impl Into<String>) -> OidcError {
        OidcError { reason, message: message.into() }
    }

    fn bare(reason: &'static str) -> OidcError {
        OidcError::new(reason, reason)
    }
}

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for OidcError {}

/// The S256 PKCE challenge of a verifier: base64url(SHA-256(verifier)).
pub fn pkce_challenge(verifier: &str) -> String {
    b64_url(&Sha256::digest(verifier.as_bytes()))
}

/// Checks the loopback redirect URI of an attempt:
/// `^http://127\.0\.0\.1:(\d{4,5})/oauth2/google/[A-Za-z0-9_-]{22}$` with a port of 1024 to 65535.
pub fn check_redirect_uri(uri: &str) -> Result<(), OidcError> {
    let refuse = || OidcError::new("bad_redirect_uri", "redirect URI is not a loopback one");
    let rest = uri.strip_prefix("http://127.0.0.1:").ok_or_else(refuse)?;
    let (port, tag) = rest.split_once("/oauth2/google/").ok_or_else(refuse)?;
    let port_ok = (4..=5).contains(&port.len())
        && port.bytes().all(|c| c.is_ascii_digit())
        && port.parse::<u32>().is_ok_and(|p| (1024..=65535).contains(&p));
    let tag_ok = tag.len() == 22 && tag.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
    if port_ok && tag_ok { Ok(()) } else { Err(refuse()) }
}

/// WHATWG `application/x-www-form-urlencoded` serialisation (`URLSearchParams.toString()`).
pub fn form_urlencode(pairs: &[(&str, &str)]) -> String {
    fn push(out: &mut String, s: &str) {
        for &b in s.as_bytes() {
            match b {
                b' ' => out.push('+'),
                b'*' | b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => {
                    out.push(char::from(b))
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        push(&mut out, k);
        out.push('=');
        push(&mut out, v);
    }
    out
}

/// A decoded (not verified) compact JWS.
#[derive(Clone, Debug, PartialEq)]
pub struct Jwt {
    /// The header object.
    pub header: Map<String, Value>,
    /// The claims.
    pub payload: Map<String, Value>,
    /// `<header>.<payload>` as sent.
    pub signing_input: String,
    /// The signature bytes.
    pub signature: Vec<u8>,
}

/// Splits and decodes a compact JWS (without verifying it): at most 16384 characters, three
/// base64url parts, a JSON header and payload.
pub fn decode_jwt(token: &str) -> Result<Jwt, OidcError> {
    let malformed = || OidcError::bare("malformed_token");
    if utf16_len(token) > MAX_TOKEN_LEN {
        return Err(malformed());
    }
    let parts: Vec<&str> = token.split('.').collect();
    let b64url = |p: &&str| p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
    if parts.len() != 3 || !parts.iter().all(b64url) {
        return Err(malformed());
    }
    // An array passes as an object without fields (the former server's `typeof` check).
    let object = |part: &str| -> Result<Map<String, Value>, OidcError> {
        let text = String::from_utf8_lossy(&node_b64_decode(part)).into_owned();
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(Value::Array(_)) => Ok(Map::new()),
            _ => Err(malformed()),
        }
    };
    Ok(Jwt {
        header: object(parts[0])?,
        payload: object(parts[1])?,
        signing_input: format!("{}.{}", parts[0], parts[1]),
        signature: node_b64_decode(parts[2]),
    })
}

/// The cache lifetime of a JWKS answer: its `max-age` (default 1 hour), within 1 minute and 1 day.
fn max_age_ms(cache_control: Option<&str>) -> i64 {
    let secs = cache_control
        .and_then(|cc| {
            let lower = cc.to_ascii_lowercase();
            let at = lower.find("max-age=")?;
            let digits: String = lower[at + 8..].chars().take_while(char::is_ascii_digit).collect();
            (!digits.is_empty()).then(|| digits.parse::<i64>().unwrap_or(i64::MAX))
        })
        .unwrap_or(3600);
    secs.clamp(60, 86_400) * 1000
}

#[derive(Clone, Debug)]
struct RsaKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

#[derive(Default)]
struct Jwks {
    keys: HashMap<String, RsaKey>,
    expires_at: i64,
    fetched_at: i64,
}

/// A provider's answer.
struct HttpAnswer {
    status: u16,
    cache_control: Option<String>,
    body: String,
}

/// The client of the provider (module documentation).
pub struct OidcClient {
    client_id: String,
    client_secret: String,
    endpoints: OidcEndpoints,
    clock: SharedClock,
    allow_http: bool,
    jwks: Mutex<Jwks>,
    /// One key fetch at a time; the others wait for it.
    fetching: tokio::sync::Mutex<()>,
}

impl fmt::Debug for OidcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcClient")
            .field("client_id", &self.client_id)
            .field("endpoints", &self.endpoints)
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

impl OidcClient {
    /// A client of `endpoints` with these credentials. `allow_http` permits plain HTTP endpoints
    /// (tests against a local fake provider).
    pub fn new(
        client_id: &str,
        client_secret: &str,
        endpoints: OidcEndpoints,
        clock: SharedClock,
        allow_http: bool,
    ) -> OidcClient {
        OidcClient {
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            endpoints,
            clock,
            allow_http,
            jwks: Mutex::new(Jwks::default()),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    /// The accepted `iss` values.
    pub fn issuers(&self) -> &[String] {
        &self.endpoints.issuers
    }

    /// The URL of the provider's consent page.
    pub fn authorization_url(
        &self,
        state: &str,
        nonce: &str,
        code_challenge: &str,
        redirect_uri: &str,
    ) -> Result<String, OidcError> {
        check_redirect_uri(redirect_uri)?;
        let query = form_urlencode(&[
            ("client_id", &self.client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "select_account"),
        ]);
        let sep = if self.endpoints.authorization.contains('?') { '&' } else { '?' };
        Ok(format!("{}{sep}{query}", self.endpoints.authorization))
    }

    /// Exchanges an authorization code (with this server's PKCE verifier and the redirect URI of
    /// its authorization request) for the ID token.
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<String, OidcError> {
        check_redirect_uri(redirect_uri)?;
        let body = form_urlencode(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("code_verifier", verifier),
        ]);
        let r = self.request(&self.endpoints.token, Some(body)).await?;
        let doc: Option<Value> = serde_json::from_str(&r.body).ok();
        match doc.as_ref().and_then(|d| d.get("id_token")).and_then(Value::as_str) {
            Some(token) if r.status == 200 => Ok(token.to_owned()),
            _ => {
                let error = doc
                    .as_ref()
                    .and_then(|d| d.get("error"))
                    .filter(|e| !e.is_null())
                    .map(|e| {
                        let text = e.as_str().map_or_else(|| e.to_string(), str::to_owned);
                        format!(" {}", text.chars().take(60).collect::<String>())
                    })
                    .unwrap_or_default();
                Err(OidcError::new(
                    "token_exchange_failed",
                    format!("token endpoint answer {}{error}", r.status),
                ))
            }
        }
    }

    /// Verifies an ID token and returns its claims (`sub`, `email`, `email_verified`, `name`...).
    pub async fn verify_id_token(
        &self,
        id_token: &str,
        nonce: &str,
    ) -> Result<Map<String, Value>, OidcError> {
        let jwt = decode_jwt(id_token)?;
        if jwt.header.get("alg").and_then(Value::as_str) != Some("RS256") {
            return Err(OidcError::new("bad_algorithm", "ID token algorithm must be RS256"));
        }
        let Some(kid) = jwt.header.get("kid").and_then(Value::as_str) else {
            return Err(OidcError::bare("unknown_key"));
        };
        let key = self.key_for(kid).await?;
        let components = RsaPublicKeyComponents { n: &key.n, e: &key.e };
        if components
            .verify(&RSA_PKCS1_2048_8192_SHA256, jwt.signing_input.as_bytes(), &jwt.signature)
            .is_err()
        {
            return Err(OidcError::new("bad_signature", "ID token signature invalid"));
        }
        let c = jwt.payload;
        let iss = c.get("iss").and_then(Value::as_str);
        if !iss.is_some_and(|i| self.endpoints.issuers.iter().any(|x| x == i)) {
            return Err(OidcError::bare("bad_issuer"));
        }
        let audiences: Vec<&Value> = match c.get("aud") {
            Some(Value::Array(list)) => list.iter().collect(),
            Some(v) => vec![v],
            None => vec![&Value::Null],
        };
        let is_client = |v: Option<&Value>| v.and_then(Value::as_str) == Some(self.client_id.as_str());
        if !audiences.iter().any(|a| is_client(Some(a)))
            || (audiences.len() > 1 && !is_client(c.get("azp")))
            || (c.contains_key("azp") && !is_client(c.get("azp")))
        {
            return Err(OidcError::bare("bad_audience"));
        }
        let t = self.clock.wall_ms() as f64 / 1000.0;
        let skew = SKEW_MS as f64 / 1000.0;
        if !c.get("exp").and_then(Value::as_f64).is_some_and(|exp| exp + skew >= t) {
            return Err(OidcError::bare("expired"));
        }
        if !c.get("iat").and_then(Value::as_f64).is_some_and(|iat| iat - skew <= t) {
            return Err(OidcError::bare("issued_in_future"));
        }
        let nonce_ok = c
            .get("nonce")
            .and_then(Value::as_str)
            .is_some_and(|n| !nonce.is_empty() && safe_eq(n.as_bytes(), nonce.as_bytes()));
        if !nonce_ok {
            return Err(OidcError::bare("bad_nonce"));
        }
        if !c.get("sub").and_then(Value::as_str).is_some_and(|s| !s.is_empty() && utf16_len(s) <= 255) {
            return Err(OidcError::bare("bad_subject"));
        }
        Ok(c)
    }

    /// The provider key `kid`, fetching the keys again when they expired, or when the id is
    /// unknown and the last fetch is more than a minute old.
    async fn key_for(&self, kid: &str) -> Result<RsaKey, OidcError> {
        let needs_fetch = |j: &Jwks, t: i64| {
            t >= j.expires_at || (!j.keys.contains_key(kid) && t - j.fetched_at > UNKNOWN_KID_REFETCH_MS)
        };
        if needs_fetch(&self.jwks.lock(), self.clock.wall_ms()) {
            let _one = self.fetching.lock().await;
            // Another request may have fetched them while this one waited.
            if needs_fetch(&self.jwks.lock(), self.clock.wall_ms()) {
                self.fetch_jwks().await?;
            }
        }
        self.jwks
            .lock()
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| OidcError::new("unknown_key", "no provider key for this token"))
    }

    async fn fetch_jwks(&self) -> Result<(), OidcError> {
        let r = self.request(&self.endpoints.jwks, None).await?;
        if r.status != 200 {
            return Err(OidcError::new("jwks_unavailable", format!("JWKS answer {}", r.status)));
        }
        let doc: Value = serde_json::from_str(&r.body)
            .map_err(|_| OidcError::new("jwks_unavailable", "JWKS is not JSON"))?;
        let mut keys = HashMap::new();
        for k in doc.get("keys").and_then(Value::as_array).into_iter().flatten() {
            let field = |name: &str| k.get(name).and_then(Value::as_str);
            let falsy = |name: &str| match k.get(name) {
                None | Some(Value::Null) | Some(Value::Bool(false)) => true,
                Some(Value::String(s)) => s.is_empty(),
                Some(_) => false,
            };
            let Some(kid) = field("kid").filter(|s| !s.is_empty()) else { continue };
            if field("kty") != Some("RSA")
                || !(falsy("use") || field("use") == Some("sig"))
                || !(falsy("alg") || field("alg") == Some("RS256"))
            {
                continue;
            }
            let (Some(n), Some(e)) = (field("n"), field("e")) else { continue };
            let (n, e) = (node_b64_decode(n), node_b64_decode(e));
            if n.is_empty() || e.is_empty() {
                continue;
            }
            keys.insert(kid.to_owned(), RsaKey { n, e });
        }
        let t = self.clock.wall_ms();
        *self.jwks.lock() =
            Jwks { keys, expires_at: t + max_age_ms(r.cache_control.as_deref()), fetched_at: t };
        Ok(())
    }

    /// One request to the provider: a GET, or a POST of a form when `form` is given.
    async fn request(&self, url: &str, form: Option<String>) -> Result<HttpAnswer, OidcError> {
        let uri: Uri = url.parse().map_err(|_| OidcError::new("network", "invalid endpoint URL"))?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") if self.allow_http => false,
            _ => return Err(OidcError::new("insecure_endpoint", "endpoint must be https")),
        };
        let host = uri.host().ok_or_else(|| OidcError::new("network", "endpoint without host"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let authority = uri.authority().map(|a| a.as_str().to_owned()).unwrap_or_else(|| host.clone());
        let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        let method = if form.is_some() { Method::POST } else { Method::GET };
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, authority)
            .header(ACCEPT, "application/json");
        if let Some(body) = &form {
            req = req
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(CONTENT_LENGTH, body.len());
        }
        let req = req
            .body(Full::new(Bytes::from(form.unwrap_or_default())))
            .map_err(|e| OidcError::new("network", e.to_string()))?;
        let exchange = async {
            let tcp = TcpStream::connect((host.as_str(), port)).await.map_err(network)?;
            if tls {
                let name = ServerName::try_from(host.clone())
                    .map_err(|_| OidcError::new("network", "invalid host"))?;
                let stream =
                    TlsConnector::from(default_tls_config()).connect(name, tcp).await.map_err(network)?;
                send(stream, req).await
            } else {
                send(tcp, req).await
            }
        };
        tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .unwrap_or_else(|_| Err(OidcError::new("timeout", "provider timeout")))
    }
}

fn network(e: impl fmt::Display) -> OidcError {
    OidcError::new("network", e.to_string())
}

/// Sends `req` over `io` (HTTP/1.1, one request) and reads the answer up to [`MAX_ANSWER_BYTES`].
async fn send<S>(io: S, req: Request<Full<Bytes>>) -> Result<HttpAnswer, OidcError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(network)?;
    let driver = tokio::spawn(conn);
    let result = async {
        let resp = sender.send_request(req).await.map_err(network)?;
        let status = resp.status().as_u16();
        let cache_control: Vec<&str> =
            resp.headers().get_all(CACHE_CONTROL).iter().filter_map(|v| v.to_str().ok()).collect();
        let cache_control = (!cache_control.is_empty()).then(|| cache_control.join(", "));
        let mut body = resp.into_body();
        let mut buf = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(network)?;
            if let Ok(data) = frame.into_data() {
                if buf.len() + data.len() > MAX_ANSWER_BYTES {
                    return Err(OidcError::new("response_too_large", "provider answer too large"));
                }
                buf.extend_from_slice(&data);
            }
        }
        Ok(HttpAnswer { status, cache_control, body: String::from_utf8_lossy(&buf).into_owned() })
    }
    .await;
    driver.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_and_redirects() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let tag = "IhcScoV7eDOzTEcSnqPUPt";
        assert!(check_redirect_uri(&format!("http://127.0.0.1:50443/oauth2/google/{tag}")).is_ok());
        assert!(check_redirect_uri(&format!("http://127.0.0.1:1024/oauth2/google/{tag}")).is_ok());
        for bad in [
            format!("http://127.0.0.1:1023/oauth2/google/{tag}"),
            format!("http://127.0.0.1:65536/oauth2/google/{tag}"),
            format!("http://127.0.0.1:999/oauth2/google/{tag}"),
            format!("http://127.0.0.1:123456/oauth2/google/{tag}"),
            format!("http://localhost:50443/oauth2/google/{tag}"),
            format!("https://127.0.0.1:50443/oauth2/google/{tag}"),
            format!("http://127.0.0.1:50443/oauth2/google/{tag}x"),
            format!("http://127.0.0.1:50443/oauth2/google/{tag}/"),
            "http://127.0.0.1:50443/oauth2/google/short".to_owned(),
        ] {
            assert_eq!(check_redirect_uri(&bad).unwrap_err().reason, "bad_redirect_uri", "{bad}");
        }
    }

    #[test]
    fn form_encoding_is_the_whatwg_one() {
        assert_eq!(
            form_urlencode(&[
                ("scope", "openid email profile"),
                ("redirect_uri", "http://127.0.0.1:5/a_b"),
                ("x", "*-._~!é")
            ]),
            "scope=openid+email+profile&redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fa_b&x=*-._%7E%21%C3%A9"
        );
    }

    #[test]
    fn jwt_decoding() {
        let enc = |v: &str| b64_url(v.as_bytes());
        let token =
            format!("{}.{}.{}", enc(r#"{"alg":"RS256","kid":"k"}"#), enc(r#"{"sub":"1"}"#), enc("sig"));
        let jwt = decode_jwt(&token).unwrap();
        assert_eq!(jwt.header.get("kid").unwrap(), "k");
        assert_eq!(jwt.payload.get("sub").unwrap(), "1");
        assert_eq!(jwt.signature, b"sig");
        assert!(jwt.signing_input.ends_with(&enc(r#"{"sub":"1"}"#)));
        let arr = format!("{}.{}.", enc("[1]"), enc("{}"));
        assert!(decode_jwt(&arr).unwrap().header.is_empty());
        for bad in [
            "a.b".to_owned(),
            "a.b.c.d".to_owned(),
            format!("{}.{}.x+y", enc("{}"), enc("{}")),
            format!("{}.{}.", enc("1"), enc("{}")),
            format!("{}.{}.", enc("{"), enc("{}")),
            format!("{}.{}.{}", enc("{}"), enc("{}"), "A".repeat(16_384)),
        ] {
            assert_eq!(decode_jwt(&bad).unwrap_err().reason, "malformed_token", "{bad}");
        }
    }

    #[test]
    fn jwks_lifetime() {
        assert_eq!(max_age_ms(None), 3_600_000);
        assert_eq!(max_age_ms(Some("public, max-age=19800, must-revalidate")), 19_800_000);
        assert_eq!(max_age_ms(Some("MAX-AGE=5")), 60_000);
        assert_eq!(max_age_ms(Some("max-age=99999999999999999999999")), 86_400_000);
        assert_eq!(max_age_ms(Some("no-cache")), 3_600_000);
    }
}
