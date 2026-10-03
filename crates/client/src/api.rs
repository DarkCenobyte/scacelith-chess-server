//! The HTTPS account API (docs/API.md): server info, registration, sign-in with or without a
//! second factor, sign-out, and a generic JSON request for every other endpoint.
//!
//! Sessions have no refresh call: a session token stays valid while it is used (API.md 1.4), and
//! a new one comes from a new sign-in.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{ClientError, Result, with_timeout};
use crate::http::{HttpConnection, Request, Response};
use crate::net::Endpoint;

/// Path prefix of the API.
pub const API_PREFIX: &str = "/api/v1";
/// Default deadline of one request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle keep-alive connections older than this are not reused (servers close them after a few
/// seconds).
const IDLE_REUSE: Duration = Duration::from_secs(4);
/// Idle connections kept for reuse.
const POOL_MAX: usize = 8;

/// A signed-in session.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthSession {
    /// The bearer token (`sct_...`), also the realtime `Hello.token`.
    pub token: String,
    /// End of the session (epoch ms).
    pub expires_at: i64,
    /// The account view (`GET /account/me` shape).
    pub user: Value,
}

impl AuthSession {
    fn from_body(body: &Value) -> Result<AuthSession> {
        let token = body
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Unexpected("sign-in answer without a token".into()))?;
        Ok(AuthSession {
            token: token.to_string(),
            expires_at: body.get("expiresAt").and_then(Value::as_i64).unwrap_or_default(),
            user: body.get("user").cloned().unwrap_or(Value::Null),
        })
    }
}

/// The answer of a password sign-in.
#[derive(Clone, Debug, PartialEq)]
pub enum Login {
    /// Signed in.
    Session(AuthSession),
    /// Two-step verification is on: finish with [`ApiClient::login_mfa`] within `expires_in`
    /// seconds.
    MfaRequired {
        /// Token of the second step.
        mfa_token: String,
        /// Seconds left for the second step.
        expires_in: u64,
    },
}

#[derive(Debug)]
struct Inner {
    endpoint: Endpoint,
    timeout: Duration,
    pool: Mutex<Vec<HttpConnection>>,
}

/// A client of the HTTPS API, cheap to clone (the clones share a few keep-alive connections).
///
/// ```no_run
/// # async fn demo(endpoint: scacelith_client::Endpoint) -> scacelith_client::Result<()> {
/// use scacelith_client::{ApiClient, Login};
/// let api = ApiClient::new(endpoint);
/// api.register("alice", "alice@example.org", "correct horse battery").await?;
/// let Login::Session(session) = api.login("alice", "correct horse battery", Some("tests")).await? else {
///     panic!("two-step verification is not on");
/// };
/// let me = api.get_json("/account/me", Some(&session.token)).await?;
/// # let _ = me; Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct ApiClient {
    inner: Arc<Inner>,
}

impl ApiClient {
    /// A client of the API at `endpoint` (`/api/v1` on it).
    pub fn new(endpoint: Endpoint) -> ApiClient {
        ApiClient {
            inner: Arc::new(Inner { endpoint, timeout: DEFAULT_TIMEOUT, pool: Mutex::new(Vec::new()) }),
        }
    }

