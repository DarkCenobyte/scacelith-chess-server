//! Two-step verification: setup, enable, the second sign-in step with authenticator and
//! recovery codes, re-authentication with a second factor, new recovery codes, disable, and the
//! per-account cap on codes.
//!
//! Each part uses its own account and source address: the re-authentication endpoints allow 10
//! requests per 10 minutes per account (`reauth_user`), and an account may try 10 second-factor
//! codes per 15 minutes (`AUTH_MFA_PER_ACCOUNT`).

use std::net::IpAddr;
use std::time::Duration;

use serde_json::json;

use super::{BoxFut, PASSWORD, login, new_account};
use crate::crypto;
use crate::duo::{Duo, Pair, Side, fresh_ip};
use crate::http::{Req, Resp};

/// Sets `<user>.code` on both sides to an authenticator code of a 30-second step not used yet
/// for this account (each step works once), waiting for the next step when the steps the
/// servers accept (previous, current, next) are used up.
async fn next_code(d: &mut Duo, user: &str) {
    let last_key = format!("{user}.step");
    let last: u64 = d.node.vars.get(&last_key).and_then(|v| v.parse().ok()).unwrap_or(0);
    loop {
        let now = crypto::totp_step();
        let into_step = crate::http::now_ms() % 30_000.0;
        // The previous step is only safe while the current one has more than 3 s left, and the
        // next one once the current one has less than 20 s left.
        let lowest = if into_step < 27_000.0 { now - 1 } else { now };
        let highest = if into_step > 10_000.0 { now + 1 } else { now };
        let step = (last + 1).max(lowest);
        if step <= highest {
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

/// A POST with the session `<user>.token` and a JSON body computed from the side.
fn post(path: &'static str, user: &str, body: impl Fn(&Side) -> serde_json::Value) -> impl Fn(&Side) -> Req {
    let token = format!("{user}.token");
    move |s: &Side| Req::post(path).bearer(&s.v(&token)).json(body(s))
}

/// The second sign-in step with a JSON body computed from the side.
fn mfa(body: impl Fn(&Side) -> serde_json::Value) -> impl Fn(&Side) -> Req {
    move |s: &Side| Req::post("/api/v1/auth/login/mfa").json(body(s))
}

/// The first sign-in step of `user`; saves its `mfaToken` as `<user>.mfa`.
async fn first_step(d: &mut Duo, id: &str, ip: IpAddr, user: &str) {
    let p = d.step(id, ip, 200, |_| Req::post("/api/v1/auth/login").json(json!({"login": user, "password": PASSWORD}))).await;
    d.save(&p, &format!("{user}.mfa"), "mfaToken");
}

/// Enables two-step verification for `user` (signed in as `<user>.token`): setup, enable;
/// saves the secret and the recovery codes.
async fn enable(d: &mut Duo, ip: IpAddr, user: &str) {
    let p = d.step(&format!("{user}-setup"), ip, 200, post("/api/v1/account/mfa/totp/setup", user, |_| json!({"password": PASSWORD}))).await;
    d.save(&p, &format!("{user}.secret"), "secret");
    next_code(d, user).await;
    let code = format!("{user}.code");
    let p = d.step(&format!("{user}-enable"), ip, 200, post("/api/v1/account/mfa/totp/enable", user, move |s| json!({"code": s.v(&code)}))).await;
    save_recovery_codes(d, &p, user);
}

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        errors_before_enable(d).await;
        enrolment_and_sign_in(d).await;
        recovery_codes_at_sign_in(d).await;
        wrong_codes_at_sign_in(d).await;
        reauthentication(d).await;
        new_recovery_codes(d).await;
        disable(d).await;
        code_cap(d).await;
    })
}

