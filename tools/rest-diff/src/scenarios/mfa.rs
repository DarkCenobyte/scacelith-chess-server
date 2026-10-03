//! Two-step verification: setup, enable, the second sign-in step with authenticator and
//! recovery codes, re-authentication with a second factor, new recovery codes, disable, and the
//! per-account cap on codes.

use std::time::Duration;

use serde_json::json;

use super::{BoxFut, PASSWORD, login, new_account};
use crate::crypto;
use crate::duo::{Duo, Pair, Side, fresh_ip};
use crate::http::{Req, Resp};

/// Sets `<user>.code` on both sides to an authenticator code of a 30-second step not used yet
/// for this account (each code works once), waiting for the next step when the three steps the
/// servers accept (previous, current, next) are used up.
async fn next_code(d: &mut Duo, user: &str) {
    let last_key = format!("{user}.step");
    let last: u64 = d.node.vars.get(&last_key).and_then(|v| v.parse().ok()).unwrap_or(0);
    loop {
        let now = crypto::totp_step();
        let into_step = crate::http::now_ms() % 30_000.0;
        // The previous step is only safe while the current one has more than 3 s left.
        let lowest = if into_step < 27_000.0 { now - 1 } else { now };
        let step = (last + 1).max(lowest);
        if step <= now + 1 {
            for side in [&mut d.node, &mut d.rust] {
                let code = crypto::totp(&side.v(&format!("{user}.secret")), step);
                side.vars.insert(format!("{user}.code"), code);
                side.vars.insert(last_key.clone(), step.to_string());
            }
            return;
        }
        let wait = 30_000.0 - into_step + 50.0;
        tokio::time::sleep(Duration::from_millis(wait as u64)).await;
    }
}

/// Saves the ten recovery codes of an answer as `<user>.rc0` .. `<user>.rc9`.
fn save_recovery_codes(d: &mut Duo, pair: &Pair, user: &str) {
    for i in 0..10 {
        d.save_with(pair, &format!("{user}.rc{i}"), |r: &Resp| r.json()?.get("recoveryCodes")?.as_array()?.get(i)?.scalar_text());
    }
}

fn post(path: &'static str, token_var: String, body: impl Fn(&Side) -> serde_json::Value) -> impl Fn(&Side) -> Req {
    move |s: &Side| Req::post(path).bearer(&s.v(&token_var)).json(body(s))
}

