//! Accounts and authentication (DESIGN.md sections 3, 5.9 and 8): sessions, passwords,
//! registration, e-mail confirmation and change, password reset and change, two-step verification
//! (TOTP and recovery codes), Google sign-in, brute-force defences, proof of work and security
//! events. Owner: auth. See docs/RUST-PORT.md section 8.
//!
//! ```ignore
//! let auth = Auth::new(AuthDeps::new(config, store, mailer, session_events))?;
//! let session = auth.validate_token(token).await?;   // the Bearer hook and the realtime Hello
//! let answer = auth.login(&LoginParams { .. }).await?;
//! ```
//!
//! Password hashing: every hash and verification (the dummy one of unknown accounts included)
//! goes through one hash limiter (`PASSWORD_HASH_CONCURRENCY` at once, `PASSWORD_HASH_QUEUE_MAX`
//! waiting, whole server). Each request gets one budget: all its hashes together wait at most
//! `PASSWORD_HASH_QUEUE_TIMEOUT_MS`, and once the queue is half full one client source (an IPv4
//! address or an IPv6 /48) may have at most `PASSWORD_HASH_WAITERS_PER_SOURCE` hashes waiting. A
//! refused hash fails the request before the account changed, with 503 `server_busy` or 429
//! `rate_limited` (whose rate tokens the HTTP layer gives back, [`AuthError::refund_rate`]). A new
//! password hash is only written while the stored one is still the hash the request checked, so
//! a reset always wins a race.
//!
//! Errors are [`AuthError`]s: an answer (status, code, message, extra fields) or an internal
//! failure (500).

mod accounts;
mod error;
mod events;
pub(crate) mod http;
mod identity;
mod login;
mod mfa;
mod oidc;
mod sessions;
mod sso;
mod tokens;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

pub use self::error::{AuthError, AuthResult, BUSY_RETRY_AFTER_SEC};
pub use self::events::SecurityEvents;
pub use self::identity::{
    RESERVED_USERNAMES, USERNAME_PATTERN, UsernameRules, check_username, is_valid_email, mask_email,
    normalize_email, suggest_username,
};
pub use self::oidc::{
    OidcClient, OidcEndpoints, OidcError, check_redirect_uri, decode_jwt, form_urlencode, pkce_challenge,
};
pub use self::sessions::{NewSessionToken, SESSION_CACHE_TTL_MS, SESSION_TOUCH_EVERY_MS};
pub use self::tokens::{is_link_token, is_prefixed_token};

use self::accounts::FactorRule;
use self::mfa::SecondFactor;
use self::sessions::{SESSION_CACHE_SIZE, Sessions};
use crate::clock::{self, SharedClock};
use crate::config::{Config, TlsMode};
use crate::events::SessionEvents;
use crate::ids::UserId;
use crate::log::Logger;
use crate::mail::Mailer;
use crate::security::keys::{AuthKeys, KeyError};
use crate::security::password::{
    Argon2Hasher, Argon2Params, CheckFloor, HashLimiter, HashLimiterConfig, LimitedHasher,
    LimiterConfigError, PasswordHasher, log_warm_up_failure,
};
use crate::security::pow::Pow;
use crate::security::ratelimit::{FailureCounter, FailureCounterConfig, LocalControl, LoginPowDetector};
use crate::security::secret_box::SecretBox;
use crate::store::{Store, User};

/// A validated session (RUST-PORT.md section 8.3): what the HTTP Bearer hook and the realtime
/// `Hello` need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// The account.
    pub user_id: UserId,
    /// Its username.
    pub username: String,
    /// The session's id (`DELETE /auth/sessions/:id`, the `current` flag of the list).
    pub session_id: i64,
    /// Whether the account's address is confirmed.
    pub email_verified: bool,
    /// SHA-256 of the session token (the realtime layer closes the connection opened with a
    /// revoked token).
    pub token_hash: [u8; 32],
}

/// A proof-of-work answer (`pow` of the register and login bodies).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowAnswer {
    /// The challenge issued by the server.
    pub challenge: String,
    /// The decimal nonce found by the client.
    pub nonce: String,
}

