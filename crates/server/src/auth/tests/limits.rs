//! The stricter limits of the auth family: registration, resend and forgot per address with
//! their IPv6 /48 ceilings, the reset form, second factors per account, re-authentication per
//! account, the password hashes one address can cause (`auth` and `reauth`), the Google sign-in
//! completion's /48 limit, and a refused request giving its earlier tokens back
//! (auth.limits.test.js).

use std::collections::BTreeMap;
use std::sync::Arc;

use http::Method;
use serde_json::{Value, json};

use super::{CountingHasher, Harness, PW, Setup, TEST_DEFAULTS};
use crate::config::{Config, test_config};
use crate::http::Router;
use crate::http::pages::{self, PageDeps};
use crate::http::routes::account::{self, AccountRouteDeps};
use crate::http::routes::account_export::{self, ExportRouteDeps};
use crate::http::routes::auth::{self as auth_routes, AuthRouteDeps};
use crate::http::routes::sso;
use crate::http::testing::TestResponse;
use crate::ids::UserId;
use crate::log::Logger;
use crate::security::totp::{base32_decode, totp};

const REG: &str = "/api/v1/auth/register";
const FORGOT: &str = "/api/v1/auth/password/forgot";
const LOGIN: &str = "/api/v1/auth/login";
const HOUR: i64 = 3_600_000;
const FORM: &str = "application/x-www-form-urlencoded";

fn reg(i: usize) -> Value {
    json!({ "username": format!("player{i}"), "email": format!("p{i}@example.com"), "password": "ivory rook takes e5" })
}

/// The rates of every route of `router`: `"<METHOD> <path>"` (`"PAGE ..."` for a page) to
/// `key:limit/window[,48:prefix][,shared]` in order.
fn rates_of(router: &Router) -> BTreeMap<String, Vec<String>> {
    router
        .routes()
        .iter()
        .map(|r| {
            let name = if r.opts().page { format!("PAGE {}", r.label()) } else { r.label().to_owned() };
            let rates = r
                .opts()
                .rates
                .iter()
                .map(|s| {
                    let mut k = format!("{}:{}/{}", s.key, s.limit, s.window_ms);
                    if let Some(p) = s.prefix_limit {
                        k.push_str(&format!(",48:{p}"));
                    }
                    if s.shared {
                        k.push_str(",shared");
                    }
                    k
                })
                .collect();
            (name, rates)
        })
        .collect()
}