    /// The same client with another deadline per request (connections not shared).
    pub fn with_timeout(&self, timeout: Duration) -> ApiClient {
        ApiClient {
            inner: Arc::new(Inner {
                endpoint: self.inner.endpoint.clone(),
                timeout,
                pool: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The server endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    fn take_idle(&self) -> Option<HttpConnection> {
        let mut pool = self.inner.pool.lock().unwrap_or_else(|p| p.into_inner());
        while let Some(conn) = pool.pop() {
            if conn.is_reusable() && conn.idle_for() < IDLE_REUSE {
                return Some(conn);
            }
        }
        None
    }

    fn put_idle(&self, conn: HttpConnection) {
        if conn.is_reusable() {
            let mut pool = self.inner.pool.lock().unwrap_or_else(|p| p.into_inner());
            if pool.len() < POOL_MAX {
                pool.push(conn);
            }
        }
    }

    /// Sends a request. `path` is relative to `/api/v1` (`"/info"`, `"/games/42/pgn"`), unless
    /// it starts with `/api/`. A request that fails on a reused idle connection is sent again once
    /// on a new connection when its method is idempotent.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Response> {
        let target = if path.starts_with("/api/") { path.to_string() } else { format!("{API_PREFIX}{path}") };
        let body_bytes = body.map(|b| serde_json::to_vec(b).expect("a JSON value serializes"));
        let req = Request {
            method,
            target: &target,
            bearer: token,
            content_type: body_bytes.as_ref().map(|_| "application/json"),
            body: body_bytes.as_deref().unwrap_or_default(),
        };
        let idempotent = matches!(method, "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE");
        if let Some(mut conn) = self.take_idle() {
            match with_timeout(self.inner.timeout, "HTTP request", conn.send(&req)).await {
                Ok(res) => {
                    self.put_idle(conn);
                    return Ok(res);
                }
                Err(ClientError::Io(_) | ClientError::Http(_)) if idempotent => {}
                Err(e) => return Err(e),
            }
        }
        let fut = async {
            let mut conn = HttpConnection::open(&self.inner.endpoint).await?;
            let res = conn.send(&req).await?;
            Ok((conn, res))
        };
        let (conn, res) = with_timeout(self.inner.timeout, "HTTP request", fut).await?;
        self.put_idle(conn);
        Ok(res)
    }

    /// `GET path` expecting 2xx with a JSON body.
    pub async fn get_json(&self, path: &str, token: Option<&str>) -> Result<Value> {
        expect_json(self.request("GET", path, token, None).await?)
    }

    /// `POST path` with a JSON body, expecting 2xx with a JSON body.
    pub async fn post_json(&self, path: &str, token: Option<&str>, body: &Value) -> Result<Value> {
        expect_json(self.request("POST", path, token, Some(body)).await?)
    }

    /// `GET /info`.
    pub async fn info(&self) -> Result<Value> {
        self.get_json("/info", None).await
    }

    /// `POST /auth/register` (no proof of work: for servers with `POW_REGISTER_BITS=0`). The
    /// answer is `{"status":"ready"}` without e-mail verification, `{"status":"verification_sent"}`
    /// with it.
    pub async fn register(&self, username: &str, email: &str, password: &str) -> Result<Value> {
        self.post_json(
            "/auth/register",
            None,
            &json!({"username": username, "email": email, "password": password}),
        )
        .await
    }

    /// `POST /auth/login` with a user name or e-mail address.
    pub async fn login(&self, login: &str, password: &str, client_label: Option<&str>) -> Result<Login> {
        let mut body = json!({"login": login, "password": password});
        if let Some(label) = client_label {
            body["clientLabel"] = json!(label);
        }
        let answer = self.post_json("/auth/login", None, &body).await?;
        if answer.get("mfaRequired").and_then(Value::as_bool) == Some(true) {
            let mfa_token = answer.get("mfaToken").and_then(Value::as_str).unwrap_or_default().to_string();
            let expires_in = answer.get("expiresIn").and_then(Value::as_u64).unwrap_or_default();
            return Ok(Login::MfaRequired { mfa_token, expires_in });
        }
        AuthSession::from_body(&answer).map(Login::Session)
    }

    /// `POST /auth/login/mfa` with an authenticator code (or a recovery code).
    pub async fn login_mfa(&self, mfa_token: &str, code: &str) -> Result<AuthSession> {
        let answer =
            self.post_json("/auth/login/mfa", None, &json!({"mfaToken": mfa_token, "code": code})).await?;
        AuthSession::from_body(&answer)
    }

    /// Registers an account and signs it in (servers without e-mail verification).
    pub async fn register_and_login(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<AuthSession> {
        self.register(username, email, password).await?;
        match self.login(username, password, Some("scacelith-client")).await? {
            Login::Session(session) => Ok(session),
            Login::MfaRequired { .. } => {
                Err(ClientError::Unexpected("a new account asks for a second factor".into()))
            }
        }
    }

    /// `POST /auth/logout`: revokes this session.
    pub async fn logout(&self, token: &str) -> Result<()> {
        self.post_json("/auth/logout", Some(token), &json!({})).await.map(drop)
    }

    /// `POST /auth/logout-all`: revokes every session of the account.
    pub async fn logout_all(&self, token: &str) -> Result<()> {
        self.post_json("/auth/logout-all", Some(token), &json!({})).await.map(drop)
    }

    /// `GET /account/me`.
    pub async fn me(&self, token: &str) -> Result<Value> {
        self.get_json("/account/me", Some(token)).await
    }
}

/// The JSON body of a 2xx answer, [`ClientError::Api`] otherwise.
fn expect_json(res: Response) -> Result<Value> {
    if !res.is_success() {
        return Err(ClientError::Api(Box::new(res.api_error())));
    }
    if res.body.is_empty() {
        return Ok(Value::Null);
    }
    res.json()
}
