//! `/api/v1/account/*` (DESIGN.md 5.9, docs/API.md): every route needs a session. The e-mail
//! change link's pages are in [`crate::http::pages`].
//!
//! | endpoint | answer |
//! |---|---|
//! | `GET /account/me` | `{user, ratings, sanctions, ban}` |
//! | `POST /account/password {currentPassword, newPassword}` | `{status: 'password_changed'}` (other sessions revoked) |
//! | `POST /account/mfa/totp/setup {password}` | `{secret, uri, algorithm, digits, period}` |
//! | `POST /account/mfa/totp/enable {code}` | `{status: 'mfa_enabled', recoveryCodes}` |
//! | `POST /account/mfa/totp/disable {password, code?, recoveryCode?}` | `{status: 'mfa_disabled'}` |
//! | `POST /account/mfa/recovery-codes {password, code}` | `{recoveryCodes}` |
//! | `POST /account/delete {password, code?, recoveryCode?}` | `{status: 'deleted'}` |
//! | `POST /account/email {newEmail, password, code?, recoveryCode?}` | 202 `{status: 'verification_sent'}`, or 200 `{status: 'email_changed', email}` without e-mail confirmation |
//! | `PUT /account/preferences {acceptChallenges}` | `{preferences: {acceptChallenges}}` |
//!
//! Re-authentication errors: 403 `invalid_password` | `mfa_code_required` | `invalid_code`, 400
//! `password_not_set`, 429 `too_many_attempts` | `rate_limited`, 503 `server_busy`.
//!
//! Limits of the routes that ask for the password or a code ([`reauth_rates_of`]): `reauth`
//! (`AUTH_RATE_PER_IP` per 10 minutes per address, `AUTH_RATE_PER_PREFIX` per IPv6 /48, shared)
//! and `reauth_user` (`AUTH_REAUTH_PER_USER` per 10 minutes per account, shared). The reads and
//! the preferences take `account` (60 per minute per account).

use std::sync::Arc;

use serde_json::json;

use super::auth::{bind, code_field, email_field, ip_of, opt_text, password_field, session_of, text};
use crate::auth::{Auth, Credentials};
use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, Ctx, RateSpec, RouteOpts, Router, Schema, Spec};
use crate::net::guard::AUTH_REFUSAL_WEIGHT;

/// What the account endpoints need.
#[derive(Clone)]
pub struct AccountRouteDeps {
    /// The configuration (limits).
    pub config: Arc<Config>,
    /// The auth service.
    pub auth: Auth,
}

/// The limits of a route that asks for the password: `reauth` per address and IPv6 /48, then
/// `reauth_user` per account (the export takes them too).
pub fn reauth_rates_of(config: &Config) -> [RateSpec; 2] {
    let mut per_address = RateSpec::new("reauth", config.auth_rate_per_ip as f64, 600_000)
        .shared()
        .abuse_weight(AUTH_REFUSAL_WEIGHT);
    if config.auth_rate_per_prefix > 0 {
        per_address = per_address.prefix_limit(config.auth_rate_per_prefix as f64);
    }
    [
        per_address,
        RateSpec::new("reauth_user", config.auth_reauth_per_user as f64, 600_000).by_user().shared(),
    ]
}

/// The credentials of a body `{password, code?, recoveryCode?}`.
pub(crate) fn credentials_of(ctx: &Ctx) -> Credentials {
    Credentials {
        password: text(&ctx.body, "password"),
        code: opt_text(&ctx.body, "code"),
        recovery_code: opt_text(&ctx.body, "recoveryCode"),
    }
}

/// The schema of a body `{password, code?, recoveryCode?}`.
pub(crate) fn credentials_schema() -> Schema {
    Schema::new()
        .field("password", password_field())
        .field("code", code_field().optional())
        .field("recoveryCode", code_field().optional())
}

/// Registers the endpoints.
pub fn register(router: &mut Router, deps: AccountRouteDeps) {
    let [reauth, reauth_user] = reauth_rates_of(&deps.config);
    let d = Arc::new(deps);
    let read_rate = RateSpec::new("account", 60.0, 60_000).by_user();
    let reauth_opts = |body: Schema| {
        RouteOpts::new().auth(AuthMode::Required).rate(reauth.clone()).rate(reauth_user.clone()).body(body)
    };

    router.get(
        "/account/me",
        RouteOpts::new().auth(AuthMode::Required).rate(read_rate.clone()),
        bind(&d, me),
    );
    router.post(
        "/account/password",
        reauth_opts(
            Schema::new().field("currentPassword", password_field()).field("newPassword", password_field()),
        ),
        bind(&d, change_password),
    );
    router.post(
        "/account/mfa/totp/setup",
        reauth_opts(Schema::new().field("password", password_field())),
        bind(&d, mfa_setup),
    );
    router.post(
        "/account/mfa/totp/enable",
        reauth_opts(
            Schema::new().field(
                "code",
                Spec::string()
                    .min_len(6)
                    .max_len(6)
                    .pattern(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit())),
            ),
        ),
        bind(&d, mfa_enable),
    );
    router.post("/account/mfa/totp/disable", reauth_opts(credentials_schema()), bind(&d, mfa_disable));
    router.post(
        "/account/mfa/recovery-codes",
        reauth_opts(Schema::new().field("password", password_field()).field("code", code_field())),
        bind(&d, recovery_codes),
    );
    router.post("/account/delete", reauth_opts(credentials_schema()), bind(&d, delete_account));
    router.post(
        "/account/email",
        reauth_opts(
            Schema::new()
                .field("newEmail", email_field())
                .field("password", password_field())
                .field("code", code_field().optional())
                .field("recoveryCode", code_field().optional()),
        ),
        bind(&d, change_email),
    );
    router.put(
        "/account/preferences",
        RouteOpts::new()
            .auth(AuthMode::Required)
            .rate(read_rate)
            .body(Schema::new().field("acceptChallenges", Spec::one_of([json!("all"), json!("none")]))),
        bind(&d, preferences),
    );
}

async fn me(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.me(&session).await?))
}

async fn change_password(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    let b = &ctx.body;
    let body = d
        .auth
        .change_password(&session, &text(b, "currentPassword"), &text(b, "newPassword"), Some(&ip_of(&ctx)))
        .await?;
    Ok(Answer::json(body))
}

async fn mfa_setup(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.mfa_setup(&session, &text(&ctx.body, "password"), Some(&ip_of(&ctx))).await?))
}

async fn mfa_enable(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.mfa_enable(&session, &text(&ctx.body, "code"), Some(&ip_of(&ctx))).await?))
}

async fn mfa_disable(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.mfa_disable(&session, &credentials_of(&ctx), Some(&ip_of(&ctx))).await?))
}

async fn recovery_codes(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    let b = &ctx.body;
    let body = d
        .auth
        .regenerate_recovery_codes(&session, &text(b, "password"), b["code"].as_str(), Some(&ip_of(&ctx)))
        .await?;
    Ok(Answer::json(body))
}

async fn delete_account(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.delete_account(&session, &credentials_of(&ctx), Some(&ip_of(&ctx))).await?))
}

async fn change_email(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    let a = d
        .auth
        .change_email(&session, &text(&ctx.body, "newEmail"), &credentials_of(&ctx), Some(&ip_of(&ctx)))
        .await?;
    Ok(Answer::json(a.body).status(a.status))
}

async fn preferences(d: Arc<AccountRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let session = session_of(&ctx)?;
    Ok(Answer::json(d.auth.set_preferences(&session, &text(&ctx.body, "acceptChallenges")).await?))
}