/// Enables two-step verification for `user` (signed in as `<user>.token`): setup, enable;
/// saves the secret and the recovery codes.
async fn enable(d: &mut Duo, ip: std::net::IpAddr, user: &str) {
    let tok = format!("{user}.token");
    let p = d.step(&format!("{user}-setup"), ip, 200, post("/api/v1/account/mfa/totp/setup", tok.clone(), |_| json!({"password": PASSWORD}))).await;
    d.save(&p, &format!("{user}.secret"), "secret");
    next_code(d, user).await;
    let code = format!("{user}.code");
    let p = d
        .step(&format!("{user}-enable"), ip, 200, post("/api/v1/account/mfa/totp/enable", tok, move |s| json!({"code": s.v(&code)})))
        .await;
    save_recovery_codes(d, &p, user);
}

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "sam", "sam@example.org").await;
        let ip = fresh_ip();
        let tok = || "sam.token".to_string();
        d.step("enable-without-setup", ip, 409, post("/api/v1/account/mfa/totp/enable", tok(), |_| json!({"code": "123456"}))).await;
        d.step("disable-not-enabled", ip, 409, post("/api/v1/account/mfa/totp/disable", tok(), |_| json!({"password": PASSWORD, "code": "123456"}))).await;
        d.step("recovery-not-enabled", ip, 409, post("/api/v1/account/mfa/recovery-codes", tok(), |_| json!({"password": PASSWORD, "code": "123456"}))).await;
        d.step("setup-no-password", ip, 400, post("/api/v1/account/mfa/totp/setup", tok(), |_| json!({}))).await;
        d.step("setup-wrong-password", ip, 403, post("/api/v1/account/mfa/totp/setup", tok(), |_| json!({"password": "wrong"}))).await;
        d.step("setup", ip, 200, post("/api/v1/account/mfa/totp/setup", tok(), |_| json!({"password": PASSWORD}))).await;
        let p = d.step("setup-again", ip, 200, post("/api/v1/account/mfa/totp/setup", tok(), |_| json!({"password": PASSWORD}))).await;
        d.save(&p, "sam.secret", "secret");
        d.step("me-pending-setup", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("sam.token"))).await;
        d.step("enable-five-digits", ip, 400, post("/api/v1/account/mfa/totp/enable", tok(), |_| json!({"code": "12345"}))).await;
        d.step("enable-letters", ip, 400, post("/api/v1/account/mfa/totp/enable", tok(), |_| json!({"code": "12345a"}))).await;
        d.step("enable-number", ip, 400, post("/api/v1/account/mfa/totp/enable", tok(), |_| json!({"code": 123456}))).await;
        d.step("enable-extra", ip, 400, post("/api/v1/account/mfa/totp/enable", tok(), |_| json!({"code": "123456", "password": PASSWORD}))).await;
        d.step("enable-wrong-code", ip, 403, post("/api/v1/account/mfa/totp/enable", tok(), |s| json!({"code": wrong_code(s, "sam")}))).await;
        next_code(d, "sam").await;
        let p = d.step("enable", ip, 200, post("/api/v1/account/mfa/totp/enable", tok(), |s| json!({"code": s.v("sam.code")}))).await;
        save_recovery_codes(d, &p, "sam");
        d.mail_count("enable-mail", "sam@example.org", 0, 500).await;
        d.step("enable-again", ip, 409, post("/api/v1/account/mfa/totp/enable", tok(), |_| json!({"code": "123456"}))).await;
        d.step("setup-enabled", ip, 409, post("/api/v1/account/mfa/totp/setup", tok(), |_| json!({"password": "wrong"}))).await;
        d.step("me-enabled", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("sam.token"))).await;

        // The second step of a sign-in.
        let ip = fresh_ip();
        let p = d.step("login-step-one", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "sam", "password": PASSWORD}))).await;
        d.save(&p, "sam.mfa", "mfaToken");
        let mfa = |body: fn(&Side) -> serde_json::Value| move |s: &Side| Req::post("/api/v1/auth/login/mfa").json(body(s));
        d.step("mfa-no-code", ip, 400, mfa(|s| json!({"mfaToken": s.v("sam.mfa")}))).await;
        d.step("mfa-both-codes", ip, 0, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": wrong_code(s, "sam"), "recoveryCode": "aaaa-bbbb-cc"}))).await;
        d.step("mfa-wrong-code", ip, 401, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": wrong_code(s, "sam")}))).await;
        d.step("mfa-bad-token", ip, 401, mfa(|_| json!({"mfaToken": "mfa_nope", "code": "123456"}))).await;
        d.step("mfa-session-token", ip, 401, mfa(|s| json!({"mfaToken": s.v("sam.token"), "code": "123456"}))).await;
        next_code(d, "sam").await;
        d.step("mfa-code", ip, 200, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": s.v("sam.code")}))).await;
        d.step("mfa-token-used", ip, 401, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": s.v("sam.code")}))).await;
        // A used authenticator code is refused until the next step.
        let p = d.step("login-step-one-b", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "sam", "password": PASSWORD}))).await;
        d.save(&p, "sam.mfa", "mfaToken");
        d.step("mfa-code-reused", ip, 401, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": s.v("sam.code")}))).await;
        d.step("mfa-recovery", ip, 200, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "recoveryCode": s.v("sam.rc0")}))).await;
        let p = d.step("login-step-one-c", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "sam", "password": PASSWORD}))).await;
        d.save(&p, "sam.mfa", "mfaToken");
        d.step("mfa-recovery-reused", ip, 401, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "recoveryCode": s.v("sam.rc0")}))).await;
        d.step("mfa-recovery-in-code", ip, 200, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": s.v("sam.rc1")}))).await;
        let p = d.step("login-step-one-d", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "sam", "password": PASSWORD}))).await;
        d.save(&p, "sam.mfa", "mfaToken");
        d.step("mfa-recovery-formatted", ip, 200, mfa(|s| {
            let rc = s.v("sam.rc2").to_uppercase().replace('-', " ");
            json!({"mfaToken": s.v("sam.mfa"), "recoveryCode": format!(" {rc} ")})
        }))
        .await;
        // Five wrong codes end the step.
        let ip = fresh_ip();
        let p = d.step("login-step-one-e", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "sam", "password": PASSWORD}))).await;
        d.save(&p, "sam.mfa", "mfaToken");
        for i in 1..=6 {
            d.step(&format!("mfa-wrong-{i}"), ip, 0, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "code": wrong_code(s, "sam")}))).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        d.step("me-recovery-used", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("sam.token"))).await;

        // Re-authentication with a second factor.
        let ip = fresh_ip();
        let tk = || "sam.token".to_string();
        d.step("email-without-code", ip, 403, post("/api/v1/account/email", tk(), |_| json!({"newEmail": "sam2@example.org", "password": PASSWORD}))).await;
        tokio::time::sleep(Duration::from_millis(4500)).await;
        d.step("email-wrong-code", ip, 0, post("/api/v1/account/email", tk(), |s| json!({"newEmail": "sam2@example.org", "password": PASSWORD, "code": wrong_code(s, "sam")}))).await;
        tokio::time::sleep(Duration::from_millis(8500)).await;
        d.step("email-wrong-password-with-code", ip, 0, post("/api/v1/account/email", tk(), |s| json!({"newEmail": "sam2@example.org", "password": "wrong", "code": s.v("sam.rc3")}))).await;
        tokio::time::sleep(Duration::from_millis(16500)).await;
        d.step("email-recovery-code", ip, 0, post("/api/v1/account/email", tk(), |s| json!({"newEmail": "sam2@example.org", "password": PASSWORD, "recoveryCode": s.v("sam.rc4")}))).await;
        d.mail_count("email-recovery-code-mails", "sam2@example.org", 1, 800).await;
        d.mail_count("email-recovery-code-notice", "sam@example.org", 1, 0).await;
        d.step("password-change-no-code", ip, 0, post("/api/v1/account/password", tk(), |_| json!({"currentPassword": PASSWORD, "newPassword": PASSWORD}))).await;
        d.mail_count("password-change-mail", "sam@example.org", 1, 800).await;
        d.step("me-after-reauth", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("sam.token"))).await;

        // New recovery codes: an authenticator code only.
        let ip = fresh_ip();
        d.step("recovery-codes-with-recovery", ip, 403, post("/api/v1/account/mfa/recovery-codes", tk(), |s| json!({"password": PASSWORD, "code": s.v("sam.rc5")}))).await;
        d.step("recovery-codes-field", ip, 400, post("/api/v1/account/mfa/recovery-codes", tk(), |s| json!({"password": PASSWORD, "recoveryCode": s.v("sam.rc5")}))).await;
        d.step("recovery-codes-no-code", ip, 0, post("/api/v1/account/mfa/recovery-codes", tk(), |_| json!({"password": PASSWORD}))).await;
        next_code(d, "sam").await;
        let p = d.step("recovery-codes", ip, 200, post("/api/v1/account/mfa/recovery-codes", tk(), |s| json!({"password": PASSWORD, "code": s.v("sam.code")}))).await;
        let old_rc6 = (d.node.v("sam.rc6"), d.rust.v("sam.rc6"));
        save_recovery_codes(d, &p, "sam");
        d.node.vars.insert("sam.old".into(), old_rc6.0);
        d.rust.vars.insert("sam.old".into(), old_rc6.1);
        let p = d.step("login-step-one-f", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "sam", "password": PASSWORD}))).await;
        d.save(&p, "sam.mfa", "mfaToken");
        d.step("mfa-old-recovery", ip, 401, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "recoveryCode": s.v("sam.old")}))).await;
        d.step("mfa-new-recovery", ip, 200, mfa(|s| json!({"mfaToken": s.v("sam.mfa"), "recoveryCode": s.v("sam.rc0")}))).await;

        // The export and the deletion take either factor.
        d.step("export-without-code", ip, 403, post("/api/v1/account/export", tk(), |_| json!({"password": PASSWORD}))).await;
        d.step("export-recovery-code", ip, 200, post("/api/v1/account/export", tk(), |s| json!({"password": PASSWORD, "code": s.v("sam.rc1")}))).await;

        // Disable.
        let ip = fresh_ip();
        d.step("disable-no-code", ip, 403, post("/api/v1/account/mfa/totp/disable", tk(), |_| json!({"password": PASSWORD}))).await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        next_code(d, "sam").await;
        d.step("disable", ip, 200, post("/api/v1/account/mfa/totp/disable", tk(), |s| json!({"password": PASSWORD, "code": s.v("sam.code")}))).await;
        d.mail("disable-mail", "sam@example.org", None).await;
        d.step("disable-again", ip, 409, post("/api/v1/account/mfa/totp/disable", tk(), |_| json!({"password": PASSWORD, "code": "123456"}))).await;
        login(d, ip, "sam", PASSWORD, "sam.plain").await;
        d.step("me-disabled", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("sam.plain"))).await;

        // The cap: AUTH_MFA_PER_ACCOUNT (10) codes per 15 minutes, right or wrong.
        let ip = fresh_ip();
        new_account(d, ip, "uma", "uma@example.org").await;
        enable(d, ip, "uma").await;
        let mut ip = fresh_ip();
        for i in 0..10 {
            if i == 6 {
                ip = fresh_ip();
            }
            let p = d.step(&format!("cap-login-{i}"), ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "uma", "password": PASSWORD}))).await;
            d.save(&p, "uma.mfa", "mfaToken");
            let rc = format!("uma.rc{i}");
            d.step(&format!("cap-code-{i}"), ip, 0, move |s: &Side| {
                Req::post("/api/v1/auth/login/mfa").json(json!({"mfaToken": s.v("uma.mfa"), "recoveryCode": s.v(&rc)}))
            })
            .await;
        }
        let p = d.step("cap-login-11", ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": "uma", "password": PASSWORD}))).await;
        d.save(&p, "uma.mfa", "mfaToken");
        next_code(d, "uma").await;
        d.step("cap-refused", ip, 429, |s| Req::post("/api/v1/auth/login/mfa").json(json!({"mfaToken": s.v("uma.mfa"), "code": s.v("uma.code")}))).await;
        d.step("cap-reauth-refused", ip, 0, post("/api/v1/account/export", "uma.token".into(), |s| json!({"password": PASSWORD, "code": s.v("uma.code")}))).await;
    })
}

/// A 6-digit code that is not the account's current one (nor its neighbours).
fn wrong_code(s: &Side, user: &str) -> String {
    let secret = s.v(&format!("{user}.secret"));
    let now = crypto::totp_step();
    let valid: Vec<String> = (now.saturating_sub(2)..=now + 2).map(|c| crypto::totp(&secret, c)).collect();
    let mut n = 0u32;
    loop {
        let candidate = format!("{:06}", 111_111 + n * 7);
        if !valid.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

