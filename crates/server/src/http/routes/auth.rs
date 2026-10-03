//! `/api/v1/auth/*` (except Google sign-in, [`super::sso`]); the pages of the e-mail links are in
//! [`crate::http::pages`] (DESIGN.md 5.9, docs/API.md).
//!
//! | endpoint | answer |
//! |---|---|
//! | `POST /auth/register {username, email, password, pow?}` | 202 `{status: 'verification_sent'}`, 201 `{status: 'ready'}` without e-mail confirmation |
//! | `POST /auth/login {login, password, clientLabel?, pow?}` | `{token, expiresAt, user}` or `{mfaRequired: true, mfaToken, expiresIn}` |
//! | `POST /auth/login/mfa {mfaToken, code?, recoveryCode?}` | `{token, expiresAt, user}` |
//! | `POST /auth/logout`, `POST /auth/logout-all` (session) | `{status: 'logged_out'}` |
//! | `GET /auth/sessions` (session) | `{sessions: [{id, createdAt, lastSeenAt, expiresAt, clientLabel, current}]}` |
//! | `DELETE /auth/sessions/:id` (session) | `{status: 'revoked'}`, 404 |
//! | `POST /auth/verify-email/resend {email}` | 202 `{status: 'accepted'}` |
//! | `POST /auth/password/forgot {email}` | 202 `{status: 'accepted'}` |
//! | `POST /auth/password/reset {token, newPassword}` | `{status: 'password_reset'}` |
//!
//! The errors are those of the auth service ([`crate::auth`]): 400 `invalid_username` |
//! `invalid_email` | `weak_password`, 401 `invalid_credentials` | `invalid_mfa_token` |
//! `invalid_code`, 403 `email_unverified` | `banned` | `registration_closed`, 409, 428
//! `pow_required`, 429 `too_many_attempts` | `rate_limited`, 503 `server_busy`.
//!
//! Limits: `auth` (`AUTH_RATE_PER_IP` per 10 minutes per address or IPv6 /64,
//! `AUTH_RATE_PER_PREFIX` per IPv6 /48, shared) on every endpoint that hashes a password or sends
//! an e-mail without a session; on top of it, per hour (3 times the limit per IPv6 /48, shared):
//! `auth_register`, `auth_mail` (resend), `auth_forgot` and `auth_forgot_day` (forgot),
//! `auth_reset` (reset). Their refusals count [`AUTH_REFUSAL_WEIGHT`] times toward a block of the
//! address. The session endpoints take `sessions` (60 per minute per account).

use std::future::Future;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::auth::{Auth, LoginParams, MfaLoginParams, PowAnswer, RegisterParams, SessionInfo};
use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RateSpec, RouteOpts, Router, Schema, Spec};
use crate::net::guard::AUTH_REFUSAL_WEIGHT;

/// What the auth endpoints need.
#[derive(Clone)]
pub struct AuthRouteDeps {
    /// The configuration (limits).
    pub config: Arc<Config>,
    /// The auth service.
    pub auth: Auth,
}

// ---- fields and limits shared by the account, SSO, export and page routes ----------------------