/// Errors of the enrolment endpoints before two-step verification is on (10 requests), then
/// the account's re-authentication limit.
async fn errors_before_enable(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "sam", "sam@example.org").await;
    let ip = fresh_ip();
    d.step("enable-without-setup", ip, 409, post("/api/v1/account/mfa/totp/enable", "sam", |_| json!({"code": "123456"}))).await;
    d.step("disable-not-enabled", ip, 409, post("/api/v1/account/mfa/totp/disable", "sam", |_| json!({"password": PASSWORD, "code": "123456"}))).await;
    d.step("recovery-not-enabled", ip, 409, post("/api/v1/account/mfa/recovery-codes", "sam", |_| json!({"password": PASSWORD, "code": "123456"}))).await;
    d.step("setup-no-password", ip, 400, post("/api/v1/account/mfa/totp/setup", "sam", |_| json!({}))).await;
    d.step("setup-wrong-password", ip, 403, post("/api/v1/account/mfa/totp/setup", "sam", |_| json!({"password": "wrong"}))).await;
    d.step("setup", ip, 200, post("/api/v1/account/mfa/totp/setup", "sam", |_| json!({"password": PASSWORD}))).await;
    d.step("me-pending-setup", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("sam.token"))).await;
    d.step("enable-five-digits", ip, 400, post("/api/v1/account/mfa/totp/enable", "sam", |_| json!({"code": "12345"}))).await;
    d.step("enable-letters", ip, 400, post("/api/v1/account/mfa/totp/enable", "sam", |_| json!({"code": "12345a"}))).await;
    d.step("enable-number", ip, 400, post("/api/v1/account/mfa/totp/enable", "sam", |_| json!({"code": 123456}))).await;
    d.step("enable-extra", ip, 400, post("/api/v1/account/mfa/totp/enable", "sam", |_| json!({"code": "123456", "password": PASSWORD}))).await;
    d.step("reauth-user-limit", ip, 429, post("/api/v1/account/mfa/totp/setup", "sam", |_| json!({"password": PASSWORD}))).await;
}