/// `POST /auth/register`.
#[derive(Clone, Debug, Default)]
pub struct RegisterParams {
    pub username: String,
    pub email: String,
    pub password: String,
    pub pow: Option<PowAnswer>,
    /// The client address.
    pub ip: Option<String>,
}

/// `POST /auth/login`.
#[derive(Clone, Debug, Default)]
pub struct LoginParams {
    /// A username or an e-mail address.
    pub login: String,
    pub password: String,
    pub client_label: Option<String>,
    pub pow: Option<PowAnswer>,
    pub ip: Option<String>,
}

/// `POST /auth/login/mfa`.
#[derive(Clone, Debug, Default)]
pub struct MfaLoginParams {
    pub mfa_token: String,
    pub code: Option<String>,
    pub recovery_code: Option<String>,
    pub ip: Option<String>,
}

/// The credentials of a re-authenticated account change: the password, plus an authenticator
/// code or a recovery code when two-step verification is on.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub password: String,
    pub code: Option<String>,
    pub recovery_code: Option<String>,
}

impl Credentials {
    fn factor(&self) -> SecondFactor<'_> {
        SecondFactor { code: self.code.as_deref(), recovery_code: self.recovery_code.as_deref() }
    }
}

/// `POST /auth/sso/google/finish`.
#[derive(Clone, Debug, Default)]
pub struct SsoFinishParams {
    pub attempt_id: String,
    pub code_verifier: String,
    pub state: String,
    pub code: String,
    /// The `iss` Google sent to the game's listener, when it sent one.
    pub iss: Option<String>,
    pub client_label: Option<String>,
    pub ip: Option<String>,
}

/// `POST /auth/sso/google/link`.
#[derive(Clone, Debug, Default)]
pub struct SsoLinkParams {
    pub link_ticket: String,
    pub password: String,
    pub client_label: Option<String>,
    pub pow: Option<PowAnswer>,
    pub ip: Option<String>,
}

/// `POST /auth/sso/complete`.
#[derive(Clone, Debug, Default)]
pub struct SsoCompleteParams {
    pub sso_ticket: String,
    pub username: String,
    pub client_label: Option<String>,
    pub ip: Option<String>,
}

/// An answer whose status depends on the configuration (`register`, `change_email`).
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// HTTP status.
    pub status: u16,
    /// JSON body.
    pub body: Value,
}

/// The outcome of an e-mail confirmation link (`POST /verify-email`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The address is confirmed (or the pending signup's account created).
    Confirmed,
    /// Unknown, used or expired link.
    Invalid,
    /// The pending signup's username or address was taken meanwhile.
    Taken,
}

/// The outcome of an e-mail change link (`POST /confirm-email-change`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmailChangeOutcome {
    /// The address changed to `email`.
    Changed { email: String },
    /// Unknown, used, expired or stale link.
    Invalid,
    /// Another account took the address meanwhile.
    Taken,
}

/// The Google sign-in settings beyond the configuration.
#[derive(Clone, Debug)]
pub struct OidcOptions {
    /// The provider's endpoints (default: Google's).
    pub endpoints: OidcEndpoints,
    /// Allows plain HTTP endpoints (tests against a local fake provider).
    pub allow_http: bool,
}

impl Default for OidcOptions {
    fn default() -> OidcOptions {
        OidcOptions { endpoints: OidcEndpoints::google(), allow_http: false }
    }
}

/// What the auth service is built from.
#[derive(Clone)]
pub struct AuthDeps {
    pub config: Arc<Config>,
    pub store: Store,
    pub mailer: Mailer,
    /// Told about revoked sessions (the realtime layer closes their connections).
    pub session_events: Arc<dyn SessionEvents>,
    /// Wall time of tokens, sessions and events; monotonic time of the counters.
    pub clock: SharedClock,
    pub log: Logger,
    /// The whole-server control counters and single-use keys (shared with the HTTP layer when
    /// given; otherwise the service has its own).
    pub control: Option<Arc<LocalControl>>,
    /// Replaces the Argon2id hasher (tests).
    pub password_hasher: Option<Arc<dyn PasswordHasher>>,
    /// Google sign-in endpoints.
    pub oidc: OidcOptions,
    /// Sessions kept in the validation cache.
    pub session_cache_size: usize,
}

