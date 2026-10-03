//! Registration (with e-mail verification), sign-in and its failure throttle, sessions, the
//! password change and the password reset by mail.

use std::time::Duration;

use serde_json::json;

use super::{BoxFut, PASSWORD, login, new_account};
use crate::duo::{Duo, fresh_ip};
use crate::http::Req;

const REGISTER: &str = "/api/v1/auth/register";
const LOGIN: &str = "/api/v1/auth/login";

fn register(user: &str, email: &str, password: &str) -> Req {
    Req::post(REGISTER).json(json!({"username": user, "email": email, "password": password}))
}

/// Registration, the confirmation link, the waiting signup and its username, resends.
pub fn signup(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        // Invalid input, in the order the server checks it.
        let invalid: Vec<(&str, Req, u16)> = vec![
            ("username-short", register("ab", "x1@example.org", PASSWORD), 400),
            ("username-long", register(&"u".repeat(21), "x1@example.org", PASSWORD), 400),
            ("username-space", register("a b", "x1@example.org", PASSWORD), 400),
            ("username-underscore-first", register("_abc", "x1@example.org", PASSWORD), 400),
            ("username-dash-first", register("-abc", "x1@example.org", PASSWORD), 400),
            ("username-dot", register("a.bc", "x1@example.org", PASSWORD), 400),
            ("username-accent", register("\u{e9}lise", "x1@example.org", PASSWORD), 400),
            ("username-reserved", register("admin", "x1@example.org", PASSWORD), 400),
            ("username-reserved-case", register("Admin", "x1@example.org", PASSWORD), 400),
            ("username-reserved-prefix", register("moderator7", "x1@example.org", PASSWORD), 400),
            ("username-reserved-white", register("white", "x1@example.org", PASSWORD), 400),
            ("email-no-at", register("valid1", "example.org", PASSWORD), 400),
            ("email-no-dot", register("valid1", "a@localhost", PASSWORD), 400),
            ("email-double-dot", register("valid1", "a..b@example.org", PASSWORD), 400),
            ("email-dot-domain", register("valid1", "a@.example.org", PASSWORD), 400),
            ("email-unicode", register("valid1", "\u{e9}@example.org", PASSWORD), 400),
            ("email-space", register("valid1", "a b@example.org", PASSWORD), 400),
            ("all-invalid", register("a", "b", "c"), 400),
            ("password-short", register("valid1", "x1@example.org", "short"), 400),
            ("password-9", register("valid1", "x1@example.org", "123456789"), 400),
            ("password-long", register("valid1", "x1@example.org", &"p".repeat(257)), 400),
            ("password-multibyte", register("valid1", "x1@example.org", &"\u{e9}".repeat(129)), 400),
            ("password-username", register("valid1", "x1@example.org", "my valid1 password"), 400),
            ("password-username-case", register("Valid1", "x1@example.org", "my VALID1 password"), 400),
            ("password-email", register("valid1", "secretlocal@example.org", "the secretlocal part"), 400),
            ("password-common", register("valid1", "x1@example.org", "password123"), 400),
            ("password-common-2", register("valid1", "x1@example.org", "1234567890"), 400),
        ];
        let mut at = ip;
        for (i, (name, req, expect)) in invalid.into_iter().enumerate() {
            if i % 9 == 8 {
                at = fresh_ip();
            }
            d.step(&format!("register-{name}"), at, expect, |_| req.clone()).await;
        }

        // A signup waits for its link; the username is held; sign-in and profile do not exist.
        let ip = fresh_ip();
        d.step("register-alice", ip, 202, |_| register("alice", "alice@example.org", PASSWORD)).await;
        d.mail("alice-verification-mail", "alice@example.org", Some("alice.verify")).await;
        d.step("login-before-verification", ip, 401, |_| {
            Req::post(LOGIN).json(json!({"login": "alice", "password": PASSWORD}))
        })
        .await;
        d.step("login-email-before-verification", ip, 401, |_| {
            Req::post(LOGIN).json(json!({"login": "alice@example.org", "password": PASSWORD}))
        })
        .await;
        d.step("profile-before-verification", ip, 404, |_| Req::get("/api/v1/players/alice")).await;
        d.step("register-held-username", ip, 409, |_| register("alice", "other@example.org", PASSWORD)).await;
        d.step("register-held-username-case", ip, 409, |_| register("ALICE", "other@example.org", PASSWORD)).await;
        d.step("resend-pending", ip, 202, |_| {
            Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": "alice@example.org"}))
        })
        .await;
        // The resend sends a new link, which replaces the first one.
        d.mail("resend-pending-mail", "alice@example.org", Some("alice.verify")).await;
        d.step("resend-unknown", ip, 202, |_| {
            Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": "nobody@example.org"}))
        })
        .await;
        d.step("resend-upper-case", ip, 202, |_| {
            Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": " ALICE@Example.org "}))
        })
        .await;
        d.step("resend-invalid", ip, 202, |_| Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": "x"}))).await;
        d.mail_count("resend-unknown-mail", "nobody@example.org", 0, 300).await;

        // The confirmation page: GET shows a button, POST confirms (creates the account).
        d.step("verify-page", ip, 200, |s| Req::get(format!("/verify-email?token={}", s.v("alice.verify")))).await;
        d.step("verify-page-bad-token", ip, 400, |_| Req::get("/verify-email?token=nope")).await;
        d.step("verify-post", ip, 200, |s| Req::post("/verify-email").form(&[("token", &s.v("alice.verify"))])).await;
        d.step("verify-post-again", ip, 400, |s| Req::post("/verify-email").form(&[("token", &s.v("alice.verify"))])).await;
        d.step("verify-page-used", ip, 400, |s| Req::get(format!("/verify-email?token={}", s.v("alice.verify")))).await;
        login(d, ip, "alice", PASSWORD, "alice.token").await;
        d.step("profile-after-verification", ip, 200, |_| Req::get("/api/v1/players/alice")).await;
        d.step("me-after-verification", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("alice.token"))).await;

        // Taken username and address of an existing account.
        let ip = fresh_ip();
        d.step("register-taken-username", ip, 409, |_| register("alice", "new@example.org", PASSWORD)).await;
        d.step("register-taken-username-case", ip, 409, |_| register("Alice", "new@example.org", PASSWORD)).await;
        d.step("register-existing-email", ip, 202, |_| register("alice2", "alice@example.org", PASSWORD)).await;
        d.mail("existing-email-notice", "alice@example.org", None).await;
        d.step("register-existing-email-upper", ip, 202, |_| register("alice3", " Alice@EXAMPLE.org ", PASSWORD)).await;
        d.mail_count("existing-email-notice-hourly", "alice@example.org", 0, 800).await;
        d.step("login-existing-email-signup", ip, 401, |_| {
            Req::post(LOGIN).json(json!({"login": "alice2", "password": PASSWORD}))
        })
        .await;
        d.step("resend-existing-account", ip, 202, |_| {
            Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": "alice@example.org"}))
        })
        .await;
        d.mail_count("resend-existing-account-mail", "alice@example.org", 0, 800).await;

        // A new signup with the same address replaces the waiting one and frees its username.
        let ip = fresh_ip();
        d.step("register-carol", ip, 202, |_| register("carol", "carol@example.org", PASSWORD)).await;
        d.mail("carol-mail", "carol@example.org", Some("carol.verify")).await;
        d.step("register-carol-replaced", ip, 202, |_| register("caroline", "carol@example.org", PASSWORD)).await;
        d.mail_count("carol-replaced-mail", "carol@example.org", 0, 800).await;
        d.step("register-carol-freed", ip, 202, |_| register("carol", "carol2@example.org", PASSWORD)).await;
        d.mail("carol2-mail", "carol2@example.org", Some("carol2.verify")).await;
        d.step("verify-replaced-link", ip, 400, |s| Req::post("/verify-email").form(&[("token", &s.v("carol.verify"))])).await;
        d.step("register-caroline-held", ip, 409, |_| register("caroline", "x2@example.org", PASSWORD)).await;
        d.step("verify-carol2", ip, 200, |s| Req::post("/verify-email").form(&[("token", &s.v("carol2.verify"))])).await;
        d.step("profile-carol", ip, 200, |_| Req::get("/api/v1/players/carol")).await;

        // A signup whose username another account took before its link was used: 409 page.
        let ip = fresh_ip();
        d.step("register-dave", ip, 202, |_| register("dave", "dave@example.org", PASSWORD)).await;
        d.mail("dave-mail", "dave@example.org", Some("dave.verify")).await;
        d.step("register-dave-other-address", ip, 409, |_| register("dave", "dave2@example.org", PASSWORD)).await;
        d.step("verify-dave", ip, 200, |s| Req::post("/verify-email").form(&[("token", &s.v("dave.verify"))])).await;

        // A form without a token, with two tokens, with JSON.
        d.step("verify-post-no-token", ip, 400, |_| Req::post("/verify-email").form(&[])).await;
        d.step("verify-post-two-tokens", ip, 400, |_| Req::post("/verify-email").form(&[("token", "a"), ("token", "b")])).await;
        d.step("verify-post-json", ip, 400, |_| Req::post("/verify-email").json(json!({"token": "x"}))).await;
        d.step("verify-post-text", ip, 415, |_| Req::post("/verify-email").body_bytes("text/plain", b"token=x".to_vec())).await;
        d.step("verify-post-unknown-field", ip, 400, |_| Req::post("/verify-email").form(&[("token", "x"), ("x", "y")])).await;
        d.step("verify-get-no-token", ip, 400, |_| Req::get("/verify-email")).await;
        d.step("verify-get-empty-token", ip, 400, |_| Req::get("/verify-email?token=")).await;
    })
}