/// Setup, enable, and the second sign-in step with authenticator codes.
async fn enrolment_and_sign_in(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "tim", "tim@example.org").await;
    let ip = fresh_ip();
    d.step("setup-first", ip, 200, post("/api/v1/account/mfa/totp/setup", "tim", |_| json!({"password": PASSWORD}))).await;
    let p = d.step("setup-again", ip, 200, post("/api/v1/account/mfa/totp/setup", "tim", |_| json!({"password": PASSWORD}))).await;
    d.save(&p, "tim.secret", "secret");
    d.step("enable-wrong-code", ip, 403, post("/api/v1/account/mfa/totp/enable", "tim", |s| json!({"code": wrong_code(s, "tim")}))).await;
    next_code(d, "tim").await;
    let p = d.step("enable", ip, 200, post("/api/v1/account/mfa/totp/enable", "tim", |s| json!({"code": s.v("tim.code")}))).await;
    save_recovery_codes(d, &p, "tim");
    d.mail_count("enable-mail", "tim@example.org", 0, 500).await;
    d.step("enable-again", ip, 409, post("/api/v1/account/mfa/totp/enable", "tim", |_| json!({"code": "123456"}))).await;
    d.step("setup-enabled", ip, 409, post("/api/v1/account/mfa/totp/setup", "tim", |_| json!({"password": "wrong"}))).await;
    d.step("me-enabled", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("tim.token"))).await;

    let ip = fresh_ip();
    first_step(d, "login-step-one", ip, "tim").await;
    d.step("mfa-no-code", ip, 400, mfa(|s| json!({"mfaToken": s.v("tim.mfa")}))).await;
    d.step("mfa-empty-code", ip, 400, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": ""}))).await;
    d.step("mfa-long-code", ip, 400, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": "1".repeat(33)}))).await;
    d.step("mfa-extra-field", ip, 400, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": "123456", "x": 1}))).await;
    d.step("mfa-wrong-code", ip, 401, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": wrong_code(s, "tim")}))).await;
    d.step("mfa-bad-token", ip, 401, mfa(|_| json!({"mfaToken": "mfa_nope", "code": "123456"}))).await;
    d.step("mfa-session-token", ip, 401, mfa(|s| json!({"mfaToken": s.v("tim.token"), "code": "123456"}))).await;
    next_code(d, "tim").await;
    d.step("mfa-code", ip, 200, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": s.v("tim.code")}))).await;
    d.step("mfa-token-used", ip, 401, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": s.v("tim.code")}))).await;
    // A used authenticator code is refused.
    first_step(d, "login-step-one-b", ip, "tim").await;
    d.step("mfa-code-reused", ip, 401, mfa(|s| json!({"mfaToken": s.v("tim.mfa"), "code": s.v("tim.code")}))).await;
    next_code(d, "tim").await;
    d.step("mfa-code-spaced", ip, 200, mfa(|s| {
        let c = s.v("tim.code");
        let (a, b) = c.split_at(c.len().min(3));
        json!({"mfaToken": s.v("tim.mfa"), "code": format!(" {a} {b} ")})
    }))
    .await;
}

/// Recovery codes at the second sign-in step.
async fn recovery_codes_at_sign_in(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "vic", "vic@example.org").await;
    let ip = fresh_ip();
    enable(d, ip, "vic").await;
    let ip = fresh_ip();
    first_step(d, "login-step-one", ip, "vic").await;
    d.step("mfa-recovery", ip, 200, mfa(|s| json!({"mfaToken": s.v("vic.mfa"), "recoveryCode": s.v("vic.rc0")}))).await;
    first_step(d, "login-step-one-b", ip, "vic").await;
    d.step("mfa-recovery-reused", ip, 401, mfa(|s| json!({"mfaToken": s.v("vic.mfa"), "recoveryCode": s.v("vic.rc0")}))).await;
    d.step("mfa-recovery-in-code", ip, 200, mfa(|s| json!({"mfaToken": s.v("vic.mfa"), "code": s.v("vic.rc1")}))).await;
    first_step(d, "login-step-one-c", ip, "vic").await;
    d.step("mfa-recovery-formatted", ip, 200, mfa(|s| {
        let rc = s.v("vic.rc2").to_uppercase().replace('-', " ");
        json!({"mfaToken": s.v("vic.mfa"), "recoveryCode": format!(" {rc} ")})
    }))
    .await;
    first_step(d, "login-step-one-d", ip, "vic").await;
    d.step("mfa-both-wrong-code", ip, 0, mfa(|s| json!({"mfaToken": s.v("vic.mfa"), "code": wrong_code(s, "vic"), "recoveryCode": s.v("vic.rc3")}))).await;
    d.step("mfa-both-wrong-recovery", ip, 0, mfa(|s| json!({"mfaToken": s.v("vic.mfa"), "code": s.v("vic.rc4"), "recoveryCode": "aaaa-bbbb-cc"}))).await;
    d.step("mfa-malformed-recovery", ip, 0, mfa(|s| json!({"mfaToken": s.v("vic.mfa"), "recoveryCode": "not a code"}))).await;
    // Security events are saved in batches one second after the first one.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    d.step("export-security-events", ip, 200, post("/api/v1/account/export", "vic", |s| json!({"password": PASSWORD, "recoveryCode": s.v("vic.rc5")}))).await;
}

/// Five wrong codes end a sign-in step; the failure delay of the account.
async fn wrong_codes_at_sign_in(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "wes", "wes@example.org").await;
    let ip = fresh_ip();
    enable(d, ip, "wes").await;
    let ip = fresh_ip();
    first_step(d, "login-step-one", ip, "wes").await;
    for i in 1..=6 {
        d.step(&format!("mfa-wrong-{i}"), ip, 0, mfa(|s| json!({"mfaToken": s.v("wes.mfa"), "code": wrong_code(s, "wes")}))).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    first_step(d, "login-step-one-b", ip, "wes").await;
    d.step("mfa-delayed", ip, 0, mfa(|s| json!({"mfaToken": s.v("wes.mfa"), "recoveryCode": s.v("wes.rc0")}))).await;
}

/// Re-authentication with a second factor (e-mail change), and a password change without one.
async fn reauthentication(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "xia", "xia@example.org").await;
    let ip = fresh_ip();
    enable(d, ip, "xia").await;
    let email = "/api/v1/account/email";
    d.step("email-without-code", ip, 403, post(email, "xia", |_| json!({"newEmail": "xia2@example.org", "password": PASSWORD}))).await;
    d.step("email-wrong-code", ip, 0, post(email, "xia", |s| json!({"newEmail": "xia2@example.org", "password": PASSWORD, "code": wrong_code(s, "xia")}))).await;
    d.step("email-wrong-password-with-code", ip, 0, post(email, "xia", |s| json!({"newEmail": "xia2@example.org", "password": "wrong", "code": s.v("xia.rc3")}))).await;
    d.step("email-recovery-code", ip, 0, post(email, "xia", |s| json!({"newEmail": "xia2@example.org", "password": PASSWORD, "recoveryCode": s.v("xia.rc4")}))).await;
    d.mail_count("email-recovery-code-mails", "xia2@example.org", 1, 800).await;
    d.mail_count("email-recovery-code-notice", "xia@example.org", 1, 0).await;
    d.step("password-change-no-code", ip, 0, post("/api/v1/account/password", "xia", |_| json!({"currentPassword": PASSWORD, "newPassword": "another long passphrase"}))).await;
    d.mail_count("password-change-mail", "xia@example.org", 1, 800).await;
    d.step("me-after-reauth", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("xia.token"))).await;
}

/// New recovery codes, which need an authenticator code; the export with either factor.
async fn new_recovery_codes(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "yan", "yan@example.org").await;
    let ip = fresh_ip();
    enable(d, ip, "yan").await;
    let rc = "/api/v1/account/mfa/recovery-codes";
    d.step("recovery-codes-with-recovery", ip, 403, post(rc, "yan", |s| json!({"password": PASSWORD, "code": s.v("yan.rc5")}))).await;
    d.step("recovery-codes-field", ip, 400, post(rc, "yan", |s| json!({"password": PASSWORD, "recoveryCode": s.v("yan.rc5")}))).await;
    d.step("recovery-codes-no-code", ip, 0, post(rc, "yan", |_| json!({"password": PASSWORD}))).await;
    next_code(d, "yan").await;
    let p = d.step("recovery-codes", ip, 200, post(rc, "yan", |s| json!({"password": PASSWORD, "code": s.v("yan.code")}))).await;
    let old = (d.node.v("yan.rc6"), d.rust.v("yan.rc6"));
    save_recovery_codes(d, &p, "yan");
    d.node.vars.insert("yan.old".into(), old.0);
    d.rust.vars.insert("yan.old".into(), old.1);
    let ip2 = fresh_ip();
    first_step(d, "login-step-one", ip2, "yan").await;
    d.step("mfa-old-recovery", ip2, 401, mfa(|s| json!({"mfaToken": s.v("yan.mfa"), "recoveryCode": s.v("yan.old")}))).await;
    d.step("mfa-new-recovery", ip2, 200, mfa(|s| json!({"mfaToken": s.v("yan.mfa"), "recoveryCode": s.v("yan.rc0")}))).await;
    d.step("export-without-code", ip, 403, post("/api/v1/account/export", "yan", |_| json!({"password": PASSWORD}))).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    d.step("export-recovery-code", ip, 200, post("/api/v1/account/export", "yan", |s| json!({"password": PASSWORD, "code": s.v("yan.rc1")}))).await;
}

/// Disable.
async fn disable(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "zed", "zed@example.org").await;
    let ip = fresh_ip();
    enable(d, ip, "zed").await;
    let off = "/api/v1/account/mfa/totp/disable";
    d.step("disable-no-code", ip, 403, post(off, "zed", |_| json!({"password": PASSWORD}))).await;
    d.step("disable-wrong-password", ip, 403, post(off, "zed", |s| json!({"password": "wrong", "recoveryCode": s.v("zed.rc0")}))).await;
    next_code(d, "zed").await;
    d.step("disable", ip, 200, post(off, "zed", |s| json!({"password": PASSWORD, "code": s.v("zed.code")}))).await;
    d.mail("disable-mail", "zed@example.org", None).await;
    d.step("disable-again", ip, 409, post(off, "zed", |_| json!({"password": PASSWORD, "code": "123456"}))).await;
    login(d, ip, "zed", PASSWORD, "zed.plain").await;
    d.step("me-disabled", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("zed.plain"))).await;
    d.step("setup-after-disable", ip, 200, post("/api/v1/account/mfa/totp/setup", "zed", |_| json!({"password": PASSWORD}))).await;
}