#[tokio::test]
async fn the_rates_of_the_auth_family_keys_windows_48_ceilings_shared_and_refusal_weight() {
    let h = Harness::new().await;
    // The server's defaults, not the raised limits of the test servers.
    let config: Arc<Config> = Arc::new(test_config(&[]).unwrap());
    let deps = AuthRouteDeps { config: config.clone(), auth: h.auth.clone() };

    let mut auth = Router::new();
    auth_routes::register(&mut auth, deps.clone());
    pages::register(&mut auth, PageDeps { config: config.clone(), auth: h.auth.clone() });
    let rates = rates_of(&auth);
    let of = |name: &str| rates.get(name).cloned().unwrap_or_else(|| panic!("no route {name}: {rates:?}"));
    let auth_rate = "auth:20/600000,48:100,shared";
    assert_eq!(of("POST /api/v1/auth/register"), [auth_rate, "auth_register:10/3600000,48:30,shared"]);
    assert_eq!(of("POST /api/v1/auth/verify-email/resend"), [auth_rate, "auth_mail:10/3600000,48:30,shared"]);
    assert_eq!(
        of("POST /api/v1/auth/password/forgot"),
        [auth_rate, "auth_forgot:3/3600000,48:9,shared", "auth_forgot_day:10/86400000,48:30,shared"]
    );
    assert_eq!(of("POST /api/v1/auth/password/reset"), [auth_rate, "auth_reset:10/3600000,48:30,shared"]);
    assert_eq!(
        of("PAGE POST /reset-password"),
        of("POST /api/v1/auth/password/reset"),
        "the reset form counts like the API"
    );
    assert_eq!(of("POST /api/v1/auth/login"), [auth_rate]);
    assert_eq!(of("PAGE POST /verify-email"), [auth_rate]);
    assert_eq!(of("PAGE POST /confirm-email-change"), [auth_rate]);
    for r in auth.routes() {
        for s in r.opts().rates.iter().filter(|s| s.key.starts_with("auth")) {
            assert_eq!(s.abuse_weight, 5.0, "{} {}: refusals weigh 5 toward a block", r.label(), s.key);
        }
    }

    let mut sso_router = Router::new();
    sso::register(&mut sso_router, deps.clone());
    let rates = rates_of(&sso_router);
    assert_eq!(rates["POST /api/v1/auth/sso/google/start"], ["sso_start:30/600000,48:90,shared"]);
    assert_eq!(rates["POST /api/v1/auth/sso/google/finish"], ["sso_finish:30/60000"]);
    assert_eq!(rates["POST /api/v1/auth/sso/google/link"], [auth_rate], "the password step: the auth bucket");
    assert_eq!(rates["POST /api/v1/auth/sso/complete"], [auth_rate], "the auth bucket, its /48 included");
    assert_eq!(
        rates.keys().collect::<Vec<_>>(),
        [
            "POST /api/v1/auth/sso/complete",
            "POST /api/v1/auth/sso/google/finish",
            "POST /api/v1/auth/sso/google/link",
            "POST /api/v1/auth/sso/google/start"
        ],
        "no poll route, no callback page"
    );

    let mut account_router = Router::new();
    account::register(&mut account_router, AccountRouteDeps { config: config.clone(), auth: h.auth.clone() });
    let rates = rates_of(&account_router);
    for p in [
        "/account/password",
        "/account/mfa/totp/setup",
        "/account/mfa/totp/enable",
        "/account/mfa/totp/disable",
        "/account/mfa/recovery-codes",
        "/account/delete",
        "/account/email",
    ] {
        let name = format!("POST /api/v1{p}");
        assert_eq!(rates[&name], ["reauth:20/600000,48:100,shared", "reauth_user:10/600000,shared"], "{p}");
        let route = account_router.routes().iter().find(|r| r.label() == name).unwrap();
        assert!(route.opts().rates[1].by_user, "{p}: per account");
    }

    let mut export_router = Router::new();
    account_export::register(
        &mut export_router,
        ExportRouteDeps {
            config,
            store: h.store.clone(),
            auth: h.auth.clone(),
            history_summary: |_, _| serde_json::Map::new(),
            log: Logger::root().child("http"),
        },
    );
    assert_eq!(
        rates_of(&export_router)["POST /api/v1/account/export"],
        ["account_export:5/3600000,shared", "reauth:20/600000,48:100,shared", "reauth_user:10/600000,shared"]
    );
}

fn assert_limited(r: &TestResponse) {
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")), "{}", r.text());
    let secs = r.json()["retryAfter"].as_u64().unwrap();
    assert!(secs > 0);
    assert_eq!(r.header("retry-after"), Some(secs.to_string().as_str()));
}

#[tokio::test]
async fn registration_per_address_and_3_times_that_per_ipv6_48_with_the_auth_tokens_of_a_refusal_given_back()
{
    let h = Harness::with_env(&[("AUTH_REGISTER_PER_HOUR", "2"), ("AUTH_RATE_PER_IP", "5")]).await;
    let register = |ip: &str, n: usize| h.call_from(ip, Method::POST, REG).json(&reg(n)).send();
    let mut n = 0;
    for _ in 0..2 {
        assert_eq!(register("192.0.2.10", n).await.status, 202);
        n += 1;
    }
    for _ in 0..4 {
        assert_limited(&register("192.0.2.10", n).await);
        n += 1;
    }
    // auth (5 per 10 minutes): 2 spent by the registrations; the 4 refused ones gave theirs back.
    let login = |password: &str| {
        h.call_from("192.0.2.10", Method::POST, LOGIN)
            .json(&json!({ "login": "nobody", "password": password }))
            .send()
    };
    for i in 0..3 {
        assert_eq!(
            login("wrong password").await.status,
            401,
            "login {i}: the auth bucket still has its tokens"
        );
    }
    assert_eq!(login("x").await.status, 429);
    assert_eq!(register("192.0.2.11", n).await.status, 202, "another address");
    n += 1;

    // IPv6: 2 per /64, 6 per /48.
    for net in 1..=3 {
        for _ in 0..2 {
            assert_eq!(register(&format!("2001:db8:7:{net}::1"), n).await.status, 202);
            n += 1;
        }
        assert_eq!(register(&format!("2001:db8:7:{net}::2"), n).await.status, 429, "/64 {net}");
        n += 1;
    }
    assert_limited(&register("2001:db8:7:4::1", n).await);
    assert_eq!(register("2001:db8:8:1::1", n + 1).await.status, 202, "another /48");
}

