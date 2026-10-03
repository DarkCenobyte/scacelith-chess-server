//! The scenarios of the profiles with other settings: no e-mail verification (`open`),
//! registration closed (`closed`), other account rules and limits with the GIFs off (`custom`),
//! plain HTTP behind a trusted proxy (`proxy`), and the per-address layer with low numbers
//! (`abuse`: the address block and the requests in progress).

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use serde_json::json;

use super::{BoxFut, PASSWORD, login, new_account};
use crate::duo::{Duo, Side, fresh_ip};
use crate::http::{Client, Req};

fn register(user: &str, email: &str, password: &str) -> Req {
    Req::post("/api/v1/auth/register").json(json!({"username": user, "email": email, "password": password}))
}

/// `REQUIRE_EMAIL_VERIFICATION=false`: accounts at once, immediate e-mail changes.
pub fn open(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        d.step("info", ip, 200, |_| Req::get("/api/v1/info")).await;
        d.step("register", ip, 201, |_| register("opal", "opal@example.org", PASSWORD)).await;
        d.mail_count("register-mail", "opal@example.org", 0, 800).await;
        d.step("register-same-name", ip, 409, |_| register("Opal", "opal2@example.org", PASSWORD)).await;
        d.step("register-same-email", ip, 409, |_| register("opal2", "OPAL@example.org", PASSWORD)).await;
        d.mail_count("register-same-email-mail", "opal@example.org", 1, 800).await;
        d.step("register-invalid", ip, 400, |_| register("o", "opal3@example.org", PASSWORD)).await;
        d.step("register-other", ip, 201, |_| register("otto", "otto@example.org", PASSWORD)).await;
        login(d, ip, "opal", PASSWORD, "opal.token").await;
        login(d, ip, "otto", PASSWORD, "otto.token").await;
        d.step("me", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("opal.token"))).await;
        d.step("resend", ip, 202, |_| {
            Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": "opal@example.org"}))
        })
        .await;
        d.mail_count("resend-mail", "opal@example.org", 0, 800).await;
        d.step("verify-page", ip, 400, |_| Req::get("/verify-email?token=abc")).await;

        let ip = fresh_ip();
        let change = |email: &'static str| {
            move |s: &Side| {
                Req::post("/api/v1/account/email")
                    .bearer(&s.v("opal.token"))
                    .json(json!({"newEmail": email, "password": PASSWORD}))
            }
        };
        d.step("email-taken", ip, 409, change("otto@example.org")).await;
        d.mail_count("email-taken-notice", "otto@example.org", 1, 800).await;
        d.step("email-change", ip, 200, change(" Opal.New@Example.org ")).await;
        d.mail("email-change-notice", "opal@example.org", None).await;
        d.mail_count("email-change-new-address", "opal.new@example.org", 0, 300).await;
        d.step("me-changed", ip, 200, |s| Req::get("/api/v1/account/me").bearer(&s.v("opal.token"))).await;
        d.step("login-new-address", ip, 200, |_| {
            Req::post("/api/v1/auth/login")
                .json(json!({"login": "opal.new@example.org", "password": PASSWORD}))
        })
        .await;
        d.step("forgot", ip, 202, |_| {
            Req::post("/api/v1/auth/password/forgot").json(json!({"email": "opal.new@example.org"}))
        })
        .await;
        d.mail("forgot-mail", "opal.new@example.org", Some("opal.reset")).await;
        d.step("reset", ip, 200, |s| {
            Req::post("/api/v1/auth/password/reset")
                .json(json!({"token": s.v("opal.reset"), "newPassword": "a different passphrase"}))
        })
        .await;
        d.step("old-session", ip, 401, |s| Req::get("/api/v1/account/me").bearer(&s.v("opal.token"))).await;
        d.step("export", ip, 200, |s| {
            Req::post("/api/v1/account/export").bearer(&s.v("otto.token")).json(json!({"password": PASSWORD}))
        })
        .await;
    })
}

/// `REGISTRATION=closed`, a message of the day.
pub fn closed(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        d.step("info", ip, 200, |_| Req::get("/api/v1/info")).await;
        d.step("register", ip, 403, |_| register("carl", "carl@example.org", PASSWORD)).await;
        d.step("register-invalid", ip, 403, |_| register("c", "not an address", "x")).await;
        d.step("register-bad-body", ip, 0, |_| {
            Req::post("/api/v1/auth/register").json(json!({"username": 5}))
        })
        .await;
        d.mail_count("register-mail", "carl@example.org", 0, 500).await;
        d.step("login", ip, 401, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "carl", "password": PASSWORD}))
        })
        .await;
        d.step("resend", ip, 202, |_| {
            Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": "carl@example.org"}))
        })
        .await;
        d.step("forgot", ip, 202, |_| {
            Req::post("/api/v1/auth/password/forgot").json(json!({"email": "carl@example.org"}))
        })
        .await;
        d.step("sso-start", ip, 0, |_| Req::post("/api/v1/auth/sso/google/start").json(json!({}))).await;
        d.step("leaderboard", ip, 200, |_| Req::get("/api/v1/leaderboard?category=3%2B2")).await;
    })
}

