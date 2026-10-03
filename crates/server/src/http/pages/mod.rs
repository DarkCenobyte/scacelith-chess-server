//! The HTML pages of the e-mail links (`/verify-email`, `/reset-password`,
//! `/confirm-email-change`) and their layout.
//!
//! Pages register with [`Router::page`]. The layout's message page also renders the errors of
//! page routes: hand [`layout::error_page_renderer`] to [`crate::http::ApiBuilder::page_renderer`].
//!
//! | page | answer |
//! |---|---|
//! | `GET /verify-email?token=` | a confirmation button (the token is not used: link scanners open links), 400 "link invalid or expired" |
//! | `POST /verify-email` (form: `token`) | "E-mail address confirmed", 400 invalid or expired, 409 "Account not created" (a pending signup's username or address was taken meanwhile) |
//! | `GET /reset-password?token=` | the new password form, 400 invalid or expired |
//! | `POST /reset-password` (form: `token`, `newPassword`, `confirmPassword`) | "Password changed", 400 the form with the error (different passwords, weak password), 400 invalid or expired, 503 / 429 the form again with `Retry-After` (password hash queue full, the link still works) |
//! | `GET /confirm-email-change?token=` | the new address and a confirmation button, 400 invalid or expired |
//! | `POST /confirm-email-change` (form: `token`) | "E-mail address changed", 400 invalid or expired, 409 "Address already used" |
//!
//! Limits: the GET pages take `page` (60 per minute per address); the POST pages take `auth`, and
//! `POST /reset-password` also `auth_reset` ([`crate::http::routes::auth`]).

pub mod email_change;
pub mod layout;
pub mod reset_password;
pub mod verify_email;

use std::sync::Arc;

use http::Method;

use super::router::Router;
use super::routes::auth::{
    auth_family_rate, auth_rate_of, bind, ip_of, link_token_field, page_rate, password_field, text,
};
use super::{Answer, ApiError, Ctx, RouteOpts, Schema};
use crate::auth::http::retry_after_header;
use crate::auth::{Auth, EmailChangeOutcome, LinkOutcome};
use crate::config::Config;

/// What the pages need.
#[derive(Clone)]
pub struct PageDeps {
    /// The configuration (server name, password length, limits).
    pub config: Arc<Config>,
    /// The auth service.
    pub auth: Auth,
}

/// Registers the pages.
pub fn register(router: &mut Router, deps: PageDeps) {
    let auth_rate = auth_rate_of(&deps.config);
    let reset_rate = auth_family_rate("auth_reset", deps.config.auth_reset_per_hour, 3_600_000);
    let d = Arc::new(deps);
    let token_body = || Schema::new().field("token", link_token_field());

    router.page(Method::GET, "/verify-email", RouteOpts::new().rate(page_rate()), bind(&d, verify_get));
    router.page(
        Method::POST,
        "/verify-email",
        RouteOpts::new().rate(auth_rate.clone()).body(token_body()),
        bind(&d, verify_post),
    );
    router.page(Method::GET, "/reset-password", RouteOpts::new().rate(page_rate()), bind(&d, reset_get));
    router.page(
        Method::POST,
        "/reset-password",
        RouteOpts::new().rate(auth_rate.clone()).rate(reset_rate).body(
            Schema::new()
                .field("token", link_token_field())
                .field("newPassword", password_field())
                .field("confirmPassword", password_field()),
        ),
        bind(&d, reset_post),
    );
    router.page(
        Method::GET,
        "/confirm-email-change",
        RouteOpts::new().rate(page_rate()),
        bind(&d, email_change_get),
    );
    router.page(
        Method::POST,
        "/confirm-email-change",
        RouteOpts::new().rate(auth_rate).body(token_body()),
        bind(&d, email_change_post),
    );
}

fn page(status: u16, html: String) -> Answer {
    Answer::html(html).status(status)
}

async fn verify_get(d: Arc<PageDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = &d.config.server_name;
    let token = ctx.query_str("token").unwrap_or_default();
    if !d.auth.peek_verification(token).await? {
        return Ok(page(400, verify_email::verify_invalid(name)));
    }
    Ok(page(200, verify_email::verify_form(name, token)))
}

async fn verify_post(d: Arc<PageDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = &d.config.server_name;
    Ok(match d.auth.verify_email(&text(&ctx.body, "token"), Some(&ip_of(&ctx))).await? {
        LinkOutcome::Confirmed => page(200, verify_email::verify_done(name)),
        LinkOutcome::Taken => page(409, verify_email::verify_taken(name)),
        LinkOutcome::Invalid => page(400, verify_email::verify_invalid(name)),
    })
}

async fn reset_get(d: Arc<PageDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = &d.config.server_name;
    let token = ctx.query_str("token").unwrap_or_default();
    if !d.auth.peek_reset_token(token).await? {
        return Ok(page(400, reset_password::reset_invalid(name)));
    }
    Ok(page(200, reset_password::reset_form(name, token, d.config.password_min_length, None)))
}

async fn reset_post(d: Arc<PageDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = &d.config.server_name;
    let (token, new_password) = (text(&ctx.body, "token"), text(&ctx.body, "newPassword"));
    let form = |error: &str| {
        page(400, reset_password::reset_form(name, &token, d.config.password_min_length, Some(error)))
    };
    if !d.auth.peek_reset_token(&token).await? {
        return Ok(page(400, reset_password::reset_invalid(name)));
    }
    if new_password != text(&ctx.body, "confirmPassword") {
        return Ok(form("The two passwords are different."));
    }
    let err = match d.auth.reset_password(&token, &new_password, Some(&ip_of(&ctx))).await {
        Ok(_) => return Ok(page(200, reset_password::reset_done(name))),
        Err(e) => ApiError::from(e),
    };
    match err.code.as_ref() {
        "weak_password" => Ok(form(&err.message)),
        "invalid_token" => Ok(page(400, reset_password::reset_invalid(name))),
        // The link still works: the form again, so that the user can simply send it again.
        "server_busy" | "rate_limited" => {
            let mut answer = form(&err.message).status(err.status);
            if let Some(secs) = retry_after_header(&err) {
                answer = answer.header("Retry-After", secs);
            }
            answer.refund_rate = err.refund_rate;
            Ok(answer)
        }
        _ => Err(err),
    }
}

async fn email_change_get(d: Arc<PageDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = &d.config.server_name;
    let token = ctx.query_str("token").unwrap_or_default();
    Ok(match d.auth.peek_email_change(token).await? {
        Some((username, email)) => page(200, email_change::email_change_form(name, token, &email, &username)),
        None => page(400, email_change::email_change_invalid(name)),
    })
}

async fn email_change_post(d: Arc<PageDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let name = &d.config.server_name;
    Ok(match d.auth.confirm_email_change(&text(&ctx.body, "token"), Some(&ip_of(&ctx))).await? {
        EmailChangeOutcome::Changed { email } => page(200, email_change::email_change_done(name, &email)),
        EmailChangeOutcome::Taken => page(409, email_change::email_change_taken(name)),
        EmailChangeOutcome::Invalid => page(400, email_change::email_change_invalid(name)),
    })
}