#[tokio::test]
async fn password_recovery_3_per_hour_and_10_per_day_per_address_with_the_same_answers_for_any_address() {
    // The defaults: 3 per hour, 10 per day.
    let h = Harness::with_env(&[("AUTH_FORGOT_PER_HOUR", ""), ("AUTH_FORGOT_PER_DAY", "")]).await;
    assert_eq!((h.config.auth_forgot_per_hour, h.config.auth_forgot_per_day), (3, 10));
    h.create_user("alice").await;
    let ip = "198.51.100.20";
    let forgot = |email: &str, from: &str| {
        h.call_from(from, Method::POST, FORGOT).json(&json!({ "email": email })).send()
    };
    for email in ["alice@example.com", "ghost@example.com", "alice@example.com"] {
        let r = forgot(email, ip).await;
        assert_eq!((r.status, r.json()), (202, json!({ "status": "accepted" })));
    }
    let known = forgot("alice@example.com", ip).await;
    let unknown = forgot("ghost@example.com", ip).await;
    assert_eq!(
        (known.status, known.json()["error"].clone()),
        (429, json!("rate_limited")),
        "the fourth of the hour"
    );
    assert_eq!((unknown.status, unknown.json()["error"].clone()), (429, json!("rate_limited")));
    let without_delay = |r: &TestResponse| {
        let mut body = r.json();
        body.as_object_mut().unwrap().remove("retryAfter");
        body
    };
    assert_eq!(without_delay(&known), without_delay(&unknown), "a refusal says nothing about the address");
    assert!(known.json()["retryAfter"].as_u64().unwrap() > 60, "most of an hour");
    assert_eq!(forgot("ghost@example.com", "198.51.100.21").await.status, 202, "another address");

    // The hour slides by; the day does not: 3 + 3 + 3 + 1 = 10, then the day's limit.
    for _ in 0..2 {
        h.advance(2 * HOUR);
        for i in 0..3 {
            assert_eq!(forgot(&format!("x{i}@example.com"), ip).await.status, 202);
        }
        assert_eq!(forgot("y@example.com", ip).await.status, 429);
    }
    h.advance(2 * HOUR);
    assert_eq!(forgot("z@example.com", ip).await.status, 202, "the tenth of the day");
    let day = forgot("z2@example.com", ip).await;
    assert_eq!(
        (day.status, day.json()["error"].clone()),
        (429, json!("rate_limited")),
        "the eleventh of the day"
    );
    let secs = day.json()["retryAfter"].as_u64().unwrap();
    assert!(secs > 3600, "the day's window ({secs} s)");
    // The refusals by the day's limit gave the hour's tokens back: tomorrow, 3 again.
    h.advance(30 * HOUR);
    for i in 0..3 {
        assert_eq!(forgot(&format!("n{i}@example.com"), ip).await.status, 202, "next day {i}");
    }
    assert_eq!(forgot("n3@example.com", ip).await.status, 429);
    // One reset e-mail to alice per 5 minutes (the per-address throttle) on top of it all.
    let mails = h.sent().await;
    assert_eq!(
        mails.iter().filter(|m| m.to == "alice@example.com").count(),
        1,
        "two accepted requests within 5 minutes"
    );
}

#[tokio::test]
async fn password_recovery_an_ipv6_48_gets_3_times_the_limits_of_one_64() {
    let h = Harness::with_env(&[("AUTH_FORGOT_PER_HOUR", "3"), ("AUTH_FORGOT_PER_DAY", "10")]).await;
    let mut accepted = 0;
    for net in 1..=4 {
        for i in 0..3 {
            let r = h
                .call_from(&format!("2001:db8:42:{net}::9"), Method::POST, FORGOT)
                .json(&json!({ "email": format!("a{net}{i}@example.com") }))
                .send()
                .await;
            if r.status == 202 {
                accepted += 1;
            } else {
                assert_eq!(r.status, 429);
            }
        }
    }
    assert_eq!(accepted, 9, "3 per hour per /64, 9 per hour for the /48");
}