impl AuthDeps {
    /// The dependencies of the server: system clock, `auth` logger, own control counters, the
    /// default hasher and Google's endpoints.
    pub fn new(
        config: Arc<Config>,
        store: Store,
        mailer: Mailer,
        session_events: Arc<dyn SessionEvents>,
    ) -> AuthDeps {
        AuthDeps {
            config,
            store,
            mailer,
            session_events,
            clock: clock::system(),
            log: Logger::root().child("auth"),
            control: None,
            password_hasher: None,
            oidc: OidcOptions::default(),
            session_cache_size: SESSION_CACHE_SIZE,
        }
    }
}

impl fmt::Debug for AuthDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthDeps")
            .field("store", &self.store)
            .field("oidc", &self.oidc)
            .finish_non_exhaustive()
    }
}

/// Why the auth service could not be built.
#[derive(Debug)]
pub enum AuthInitError {
    /// `SERVER_SECRET` is too short.
    Keys(KeyError),
    /// The `PASSWORD_HASH_*` settings are out of range.
    HashLimiter(LimiterConfigError),
}

impl fmt::Display for AuthInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthInitError::Keys(e) => write!(f, "auth keys: {e}"),
            AuthInitError::HashLimiter(e) => write!(f, "password hash limiter: {e}"),
        }
    }
}

impl std::error::Error for AuthInitError {}

/// The public base URL of the server's pages (e-mail links): `http` when `TLS_MODE=off`, the
/// public host as written (an IPv6 literal in brackets), the port unless it is the scheme's
/// default.
pub fn public_base_url(config: &Config) -> String {
    let https = config.tls_mode != TlsMode::Off;
    let (proto, default_port) = if https { ("https", 443) } else { ("http", 80) };
    let mut host = config.server_public_host.clone();
    if host.contains(':') && !host.starts_with('[') {
        host = format!("[{host}]");
    }
    if config.public_api_port == default_port {
        format!("{proto}://{host}")
    } else {
        format!("{proto}://{host}:{}", config.public_api_port)
    }
}

/// The shared state of the service; the flows add their methods in their own modules.
pub(crate) struct Inner {
    config: Arc<Config>,
    store: Store,
    clock: SharedClock,
    log: Logger,
    keys: AuthKeys,
    control: Arc<LocalControl>,
    hasher: LimitedHasher,
    secret_box: SecretBox,
    pow: Pow,
    events: SecurityEvents,
    mailer: Mailer,
    sessions: Sessions,
    /// Login failures per login string (`l:<login>`).
    failures: FailureCounter,
    /// Wrong codes of the MFA step per account (`m<id>`).
    mfa_failures: FailureCounter,
    /// Failed re-authentications per account (`r<id>`).
    reauth_failures: FailureCounter,
    login_pow: LoginPowDetector,
    oidc: Option<OidcClient>,
    base_url: String,
    usernames: UsernameRules,
}

impl Inner {
    /// Wall time in ms.
    fn now(&self) -> i64 {
        self.clock.wall_ms()
    }
}

/// The auth service (module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth").field("sso", &self.inner.sso_enabled()).finish_non_exhaustive()
    }
}

