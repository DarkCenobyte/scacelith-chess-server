//! The HTML pages of the e-mail links: `/verify-email` (a signup and an account's address),
//! `/reset-password` with its form errors, the body types a page takes, the 409 of a signup whose
//! user name another account took meanwhile. (`/confirm-email-change` is in `account`.)

use serde_json::json;

use super::{BoxFut, PASSWORD, login, new_account};
use crate::duo::{Duo, fresh_ip};
use crate::http::Req;

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        verify_pages(d).await;
        signup_taken(d).await;
        reset_pages(d).await;
    })
}

async fn verify_pages(d: &mut Duo) {
    let ip = fresh_ip();
    d.step("pam-register", ip, 202, |_| {
        Req::post("/api/v1/auth/register")
            .json(json!({"username": "pam", "email": "pam@example.org", "password": PASSWORD}))
    })
    .await;
    d.mail("pam-mail", "pam@example.org", Some("pam.verify")).await;
    let ip = fresh_ip();
    d.step("verify-page", ip, 200, |s| Req::get(format!("/verify-email?token={}", s.v("pam.verify")))).await;
    d.step("verify-page-head", ip, 200, |s| {
        Req::new("HEAD", format!("/verify-email?token={}", s.v("pam.verify")))
    })
    .await;
    d.step("verify-page-no-token", ip, 400, |_| Req::get("/verify-email")).await;
    d.step("verify-page-empty-token", ip, 400, |_| Req::get("/verify-email?token=")).await;
    d.step("verify-page-bad-token", ip, 400, |_| Req::get("/verify-email?token=abc")).await;
    d.step("verify-page-long-token", ip, 400, |_| {
        Req::get(format!("/verify-email?token={}", "a".repeat(200)))
    })
    .await;
    d.step("verify-page-encoded", ip, 0, |s| {
        Req::get(format!("/verify-email?token={}&x=%41", s.v("pam.verify")))
    })
    .await;
    d.step("verify-page-trailing-slash", ip, 0, |s| {
        Req::get(format!("/verify-email/?token={}", s.v("pam.verify")))
    })
    .await;
    d.step("verify-page-put", ip, 0, |_| Req::new("PUT", "/verify-email").form(&[("token", "abc")])).await;
    d.step("verify-page-options", ip, 0, |_| Req::new("OPTIONS", "/verify-email")).await;

    // The POST: body types and fields.
    let page = "/verify-email";
    d.step("verify-post-empty", ip, 400, move |_| Req::post(page)).await;
    d.step("verify-post-no-token-field", ip, 400, move |_| Req::post(page).form(&[("other", "1")])).await;
    d.step("verify-post-extra-field", ip, 0, move |s| {
        Req::post(page).form(&[("token", &s.v("pam.verify")), ("submit", "1")])
    })
    .await;
    d.step("verify-post-twice", ip, 400, move |s| {
        Req::post(page).form(&[("token", "abc"), ("token", &s.v("pam.verify"))])
    })
    .await;
    d.step("verify-post-text", ip, 415, move |s| {
        Req::post(page).body_bytes("text/plain", format!("token={}", s.v("pam.verify")))
    })
    .await;
    d.step("verify-post-latin1", ip, 415, move |s| {
        Req::post(page).body_bytes(
            "application/x-www-form-urlencoded; charset=latin1",
            format!("token={}", s.v("pam.verify")),
        )
    })
    .await;
    d.step("verify-post-multipart", ip, 415, move |_| {
        Req::post(page).body_bytes(
            "multipart/form-data; boundary=x",
            "--x\r\nContent-Disposition: form-data; name=\"token\"\r\n\r\nabc\r\n--x--\r\n",
        )
    })
    .await;
    d.step("verify-post-untyped", ip, 0, move |s| {
        Req::post(page).body_untyped(format!("token={}", s.v("pam.verify")))
    })
    .await;
    d.step("verify-post-bad-json", ip, 400, move |_| {
        Req::post(page).body_bytes("application/json", "{\"token\":")
    })
    .await;
    d.step("verify-post-json-number", ip, 400, move |_| Req::post(page).json(json!({"token": 5}))).await;
    d.step("verify-post-form-utf8", ip, 400, move |_| {
        Req::post(page)
            .body_bytes("application/x-www-form-urlencoded; charset=UTF-8", "token=%C3%A9t%C3%A9+x&")
    })
    .await;
    d.step("verify-post-bad-token", ip, 400, move |_| Req::post(page).form(&[("token", "abc")])).await;
    d.step("verify-post", ip, 200, move |s| Req::post(page).form(&[("token", &s.v("pam.verify"))])).await;
    d.step("verify-post-again", ip, 400, move |s| Req::post(page).form(&[("token", &s.v("pam.verify"))]))
        .await;
    d.step("verify-page-used", ip, 400, |s| Req::get(format!("/verify-email?token={}", s.v("pam.verify"))))
        .await;
    login(d, ip, "pam", PASSWORD, "pam.token").await;
}