#[tokio::test]
async fn resends_per_address_and_reset_submissions_per_address_the_form_included() {
    let h = Harness::with_env(&[("AUTH_MAIL_PER_HOUR", "2"), ("AUTH_RESET_PER_HOUR", "2")]).await;
    h.create_user_with("alice", Some("alice@example.com"), Some(PW), false).await;
    let resend = |email: &str| {
        h.call_from("203.0.113.5", Method::POST, "/api/v1/auth/verify-email/resend")
            .json(&json!({ "email": email }))
            .send()
    };
    assert_eq!(resend("alice@example.com").await.status, 202);
    assert_eq!(resend("ghost@example.com").await.status, 202);
    let r = resend("alice@example.com").await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));

    let token = "x".repeat(43);
    let reset = |ip: &str| {
        h.call_from(ip, Method::POST, "/api/v1/auth/password/reset")
            .json(&json!({ "token": token, "newPassword": "a new long password" }))
            .send()
    };
    let form = || {
        h.call_from("203.0.113.6", Method::POST, "/reset-password")
            .body(FORM, format!("token={token}&newPassword=aaaaaaaaaaaa&confirmPassword=aaaaaaaaaaaa"))
            .send()
    };
    assert_eq!(reset("203.0.113.6").await.status, 400, "invalid token");
    assert_eq!(form().await.status, 400);
    let r = reset("203.0.113.6").await;
    assert_eq!(
        (r.status, r.json()["error"].clone()),
        (429, json!("rate_limited")),
        "the API and the form share auth_reset"
    );
    let page = form().await;
    assert_eq!(page.status, 429);
    assert!(page.header("content-type").unwrap().starts_with("text/html"));
    assert_eq!(reset("203.0.113.7").await.status, 400, "another address");
}

struct Enrolled {
    id: UserId,
    token: String,
    secret: Vec<u8>,
    recovery_codes: Vec<String>,
}

async fn enroll(h: &Harness, username: &str) -> Enrolled {
    let id = h.create_user(username).await;
    let token = h.token(username, PW).await;
    let setup = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = base32_decode(setup.json()["secret"].as_str().unwrap()).unwrap();
    let en =
        h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    assert_eq!(en.status, 200);
    h.advance(30_000);
    let recovery_codes = en.json()["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    Enrolled { id, token, secret, recovery_codes }
}

#[tokio::test]
async fn second_factors_per_account_every_15_minutes_refused_before_the_code_is_checked() {
    let h = Harness::with_env(&[("AUTH_MFA_PER_ACCOUNT", "3")]).await;
    let alice = enroll(&h, "alice").await;
    let mfa_token = async |login: &str| -> String {
        let r = h.post(LOGIN, json!({ "login": login, "password": PW })).await;
        r.json()["mfaToken"].as_str().expect("an MFA step").to_owned()
    };
    let step =
        |ip: &str, body: Value| h.call_from(ip, Method::POST, "/api/v1/auth/login/mfa").json(&body).send();
    // Three wrong codes from three addresses.
    for (i, code) in ["000000", "000001", "000002"].into_iter().enumerate() {
        let r = step(
            &format!("192.0.2.{}", i + 1),
            json!({ "mfaToken": mfa_token("alice").await, "code": code }),
        )
        .await;
        assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_code")));
    }
    // The fourth: refused before the code is looked at; the recovery code stays.
    let codes = h.store.mfa().count_recovery_codes(alice.id).await.unwrap();
    let token = mfa_token("alice").await;
    let r = step("192.0.2.4", json!({ "mfaToken": token, "recoveryCode": alice.recovery_codes[0] })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("too_many_attempts")));
    // The three count in full until the 15 minutes roll over, then decay over the next 15.
    let secs = r.json()["retryAfter"].as_u64().unwrap();
    assert!(secs > 0 && secs <= 1200, "retryAfter {secs}");
    assert_eq!(h.store.mfa().count_recovery_codes(alice.id).await.unwrap(), codes, "no recovery code spent");
    let r = step("192.0.2.5", json!({ "mfaToken": token, "code": totp(&alice.secret, h.now()) })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("too_many_attempts")));
    // Re-authentication checks the second factor through the same limit.
    let r = h
        .post_as(
            &alice.token,
            "/api/v1/account/mfa/totp/disable",
            json!({ "password": PW, "code": totp(&alice.secret, h.now()) }),
        )
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("too_many_attempts")));
    // Another account is not concerned; 30 minutes later alice signs in with a recovery code.
    let bob = enroll(&h, "bob").await;
    let bt = mfa_token("bob").await;
    let r = step("203.0.113.10", json!({ "mfaToken": bt, "code": totp(&bob.secret, h.now()) })).await;
    assert_eq!(r.status, 200);
    h.advance(30 * 60_000);
    let r = step(
        "203.0.113.10",
        json!({ "mfaToken": mfa_token("alice").await, "recoveryCode": alice.recovery_codes[0] }),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.store.mfa().count_recovery_codes(alice.id).await.unwrap(), codes - 1);
}

