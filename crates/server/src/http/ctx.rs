//! What a handler receives ([`Ctx`]) and the authentication hook ([`Authenticator`]) that the
//! auth module implements.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use http::{HeaderMap, Method};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use super::answer::ApiError;
use super::rates::{RateLimits, RateSpec, Taken};
use crate::ids::UserId;
use crate::net::guard::AddressKeys;

/// The session behind a valid bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfo {
    /// The account.
    pub user_id: UserId,
    /// Its username.
    pub username: String,
    /// The session row.
    pub session_id: i64,
    /// Whether the account's e-mail address is confirmed.
    pub email_verified: bool,
    /// The digest of the token, when the validator knows it.
    pub token_hash: Option<String>,
}

/// Validates the bearer tokens of the API (implemented by the auth module).
///
/// `Ok(None)` refuses the token (401 `invalid_token`); an error is answered as it is (for
/// example 503 `server_busy` when the store cannot answer).
pub trait Authenticator: Send + Sync + 'static {
    /// Looks up the session of `token` (1 to 512 printable ASCII characters).
    fn validate_token(&self, token: &str) -> impl Future<Output = Result<Option<AuthInfo>, ApiError>> + Send;
}

/// An authenticator that refuses every token (a server without accounts, tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAuthenticator;

impl Authenticator for NoAuthenticator {
    async fn validate_token(&self, _token: &str) -> Result<Option<AuthInfo>, ApiError> {
        Ok(None)
    }
}

type AuthFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<AuthInfo>, ApiError>> + Send + 'a>>;

/// The object-safe form of [`Authenticator`], stored by the API.
pub(crate) trait DynAuthenticator: Send + Sync {
    fn validate<'a>(&'a self, token: &'a str) -> AuthFuture<'a>;
}

impl<T: Authenticator> DynAuthenticator for T {
    fn validate<'a>(&'a self, token: &'a str) -> AuthFuture<'a> {
        Box::pin(self.validate_token(token))
    }
}

/// The request as a handler sees it.
pub struct Ctx {
    /// The client's address (the peer, or the trusted proxy's `X-Forwarded-For`).
    pub ip: IpAddr,
    /// The method (`GET` for a `HEAD`, which runs the GET handler).
    pub method: Method,
    /// The request headers.
    pub headers: HeaderMap,
    /// The decoded path parameters, in path order.
    pub params: Vec<(String, String)>,
    /// The query: the validated object when the route has a query schema, else every parameter
    /// as a string (the first occurrence of a name wins).
    pub query: Map<String, Value>,
    /// The validated body (`{}` for methods without a body or an empty body).
    pub body: Value,
    /// The session, when a valid token came with the request.
    pub auth: Option<AuthInfo>,
    /// Wall-clock time of the request (ms since the epoch).
    pub now_ms: i64,
    /// The route's path pattern (`/api/v1/players/:username`).
    pub route: String,
    pub(crate) keys: AddressKeys,
    pub(crate) rates: Arc<RateLimits>,
    pub(crate) taken: Arc<Mutex<Vec<Taken>>>,
}

impl fmt::Debug for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("method", &self.method)
            .field("route", &self.route)
            .field("params", &self.params)
            .field("user", &self.user_id())
            .finish()
    }
}

impl Ctx {
    /// The decoded path parameter `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The query parameter `name` when it is a string.
    pub fn query_str(&self, name: &str) -> Option<&str> {
        self.query.get(name).and_then(Value::as_str)
    }

    /// The signed-in account.
    pub fn user_id(&self) -> Option<UserId> {
        self.auth.as_ref().map(|a| a.user_id)
    }

    /// Takes more rates during the handler (all or none). The tokens join the route's: a
    /// `refund_rate` answer gives them back too. A refusal gives back only this batch and is
    /// counted toward an address block like a route limit when the handler returns it.
    pub fn take_rates(&self, rates: &[RateSpec]) -> Result<(), ApiError> {
        let more = self.rates.check(rates, &self.keys, self.user_id())?;
        self.taken.lock().extend(more);
        Ok(())
    }
}
