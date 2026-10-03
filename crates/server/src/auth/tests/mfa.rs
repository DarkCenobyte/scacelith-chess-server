//! Two-step verification: enrolment, the second login step, replay protection, recovery codes,
//! disabling, and the password changes that end a pending step (auth.mfa.test.js).

use serde_json::{Value, json};
use tokio::task::JoinSet;

use super::{Harness, NEW_PW, PW, Setup};
use crate::ids::UserId;
use crate::security::totp::{base32_decode, hotp, totp, totp_step};
use crate::store::{NewSanction, SanctionKind, Source, UserUpdate};

const MFA: &str = "/api/v1/auth/login/mfa";

/// A server with a logged-in user who enrolled TOTP.
struct Enrolled {
    h: Harness,
    id: UserId,
    token: String,
    secret: Vec<u8>,
    recovery_codes: Vec<String>,
}

async fn enrolled(setup: Setup) -> Enrolled {
    let h = Harness::build(setup).await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let r = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let secret = base32_decode(r.json()["secret"].as_str().unwrap()).unwrap();
    let r =
        h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let recovery_codes = r.json()["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    h.advance(30_000); // the next code is a new step
    Enrolled { h, id, token, secret, recovery_codes }
}

/// Logs in with the password: the `mfaToken` of the second step.
async fn mfa_step(h: &Harness) -> String {
    let r = h.post("/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await;
    assert_eq!(r.status, 200);
    let body = r.json();
    assert_eq!(body["mfaRequired"], true);
    let tok = body["mfaToken"].as_str().unwrap().to_owned();
    assert!(crate::auth::is_prefixed_token(&tok, "mfa_"), "{tok}");
    assert_eq!(body["expiresIn"], 300);
    assert_eq!(body.get("token"), None, "no session before the second factor");
    assert_eq!(super::keys(&body), ["mfaRequired", "mfaToken", "expiresIn"]);
    tok
}

fn code_at(secret: &[u8], step: i64) -> String {
    hotp(secret, u64::try_from(step).unwrap(), 6)
}

async fn live_sessions(h: &Harness, id: UserId) -> usize {
    h.store
        .read(move |db| db.sessions().list_for_user(id))
        .await
        .unwrap()
        .iter()
        .filter(|s| s.revoked_at.is_none())
        .count()
}

async fn recovery_count(h: &Harness, id: UserId) -> i64 {
    h.store.mfa().count_recovery_codes(id).await.unwrap()
}

#[tokio::test]
async fn setup_needs_the_password_and_gives_a_secret_and_an_otpauth_uri_pending_until_enabled() {
    let h = Harness::with_env(&[("SERVER_NAME", "Scacelith Club")]).await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let setup = "/api/v1/account/mfa/totp/setup";
    let r = h.post_as(&token, setup, json!({ "password": "wrong password" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")));
    assert_eq!(h.post(setup, json!({ "password": PW })).await.status, 401, "needs a session");
    let r = h.post_as(&token, setup, json!({ "password": PW })).await;
    assert_eq!(r.status, 200);
    let body = r.json();
    let secret = body["secret"].as_str().unwrap();
    assert!(
        secret.len() == 32 && secret.bytes().all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
    );
    assert_eq!(
        body["uri"],
        format!(
            "otpauth://totp/Scacelith%20Club:alice?secret={secret}&issuer=Scacelith%20Club&algorithm=SHA1&digits=6&period=30"
        )
    );
    assert_eq!(super::keys(&body), ["secret", "uri", "algorithm", "digits", "period"]);
    let row = h.user(id).await;
    assert!(!row.mfa_enabled);
    let pending = row.pending_mfa_secret_enc.unwrap();
    assert!(pending.starts_with("v1."), "{pending}");
    assert!(!pending.contains(secret), "encrypted at rest");
    assert_eq!(
        h.login("alice", PW).await.get("mfaRequired"),
        None,
        "a pending secret does not change the login"
    );
    let enable = "/api/v1/account/mfa/totp/enable";
    let r = h.post_as(&token, enable, json!({ "code": "000000" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_code")));
    assert_eq!(h.post_as(&token, enable, json!({ "code": "abcdef" })).await.status, 400);
}

#[tokio::test]
async fn enable_gives_10_hashed_recovery_codes_and_login_then_needs_the_second_factor() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    assert_eq!(e.recovery_codes.len(), 10);
    for c in &e.recovery_codes {
        let parts: Vec<&str> = c.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [4, 4, 2], "{c}");
        assert!(c.bytes().all(|b| b == b'-' || b.is_ascii_digit() || b.is_ascii_lowercase()), "{c}");
    }
    let id = e.id;
    let hashes: Vec<String> = h
        .store
        .read(move |db| {
            db.all("SELECT code_hash FROM mfa_recovery_codes WHERE user_id = ?1", [id], |r| r.get(0))
        })
        .await
        .unwrap();
    assert_eq!(hashes.len(), 10);
    assert!(hashes.iter().all(|x| x.len() == 64 && x.bytes().all(|b| b.is_ascii_hexdigit())));
    let row = h.user(e.id).await;
    assert!(row.mfa_enabled);
    assert_eq!(row.pending_mfa_secret_enc, None);
    let r = h.post_as(&e.token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    assert_eq!(r.json()["error"], "mfa_already_enabled");
    let tok = mfa_step(h).await;
    let r = h.post(MFA, json!({ "mfaToken": tok, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.status, 200);
    assert!(r.json()["token"].as_str().unwrap().starts_with("sct_"));
    assert_eq!(r.json()["user"]["mfaEnabled"], true);
    let again = h.post(MFA, json!({ "mfaToken": tok, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(
        (again.status, again.json()["error"].clone()),
        (401, json!("invalid_mfa_token")),
        "single use"
    );
}

#[tokio::test]
async fn totp_window_and_replay_protection() {
    let e = enrolled(Setup::default()).await;
    let (h, secret) = (&e.h, &e.secret);
    // The step before the current one was used by the enable call: a replay.
    let r = h
        .post(MFA, json!({ "mfaToken": mfa_step(h).await, "code": code_at(secret, totp_step(h.now()) - 1) }))
        .await;
    assert_eq!(r.status, 401);
    h.advance(30_000);
    let step = totp_step(h.now());
    // A code of the previous step is still accepted...
    let r = h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "code": code_at(secret, step - 1) })).await;
    assert_eq!(r.status, 200);
    // ...but not twice, nor any older step once a newer one was used.
    let r = h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "code": code_at(secret, step) })).await;
    assert_eq!(r.status, 200);
    let t2 = mfa_step(h).await;
    let r = h.post(MFA, json!({ "mfaToken": t2, "code": code_at(secret, step) })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_code")), "replay");
    let r = h.post(MFA, json!({ "mfaToken": t2, "code": code_at(secret, step - 1) })).await;
    assert_eq!(r.status, 401, "older step");
    let r = h.post(MFA, json!({ "mfaToken": t2, "code": code_at(secret, step + 2) })).await;
    assert_eq!(r.status, 401, "outside the window");
    let r = h.post(MFA, json!({ "mfaToken": t2, "code": code_at(secret, step + 1) })).await;
    assert_eq!(r.status, 200, "the next step, within the window");
}

#[tokio::test]
async fn recovery_codes_are_single_use_in_code_or_recovery_code_in_any_case_and_spacing() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let spaced = e.recovery_codes[0].to_uppercase().replace('-', " ");
    let r = h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "recoveryCode": spaced })).await;
    assert_eq!(r.status, 200);
    let r = h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "recoveryCode": e.recovery_codes[0] })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_code")));
    let r = h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "code": e.recovery_codes[1] })).await;
    assert_eq!(r.status, 200, "the client may send a recovery code as \"code\"");
    assert_eq!(recovery_count(h, e.id).await, 8);
    assert_eq!(h.event_kinds().await.iter().filter(|k| *k == "recovery_code_used").count(), 2);
}