#[tokio::test]
async fn re_authentication_per_account_every_10_minutes_whatever_the_address() {
    let h = Harness::with_env(&[("AUTH_REAUTH_PER_USER", "2")]).await;
    h.create_user("alice").await;
    h.create_user("bob").await;
    let alice = h.token("alice", PW).await;
    let bob = h.token("bob", PW).await;
    let change = |token: &str, ip: &str| {
        h.call_from(ip, Method::POST, "/api/v1/account/password")
            .bearer(token)
            .json(&json!({ "currentPassword": "guess guess guess", "newPassword": "whatever new one" }))
            .send()
    };
    assert_eq!(change(&alice, "192.0.2.1").await.status, 403);
    assert_eq!(change(&alice, "198.51.100.1").await.status, 403);
    let r = change(&alice, "203.0.113.1").await;
    assert_eq!(
        (r.status, r.json()["error"].clone()),
        (429, json!("rate_limited")),
        "a third address: the account is limited"
    );
    let del = h
        .call_from("203.0.113.2", Method::POST, "/api/v1/account/delete")
        .bearer(&alice)
        .json(&json!({ "password": PW }))
        .send()
        .await;
    assert_eq!(del.status, 429, "every route that asks for the password");
    assert_eq!(change(&bob, "203.0.113.1").await.status, 403, "another account from the same address");
    h.advance(21 * 60_000);
    assert_eq!(change(&alice, "203.0.113.1").await.status, 403, "once the 10-minute window has passed");
}

#[tokio::test]
async fn the_password_hashes_of_one_address_auth_rate_per_ip_in_auth_as_many_again_in_reauth() {
    let counter = CountingHasher::new();
    let h =
        Harness::build(Setup { hasher: Some(counter.clone()), ..Setup::env(&[("AUTH_RATE_PER_IP", "3")]) })
            .await;
    let names = ["alice", "bob", "carol"];
    for n in names {
        h.create_user(n).await;
    }
    let work = || counter.hashes() + counter.checks();
    let ip = "192.0.2.44";
    let before = work();
    let mut tokens = Vec::new();
    for n in names {
        let r =
            h.call_from(ip, Method::POST, LOGIN).json(&json!({ "login": n, "password": PW })).send().await;
        assert_eq!(r.status, 200);
        tokens.push(r.json()["token"].as_str().unwrap().to_owned());
    }
    let r =
        h.call_from(ip, Method::POST, LOGIN).json(&json!({ "login": "alice", "password": PW })).send().await;
    assert_eq!(r.status, 429, "`auth` spent");
    assert_eq!(work() - before, 3);
    let change = |token: &str| {
        h.call_from(ip, Method::POST, "/api/v1/account/password")
            .bearer(token)
            .json(&json!({ "currentPassword": PW, "newPassword": "ivory rook takes e5" }))
            .send()
    };
    for token in &tokens {
        assert_eq!(change(token).await.status, 200, "`reauth` is a bucket of its own");
    }
    assert_eq!(work() - before, 3 + 3 * 2, "a change checks the current password and hashes the new one");
    assert_eq!(change(&tokens[0]).await.status, 429, "`reauth` spent");
    assert_eq!(
        work() - before,
        9,
        "so one address makes at most 3 times AUTH_RATE_PER_IP hashes per 10 minutes"
    );
}

#[tokio::test]
async fn the_google_sign_in_completion_has_the_auth_limit_with_its_ipv6_48_ceiling() {
    let h = Harness::with_env(&[("AUTH_RATE_PER_IP", "2"), ("AUTH_RATE_PER_PREFIX", "3")]).await;
    let complete = |ip: &str| {
        h.call_from(ip, Method::POST, "/api/v1/auth/sso/complete")
            .json(&json!({ "ssoTicket": "sso_x", "username": "someone" }))
            .send()
    };
    assert_ne!(complete("2001:db8:5:1::1").await.status, 429);
    assert_ne!(complete("2001:db8:5:1::1").await.status, 429);
    assert_eq!(complete("2001:db8:5:1::1").await.status, 429, "the /64");
    assert_ne!(complete("2001:db8:5:2::1").await.status, 429);
    assert_eq!(complete("2001:db8:5:3::1").await.status, 429, "the /48: 3");
}

#[test]
fn the_test_servers_raise_every_auth_limit() {
    for key in [
        "AUTH_REGISTER_PER_HOUR",
        "AUTH_MAIL_PER_HOUR",
        "AUTH_FORGOT_PER_HOUR",
        "AUTH_FORGOT_PER_DAY",
        "AUTH_RESET_PER_HOUR",
        "AUTH_MFA_PER_ACCOUNT",
        "AUTH_REAUTH_PER_USER",
        "USER_RATE_PER_MIN",
    ] {
        let value = TEST_DEFAULTS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or("0");
        assert!(value.parse::<i64>().unwrap() >= 10_000, "{key}");
    }
}
