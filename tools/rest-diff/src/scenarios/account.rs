//! The account: its view, preferences, the e-mail address change and its confirmation page, the
//! data export and the deletion.

use serde_json::json;

use super::{BoxFut, PASSWORD, login, new_account};
use crate::duo::{Duo, Side, fresh_ip};
use crate::http::Req;

fn change_email(token_var: &'static str, body: serde_json::Value) -> impl Fn(&Side) -> Req {
    move |s: &Side| Req::post("/api/v1/account/email").bearer(&s.v(token_var)).json(body.clone())
}

/// The e-mail address change, its link and page, the notices.
pub fn email_change(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "mia", "mia@example.org").await;
        new_account(d, ip, "ned", "ned@example.org").await;
        // Ten requests of mia (the account's re-authentication limit).
        let ip = fresh_ip();
        let body = |email: &str, pw: &str| json!({"newEmail": email, "password": pw});
        d.step("invalid-email", ip, 400, change_email("mia.token", body("not-an-address", PASSWORD))).await;
        d.step("same-email", ip, 400, change_email("mia.token", body("mia@example.org", PASSWORD))).await;
        d.step("same-email-case", ip, 400, change_email("mia.token", body(" MIA@Example.org ", PASSWORD))).await;
        d.step("same-email-wrong-password", ip, 400, change_email("mia.token", body("mia@example.org", "nope"))).await;
        d.step("wrong-password", ip, 403, change_email("mia.token", body("mia.new@example.org", "wrong password"))).await;
        d.step("missing-password", ip, 400, |s| {
            Req::post("/api/v1/account/email").bearer(&s.v("mia.token")).json(json!({"newEmail": "mia.new@example.org"}))
        })
        .await;
        // A code without two-step verification is ignored.
        d.step("request", ip, 202, |s| {
            Req::post("/api/v1/account/email")
                .bearer(&s.v("mia.token"))
                .json(json!({"newEmail": " Mia.New@Example.org ", "password": PASSWORD, "code": "123456"}))
        })
        .await;
        d.mail("request-link-mail", "mia.new@example.org", Some("mia.change")).await;
        d.mail("request-notice-mail", "mia@example.org", None).await;
        d.step("me-pending", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("mia.token"))).await;
        // The same request again within 5 minutes: no new link, the same answer, a notice.
        d.step("request-again", ip, 202, change_email("mia.token", body("mia.new@example.org", PASSWORD))).await;
        d.mail_count("request-again-mail", "mia.new@example.org", 0, 800).await;
        d.mail_count("request-again-notice", "mia@example.org", 1, 0).await;

        d.step("confirm-page", ip, 200, |s| Req::get(format!("/confirm-email-change?token={}", s.v("mia.change")))).await;
        d.step("confirm-page-head", ip, 200, |s| Req::new("HEAD", format!("/confirm-email-change?token={}", s.v("mia.change")))).await;
        d.step("confirm-page-bad", ip, 400, |_| Req::get("/confirm-email-change?token=abc")).await;
        d.step("confirm-page-no-token", ip, 400, |_| Req::get("/confirm-email-change")).await;
        d.step("confirm-page-two-tokens", ip, 0, |s| {
            Req::get(format!("/confirm-email-change?token=abc&token={}", s.v("mia.change")))
        })
        .await;
        d.step("confirm-post-bad", ip, 400, |_| Req::post("/confirm-email-change").form(&[("token", "abc")])).await;
        // A page also takes a JSON body.
        d.step("confirm-post-json", ip, 400, |_| Req::post("/confirm-email-change").json(json!({"token": "abc"}))).await;
        d.step("confirm-post", ip, 200, |s| Req::post("/confirm-email-change").form(&[("token", &s.v("mia.change"))])).await;
        d.mail("confirmed-old-address-mail", "mia@example.org", None).await;
        d.step("confirm-post-again", ip, 400, |s| Req::post("/confirm-email-change").form(&[("token", &s.v("mia.change"))])).await;
        d.step("confirm-page-used", ip, 400, |s| Req::get(format!("/confirm-email-change?token={}", s.v("mia.change")))).await;
        d.step("me-changed", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("mia.token"))).await;
        d.step("login-new-address", ip, 200, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "mia.new@example.org", "password": PASSWORD}))
        })
        .await;
        d.step("login-old-address", ip, 401, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "mia@example.org", "password": PASSWORD}))
        })
        .await;

        // A change to the address of another account: the same answer, a notice to its owner,
        // and no link.
        let ip = fresh_ip();
        d.step("taken-address", ip, 202, change_email("mia.token", body("ned@example.org", PASSWORD))).await;
        d.mail("taken-address-notice", "ned@example.org", None).await;
        d.mail("taken-address-old-notice", "mia.new@example.org", None).await;
        d.step("me-pending-taken", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("mia.token"))).await;

        // A pending change cancelled by a password change.
        let ip = fresh_ip();
        new_account(d, ip, "nina", "nina@example.org").await;
        d.step("request-before-password-change", ip, 202, change_email("nina.token", body("nina2@example.org", PASSWORD))).await;
        d.mail("cancelled-link-mail", "nina2@example.org", Some("nina.change")).await;
        d.mail("cancelled-notice-mail", "nina@example.org", None).await;
        d.step("password-change", ip, 200, |s| {
            Req::post("/api/v1/account/password")
                .bearer(&s.v("nina.token"))
                .json(json!({"currentPassword": PASSWORD, "newPassword": "my second passphrase"}))
        })
        .await;
        d.mail("password-change-mail", "nina@example.org", None).await;
        d.step("me-after-password-change", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("nina.token"))).await;
        d.step("cancelled-link-page", ip, 400, |s| Req::get(format!("/confirm-email-change?token={}", s.v("nina.change")))).await;
        d.step("cancelled-link-post", ip, 400, |s| Req::post("/confirm-email-change").form(&[("token", &s.v("nina.change"))])).await;

        // A link whose address another account took meanwhile: 409.
        let ip = fresh_ip();
        new_account(d, ip, "olga", "olga@example.org").await;
        d.step("race-request", ip, 202, change_email("olga.token", body("race@example.org", PASSWORD))).await;
        d.mail("race-link-mail", "race@example.org", Some("olga.change")).await;
        d.mail("race-notice-mail", "olga@example.org", None).await;
        new_account(d, ip, "pete", "race@example.org").await;
        d.step("race-confirm-page", ip, 0, |s| Req::get(format!("/confirm-email-change?token={}", s.v("olga.change")))).await;
        d.step("race-confirm", ip, 409, |s| Req::post("/confirm-email-change").form(&[("token", &s.v("olga.change"))])).await;
    })
}

