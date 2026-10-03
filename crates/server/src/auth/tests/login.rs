//! Password login: answers, failures and their counters, the login proof of work, hash upgrades,
//! the session limit (auth.login.test.js).

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use super::{Harness, PW, Setup, sha256_hex};
use crate::auth::public_base_url;
use crate::config::{Config, TlsMode};
use crate::security::password::{Argon2Hasher, Argon2Params, PasswordHasher};
use crate::security::pow::solve_pow;
use crate::store::{NewSanction, NewSession, NewUser, SanctionKind, Source};

const LOGIN: &str = "/api/v1/auth/login";

async fn attempt(h: &Harness, login: &str, password: &str) -> crate::http::testing::TestResponse {
    h.post(LOGIN, json!({ "login": login, "password": password })).await
}

#[tokio::test]
async fn login_by_username_or_email_case_insensitive_gives_a_session_and_the_user_view() {
    let h = Harness::new().await;
    let id = h.create_user("Alice").await;
    for login in ["Alice", "alice", "ALICE@EXAMPLE.COM", " alice@example.com "] {
        let r = h.post(LOGIN, json!({ "login": login, "password": PW, "clientLabel": "Windows 11" })).await;
        assert_eq!(r.status, 200, "{login}");
        let body = r.json();
        let token = body["token"].as_str().unwrap();
        assert!(crate::auth::is_prefixed_token(token, "sct_"), "{token}");
        assert_eq!(body["expiresAt"], json!(h.now() + 90 * super::DAY_MS));
        assert_eq!(
            body["user"],
            json!({
                "id": id, "username": "Alice", "email": "alice@example.com", "emailVerified": true,
                "mfaEnabled": false, "googleLinked": false, "hasPassword": true, "acceptChallenges": "all",
                "createdAt": h.now(), "lastLoginAt": h.now(), "pendingEmail": null,
            })
        );
        assert_eq!(super::keys(&body), ["token", "expiresAt", "user"]);
        let hash = sha256_hex(token);
        let row = h.store.read(move |db| db.sessions().by_token_hash(&hash)).await.unwrap();
        assert!(row.is_some(), "the store keeps the token's SHA-256");
    }
    let rows = h.store.read(move |db| db.sessions().all_for_user(id)).await.unwrap();
    assert_eq!(rows[0].client_label.as_deref(), Some("Windows 11"));
    assert_eq!(h.user(id).await.last_login_at, Some(h.now()));
}

#[tokio::test]
async fn unknown_user_wrong_password_and_passwordless_account_get_identical_answers() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    h.create_user_with("googler", Some("googler@example.com"), None, true).await;
    for (login, password) in [
        ("nobody", "whatever pass"),
        ("alice", "wrong password"),
        ("googler", "anything at all"),
        ("nobody@example.com", "x"),
    ] {
        let r = attempt(&h, login, password).await;
        assert_eq!(
            (r.status, r.json()),
            (
                401,
                json!({ "error": "invalid_credentials", "message": "Wrong user name, e-mail or password." })
            ),
            "{login}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_unknown_user_and_wrong_password_take_similar_time() {
    let hasher: Arc<dyn PasswordHasher> = Arc::new(Argon2Hasher::new(Argon2Params {
        memory_kib: 4096,
        passes: 1,
        lanes: 1,
        ..Argon2Params::DEFAULT
    }));
    let h = Harness::build(Setup {
        hasher: Some(hasher),
        ..Setup::env(&[("AUTH_FAILURES_PER_ACCOUNT", "1000")])
    })
    .await;
    h.create_user("alice").await;
    let time = async |login: &str| {
        let t0 = Instant::now();
        assert_eq!(attempt(&h, login, "not the password").await.status, 401);
        t0.elapsed().as_secs_f64() * 1000.0
    };
    time("warm-up").await;
    let (mut known, mut unknown) = (Vec::new(), Vec::new());
    // Interleaved samples and medians: load from other processes affects both sides alike.
    for i in 0..15 {
        known.push(time("alice").await);
        unknown.push(time(&format!("ghost{i}")).await);
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[7]
    };
    let (k, u) = (median(&mut known), median(&mut unknown));
    let ratio = k / u;
    assert!(ratio > 0.6 && ratio < 1.67, "median known {k:.1} ms, unknown {u:.1} ms");
}

#[tokio::test]
async fn email_unverified_and_banned_only_after_the_password_matched() {
    let h = Harness::new().await;
    let id = h.create_user("banned1").await;
    let now = h.now();
    let ban = NewSanction {
        user_id: id,
        kind: SanctionKind::Ban,
        reason: Some("cheating".into()),
        source: Source::Auto,
        game_id: None,
        starts_at: now - 1000,
        ends_at: Some(now + 3_600_000),
        created_by: None,
        created_at: now,
    };
    h.store.write(move |db| db.sanctions().create(&ban)).await.unwrap();
    assert_eq!(attempt(&h, "banned1", "wrong password").await.json()["error"], "invalid_credentials");
    let r = attempt(&h, "banned1", PW).await;
    let body = r.json();
    assert_eq!((r.status, &body["error"], &body["until"]), (403, &json!("banned"), &json!(now + 3_600_000)));
    h.advance(3_600_001);
    assert_eq!(attempt(&h, "banned1", PW).await.status, 200, "ban over");
    h.create_user_with("unverified", Some("unverified@example.com"), Some(PW), false).await;
    let r = attempt(&h, "unverified", PW).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("email_unverified")));
}