/// A signup whose address an account took before its link was used: the link gets 409 and
/// creates nothing. (A second signup cannot take the user name: a waiting signup holds it.)
async fn signup_taken(d: &mut Duo) {
    let ip = fresh_ip();
    d.step("rex-register", ip, 202, |_| {
        Req::post("/api/v1/auth/register")
            .json(json!({"username": "rex", "email": "rex1@example.org", "password": PASSWORD}))
    })
    .await;
    d.mail("rex-mail", "rex1@example.org", Some("rex.one")).await;
    d.step("rex-register-same-name", ip, 409, |_| {
        Req::post("/api/v1/auth/register")
            .json(json!({"username": "Rex", "email": "rex2@example.org", "password": PASSWORD}))
    })
    .await;
    // pam takes the address with an e-mail change.
    d.step("pam-takes-address", ip, 202, |s| {
        Req::post("/api/v1/account/email")
            .bearer(&s.v("pam.token"))
            .json(json!({"newEmail": "rex1@example.org", "password": PASSWORD}))
    })
    .await;
    d.mail("pam-change-mail", "rex1@example.org", Some("pam.change")).await;
    d.mail("pam-change-notice", "pam@example.org", None).await;
    d.step("pam-confirms", ip, 200, |s| {
        Req::post("/confirm-email-change").form(&[("token", &s.v("pam.change"))])
    })
    .await;
    d.step("rex-page", ip, 0, |s| Req::get(format!("/verify-email?token={}", s.v("rex.one")))).await;
    d.step("rex-verify-taken", ip, 409, |s| Req::post("/verify-email").form(&[("token", &s.v("rex.one"))]))
        .await;
    d.step("rex-verify-again", ip, 0, |s| Req::post("/verify-email").form(&[("token", &s.v("rex.one"))]))
        .await;
    d.step("rex-login", ip, 401, |_| {
        Req::post("/api/v1/auth/login").json(json!({"login": "rex", "password": PASSWORD}))
    })
    .await;
    d.step("rex-profile", ip, 404, |_| Req::get("/api/v1/players/rex")).await;
    d.step("rex-name-free", ip, 0, |_| {
        Req::post("/api/v1/auth/register")
            .json(json!({"username": "rex", "email": "rex3@example.org", "password": PASSWORD}))
    })
    .await;
}

/// The password reset page and its form.
async fn reset_pages(d: &mut Duo) {
    let ip = fresh_ip();
    new_account(d, ip, "ross", "ross@example.org").await;
    login(d, ip, "ross", PASSWORD, "ross.other").await;
    let ip = fresh_ip();
    d.step("forgot", ip, 202, |_| {
        Req::post("/api/v1/auth/password/forgot").json(json!({"email": "ross@example.org"}))
    })
    .await;
    d.mail("forgot-mail", "ross@example.org", Some("ross.reset")).await;
    let page = "/reset-password";
    d.step("reset-page", ip, 200, |s| Req::get(format!("/reset-password?token={}", s.v("ross.reset")))).await;
    d.step("reset-page-head", ip, 200, |s| {
        Req::new("HEAD", format!("/reset-password?token={}", s.v("ross.reset")))
    })
    .await;
    d.step("reset-page-bad", ip, 400, |_| Req::get("/reset-password?token=abc")).await;
    d.step("reset-page-none", ip, 400, |_| Req::get("/reset-password")).await;
    d.step("reset-post-bad-token", ip, 400, move |_| {
        Req::post(page).form(&[
            ("token", "abc"),
            ("newPassword", "a new long passphrase"),
            ("confirmPassword", "a new long passphrase"),
        ])
    })
    .await;
    d.step("reset-post-differ", ip, 400, move |s| {
        Req::post(page).form(&[
            ("token", &s.v("ross.reset")),
            ("newPassword", "a new long passphrase"),
            ("confirmPassword", "another one"),
        ])
    })
    .await;
    d.step("reset-post-weak", ip, 400, move |s| {
        Req::post(page).form(&[
            ("token", &s.v("ross.reset")),
            ("newPassword", "short"),
            ("confirmPassword", "short"),
        ])
    })
    .await;
    d.step("reset-post-username", ip, 0, move |s| {
        Req::post(page).form(&[
            ("token", &s.v("ross.reset")),
            ("newPassword", "ross ross ross ross"),
            ("confirmPassword", "ross ross ross ross"),
        ])
    })
    .await;
    d.step("reset-post-missing-confirm", ip, 400, move |s| {
        Req::post(page).form(&[("token", &s.v("ross.reset")), ("newPassword", "a new long passphrase")])
    })
    .await;
    d.step("reset-post-html-chars", ip, 400, move |s| {
        Req::post(page).form(&[
            ("token", &format!("{}\"<b>", s.v("ross.reset"))),
            ("newPassword", "<script>x</script>"),
            ("confirmPassword", "<i>"),
        ])
    })
    .await;
    d.step("reset-post-differ-escaped", ip, 400, move |s| {
        Req::post(page).form(&[
            ("token", &s.v("ross.reset")),
            ("newPassword", "<script>x</script>&amp;"),
            ("confirmPassword", "<i>\"'"),
        ])
    })
    .await;
    d.step("reset-post", ip, 200, move |s| {
        Req::post(page).form(&[
            ("token", &s.v("ross.reset")),
            ("newPassword", "a new long passphrase"),
            ("confirmPassword", "a new long passphrase"),
        ])
    })
    .await;
    d.mail("reset-done-mail", "ross@example.org", None).await;
    d.step("reset-post-again", ip, 400, move |s| {
        Req::post(page).form(&[
            ("token", &s.v("ross.reset")),
            ("newPassword", "a new long passphrase"),
            ("confirmPassword", "a new long passphrase"),
        ])
    })
    .await;
    d.step("reset-page-used", ip, 400, |s| Req::get(format!("/reset-password?token={}", s.v("ross.reset"))))
        .await;
    d.step("reset-signed-out", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("ross.token"))).await;
    d.step("reset-signed-out-other", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("ross.other")))
        .await;
    d.step("reset-old-password", ip, 401, |_| {
        Req::post("/api/v1/auth/login").json(json!({"login": "ross", "password": PASSWORD}))
    })
    .await;
    d.step("reset-new-password", ip, 200, |_| {
        Req::post("/api/v1/auth/login").json(json!({"login": "ross", "password": "a new long passphrase"}))
    })
    .await;
}