/// Sign-in by name and address, wrong passwords, the failure delay, bans.
pub fn sign_in(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "erin", "erin@example.org").await;
        let ip = fresh_ip();
        d.step("by-email", ip, 200, |_| Req::post(LOGIN).json(json!({"login": "erin@example.org", "password": PASSWORD}))).await;
        d.step("by-email-case", ip, 200, |_| Req::post(LOGIN).json(json!({"login": " ERIN@example.ORG ", "password": PASSWORD}))).await;
        d.step("by-name-case", ip, 0, |_| Req::post(LOGIN).json(json!({"login": "ERIN", "password": PASSWORD}))).await;
        d.step("with-label", ip, 200, |_| {
            Req::post(LOGIN).json(json!({"login": "erin", "password": PASSWORD, "clientLabel": "Scacelith 1.4 (Windows)"}))
        })
        .await;
        d.step("label-empty", ip, 200, |_| Req::post(LOGIN).json(json!({"login": "erin", "password": PASSWORD, "clientLabel": ""}))).await;
        d.step("unknown-user", ip, 401, |_| Req::post(LOGIN).json(json!({"login": "nosuchuser", "password": PASSWORD}))).await;
        d.step("unknown-email", ip, 401, |_| Req::post(LOGIN).json(json!({"login": "nobody@x.org", "password": PASSWORD}))).await;
        d.step("password-case", ip, 401, |_| Req::post(LOGIN).json(json!({"login": "erin", "password": "Correct horse battery"}))).await;
        d.step("password-trailing-space", ip, 401, |_| {
            Req::post(LOGIN).json(json!({"login": "erin", "password": format!("{PASSWORD} ")}))
        })
        .await;

        // The failure delay: from the 5th failure on one login name, 2 s, 4 s...
        let ip = fresh_ip();
        new_account(d, ip, "frank", "frank@example.org").await;
        let wrong = || Req::post(LOGIN).json(json!({"login": "frank", "password": "wrong password!"}));
        let ip = fresh_ip();
        for i in 1..=3 {
            d.step(&format!("wrong-{i}"), ip, 401, |_| wrong()).await;
        }
        // The failures count per login name, whatever its form: the address counts too.
        d.step("wrong-4-by-email", ip, 401, |_| {
            Req::post(LOGIN).json(json!({"login": "frank@example.org", "password": "wrong password!"}))
        })
        .await;
        d.step("wrong-5", ip, 401, |_| wrong()).await;
        d.step("wrong-6", ip, 401, |_| wrong()).await;
        d.step("wrong-7-delayed", ip, 429, |_| wrong()).await;
        d.step("right-during-delay", ip, 429, |_| Req::post(LOGIN).json(json!({"login": "frank", "password": PASSWORD}))).await;
        d.step("delay-other-address", fresh_ip(), 429, |_| wrong()).await;
        d.step("delay-by-email", ip, 0, |_| {
            Req::post(LOGIN).json(json!({"login": "frank@example.org", "password": PASSWORD}))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(2300)).await;
        d.step("wrong-after-delay", ip, 401, |_| wrong()).await;
        d.step("delay-doubled", ip, 429, |_| wrong()).await;
        tokio::time::sleep(Duration::from_millis(4300)).await;
        d.step("right-after-delay", ip, 200, |_| Req::post(LOGIN).json(json!({"login": "frank", "password": PASSWORD}))).await;
        d.step("wrong-after-success", ip, 401, |_| wrong()).await;

        // A banned account: 403 banned with `until`, after a correct password only.
        let ip = fresh_ip();
        new_account(d, ip, "gina", "gina@example.org").await;
        d.admin(&["user", "ban", "gina", "--hours", "5", "--reason", "rude"]).await;
        d.step("banned", ip, 403, |_| Req::post(LOGIN).json(json!({"login": "gina", "password": PASSWORD}))).await;
        d.step("banned-wrong-password", ip, 401, |_| Req::post(LOGIN).json(json!({"login": "gina", "password": "nope nope nope"}))).await;
        d.step("banned-me", ip, 0, |s| Req::get("/api/v1/account/me").bearer(&s.v("gina.token"))).await;
        d.step("banned-profile", ip, 0, |_| Req::get("/api/v1/players/gina")).await;
        d.admin(&["user", "unban", "gina"]).await;
        d.step("unbanned", ip, 200, |_| Req::post(LOGIN).json(json!({"login": "gina", "password": PASSWORD}))).await;
        d.step("unbanned-me", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("gina.token"))).await;
    })
}