#[tokio::test]
async fn the_mfa_token_ends_after_5_wrong_codes_or_5_minutes_with_a_per_account_delay() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let tok = mfa_step(h).await;
    for i in 0..5 {
        let r = h.post(MFA, json!({ "mfaToken": tok, "code": (100_000 + i).to_string() })).await;
        assert!([401, 429].contains(&r.status), "{}", r.text());
        if r.status == 429 {
            assert_eq!(r.json()["error"], "too_many_attempts");
            h.advance(r.json()["retryAfter"].as_i64().unwrap() * 1000);
        }
    }
    let r = h.post(MFA, json!({ "mfaToken": tok, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.json()["error"], "invalid_mfa_token", "burned after 5 wrong codes");
    h.advance(15 * 60_000);
    let tok2 = mfa_step(h).await;
    h.advance(5 * 60_000 + 1);
    let r = h.post(MFA, json!({ "mfaToken": tok2, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.json()["error"], "invalid_mfa_token", "expired");
    let r = h.post(MFA, json!({ "mfaToken": format!("mfa_{}", "x".repeat(43)), "code": "123456" })).await;
    assert_eq!(r.json()["error"], "invalid_mfa_token");
    let r = h.post(MFA, json!({ "mfaToken": mfa_step(h).await })).await;
    assert_eq!(r.status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_codes_sent_at_once_still_end_the_mfa_token_after_5() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let tok = mfa_step(h).await;
    let mut set = JoinSet::new();
    for i in 0..9 {
        let req = h
            .call(http::Method::POST, MFA)
            .json(&json!({ "mfaToken": tok, "code": (100_000 + i).to_string() }));
        set.spawn(req.send());
    }
    let mut errors = Vec::new();
    while let Some(r) = set.join_next().await {
        errors.push(r.unwrap().json()["error"].as_str().unwrap().to_owned());
    }
    // Exactly 5 codes are refused as wrong; the others find the step ended (or, once the 5
    // failures are counted, the account's delay running: the Node server's IPC round trip let
    // all 9 reach the code check first).
    let count = |code: &str| errors.iter().filter(|e| *e == code).count();
    assert_eq!(count("invalid_code"), 5, "{errors:?}");
    assert_eq!(count("invalid_mfa_token") + count("too_many_attempts"), 4, "{errors:?}");
    // Every code checked is recorded: the five refused as wrong, and those checked while the
    // fifth ended the step. A request that finds the step already ended checks no code.
    let checked = 9 - count("too_many_attempts");
    let failed = h.event_kinds().await.iter().filter(|k| *k == "mfa_failed").count();
    assert!((5..=checked).contains(&failed), "{failed} mfa_failed events for {errors:?}");
    let r = h.post(MFA, json!({ "mfaToken": tok, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.json()["error"], "invalid_mfa_token", "ended");
}

#[tokio::test]
async fn a_ban_decided_between_the_two_steps_is_enforced_at_the_second() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let tok = mfa_step(h).await;
    let ban = NewSanction {
        user_id: e.id,
        kind: SanctionKind::Ban,
        reason: None,
        source: Source::Moderator,
        game_id: None,
        starts_at: h.now() - 1,
        ends_at: None,
        created_by: None,
        created_at: h.now(),
    };
    h.store.write(move |db| db.sanctions().create(&ban)).await.unwrap();
    let r = h.post(MFA, json!({ "mfaToken": tok, "code": totp(&e.secret, h.now()) })).await;
    let body = r.json();
    assert_eq!((r.status, &body["error"], &body["until"]), (403, &json!("banned"), &Value::Null));
}

#[tokio::test]
async fn disable_needs_the_password_and_a_code_or_recovery_code_and_sends_a_notice() {
    let e = enrolled(Setup::default()).await;
    let (h, token) = (&e.h, e.token.as_str());
    let disable = "/api/v1/account/mfa/totp/disable";
    let r = h.post_as(token, disable, json!({ "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("mfa_code_required")));
    let r = h
        .post_as(token, disable, json!({ "password": "bad password", "code": totp(&e.secret, h.now()) }))
        .await;
    assert_eq!(r.json()["error"], "invalid_password");
    let r = h.post_as(token, disable, json!({ "password": PW, "code": "000000" })).await;
    assert_eq!(r.json()["error"], "invalid_code");
    let r = h.post_as(token, disable, json!({ "password": PW, "recoveryCode": e.recovery_codes[3] })).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "mfa_disabled" })));
    let row = h.user(e.id).await;
    assert!(!row.mfa_enabled);
    assert_eq!(row.mfa_secret_enc, None);
    assert_eq!(recovery_count(h, e.id).await, 0);
    assert!(h.sent().await.iter().any(|m| m.subject.contains("Two-step verification was turned off")));
    assert_eq!(h.login("alice", PW).await.get("mfaRequired"), None);
    let r = h.post_as(token, disable, json!({ "password": PW, "code": "123456" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (409, json!("mfa_not_enabled")));
}

#[tokio::test]
async fn recovery_codes_are_regenerated_with_the_password_and_a_totp_code_not_a_recovery_code() {
    let e = enrolled(Setup::default()).await;
    let (h, token) = (&e.h, e.token.as_str());
    let path = "/api/v1/account/mfa/recovery-codes";
    let r = h.post_as(token, path, json!({ "password": PW, "code": e.recovery_codes[0] })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_code")));
    let r = h.post_as(token, path, json!({ "password": PW, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.status, 200);
    let fresh: Vec<String> = r.json()["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(fresh.len(), 10);
    assert_ne!(fresh, e.recovery_codes);
    let old =
        h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "recoveryCode": e.recovery_codes[5] })).await;
    assert_eq!(old.status, 401, "the old codes no longer work");
    let new = h.post(MFA, json!({ "mfaToken": mfa_step(h).await, "recoveryCode": fresh[5] })).await;
    assert_eq!(new.status, 200);
}

#[tokio::test]
async fn reauthentication_failures_are_throttled_per_account() {
    let h = Harness::with_env(&[("AUTH_FAILURES_PER_ACCOUNT", "2")]).await;
    h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let setup = "/api/v1/account/mfa/totp/setup";
    for _ in 0..2 {
        assert_eq!(h.post_as(&token, setup, json!({ "password": "nope nope" })).await.status, 403);
    }
    let r = h.post_as(&token, setup, json!({ "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("too_many_attempts")));
    h.advance(r.json()["retryAfter"].as_i64().unwrap() * 1000);
    assert_eq!(h.post_as(&token, setup, json!({ "password": PW })).await.status, 200);
}

#[tokio::test]
async fn enable_without_setup() {
    let h = Harness::new().await;
    h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let r = h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": "123456" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (409, json!("mfa_setup_required")));
}

#[tokio::test]
async fn a_password_reset_between_the_two_steps_of_a_login_ends_the_mfa_step() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let step = mfa_step(h).await; // someone who has the old password
    let step2 = mfa_step(h).await;
    h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    let mail =
        h.sent().await.into_iter().rev().find(|m| m.subject.contains("Reset your")).expect("the reset mail");
    let reset = super::token_of(&super::link_in(&mail.text).unwrap()).unwrap();
    let r = h.post("/api/v1/auth/password/reset", json!({ "token": reset, "newPassword": NEW_PW })).await;
    assert_eq!(r.status, 200);
    assert_eq!(live_sessions(h, e.id).await, 0);
    let r = h.post(MFA, json!({ "mfaToken": step, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_mfa_token")), "{}", r.text());
    assert_eq!(live_sessions(h, e.id).await, 0, "no session opened with the old password");
    // Refused before the code is checked: no recovery code is spent on a dead step.
    let r = h.post(MFA, json!({ "mfaToken": step2, "recoveryCode": e.recovery_codes[0] })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_mfa_token")));
    assert_eq!(recovery_count(h, e.id).await, 10);
    let r = h.post(MFA, json!({ "mfaToken": step, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.json()["error"], "invalid_mfa_token", "the step is spent");
    // A login with the new password goes through both steps.
    let fresh = h.post("/api/v1/auth/login", json!({ "login": "alice", "password": NEW_PW })).await.json();
    assert_eq!(fresh["mfaRequired"], true);
    let r = h.post(MFA, json!({ "mfaToken": fresh["mfaToken"], "recoveryCode": e.recovery_codes[0] })).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn a_password_change_between_the_two_steps_of_a_login_ends_the_mfa_step() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let step = mfa_step(h).await;
    let body = json!({ "currentPassword": PW, "newPassword": NEW_PW });
    let r = h.post_as(&e.token, "/api/v1/account/password", body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let before = live_sessions(h, e.id).await;
    let r = h.post(MFA, json!({ "mfaToken": step, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_mfa_token")), "{}", r.text());
    assert_eq!(live_sessions(h, e.id).await, before, "no session opened with the old password");
}

#[tokio::test]
async fn a_password_changed_elsewhere_before_the_code_is_checked_still_wins() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let step = mfa_step(h).await;
    // Another process writes a new hash between the two steps.
    let other = h.hasher.hash("someone else's passphrase").unwrap();
    let update = UserUpdate { password_hash: Some(Some(other)), ..UserUpdate::default() };
    h.store.users().update(e.id, update).await.unwrap();
    let before = live_sessions(h, e.id).await;
    let r = h.post(MFA, json!({ "mfaToken": step, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_mfa_token")), "{}", r.text());
    assert_eq!(live_sessions(h, e.id).await, before, "no session opened");
}

#[tokio::test]
async fn the_mfa_step_keeps_a_digest_of_the_password_hash_and_a_step_without_it_still_completes() {
    let e = enrolled(Setup::default()).await;
    let h = &e.h;
    let step = mfa_step(h).await;
    let (id, now) = (e.id, h.now());
    let row = h.store.read(move |db| db.tokens().live_for_user(id, "mfa_login", now)).await.unwrap().unwrap();
    let mut data = crate::auth::tokens::data_of(&row);
    let pwh = data["pwh"].as_str().unwrap().to_owned();
    assert!(pwh.len() == 64 && pwh.bytes().all(|b| b.is_ascii_hexdigit()));
    let stored = h.user(e.id).await.password_hash.unwrap();
    assert!(!Value::Object(data.clone()).to_string().contains(&stored), "not the stored hash itself");
    // The row as an earlier build wrote it: {attempts, clientLabel, method}.
    data.remove("pwh");
    let (text, row_id) = (Value::Object(data).to_string(), row.id);
    h.store
        .write(move |db| {
            db.exec("UPDATE tokens SET data = ?1 WHERE id = ?2", rusqlite::params![text, row_id])
        })
        .await
        .unwrap();
    let r = h.post(MFA, json!({ "mfaToken": step, "code": totp(&e.secret, h.now()) })).await;
    assert_eq!(r.status, 200, "{}", r.text());
}