#[tokio::test]
async fn per_account_failure_counter_exponential_delay_identical_for_unknown_accounts() {
    let h = Harness::with_env(&[("AUTH_FAILURES_PER_ACCOUNT", "3")]).await;
    h.create_user("alice").await;
    for login in ["alice", "nobody"] {
        for _ in 0..3 {
            assert_eq!(attempt(&h, login, "wrong password").await.status, 401);
        }
    }
    let a = attempt(&h, "alice", PW).await;
    let b = attempt(&h, "nobody", "wrong password").await;
    let refused = json!({ "error": "too_many_attempts", "message": "Too many attempts; wait before trying again.", "retryAfter": 2 });
    assert_eq!((a.status, a.json()), (429, refused));
    assert_eq!((b.status, b.json()), (a.status, a.json()), "unknown accounts are throttled the same way");
    assert_eq!(a.header("retry-after"), Some("2"));
    h.advance(2000);
    assert_eq!(attempt(&h, "alice", "wrong password").await.status, 401);
    assert_eq!(attempt(&h, "alice", "wrong password").await.json()["retryAfter"], 4, "doubled");
    h.advance(4000);
    assert_eq!(attempt(&h, "alice", PW).await.status, 200, "the right password after the delay");
    assert_eq!(attempt(&h, "alice", "wrong password").await.status, 401, "counter reset by the success");
    let events = h.events().await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    for kind in ["login_failed", "login_lockout", "login_throttled", "login"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let failed = events.iter().find(|e| e.kind == "login_failed").unwrap();
    assert_eq!(failed.ip.as_deref(), Some("203.0.113.10"));
    assert!(matches!(failed.detail, Some(Value::Object(_))), "the detail is stored as one JSON object");
}

#[tokio::test]
async fn a_credential_stuffing_wave_turns_on_the_login_proof_of_work() {
    let h = Harness::with_env(&[("POW_LOGIN_BITS", "6"), ("POW_LOGIN_TRIGGER_PER_MIN", "4")]).await;
    h.create_user("alice").await;
    for i in 0..4 {
        let r = h
            .call_from(&format!("198.51.100.{i}"), http::Method::POST, LOGIN)
            .json(&json!({ "login": format!("victim{i}"), "password": "guess guess" }))
            .send()
            .await;
        assert_eq!(r.status, 401);
    }
    assert!(h.auth.login_pow_active());
    let r = attempt(&h, "alice", PW).await;
    assert_eq!(r.status, 428);
    let pow = r.json()["pow"].clone();
    assert_eq!(pow["bits"], 6);
    let challenge = pow["challenge"].as_str().unwrap();
    let answer = json!({ "challenge": challenge, "nonce": solve_pow(challenge, 6) });
    let body = json!({ "login": "alice", "password": PW, "pow": answer });
    assert_eq!(h.post(LOGIN, body.clone()).await.status, 200);
    let r = h.post(LOGIN, body).await;
    assert_eq!((r.status, r.json()["reason"].clone()), (428, json!("replayed")));
    h.advance(5 * 60_000 + 1);
    assert!(!h.auth.login_pow_active(), "the wave is over");
    assert_eq!(attempt(&h, "alice", PW).await.status, 200);
}

#[tokio::test]
async fn outdated_hashes_are_upgraded_at_login() {
    let h = Harness::new().await;
    let old =
        Argon2Hasher::new(Argon2Params { memory_kib: 32, passes: 1, lanes: 1, ..Argon2Params::DEFAULT });
    let user = NewUser {
        username: "legacy".into(),
        email: Some("legacy@example.com".into()),
        password_hash: Some(old.hash("legacy passphrase 1").unwrap()),
        email_verified: true,
        accept_challenges: true,
        created_at: h.now(),
    };
    let id = h.store.users().create(user).await.unwrap();
    h.login("legacy", "legacy passphrase 1").await;
    let stored = h.user(id).await.password_hash.unwrap();
    assert!(stored.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{stored}");
    h.login("legacy", "legacy passphrase 1").await;
}

#[tokio::test]
async fn max_sessions_per_user_revokes_the_oldest_session_and_drops_it_from_the_cache() {
    let h = Harness::with_env(&[("MAX_SESSIONS_PER_USER", "2")]).await;
    h.create_user("alice").await;
    let a = h.token("alice", PW).await;
    h.advance(1000);
    let b = h.token("alice", PW).await;
    assert_eq!(h.me_status(&a).await, 200, "cached");
    h.advance(1000);
    let c = h.token("alice", PW).await;
    assert_eq!(h.me_status(&a).await, 401);
    assert_eq!(h.me_status(&b).await, 200);
    assert_eq!(h.me_status(&c).await, 200);
    let calls = h.revoked.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, Some(vec![sha256_hex(&a)]));
}

#[tokio::test]
async fn max_sessions_per_user_broadcasts_the_session_revoked_by_a_login_elsewhere_too() {
    let h = Harness::with_env(&[("MAX_SESSIONS_PER_USER", "2")]).await;
    let id = h.create_user("alice").await;
    let a = h.token("alice", PW).await;
    h.advance(1000);
    // A login of the same account elsewhere inserted its session just before this one.
    let now = h.now();
    let other = NewSession {
        user_id: id,
        token_hash: "f".repeat(64),
        created_at: now,
        expires_at: now + 90 * super::DAY_MS,
        idle_expires_at: None,
        client_label: None,
        ip: None,
    };
    h.store.sessions().create(other).await.unwrap();
    h.token("alice", PW).await;
    assert_eq!(h.revoked.calls(), [(id, Some(vec![sha256_hex(&a)]))]);
    assert_eq!(h.me_status(&a).await, 401);
}

#[tokio::test]
async fn request_validation_of_the_login_body() {
    let h = Harness::new().await;
    let bodies = [
        json!({}),
        json!({ "login": "a" }),
        json!({ "login": "a", "password": "b", "admin": true }),
        json!({ "login": "", "password": "x" }),
        json!({ "login": "a", "password": "x".repeat(1025) }),
    ];
    for body in bodies {
        let r = h.post(LOGIN, body.clone()).await;
        assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_request")), "{body}");
    }
}

#[test]
fn public_base_url_of_email_links() {
    let mut c = Config::for_tests();
    c.server_public_host = "h.example".into();
    c.public_api_port = 443;
    assert_eq!(public_base_url(&c), "http://h.example:443");
    c.tls_mode = TlsMode::Native;
    assert_eq!(public_base_url(&c), "https://h.example");
    c.tls_mode = TlsMode::Proxy;
    c.server_public_host = "::1".into();
    c.public_api_port = 8443;
    assert_eq!(public_base_url(&c), "https://[::1]:8443");
}