/// Other account rules and limits, custom time controls and GIFs off.
pub fn custom(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        d.step("info", ip, 200, |_| Req::get("/api/v1/info")).await;
        d.step("username-3", ip, 400, |_| register("abc", "abc@example.org", PASSWORD)).await;
        d.step("username-17", ip, 400, |_| register("abcdefghijklmnopq", "abc@example.org", PASSWORD)).await;
        d.step("password-11", ip, 400, |_| register("abcd", "abcd@example.org", "elevenchars")).await;
        d.step("password-12", ip, 202, |_| register("abcd", "abcd@example.org", "twelve chars")).await;
        d.mail("password-12-mail", "abcd@example.org", None).await;
        new_account(d, ip, "ulla", "ulla@example.org").await;
        new_account(d, ip, "abcdefghijklmnop", "long@example.org").await;

        // HTTP_BODY_LIMIT=2048: the body of a sign-in at the limit and one byte over.
        let ip = fresh_ip();
        let login_body = |size: usize| {
            let pad = size - r#"{"login":"ulla","password":""}"#.len();
            format!(r#"{{"login":"ulla","password":"{}"}}"#, "p".repeat(pad))
        };
        d.step("body-at-limit", ip, 0, move |_| {
            Req::post("/api/v1/auth/login").body_bytes("application/json", login_body(2048))
        })
        .await;
        d.step("body-over-limit", ip, 413, move |_| {
            Req::post("/api/v1/auth/login").body_bytes("application/json", login_body(2049))
        })
        .await;
        d.step("password-change-over-limit", ip, 413, |s| {
            Req::post("/api/v1/account/password")
                .bearer(&s.v("ulla.token"))
                .json(json!({"currentPassword": PASSWORD, "newPassword": "n".repeat(2100)}))
        })
        .await;

        // GIF_ENABLED=false.
        let ip = fresh_ip();
        d.step("gif-get", ip, 404, |s| Req::get("/api/v1/games/1/gif").bearer(&s.v("ulla.token"))).await;
        d.step("gif-get-bad-id", ip, 0, |s| Req::get("/api/v1/games/abc/gif").bearer(&s.v("ulla.token")))
            .await;
        d.step("gif-get-bad-option", ip, 0, |s| {
            Req::get("/api/v1/games/1/gif?size=huge").bearer(&s.v("ulla.token"))
        })
        .await;
        d.step("gif-get-no-token", ip, 401, |_| Req::get("/api/v1/games/1/gif")).await;
        d.step("gif-post", ip, 404, |s| {
            Req::post("/api/v1/gif").bearer(&s.v("ulla.token")).json(json!({"pgn": "1. e4 *"}))
        })
        .await;
        d.step("gif-post-invalid", ip, 0, |s| {
            Req::post("/api/v1/gif").bearer(&s.v("ulla.token")).json(json!({"pgn": 5}))
        })
        .await;
        d.step("gif-post-large", ip, 0, |s| {
            Req::post("/api/v1/gif").bearer(&s.v("ulla.token")).json(json!({"pgn": "x".repeat(4000)}))
        })
        .await;

        // RATED_CATEGORIES=3+2,10+0, no custom time controls.
        let ip = fresh_ip();
        for (id, path) in [
            ("leaderboard-10+0", "/api/v1/leaderboard?category=10%2B0"),
            ("leaderboard-5+0", "/api/v1/leaderboard?category=5%2B0"),
            ("leaderboard-custom", "/api/v1/leaderboard?category=custom"),
        ] {
            d.step(id, ip, 0, move |_| Req::get(path)).await;
        }
        for (id, q) in [
            ("history-5+0", "?category=5%2B0"),
            ("history-10+0", "?category=10%2B0"),
            ("history-custom", "?category=custom"),
        ] {
            d.step(id, ip, 0, move |s: &Side| {
                Req::get(format!("/api/v1/account/games{q}")).bearer(&s.v("ulla.token"))
            })
            .await;
        }
        d.step("profile-long-name", ip, 200, |_| Req::get("/api/v1/players/abcdefghijklmnop")).await;
        d.step("profile-name-over-max", ip, 0, |_| Req::get("/api/v1/players/abcdefghijklmnopqrstu")).await;
    })
}