/// Sessions: the list, signing out one, this one or every one, the cap per account.
pub fn sessions(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "hank", "hank@example.org").await;
        let p = d
            .step("second-login", ip, 200, |_| {
                Req::post(LOGIN).json(json!({"login": "hank", "password": PASSWORD, "clientLabel": "Laptop"}))
            })
            .await;
        d.save(&p, "hank.token2", "token");
        d.step("list", ip, 200, |s| Req::get("/api/v1/auth/sessions").bearer(&s.v("hank.token"))).await;
        let p = d.step("list-from-second", ip, 200, |s| Req::get("/api/v1/auth/sessions").bearer(&s.v("hank.token2"))).await;
        d.save(&p, "hank.session2", "sessions.0.id");
        d.save_with(&p, "hank.session1", |r| {
            let j = r.json()?;
            let list = j.get("sessions")?.as_array()?.to_vec();
            list.iter().find(|s| s.get("current").is_some_and(|c| *c == crate::json::J::Bool(false)))?.get("id")?.scalar_text()
        });
        d.step("list-query", ip, 200, |s| Req::get("/api/v1/auth/sessions?x=1").bearer(&s.v("hank.token"))).await;
        d.step("delete-unknown", ip, 404, |s| Req::new("DELETE", "/api/v1/auth/sessions/999999").bearer(&s.v("hank.token"))).await;
        d.step("delete-not-number", ip, 0, |s| Req::new("DELETE", "/api/v1/auth/sessions/abc").bearer(&s.v("hank.token"))).await;
        d.step("delete-zero", ip, 0, |s| Req::new("DELETE", "/api/v1/auth/sessions/0").bearer(&s.v("hank.token"))).await;
        d.step("delete-negative", ip, 0, |s| Req::new("DELETE", "/api/v1/auth/sessions/-1").bearer(&s.v("hank.token"))).await;
        d.step("delete-float", ip, 0, |s| Req::new("DELETE", "/api/v1/auth/sessions/1.5").bearer(&s.v("hank.token"))).await;
        d.step("delete-huge", ip, 0, |s| {
            Req::new("DELETE", "/api/v1/auth/sessions/99999999999999999999").bearer(&s.v("hank.token"))
        })
        .await;
        d.step("delete-with-body", ip, 0, |s| {
            Req::new("DELETE", format!("/api/v1/auth/sessions/{}", s.v("hank.session2")))
                .bearer(&s.v("hank.token"))
                .json(json!({"x": 1}))
        })
        .await;
        d.step("delete-other", ip, 200, |s| {
            Req::new("DELETE", format!("/api/v1/auth/sessions/{}", s.v("hank.session2"))).bearer(&s.v("hank.token"))
        })
        .await;
        d.step("deleted-token", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("hank.token2"))).await;
        d.step("delete-again", ip, 404, |s| {
            Req::new("DELETE", format!("/api/v1/auth/sessions/{}", s.v("hank.session2"))).bearer(&s.v("hank.token"))
        })
        .await;
        // Another account's session.
        let ip2 = fresh_ip();
        new_account(d, ip2, "ivy", "ivy@example.org").await;
        d.step("delete-foreign", ip2, 404, |s| {
            Req::new("DELETE", format!("/api/v1/auth/sessions/{}", s.v("hank.session1"))).bearer(&s.v("ivy.token"))
        })
        .await;
        d.step("logout-with-body", ip, 400, |s| Req::post("/api/v1/auth/logout").bearer(&s.v("hank.token")).json(json!({"a": 1}))).await;
        d.step("logout-empty-json", ip, 200, |s| Req::post("/api/v1/auth/logout").bearer(&s.v("hank.token")).json(json!({}))).await;
        d.step("logged-out-token", ip, 401, |s| Req::get("/api/v1/auth/sessions").bearer(&s.v("hank.token"))).await;
        d.step("logout-again", ip, 401, |s| Req::post("/api/v1/auth/logout").bearer(&s.v("hank.token"))).await;
        d.step("logout-no-token", ip, 401, |_| Req::post("/api/v1/auth/logout")).await;

        // Sign out everywhere.
        let ip = fresh_ip();
        for i in 1..=3 {
            let p = login(d, ip, "ivy", PASSWORD, &format!("ivy.t{i}")).await;
            drop(p);
        }
        d.step("logout-all", ip, 200, |s| Req::post("/api/v1/auth/logout-all").bearer(&s.v("ivy.t1"))).await;
        for (i, var) in ["ivy.t1", "ivy.t2", "ivy.t3", "ivy.token"].iter().enumerate() {
            d.step(&format!("after-logout-all-{i}"), ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v(var))).await;
        }

        // At most MAX_SESSIONS_PER_USER (10) sessions: the 11th sign-in revokes the oldest.
        let ip = fresh_ip();
        new_account(d, ip, "jack", "jack@example.org").await;
        for i in 2..=11 {
            login(d, ip, "jack", PASSWORD, &format!("jack.t{i}")).await;
        }
        d.step("cap-oldest-revoked", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("jack.token"))).await;
        d.step("cap-second-kept", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("jack.t2"))).await;
        d.step("cap-list", ip, 200, |s| Req::get("/api/v1/auth/sessions").bearer(&s.v("jack.t11"))).await;
    })
}