/// The optional `pow` answer of the register, login and link bodies.
pub fn pow_field() -> Spec {
    Spec::object(
        Schema::new().field("challenge", Spec::string().min_len(16).max_len(512)).field(
            "nonce",
            Spec::string()
                .min_len(1)
                .max_len(20)
                .pattern(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
        ),
    )
    .optional()
}

/// A password field (1 to 1024 characters; the policy says more).
pub fn password_field() -> Spec {
    Spec::string().min_len(1).max_len(1024)
}

/// An authenticator or recovery code field (1 to 32 characters).
pub fn code_field() -> Spec {
    Spec::string().min_len(1).max_len(32)
}

/// An e-mail address field (1 to 254 characters).
pub fn email_field() -> Spec {
    Spec::string().min_len(1).max_len(254)
}

/// The token of an e-mail link (1 to 128 characters; the service checks its shape).
pub fn link_token_field() -> Spec {
    Spec::string().min_len(1).max_len(128)
}

/// The optional `clientLabel` of the login answers (at most 64 characters).
pub fn client_label_field() -> Spec {
    Spec::string().max_len(64).optional()
}

/// The `page` limit of the GET pages of the e-mail links: 60 per minute per address, one bucket.
pub fn page_rate() -> RateSpec {
    RateSpec::new("page", 60.0, 60_000)
}

/// The `auth` limit: `AUTH_RATE_PER_IP` per 10 minutes per address, `AUTH_RATE_PER_PREFIX` per
/// IPv6 /48, shared, refusals weighing [`AUTH_REFUSAL_WEIGHT`].
pub fn auth_rate_of(config: &Config) -> RateSpec {
    with_prefix(
        RateSpec::new("auth", config.auth_rate_per_ip as f64, 600_000)
            .shared()
            .abuse_weight(AUTH_REFUSAL_WEIGHT),
        config.auth_rate_per_prefix,
    )
}

/// A limit of the auth family on top of `auth`: `limit` per `window_ms` per address, 3 times that
/// per IPv6 /48, shared, refusals weighing [`AUTH_REFUSAL_WEIGHT`].
pub fn auth_family_rate(key: &'static str, limit: i64, window_ms: u64) -> RateSpec {
    with_prefix(
        RateSpec::new(key, limit as f64, window_ms).shared().abuse_weight(AUTH_REFUSAL_WEIGHT),
        3 * limit,
    )
}

fn with_prefix(rate: RateSpec, prefix_limit: i64) -> RateSpec {
    if prefix_limit > 0 { rate.prefix_limit(prefix_limit as f64) } else { rate }
}

// ---- request helpers -----------------------------------------------------------------------------

/// A handler bound to its dependencies: `f(deps, ctx)` for each request.
pub(crate) fn bind<D, F, Fut>(deps: &Arc<D>, f: F) -> impl Fn(Ctx) -> Fut + Send + Sync + 'static
where
    D: Send + Sync + 'static,
    F: Fn(Arc<D>, Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Answer, ApiError>> + Send + 'static,
{
    let deps = deps.clone();
    move |ctx| f(deps.clone(), ctx)
}

/// The session of a route that requires one.
pub(crate) fn session_of(ctx: &Ctx) -> Result<SessionInfo, ApiError> {
    ctx.auth
        .as_ref()
        .and_then(SessionInfo::from_auth_info)
        .ok_or_else(|| ApiError::internal("a session route ran without a session from the auth service"))
}

/// The client's address as the auth service records it.
pub(crate) fn ip_of(ctx: &Ctx) -> String {
    ctx.ip.to_string()
}

/// A string field of a validated body (`""` when absent).
pub(crate) fn text(body: &Value, key: &str) -> String {
    body.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// An optional string field of a validated body.
pub(crate) fn opt_text(body: &Value, key: &str) -> Option<String> {
    body.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The `pow` answer of a validated body.
pub(crate) fn pow_of(body: &Value) -> Option<PowAnswer> {
    let pow: &Map<String, Value> = body.get("pow")?.as_object()?;
    Some(PowAnswer {
        challenge: pow.get("challenge")?.as_str()?.to_owned(),
        nonce: pow.get("nonce")?.as_str()?.to_owned(),
    })
}

// ---- endpoints -----------------------------------------------------------------------------------

/// Registers the endpoints.
pub fn register(router: &mut Router, deps: AuthRouteDeps) {
    let config = deps.config.clone();
    let d = Arc::new(deps);
    let auth_rate = auth_rate_of(&config);
    let register_rate = auth_family_rate("auth_register", config.auth_register_per_hour, 3_600_000);
    let mail_rate = auth_family_rate("auth_mail", config.auth_mail_per_hour, 3_600_000);
    let forgot_rate = auth_family_rate("auth_forgot", config.auth_forgot_per_hour, 3_600_000);
    let forgot_day_rate = auth_family_rate("auth_forgot_day", config.auth_forgot_per_day, 86_400_000);
    let reset_rate = auth_family_rate("auth_reset", config.auth_reset_per_hour, 3_600_000);
    let session_opts =
        || RouteOpts::new().auth(AuthMode::Required).rate(RateSpec::new("sessions", 60.0, 60_000).by_user());

    router.post(
        "/auth/register",
        RouteOpts::new().rate(auth_rate.clone()).rate(register_rate).body(
            Schema::new()
                .field("username", Spec::string().min_len(1).max_len(64))
                .field("email", email_field())
                .field("password", password_field())
                .field("pow", pow_field()),
        ),
        bind(&d, register_account),
    );
    router.post(
        "/auth/login",
        RouteOpts::new().rate(auth_rate.clone()).body(
            Schema::new()
                .field("login", Spec::string().min_len(1).max_len(254))
                .field("password", password_field())
                .field("clientLabel", client_label_field())
                .field("pow", pow_field()),
        ),
        bind(&d, login),
    );
    router.post(
        "/auth/login/mfa",
        RouteOpts::new().rate(auth_rate.clone()).body(
            Schema::new()
                .field("mfaToken", Spec::string().min_len(1).max_len(64))
                .field("code", Spec::string().max_len(32).optional())
                .field("recoveryCode", Spec::string().max_len(32).optional()),
        ),
        bind(&d, login_mfa),
    );

    router.post("/auth/logout", session_opts(), bind(&d, logout));
    router.post("/auth/logout-all", session_opts(), bind(&d, logout_all));
    router.get("/auth/sessions", session_opts(), bind(&d, list_sessions));
    router.delete("/auth/sessions/:id", session_opts(), bind(&d, revoke_session));

    router.post(
        "/auth/verify-email/resend",
        RouteOpts::new()
            .rate(auth_rate.clone())
            .rate(mail_rate)
            .body(Schema::new().field("email", email_field())),
        bind(&d, resend_verification),
    );
    router.post(
        "/auth/password/forgot",
        RouteOpts::new()
            .rate(auth_rate.clone())
            .rate(forgot_rate)
            .rate(forgot_day_rate)
            .body(Schema::new().field("email", email_field())),
        bind(&d, forgot_password),
    );
    router.post(
        "/auth/password/reset",
        RouteOpts::new()
            .rate(auth_rate)
            .rate(reset_rate)
            .body(Schema::new().field("token", link_token_field()).field("newPassword", password_field())),
        bind(&d, reset_password),
    );
}

async fn register_account(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let params = RegisterParams {
        username: text(b, "username"),
        email: text(b, "email"),
        password: text(b, "password"),
        pow: pow_of(b),
        ip: Some(ip_of(&ctx)),
    };
    let a = d.auth.register(&params).await?;
    Ok(Answer::json(a.body).status(a.status))
}

async fn login(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let params = LoginParams {
        login: text(b, "login"),
        password: text(b, "password"),
        client_label: opt_text(b, "clientLabel"),
        pow: pow_of(b),
        ip: Some(ip_of(&ctx)),
    };
    Ok(Answer::json(d.auth.login(&params).await?))
}

async fn login_mfa(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let params = MfaLoginParams {
        mfa_token: text(b, "mfaToken"),
        code: opt_text(b, "code"),
        recovery_code: opt_text(b, "recoveryCode"),
        ip: Some(ip_of(&ctx)),
    };
    Ok(Answer::json(d.auth.login_mfa(&params).await?))
}

async fn logout(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.logout(&session, Some(&ip_of(&ctx))).await?))
}

async fn logout_all(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.logout_all(&session, Some(&ip_of(&ctx))).await?))
}

async fn list_sessions(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.list_sessions(&session).await?))
}

async fn revoke_session(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    let id = ctx.param("id").unwrap_or_default();
    Ok(Answer::json(d.auth.revoke_session(&session, id, Some(&ip_of(&ctx))).await?))
}

async fn resend_verification(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let body = d.auth.resend_verification(&text(&ctx.body, "email"), Some(&ip_of(&ctx))).await?;
    Ok(Answer::json(body).status(202))
}

async fn forgot_password(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let body = d.auth.forgot_password(&text(&ctx.body, "email"), Some(&ip_of(&ctx))).await?;
    Ok(Answer::json(body).status(202))
}

async fn reset_password(d: Arc<AuthRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let body = d.auth.reset_password(&text(b, "token"), &text(b, "newPassword"), Some(&ip_of(&ctx))).await?;
    Ok(Answer::json(body))
}