/// `TLS_MODE=proxy`: plain HTTP from a trusted proxy (127.0.0.1) that names the client in
/// `X-Forwarded-For`.
pub fn proxy(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let proxy = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let xff = |client: &'static str, req: Req| req.header("X-Forwarded-For", client);
        d.step("info", proxy, 200, move |_| xff("203.0.113.1", Req::get("/api/v1/info"))).await;
        d.step("health", proxy, 200, |_| Req::get("/healthz")).await;
        new_account(d, proxy, "pia", "pia@example.org").await;

        // Each forwarded client has its own `auth` bucket (20 per 10 minutes).
        let fail = |i: usize| {
            Req::post("/api/v1/auth/login")
                .json(json!({"login": format!("nobody{i}"), "password": "wrong password"}))
        };
        d.burst("auth-forwarded-client", proxy, 25, 20.0 / 600.0, 20, move |_, i| {
            fail(i).header("X-Forwarded-For", "203.0.113.5")
        })
        .await;
        d.step("other-forwarded-client", proxy, 401, move |_| {
            fail(100).header("X-Forwarded-For", "203.0.113.6")
        })
        .await;
        d.step("forwarded-chain", proxy, 0, move |_| {
            fail(101).header("X-Forwarded-For", "198.51.100.9, 203.0.113.5")
        })
        .await;
        d.step("forwarded-chain-trusted-last", proxy, 0, move |_| {
            fail(102).header("X-Forwarded-For", "203.0.113.5, 127.0.0.1")
        })
        .await;
        d.step("forwarded-two-headers", proxy, 0, move |_| {
            fail(103).header("X-Forwarded-For", "198.51.100.10").header("X-Forwarded-For", "203.0.113.5")
        })
        .await;
        d.step("forwarded-garbage", proxy, 0, move |_| fail(104).header("X-Forwarded-For", "not an address"))
            .await;
        d.step("forwarded-ipv6", proxy, 0, move |_| fail(105).header("X-Forwarded-For", "2001:db8::1")).await;
        d.step("forwarded-port", proxy, 0, move |_| fail(106).header("X-Forwarded-For", "203.0.113.5:4444"))
            .await;
        d.step("forwarded-empty", proxy, 0, move |_| fail(107).header("X-Forwarded-For", "")).await;
        // An untrusted peer's header is ignored: its own address counts.
        let untrusted = fresh_ip();
        d.step("untrusted-peer", untrusted, 401, move |_| fail(108).header("X-Forwarded-For", "203.0.113.5"))
            .await;

        // The address recorded for the sessions and security events.
        let ip = proxy;
        let p = d
            .step("login-forwarded", ip, 200, |_| {
                Req::post("/api/v1/auth/login")
                    .json(json!({"login": "pia", "password": PASSWORD}))
                    .header("X-Forwarded-For", "198.51.100.77")
            })
            .await;
        d.save(&p, "pia.fwd", "token");
        d.step("sessions", ip, 200, |s| {
            Req::get("/api/v1/auth/sessions")
                .bearer(&s.v("pia.fwd"))
                .header("X-Forwarded-For", "198.51.100.77")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        d.step("export", ip, 200, |s| {
            Req::post("/api/v1/account/export")
                .bearer(&s.v("pia.fwd"))
                .header("X-Forwarded-For", "2001:db8:1:2::3")
                .json(json!({"password": PASSWORD}))
        })
        .await;
        d.step("security-headers", ip, 400, |_| {
            Req::get("/verify-email?token=abc").header("X-Forwarded-Proto", "https")
        })
        .await;
    })
}

/// The per-address layer with low numbers (profile `abuse`): its threshold, the address block
/// after the refusals, the block's end, and the requests in progress.
pub fn abuse(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        // HTTP_RATE_PER_IP=60: 30 at once, then 1 per second.
        let ip = fresh_ip();
        d.burst("per-address", ip, 40, 1.0, 30, |_, _| Req::get("/api/v1/info")).await;
        // ABUSE_BLOCK_REFUSALS_PER_MIN=10: the refusals go on until the block.
        for i in 2..=12 {
            d.step(&format!("refusal-{i}"), ip, if i < 12 { 429 } else { 0 }, |_| Req::get("/api/v1/info"))
                .await;
        }
        d.step("blocked-new-connection", ip, 0, |_| Req::get("/api/v1/info").fresh()).await;
        d.step("blocked-health", ip, 0, |_| Req::get("/healthz").fresh()).await;
        // ABUSE_BLOCK_BASE_SEC=3.
        tokio::time::sleep(Duration::from_millis(4500)).await;
        d.step("after-block", ip, 200, |_| Req::get("/api/v1/info").fresh()).await;
        d.step("other-address", fresh_ip(), 200, |_| Req::get("/api/v1/info")).await;

        // IP_MAX_INFLIGHT=2: two requests waiting for their bodies, then a third one.
        let ip = fresh_ip();
        let partial = || {
            Req::raw(
                "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\"login\":",
            )
        };
        let held = |side: &Side| {
            let target = side.server.target.clone();
            let req = partial();
            async move { Client::new(target, ip).send(&req).await }
        };
        let (n1, n2, r1, r2) = (held(&d.node), held(&d.node), held(&d.rust), held(&d.rust));
        let refused = async {
            tokio::time::sleep(Duration::from_millis(700)).await;
            d.step("inflight-refused", ip, 429, |_| Req::get("/api/v1/info").fresh()).await;
        };
        let (a1, a2, b1, b2, ()) = tokio::join!(n1, n2, r1, r2, refused);
        for (i, (a, b)) in [(a1, b1), (a2, b2)].into_iter().enumerate() {
            d.compare(&format!("inflight-held-{}", i + 1), &partial(), &a, &b, 0);
        }
        d.step("inflight-free-again", ip, 200, |_| Req::get("/api/v1/info").fresh()).await;
    })
}