impl Auth {
    /// Builds the service. When called on a runtime, the password hasher's warm-up starts in the
    /// background (so that the first login of an unknown user is not faster).
    pub fn new(deps: AuthDeps) -> Result<Auth, AuthInitError> {
        let AuthDeps {
            config,
            store,
            mailer,
            session_events,
            clock,
            log,
            control,
            password_hasher,
            oidc,
            session_cache_size,
        } = deps;
        let keys = AuthKeys::from_config(&config).map_err(AuthInitError::Keys)?;
        let control = control.unwrap_or_else(|| Arc::new(LocalControl::new(clock.clone())));
        let limiter =
            HashLimiter::new(HashLimiterConfig::from_config(&config)).map_err(AuthInitError::HashLimiter)?;
        let hasher_impl =
            password_hasher.unwrap_or_else(|| Arc::new(Argon2Hasher::new(Argon2Params::DEFAULT)));
        let hasher =
            LimitedHasher::new(hasher_impl, limiter, Arc::new(CheckFloor::new(clock.clone())), log.clone());
        let threshold = |v: i64| u32::try_from(v.max(1)).unwrap_or(u32::MAX);
        let failures_of =
            |n: u32| FailureCounter::new(FailureCounterConfig::with_threshold(n), clock.clone());
        let oidc = (config.sso_google_enabled && !config.google_client_id.is_empty()).then(|| {
            let secret = config.google_client_secret.as_ref().map_or("", |s| s.as_str());
            OidcClient::new(&config.google_client_id, secret, oidc.endpoints, clock.clone(), oidc.allow_http)
        });
        let inner = Inner {
            secret_box: SecretBox::from_key(&keys.mfa),
            pow: Pow::from_keys(&keys, control.clone(), clock.clone()),
            events: SecurityEvents::new(store.clone(), log.clone(), clock.clone()),
            sessions: Sessions::new(
                &config,
                store.clone(),
                clock.clone(),
                log.clone(),
                session_events,
                session_cache_size,
            ),
            failures: failures_of(threshold(config.auth_failures_per_account)),
            mfa_failures: failures_of(5),
            reauth_failures: failures_of(threshold(config.auth_failures_per_account)),
            login_pow: LoginPowDetector::from_config(&config, control.clone(), clock.clone()),
            base_url: public_base_url(&config),
            usernames: UsernameRules::from_config(&config),
            oidc,
            keys,
            control,
            hasher,
            mailer,
            store,
            clock,
            log,
            config,
        };
        let auth = Auth { inner: Arc::new(inner) };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let inner = auth.inner.clone();
            rt.spawn(async move {
                if let Err(e) = inner.hasher.warm_up().await {
                    log_warm_up_failure(&inner.log, &e);
                }
            });
        }
        Ok(auth)
    }

    // ---- sessions ------------------------------------------------------------------------------

    /// Validates a session token (RUST-PORT.md section 8.3): `None` when it is malformed, unknown,
    /// revoked, expired, or its account is no longer active. Answers come from a cache for at
    /// most 30 s; revocations made through this service take effect at once.
    pub async fn validate_token(&self, token: &str) -> Result<Option<SessionInfo>, AuthError> {
        self.inner.sessions.validate(token).await
    }

    /// Drops cached sessions: those of `token_hashes` (lower hex), or, when empty, every session
    /// of `user_id`. For revocations written to the database by another process.
    pub fn invalidate(&self, user_id: Option<UserId>, token_hashes: &[String]) {
        self.inner.sessions.invalidate(user_id, token_hashes);
    }

    /// Opens a session for `user` without a password (tools and tests); the login answers open
    /// theirs themselves.
    pub async fn create_session(
        &self,
        user: &User,
        client_label: Option<&str>,
        ip: Option<&str>,
    ) -> AuthResult<NewSessionToken> {
        self.inner.sessions.create(user, client_label, ip).await
    }

    /// `GET /auth/sessions`: `{sessions: [{id, createdAt, lastSeenAt, expiresAt, clientLabel,
    /// current}]}`, most recently used first.
    pub async fn list_sessions(&self, session: &SessionInfo) -> AuthResult<Value> {
        self.inner.list_sessions(session).await
    }

    /// `DELETE /auth/sessions/:id` (`id` as written in the path): `{status: 'revoked'}`, 404
    /// `not_found`.
    pub async fn revoke_session(
        &self,
        session: &SessionInfo,
        id: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.revoke_session(session, id, ip).await
    }

    /// `POST /auth/logout`: revokes the request's session.
    pub async fn logout(&self, session: &SessionInfo, ip: Option<&str>) -> AuthResult<Value> {
        self.inner.logout(session, ip).await
    }

    /// `POST /auth/logout-all`: revokes every session of the account.
    pub async fn logout_all(&self, session: &SessionInfo, ip: Option<&str>) -> AuthResult<Value> {
        self.inner.logout_all(session, ip).await
    }

    /// Sessions in the validation cache.
    pub fn session_cache_len(&self) -> usize {
        self.inner.sessions.cache_len()
    }

    // ---- registration and links ------------------------------------------------------------------

    /// `POST /auth/register`: 202 `{status: 'verification_sent'}` or 201 `{status: 'ready'}`.
    pub async fn register(&self, p: &RegisterParams) -> AuthResult<Answer> {
        self.inner.register(p).await
    }

    /// `POST /auth/verify-email/resend`: `{status: 'accepted'}` (202) whatever happens.
    pub async fn resend_verification(&self, email: &str, ip: Option<&str>) -> AuthResult<Value> {
        self.inner.resend_verification(email, ip).await
    }

    /// `GET /verify-email`: whether the confirmation link is live.
    pub async fn peek_verification(&self, token: &str) -> AuthResult<bool> {
        self.inner.peek_verification(token).await
    }

    /// `POST /verify-email`: uses the confirmation link.
    pub async fn verify_email(&self, token: &str, ip: Option<&str>) -> AuthResult<LinkOutcome> {
        self.inner.verify_email(token, ip).await
    }

    /// `GET /reset-password`: whether the reset link is live.
    pub async fn peek_reset_token(&self, token: &str) -> AuthResult<bool> {
        Ok(self.inner.peek_token(tokens::PASSWORD_RESET, token).await?.is_some())
    }

    /// `POST /auth/password/forgot`: `{status: 'accepted'}` (202) whatever happens.
    pub async fn forgot_password(&self, email: &str, ip: Option<&str>) -> AuthResult<Value> {
        self.inner.forgot_password(email, ip).await
    }

    /// `POST /auth/password/reset` and the reset page: `{status: 'password_reset'}`.
    pub async fn reset_password(
        &self,
        token: &str,
        new_password: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.reset_password(token, new_password, ip).await
    }

    /// `GET /confirm-email-change`: the account name and new address of a live link.
    pub async fn peek_email_change(&self, token: &str) -> AuthResult<Option<(String, String)>> {
        self.inner.peek_email_change(token).await
    }

    /// `POST /confirm-email-change`: uses the link and applies the change.
    pub async fn confirm_email_change(
        &self,
        token: &str,
        ip: Option<&str>,
    ) -> AuthResult<EmailChangeOutcome> {
        self.inner.confirm_email_change(token, ip).await
    }

    // ---- login -----------------------------------------------------------------------------------

    /// `POST /auth/login`: `{token, expiresAt, user}` or `{mfaRequired, mfaToken, expiresIn}`.
    pub async fn login(&self, p: &LoginParams) -> AuthResult<Value> {
        self.inner.login(p).await
    }

    /// `POST /auth/login/mfa`: `{token, expiresAt, user}`.
    pub async fn login_mfa(&self, p: &MfaLoginParams) -> AuthResult<Value> {
        self.inner.login_mfa(p).await
    }

    /// True while the login proof of work is on (a wave of failed logins).
    pub fn login_pow_active(&self) -> bool {
        self.inner.login_pow_active()
    }

    // ---- account ---------------------------------------------------------------------------------

    /// `GET /account/me`: `{user, ratings, sanctions, ban}`.
    pub async fn me(&self, session: &SessionInfo) -> AuthResult<Value> {
        self.inner.me(session.user_id).await
    }

    /// The account as `GET /account/me` shows it.
    pub async fn account_view(&self, user: &User) -> Value {
        self.inner.account_view(user).await
    }

    /// `POST /account/password`: `{status: 'password_changed'}`; the other sessions are revoked.
    pub async fn change_password(
        &self,
        session: &SessionInfo,
        current_password: &str,
        new_password: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.change_password(session, current_password, new_password, ip).await
    }

    /// `PUT /account/preferences`: `{preferences: {acceptChallenges}}` (`'all'` or `'none'`).
    pub async fn set_preferences(&self, session: &SessionInfo, accept_challenges: &str) -> AuthResult<Value> {
        self.inner.set_preferences(session, accept_challenges).await
    }

    /// `POST /account/email`: 202 `{status: 'verification_sent'}`, or 200 `{status:
    /// 'email_changed', email}` without confirmation.
    pub async fn change_email(
        &self,
        session: &SessionInfo,
        new_email: &str,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<Answer> {
        self.inner.change_email(session, new_email, creds, ip).await
    }

    /// `POST /account/delete`: `{status: 'deleted'}`; the account is anonymised.
    pub async fn delete_account(
        &self,
        session: &SessionInfo,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.delete_account(session, creds, ip).await
    }

    /// The re-authentication of `POST /account/export` (password, plus a code or a recovery code
    /// when two-step verification is on): the account to export. Record the export with
    /// [`Auth::record_export`] once the document is built.
    pub async fn reauth_for_export(
        &self,
        session: &SessionInfo,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<User> {
        let budget = self.inner.hasher.budget(ip);
        self.inner
            .reauth(session.user_id, &creds.password, creds.factor(), FactorRule::Any, ip, &budget)
            .await
    }

    /// Records the `account_exported` security event.
    pub fn record_export(&self, user_id: UserId, ip: Option<&str>) {
        self.inner.events.record("account_exported", Some(user_id), ip, None);
    }

    // ---- two-step verification -------------------------------------------------------------------

    /// `POST /account/mfa/totp/setup`: `{secret, uri, algorithm, digits, period}`.
    pub async fn mfa_setup(
        &self,
        session: &SessionInfo,
        password: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.mfa_setup(session, password, ip).await
    }

    /// `POST /account/mfa/totp/enable`: `{status: 'mfa_enabled', recoveryCodes}`.
    pub async fn mfa_enable(&self, session: &SessionInfo, code: &str, ip: Option<&str>) -> AuthResult<Value> {
        self.inner.mfa_enable(session, code, ip).await
    }

    /// `POST /account/mfa/totp/disable`: `{status: 'mfa_disabled'}`.
    pub async fn mfa_disable(
        &self,
        session: &SessionInfo,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.mfa_disable(session, creds, ip).await
    }

    /// `POST /account/mfa/recovery-codes`: `{recoveryCodes}` (the password and an authenticator
    /// code).
    pub async fn regenerate_recovery_codes(
        &self,
        session: &SessionInfo,
        password: &str,
        code: Option<&str>,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.regenerate_recovery_codes(session, password, code, ip).await
    }

    // ---- Google sign-in --------------------------------------------------------------------------

    /// True when Google sign-in is on (`SSO_GOOGLE_ENABLED` and a client id).
    pub fn sso_enabled(&self) -> bool {
        self.inner.sso_enabled()
    }

    /// `POST /auth/sso/google/start`: `{attemptId, authUrl, state, expiresIn}`.
    pub async fn sso_start(
        &self,
        code_challenge: &str,
        redirect_port: u16,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        self.inner.sso_start(code_challenge, redirect_port, ip).await
    }

    /// `POST /auth/sso/google/finish`: a login answer, `{needsUsername, ssoTicket,
    /// suggestedUsername}` or `{needsPassword, linkTicket, username, expiresIn}`.
    pub async fn sso_finish(&self, p: &SsoFinishParams) -> AuthResult<Value> {
        self.inner.sso_finish(p).await
    }

    /// `POST /auth/sso/google/link`: a login answer.
    pub async fn sso_link(&self, p: &SsoLinkParams) -> AuthResult<Value> {
        self.inner.sso_link(p).await
    }

    /// `POST /auth/sso/complete`: the login answer of the new account.
    pub async fn sso_complete(&self, p: &SsoCompleteParams) -> AuthResult<Value> {
        self.inner.sso_complete(p).await
    }

    // ---- security events -------------------------------------------------------------------------

    /// The security events (record, flush).
    pub fn events(&self) -> &SecurityEvents {
        &self.inner.events
    }

    /// Saves the pending security events (shutdown).
    pub async fn close(&self) {
        self.inner.events.flush().await;
    }
}