/// The cap: `AUTH_MFA_PER_ACCOUNT` (10) codes per 15 minutes, right or wrong; the code given
/// at enable does not count.
async fn code_cap(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "uma", "uma@example.org").await;
    enable(d, ip, "uma").await;
    let mut ip = fresh_ip();
    for i in 0..10 {
        if i == 6 {
            ip = fresh_ip();
        }
        first_step(d, &format!("cap-login-{i}"), ip, "uma").await;
        let rc = format!("uma.rc{i}");
        let p = d
            .step(&format!("cap-code-{i}"), ip, 200, move |s: &Side| {
                Req::post("/api/v1/auth/login/mfa").json(json!({"mfaToken": s.v("uma.mfa"), "recoveryCode": s.v(&rc)}))
            })
            .await;
        // An account keeps 10 sessions: the newest one stays valid.
        d.save(&p, "uma.token", "token");
    }
    first_step(d, "cap-login-11", ip, "uma").await;
    next_code(d, "uma").await;
    d.step("cap-refused", ip, 429, mfa(|s| json!({"mfaToken": s.v("uma.mfa"), "code": s.v("uma.code")}))).await;
    d.step("cap-refused-reauth", ip, 429, post("/api/v1/account/export", "uma", |s| json!({"password": PASSWORD, "code": s.v("uma.code")}))).await;
    d.step("cap-without-code", ip, 403, post("/api/v1/account/export", "uma", |_| json!({"password": PASSWORD}))).await;
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
