//! The thresholds of the rate limits (docs/API.md section 1.5): each limit is driven to its first
//! `429` from a fresh client (and a fresh account for the limits counted per player), and the
//! number of requests let through, the statuses before it and the refusal are compared.
//! Limits with a window of a minute refill while a burst runs: the comparison allows what they
//! refill meanwhile.

use serde_json::json;

use super::{BoxFut, PASSWORD, new_account};
use crate::duo::{Duo, Side, fresh_ip};
use crate::http::Req;

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        auth_family(d).await;
        per_player(d).await;
        per_client(d).await;
    })
}

/// The limits of the sign-in, registration and recovery family, and `reauth`.
async fn auth_family(d: &mut Duo) {
    let per_10_min = |n: f64| n / 600.0;
    let per_hour = |n: f64| n / 3600.0;
    d.burst("auth", fresh_ip(), 25, per_10_min(20.0), 20, |_, i| {
        Req::post("/api/v1/auth/login")
            .json(json!({"login": format!("nobody{i}"), "password": "not the password"}))
    })
    .await;
    d.burst("auth-register", fresh_ip(), 15, per_hour(10.0), 10, |_, i| {
        Req::post("/api/v1/auth/register").json(json!({"username": format!("burst{i}"), "email": format!("burst{i}@example.org"), "password": PASSWORD}))
    })
    .await;
    d.burst("auth-mail", fresh_ip(), 15, per_hour(10.0), 10, |_, i| {
        Req::post("/api/v1/auth/verify-email/resend").json(json!({"email": format!("resend{i}@example.org")}))
    })
    .await;
    d.burst("auth-forgot", fresh_ip(), 6, per_hour(3.0), 3, |_, i| {
        Req::post("/api/v1/auth/password/forgot").json(json!({"email": format!("forgot{i}@example.org")}))
    })
    .await;
    d.burst("auth-reset", fresh_ip(), 15, per_hour(10.0), 10, |_, _| {
        Req::post("/api/v1/auth/password/reset")
            .json(json!({"token": "a".repeat(43), "newPassword": "a long new passphrase"}))
    })
    .await;
    // The reset page shares `auth_reset` with the API.
    d.burst("auth-reset-page", fresh_ip(), 15, per_hour(10.0), 10, |_, _| {
        Req::post("/reset-password").form(&[
            ("token", "abc"),
            ("newPassword", "a long new passphrase"),
            ("confirmPassword", "a long new passphrase"),
        ])
    })
    .await;
    // The link pages' POSTs share `auth` (20 per 10 minutes).
    d.burst("auth-verify-page", fresh_ip(), 25, per_10_min(20.0), 20, |_, _| {
        Req::post("/verify-email").form(&[("token", "abc")])
    })
    .await;

    // `reauth`: 20 per 10 minutes per client, over three accounts (each allows 10).
    let ip = fresh_ip();
    for u in ["lim1", "lim2", "lim3"] {
        new_account(d, ip, u, &format!("{u}@example.org")).await;
    }
    d.burst("reauth", fresh_ip(), 25, per_10_min(20.0), 20, |s: &Side, i| {
        let user = ["lim1", "lim2", "lim3"][(i / 10).min(2)];
        Req::post("/api/v1/account/password").bearer(&s.v(&format!("{user}.token"))).json(json!({}))
    })
    .await;
}

/// The limits counted per player.
async fn per_player(d: &mut Duo) {
    // Six accounts per address at most (`auth`: 20 requests per 10 minutes, 3 per account).
    let (ip, ip2) = (fresh_ip(), fresh_ip());
    for (i, u) in ["lim4", "lim5", "lim6", "lim7", "lim8", "lim9", "lim10"].into_iter().enumerate() {
        new_account(d, if i < 6 { ip } else { ip2 }, u, &format!("{u}@example.org")).await;
    }
    let as_user = |user: &'static str, req: fn(usize) -> Req| {
        move |s: &Side, i: usize| req(i).bearer(&s.v(&format!("{user}.token")))
    };
    d.burst(
        "account-export",
        fresh_ip(),
        8,
        5.0 / 3600.0,
        5,
        as_user("lim4", |_| Req::post("/api/v1/account/export").json(json!({}))),
    )
    .await;
    d.burst(
        "reports",
        fresh_ip(),
        35,
        30.0 / 3600.0,
        30,
        as_user("lim4", |_| Req::post("/api/v1/reports").json(json!({}))),
    )
    .await;
    d.burst("account", fresh_ip(), 75, 1.0, 60, as_user("lim5", |_| Req::get("/api/v1/account/me"))).await;
    d.burst(
        "account-preferences",
        fresh_ip(),
        10,
        1.0,
        0,
        as_user("lim5", |_| {
            Req::new("PUT", "/api/v1/account/preferences").json(json!({"acceptChallenges": "all"}))
        }),
    )
    .await;
    d.burst("account-games", fresh_ip(), 75, 1.0, 60, as_user("lim6", |_| Req::get("/api/v1/account/games")))
        .await;
    d.burst("sessions", fresh_ip(), 75, 1.0, 60, as_user("lim7", |_| Req::get("/api/v1/auth/sessions")))
        .await;
    d.burst(
        "public-read-player",
        fresh_ip(),
        75,
        1.0,
        60,
        as_user("lim8", |i| {
            Req::get(if i % 2 == 0 { "/api/v1/players/lim9" } else { "/api/v1/games/999999" })
        }),
    )
    .await;
    d.burst("gif", fresh_ip(), 40, 0.5, 30, as_user("lim10", |_| Req::get("/api/v1/games/999999/gif"))).await;
    // The account budget: 60 at once, 2 per second, whatever the endpoint.
    d.burst(
        "account-budget",
        fresh_ip(),
        90,
        2.0,
        60,
        as_user("lim9", |i| {
            Req::get(["/api/v1/account/me", "/api/v1/account/games", "/api/v1/auth/sessions"][i % 3])
        }),
    )
    .await;
}

/// The limits counted per client.
async fn per_client(d: &mut Duo) {
    d.burst("public-read", fresh_ip(), 75, 1.0, 60, |_, i| {
        Req::get(match i % 4 {
            0 => "/api/v1/players/nobody",
            1 => "/api/v1/players/nobody/games",
            2 => "/api/v1/games/999999",
            _ => "/api/v1/games/999999/pgn",
        })
    })
    .await;
    d.burst("page", fresh_ip(), 75, 1.0, 60, |_, i| {
        Req::get(match i % 3 {
            0 => "/verify-email?token=abc",
            1 => "/reset-password?token=abc",
            _ => "/confirm-email-change?token=abc",
        })
    })
    .await;
    // The per-address layer: 300 at once, 10 per second, every request.
    d.burst("per-address", fresh_ip(), 450, 10.0, 300, |_, i| {
        Req::get(match i % 3 {
            0 => "/api/v1/info",
            1 => "/healthz",
            _ => "/api/v1/leaderboard?category=3%2B2",
        })
    })
    .await;
}
