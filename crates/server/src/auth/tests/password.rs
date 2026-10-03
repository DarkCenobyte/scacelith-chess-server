//! Password reset and change, the reset page, resending the confirmation (auth.password.test.js).

use http::Method;
use serde_json::json;

use super::{Harness, NEW_PW, PW, link_in, token_of};
use crate::http::testing::TestResponse;
use crate::security::totp::{base32_decode, totp};
use crate::store::UserUpdate;

const FORM: &str = "application/x-www-form-urlencoded";

/// The token of the last reset mail to `email`.
async fn reset_token(h: &Harness, email: &str) -> String {
    let mail = h
        .sent()
        .await
        .into_iter()
        .rev()
        .find(|m| m.to == email && m.subject.contains("Reset your"))
        .expect("a reset mail");
    let link = link_in(&mail.text).unwrap();
    assert!(link.contains("/reset-password?"), "{link}");
    token_of(&link).unwrap()
}

async fn forgot(h: &Harness, email: &str) -> TestResponse {
    h.post("/api/v1/auth/password/forgot", json!({ "email": email })).await
}

async fn reset(h: &Harness, token: &str, new_password: &str) -> TestResponse {
    h.post("/api/v1/auth/password/reset", json!({ "token": token, "newPassword": new_password })).await
}

fn enc(s: &str) -> String {
    s.replace(' ', "%20")
}

#[tokio::test]
async fn forgot_answers_202_whatever_the_address_and_mails_accounts_only_once_per_5_minutes() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    for email in ["ALICE@example.com", "ghost@example.com", "not-an-email"] {
        let r = forgot(&h, email).await;
        assert_eq!((r.status, r.json()), (202, json!({ "status": "accepted" })), "{email}");
    }
    let sent = h.sent().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "alice@example.com");
    forgot(&h, "alice@example.com").await;
    assert_eq!(h.sent().await.len(), 1, "one e-mail per address per 5 minutes");
    h.advance(5 * 60_000 + 1);
    forgot(&h, "alice@example.com").await;
    assert_eq!(h.sent().await.len(), 2);
}

