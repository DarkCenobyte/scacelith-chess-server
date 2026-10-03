//! Google sign-in (DESIGN.md 5.9; the flow is described in the auth service). Off (404
//! `sso_disabled`) unless `SSO_GOOGLE_ENABLED`. Google sends the browser back to the game's own
//! listener on 127.0.0.1, never to this server.
//!
//! | endpoint | answer |
//! |---|---|
//! | `POST /auth/sso/google/start {codeChallenge, redirectPort}` | `{attemptId, authUrl, state, expiresIn}` |
//! | `POST /auth/sso/google/finish {attemptId, codeVerifier, state, code, iss?, clientLabel?}` | a login answer, `{needsUsername: true, ssoTicket, suggestedUsername}` or `{needsPassword: true, linkTicket, username, expiresIn}` |
//! | `POST /auth/sso/google/link {linkTicket, password, clientLabel?, pow?}` | a login answer (the link is stored once a code passes) |
//! | `POST /auth/sso/complete {ssoTicket, username, clientLabel?}` | `{token, expiresAt, user}` |
//!
//! Limits: start 30 per 10 minutes per address and 90 per IPv6 /48 (`sso_start`, shared: it
//! writes a token row); finish 30 per minute per address (`sso_finish`); link and complete take
//! the `auth` limit of [`super::auth`].

use std::sync::Arc;

use super::auth::{
    AuthRouteDeps, auth_rate_of, bind, client_label_field, ip_of, opt_text, password_field, pow_field,
    pow_of, text,
};
use crate::auth::{SsoCompleteParams, SsoFinishParams, SsoLinkParams};
use crate::http::{Answer, ApiError, Ctx, RateSpec, RouteOpts, Router, Schema, Spec};

/// What the Google sign-in endpoints need (those of the auth endpoints).
pub type SsoRouteDeps = AuthRouteDeps;

/// `^[A-Za-z0-9_-]{43}$`: a PKCE challenge, a `state`.
fn is_token43(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `^[A-Za-z0-9._~-]{43,128}$`: a PKCE verifier (RFC 7636).
fn is_verifier(s: &str) -> bool {
    (43..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
}

/// `^[\x21-\x7e]+$`: printable ASCII without spaces.
fn is_printable(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Registers the endpoints.
pub fn register(router: &mut Router, deps: SsoRouteDeps) {
    let auth_rate = auth_rate_of(&deps.config);
    let d = Arc::new(deps);
    let start_rate = RateSpec::new("sso_start", 30.0, 600_000).prefix_limit(90.0).shared();
    let finish_rate = RateSpec::new("sso_finish", 30.0, 60_000);

    router.post(
        "/auth/sso/google/start",
        RouteOpts::new().rate(start_rate).body(
            Schema::new()
                .field("codeChallenge", Spec::string().min_len(43).max_len(43).pattern(is_token43))
                .field("redirectPort", Spec::integer().min(1024.0).max(65535.0)),
        ),
        bind(&d, start),
    );
    router.post(
        "/auth/sso/google/finish",
        RouteOpts::new().rate(finish_rate).body(
            Schema::new()
                .field("attemptId", Spec::string().min_len(1).max_len(64))
                .field("codeVerifier", Spec::string().min_len(43).max_len(128).pattern(is_verifier))
                .field("state", Spec::string().min_len(43).max_len(43).pattern(is_token43))
                .field("code", Spec::string().min_len(1).max_len(2048).pattern(is_printable))
                .field("iss", Spec::string().min_len(1).max_len(256).optional())
                .field("clientLabel", client_label_field()),
        ),
        bind(&d, finish),
    );
    router.post(
        "/auth/sso/google/link",
        RouteOpts::new().rate(auth_rate.clone()).body(
            Schema::new()
                .field("linkTicket", Spec::string().min_len(1).max_len(64))
                .field("password", password_field())
                .field("clientLabel", client_label_field())
                .field("pow", pow_field()),
        ),
        bind(&d, link),
    );
    router.post(
        "/auth/sso/complete",
        RouteOpts::new().rate(auth_rate).body(
            Schema::new()
                .field("ssoTicket", Spec::string().min_len(1).max_len(64))
                .field("username", Spec::string().min_len(1).max_len(64))
                .field("clientLabel", client_label_field()),
        ),
        bind(&d, complete),
    );
}

async fn start(d: Arc<SsoRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let port = ctx.body["redirectPort"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or_default();
    let body = d.auth.sso_start(&text(&ctx.body, "codeChallenge"), port, Some(&ip_of(&ctx))).await?;
    Ok(Answer::json(body))
}

async fn finish(d: Arc<SsoRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let params = SsoFinishParams {
        attempt_id: text(b, "attemptId"),
        code_verifier: text(b, "codeVerifier"),
        state: text(b, "state"),
        code: text(b, "code"),
        iss: opt_text(b, "iss"),
        client_label: opt_text(b, "clientLabel"),
        ip: Some(ip_of(&ctx)),
    };
    Ok(Answer::json(d.auth.sso_finish(&params).await?))
}

async fn link(d: Arc<SsoRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let params = SsoLinkParams {
        link_ticket: text(b, "linkTicket"),
        password: text(b, "password"),
        client_label: opt_text(b, "clientLabel"),
        pow: pow_of(b),
        ip: Some(ip_of(&ctx)),
    };
    Ok(Answer::json(d.auth.sso_link(&params).await?))
}

async fn complete(d: Arc<SsoRouteDeps>, ctx: Ctx) -> Result<Answer, ApiError> {
    let b = &ctx.body;
    let params = SsoCompleteParams {
        sso_ticket: text(b, "ssoTicket"),
        username: text(b, "username"),
        client_label: opt_text(b, "clientLabel"),
        ip: Some(ip_of(&ctx)),
    };
    Ok(Answer::json(d.auth.sso_complete(&params).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        let t = "A".repeat(43);
        assert!(
            is_token43(&t) && !is_token43(&"A".repeat(42)) && !is_token43(&format!("{}.", "A".repeat(42)))
        );
        assert!(is_verifier(&format!("{}._~-", "a".repeat(40))) && !is_verifier(&"a".repeat(129)));
        assert!(!is_verifier(&format!("{}+", "a".repeat(43))));
        assert!(is_printable("4/0AX~!") && !is_printable("a b") && !is_printable(""));
    }
}