/// The account view, preferences, the export and the deletion.
pub fn account(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "quinn", "quinn@example.org").await;
        let ip = fresh_ip();
        d.step("me", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("quinn.token"))).await;
        d.step("me-head", ip, 200, |s| Req::new("HEAD", "/api/v1/account/me").bearer(&s.v("quinn.token"))).await;
        d.step("me-no-token", ip, 401, |_| Req::get("/api/v1/account/me")).await;
        let prefs = |v: serde_json::Value| {
            move |s: &Side| Req::new("PUT", "/api/v1/account/preferences").bearer(&s.v("quinn.token")).json(v.clone())
        };
        d.step("prefs-none", ip, 200, prefs(json!({"acceptChallenges": "none"}))).await;
        d.step("me-prefs-none", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("quinn.token"))).await;
        d.step("prefs-all", ip, 200, prefs(json!({"acceptChallenges": "all"}))).await;
        d.step("prefs-invalid", ip, 400, prefs(json!({"acceptChallenges": "friends"}))).await;
        d.step("prefs-empty", ip, 400, prefs(json!({}))).await;
        d.step("prefs-extra", ip, 400, prefs(json!({"acceptChallenges": "all", "x": 1}))).await;
        d.step("prefs-null", ip, 400, prefs(json!({"acceptChallenges": null}))).await;
        d.step("prefs-post", ip, 405, |s| Req::post("/api/v1/account/preferences").bearer(&s.v("quinn.token"))).await;
        d.step("prefs-no-body", ip, 400, |s| Req::new("PUT", "/api/v1/account/preferences").bearer(&s.v("quinn.token"))).await;

        // The export.
        let export = |body: serde_json::Value| {
            move |s: &Side| Req::post("/api/v1/account/export").bearer(&s.v("quinn.token")).json(body.clone())
        };
        d.step("export-wrong-password", ip, 403, export(json!({"password": "wrong"}))).await;
        d.step("export-missing-password", ip, 400, export(json!({}))).await;
        // Security events are saved in batches one second after the first one.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        d.step("export", ip, 200, export(json!({"password": PASSWORD}))).await;
        d.step("export-again", ip, 200, export(json!({"password": PASSWORD}))).await;
        d.step("export-get", ip, 405, |s| Req::get("/api/v1/account/export").bearer(&s.v("quinn.token"))).await;

        // The deletion.
        let ip = fresh_ip();
        new_account(d, ip, "rosa", "rosa@example.org").await;
        login(d, ip, "rosa", PASSWORD, "rosa.other").await;
        let delete = |body: serde_json::Value| {
            move |s: &Side| Req::post("/api/v1/account/delete").bearer(&s.v("rosa.token")).json(body.clone())
        };
        d.step("delete-wrong-password", ip, 403, delete(json!({"password": "wrong"}))).await;
        d.step("delete-extra-field", ip, 400, delete(json!({"password": PASSWORD, "confirm": true}))).await;
        d.step("delete-missing-password", ip, 400, delete(json!({}))).await;
        // A code without two-step verification is ignored.
        d.step("delete", ip, 200, delete(json!({"password": PASSWORD, "recoveryCode": "aaaa-bbbb-cc"}))).await;
        d.step("delete-again", ip, 401, delete(json!({"password": PASSWORD}))).await;
        d.step("deleted-token", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("rosa.token"))).await;
        d.step("deleted-other-token", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("rosa.other"))).await;
        d.step("deleted-profile", ip, 404, |_| Req::get("/api/v1/players/rosa")).await;
        d.step("deleted-login", ip, 401, |_| Req::post("/api/v1/auth/login").json(json!({"login": "rosa", "password": PASSWORD}))).await;
        d.step("deleted-login-email", ip, 401, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "rosa@example.org", "password": PASSWORD}))
        })
        .await;
        d.step("deleted-forgot", ip, 202, |_| Req::post("/api/v1/auth/password/forgot").json(json!({"email": "rosa@example.org"}))).await;
        d.mail_count("deleted-forgot-mail", "rosa@example.org", 0, 800).await;
        d.step("deleted-username-reuse", ip, 0, |_| {
            Req::post("/api/v1/auth/register").json(json!({"username": "rosa", "email": "rosa2@example.org", "password": PASSWORD}))
        })
        .await;
        d.step("deleted-email-reuse", ip, 202, |_| {
            Req::post("/api/v1/auth/register").json(json!({"username": "rosanna", "email": "rosa@example.org", "password": PASSWORD}))
        })
        .await;
        // The address got its confirmation mail less than 5 minutes ago.
        d.mail_count("deleted-email-reuse-mail", "rosa@example.org", 0, 800).await;
    })
}