#[tokio::test]
async fn reset_through_the_api_checks_the_policy_is_single_use_revokes_every_session_and_keeps_mfa() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let session = h.token("alice", PW).await;
    let setup = h.post_as(&session, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = base32_decode(setup.json()["secret"].as_str().unwrap()).unwrap();
    h.post_as(&session, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    let unverified = UserUpdate { email_verified: Some(false), ..UserUpdate::default() };
    h.store.users().update(id, unverified).await.unwrap();

    forgot(&h, "alice@example.com").await;
    let token = reset_token(&h, "alice@example.com").await;
    let r = reset(&h, &token, "short").await;
    let body = r.json();
    assert_eq!(
        (r.status, &body["error"], &body["reason"]),
        (400, &json!("weak_password"), &json!("too_short"))
    );
    assert_eq!(reset(&h, &token, "alice is my name").await.json()["reason"], "contains_username");
    let r = reset(&h, &token, NEW_PW).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "password_reset" })));
    let r = reset(&h, &token, "another new passphrase").await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_token")));
    assert_eq!(h.me_status(&session).await, 401, "sessions revoked");
    let row = h.user(id).await;
    assert!(row.mfa_enabled, "MFA stays enabled");
    assert!(row.email_verified, "the link proved the address");
    let l = h.post("/api/v1/auth/login", json!({ "login": "alice", "password": NEW_PW })).await;
    assert_eq!(l.json()["mfaRequired"], true);
    assert_eq!(h.post("/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await.status, 401);
    let sent = h.sent().await;
    assert!(
        sent.iter()
            .any(|m| m.subject.contains("password was changed")
                && m.text.contains("reset with an e-mail link"))
    );
}

#[tokio::test]
async fn a_reset_or_a_password_change_ends_the_other_reset_links_of_the_account() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    let next_link = async || {
        forgot(&h, "alice@example.com").await;
        let token = reset_token(&h, "alice@example.com").await;
        h.advance(5 * 60_000 + 1); // the next link mail of the address
        token
    };
    let older = next_link().await;
    let used = next_link().await;
    assert_eq!(reset(&h, &used, NEW_PW).await.status, 200);
    let r = reset(&h, &older, "the attacker keeps this one").await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_token")));

    let before = next_link().await;
    let token = h.token("alice", NEW_PW).await;
    let r = h
        .post_as(&token, "/api/v1/account/password", json!({ "currentPassword": NEW_PW, "newPassword": PW }))
        .await;
    assert_eq!(r.status, 200);
    let r = reset(&h, &before, "the attacker keeps this one").await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_token")));
    assert_eq!(h.post("/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await.status, 200);
}

#[tokio::test]
async fn reset_tokens_expire_after_an_hour() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    forgot(&h, "alice@example.com").await;
    let token = reset_token(&h, "alice@example.com").await;
    h.advance(3_600_001);
    assert_eq!(reset(&h, &token, NEW_PW).await.json()["error"], "invalid_token");
    assert_eq!(reset(&h, "garbage", NEW_PW).await.json()["error"], "invalid_token");
}

#[tokio::test]
async fn the_reset_page_form_mismatch_weak_password_success_and_used_link() {
    let h = Harness::with_env(&[("SERVER_NAME", "Club <Test>")]).await;
    h.create_user("alice").await;
    forgot(&h, "alice@example.com").await;
    let token = reset_token(&h, "alice@example.com").await;
    let page = |t: &str| h.call(Method::GET, &format!("/reset-password?token={t}")).send();
    let submit = |body: String| h.call(Method::POST, "/reset-password").body(FORM, body).send();
    let r = page(&token).await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains(&format!("<input type=\"hidden\" name=\"token\" value=\"{token}\">")));
    assert!(r.text().contains("Club &lt;Test&gt;"), "escaped");
    assert!(!r.text().to_lowercase().contains("<script"));
    let r = submit(format!("token={token}&newPassword={}&confirmPassword=different", enc(NEW_PW))).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The two passwords are different"));
    let r = submit(format!("token={token}&newPassword=qwertyuiop&confirmPassword=qwertyuiop")).await;
    assert!(r.text().contains("too common"), "{}", r.text());
    let r = submit(format!("token={token}&newPassword={0}&confirmPassword={0}", enc(NEW_PW))).await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("Password changed"));
    assert!(!r.text().contains("sct_"));
    let r = page(&token).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("invalid or expired"));
    h.login("alice", NEW_PW).await;
}

#[tokio::test]
async fn change_password_needs_the_current_one_revokes_the_other_sessions_and_keeps_this_one() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    let a = h.token("alice", PW).await;
    let b = h.token("alice", PW).await;
    let path = "/api/v1/account/password";
    let r = h.post_as(&a, path, json!({ "currentPassword": "wrong one!", "newPassword": NEW_PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")));
    let r = h.post_as(&a, path, json!({ "currentPassword": PW, "newPassword": "password123" })).await;
    assert_eq!(r.json()["error"], "weak_password");
    let r = h.post_as(&a, path, json!({ "currentPassword": PW, "newPassword": NEW_PW })).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "password_changed" })));
    assert_eq!(h.me_status(&a).await, 200);
    assert_eq!(h.me_status(&b).await, 401);
    h.login("alice", NEW_PW).await;
    assert!(h.sent().await.iter().any(|m| m.subject.contains("password was changed")));
}

#[tokio::test]
async fn resend_verification_answers_202_and_mails_unconfirmed_accounts_only() {
    let h = Harness::new().await;
    h.create_user_with("pending", Some("pending@example.com"), Some(PW), false).await;
    h.create_user("done").await;
    for email in ["pending@example.com", "done@example.com", "ghost@example.com"] {
        let r = h.post("/api/v1/auth/verify-email/resend", json!({ "email": email })).await;
        assert_eq!((r.status, r.json()), (202, json!({ "status": "accepted" })), "{email}");
    }
    let sent = h.sent().await;
    assert_eq!(sent.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["pending@example.com"]);
    let token = token_of(&link_in(&sent[0].text).unwrap()).unwrap();
    let r = h.call(Method::POST, "/verify-email").body(FORM, format!("token={token}")).send().await;
    assert_eq!(r.status, 200);
    let row = h.store.users().by_username("pending".into()).await.unwrap().unwrap();
    assert!(row.email_verified);
}