/// The password change and the password reset by mail.
pub fn password(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        new_account(d, ip, "kate", "kate@example.org").await;
        login(d, ip, "kate", PASSWORD, "kate.other").await;
        let change = |current: &str, new: &str| {
            let body = json!({"currentPassword": current, "newPassword": new});
            move |s: &crate::duo::Side| Req::post("/api/v1/account/password").bearer(&s.v("kate.token")).json(body.clone())
        };
        d.step("change-wrong-current", ip, 403, change("wrong password", "another long passphrase")).await;
        d.step("change-weak", ip, 400, change(PASSWORD, "short")).await;
        d.step("change-contains-username", ip, 400, change(PASSWORD, "kate is my password")).await;
        d.step("change-common", ip, 400, change(PASSWORD, "password123")).await;
        d.step("change-same", ip, 0, change(PASSWORD, PASSWORD)).await;
        d.mail("change-same-mail", "kate@example.org", None).await;
        login(d, ip, "kate", PASSWORD, "kate.other").await;
        d.step("change-missing", ip, 400, |s| {
            Req::post("/api/v1/account/password").bearer(&s.v("kate.token")).json(json!({"currentPassword": PASSWORD}))
        })
        .await;
        d.step("change-no-token", ip, 401, |_| {
            Req::post("/api/v1/account/password").json(json!({"currentPassword": PASSWORD, "newPassword": "x"}))
        })
        .await;
        d.step("change-ok", ip, 200, change(PASSWORD, "another long passphrase")).await;
        d.mail("change-mail", "kate@example.org", None).await;
        d.step("change-keeps-this-session", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("kate.token"))).await;
        d.step("change-revokes-others", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("kate.other"))).await;
        d.step("old-password", ip, 401, |_| Req::post(LOGIN).json(json!({"login": "kate", "password": PASSWORD}))).await;
        login(d, ip, "kate", "another long passphrase", "kate.new").await;

        // Forgot / reset.
        let ip = fresh_ip();
        d.step("forgot", ip, 202, |_| Req::post("/api/v1/auth/password/forgot").json(json!({"email": "kate@example.org"}))).await;
        d.mail("forgot-mail", "kate@example.org", Some("kate.reset")).await;
        d.step("forgot-again", ip, 202, |_| {
            Req::post("/api/v1/auth/password/forgot").json(json!({"email": "kate@example.org"}))
        })
        .await;
        d.mail_count("forgot-again-mail", "kate@example.org", 0, 800).await;
        d.step("forgot-unknown", ip, 202, |_| {
            Req::post("/api/v1/auth/password/forgot").json(json!({"email": "nobody@example.org"}))
        })
        .await;
        d.mail_count("forgot-unknown-mail", "nobody@example.org", 0, 300).await;
        let ip = fresh_ip();
        d.step("forgot-invalid", ip, 202, |_| Req::post("/api/v1/auth/password/forgot").json(json!({"email": "not an address"}))).await;
        d.step("reset-page", ip, 200, |s| Req::get(format!("/reset-password?token={}", s.v("kate.reset")))).await;
        let reset = |token_var: &'static str, pw: &'static str| {
            move |s: &crate::duo::Side| {
                Req::post("/api/v1/auth/password/reset").json(json!({"token": s.v(token_var), "newPassword": pw}))
            }
        };
        d.step("reset-bad-token", ip, 400, |_| {
            Req::post("/api/v1/auth/password/reset").json(json!({"token": "nope", "newPassword": "a third passphrase"}))
        })
        .await;
        d.step("reset-weak", ip, 400, reset("kate.reset", "short")).await;
        d.step("reset-contains-name", ip, 400, reset("kate.reset", "kate kate kate kate")).await;
        d.step("reset-ok", ip, 200, reset("kate.reset", "a third passphrase")).await;
        d.mail("reset-mail", "kate@example.org", None).await;
        d.step("reset-used", ip, 400, reset("kate.reset", "a fourth passphrase")).await;
        d.step("reset-revoked-sessions", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("kate.new"))).await;
        d.step("reset-page-used", ip, 400, |s| Req::get(format!("/reset-password?token={}", s.v("kate.reset")))).await;
        login(d, ip, "kate", "a third passphrase", "kate.after").await;

        // A password change makes the reset links of the account stop working.
        let ip = fresh_ip();
        new_account(d, ip, "leo", "leo@example.org").await;
        d.step("leo-forgot", ip, 202, |_| Req::post("/api/v1/auth/password/forgot").json(json!({"email": "leo@example.org"}))).await;
        d.mail("leo-forgot-mail", "leo@example.org", Some("leo.reset")).await;
        d.step("leo-change", ip, 200, |s| {
            Req::post("/api/v1/account/password")
                .bearer(&s.v("leo.token"))
                .json(json!({"currentPassword": PASSWORD, "newPassword": "a brand new passphrase"}))
        })
        .await;
        d.mail("leo-change-mail", "leo@example.org", None).await;
        d.step("leo-reset-after-change", ip, 400, |s| {
            Req::post("/api/v1/auth/password/reset").json(json!({"token": s.v("leo.reset"), "newPassword": "a third passphrase"}))
        })
        .await;
    })
}
