//! Sessions: validation, listing, revocation and its broadcasts, expiry, the validation cache
//! (auth.sessions.test.js).

use http::Method;
use serde_json::{Value, json};

use super::{DAY_MS, Harness, NEW_PW, PW, sha256_hex};
use crate::auth::{SESSION_CACHE_TTL_MS, SessionInfo};
use crate::ids::UserId;
use crate::store::{UserStatus, UserUpdate};

async fn setup(env: &[(&'static str, &str)]) -> (Harness, UserId) {
    let h = Harness::with_env(env).await;
    let id = h.create_user("alice").await;
    (h, id)
}

/// Revokes every session of `user` in the store only (another process; the broadcast is lost).
async fn revoke_in_store(h: &Harness, user: UserId) {
    let now = h.now();
    h.store.write(move |db| db.sessions().revoke_all_for_user(user, None, now)).await.expect("revoked");
}

async fn last_seen(h: &Harness, user: UserId) -> i64 {
    let rows = h.store.read(move |db| db.sessions().all_for_user(user)).await.expect("a read");
    rows[0].last_seen_at
}

fn other_session(list: &Value) -> Value {
    list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == json!(false))
        .cloned()
        .expect("another")
}

#[tokio::test]
async fn validate_token_shape_format_check_unknown_tokens() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", PW).await;
    let v = h.auth.validate_token(&token).await.unwrap().expect("valid");
    let mut hash = [0u8; 32];
    hex::decode_to_slice(sha256_hex(&token), &mut hash).unwrap();
    assert_eq!(
        v,
        SessionInfo {
            user_id: id,
            username: "alice".into(),
            session_id: 1,
            email_verified: true,
            token_hash: hash
        }
    );
    let bad = [
        "",
        "sct_short",
        &format!("{token}x"),
        &token.replace("sct_", "sxt_"),
        &format!("sct_{}", "!".repeat(43)),
    ];
    for b in bad {
        assert_eq!(h.auth.validate_token(b).await.unwrap(), None, "{b}");
    }
    assert_eq!(h.auth.validate_token(&format!("sct_{}", "A".repeat(43))).await.unwrap(), None);
}

#[tokio::test]
async fn sessions_list_and_revocation_of_another_session() {
    let (h, _) = setup(&[]).await;
    let a = h.login_with("alice", PW, json!({ "clientLabel": "Laptop" })).await;
    h.advance(1000);
    let b = h.login_with("alice", PW, json!({ "clientLabel": "Desktop" })).await;
    let (a, b) = (a["token"].as_str().unwrap(), b["token"].as_str().unwrap());
    let r = h.get_as(a, "/api/v1/auth/sessions").await;
    assert_eq!(r.status, 200);
    let list = r.json();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);
    let mine =
        list["sessions"].as_array().unwrap().iter().find(|s| s["current"] == json!(true)).unwrap().clone();
    assert_eq!(mine["clientLabel"], "Laptop");
    let other = other_session(&list);
    let mut fields = super::keys(&other);
    fields.sort();
    assert_eq!(fields, ["clientLabel", "createdAt", "current", "expiresAt", "id", "lastSeenAt"]);
    let path = format!("/api/v1/auth/sessions/{}", other["id"]);
    assert_eq!(h.call(Method::DELETE, &path).bearer(a).send().await.status, 200);
    assert_eq!(h.me_status(b).await, 401);
    assert_eq!(h.call(Method::DELETE, &path).bearer(a).send().await.status, 404);
    assert_eq!(h.call(Method::DELETE, "/api/v1/auth/sessions/9999").bearer(a).send().await.status, 404);
    // Another user's session cannot be revoked.
    h.create_user("bob").await;
    let c = h.token("bob", PW).await;
    let bobs = h.get_as(&c, "/api/v1/auth/sessions").await.json()["sessions"][0].clone();
    let path = format!("/api/v1/auth/sessions/{}", bobs["id"]);
    assert_eq!(h.call(Method::DELETE, &path).bearer(a).send().await.status, 404);
    assert_eq!(h.me_status(&c).await, 200);
}

#[tokio::test]
async fn logout_revokes_the_session_everywhere() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", PW).await;
    assert_eq!(h.me_status(&token).await, 200);
    let r = h.post_as(&token, "/api/v1/auth/logout", json!({})).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "logged_out" })));
    assert_eq!(h.me_status(&token).await, 401, "the local cache was dropped at once");
    assert_eq!(h.revoked.calls(), [(id, Some(vec![sha256_hex(&token)]))]);
    assert_eq!(h.post_as(&token, "/api/v1/auth/logout", json!({})).await.status, 401);
}

#[tokio::test]
async fn revoking_another_session_broadcasts_its_token_hash() {
    let (h, id) = setup(&[]).await;
    let a = h.login_with("alice", PW, json!({ "clientLabel": "Laptop" })).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = h.login_with("alice", PW, json!({ "clientLabel": "Desktop" })).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = other_session(&h.get_as(&a, "/api/v1/auth/sessions").await.json());
    let path = format!("/api/v1/auth/sessions/{}", other["id"]);
    assert_eq!(h.call(Method::DELETE, &path).bearer(&a).send().await.status, 200);
    assert_eq!(h.revoked.calls(), [(id, Some(vec![sha256_hex(&b)]))]);
    assert_eq!(h.me_status(&b).await, 401);
    assert_eq!(h.me_status(&a).await, 200);
    assert_eq!(h.post_as(&a, "/api/v1/auth/logout-all", json!({})).await.status, 200);
    assert_eq!(h.revoked.calls()[1..], [(id, None)]);
}

#[tokio::test]
async fn logout_all_a_password_reset_and_the_deletion_revoke_every_session_without_a_list() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", PW).await;
    assert_eq!(h.post_as(&token, "/api/v1/auth/logout-all", json!({})).await.status, 200);
    assert_eq!(h.revoked.calls(), [(id, None)]);

    h.token("alice", PW).await;
    h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    let reset = h.last_link_token().await;
    let r = h.post("/api/v1/auth/password/reset", json!({ "token": reset, "newPassword": NEW_PW })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.revoked.calls()[1..], [(id, None)]);

    let token = h.token("alice", NEW_PW).await;
    assert_eq!(h.post_as(&token, "/api/v1/account/delete", json!({ "password": NEW_PW })).await.status, 200);
    assert_eq!(h.revoked.calls()[2..], [(id, None)]);
}

#[tokio::test]
async fn logout_all() {
    let (h, _) = setup(&[]).await;
    let a = h.token("alice", PW).await;
    let b = h.token("alice", PW).await;
    assert_eq!(h.post_as(&a, "/api/v1/auth/logout-all", json!({})).await.status, 200);
    for t in [a, b] {
        assert_eq!(h.me_status(&t).await, 401);
    }
}

#[tokio::test]
async fn idle_expiry_sliding_with_use_and_the_absolute_maximum() {
    let (h, _) = setup(&[("SESSION_IDLE_DAYS", "2"), ("SESSION_MAX_DAYS", "5")]).await;
    let idle = h.token("alice", PW).await;
    h.advance(2 * DAY_MS + 1);
    assert_eq!(h.me_status(&idle).await, 401, "unused for 2 days");

    let used = h.token("alice", PW).await;
    for d in 0..4 {
        h.advance(DAY_MS + 3_600_000);
        assert_eq!(h.me_status(&used).await, 200, "day {}", d + 1);
    }
    h.advance(DAY_MS);
    assert_eq!(h.me_status(&used).await, 401, "older than SESSION_MAX_DAYS");
}

#[tokio::test]
async fn last_seen_is_written_at_most_every_5_minutes() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", PW).await;
    let before = last_seen(&h, id).await;
    for _ in 0..10 {
        h.auth.validate_token(&token).await.unwrap().expect("valid");
        h.advance(20_000);
    }
    assert_eq!(last_seen(&h, id).await, before, "200 s of use: no write");
    h.advance(120_000);
    h.auth.validate_token(&token).await.unwrap().expect("valid");
    let touched = last_seen(&h, id).await;
    assert_eq!(touched, h.now());
    h.advance(1000);
    h.auth.validate_token(&token).await.unwrap().expect("valid");
    assert_eq!(last_seen(&h, id).await, touched);
}

#[tokio::test]
async fn the_cache_sees_a_revocation_made_elsewhere_within_30_s_and_invalidate_drops_it_at_once() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", PW).await;
    assert!(h.auth.validate_token(&token).await.unwrap().is_some());
    revoke_in_store(&h, id).await;
    assert!(h.auth.validate_token(&token).await.unwrap().is_some(), "still cached");
    h.advance(SESSION_CACHE_TTL_MS);
    assert_eq!(h.auth.validate_token(&token).await.unwrap(), None, "reloaded after 30 s");

    let b = h.token("alice", PW).await;
    assert!(h.auth.validate_token(&b).await.unwrap().is_some());
    revoke_in_store(&h, id).await;
    h.auth.invalidate(Some(id), &[sha256_hex(&b)]);
    assert_eq!(h.auth.validate_token(&b).await.unwrap(), None, "dropped by hash");

    let c = h.token("alice", PW).await;
    assert!(h.auth.validate_token(&c).await.unwrap().is_some());
    revoke_in_store(&h, id).await;
    h.auth.invalidate(Some(id), &[]);
    assert_eq!(h.auth.validate_token(&c).await.unwrap(), None, "dropped for the whole user");
}

#[tokio::test]
async fn a_deleted_user_has_no_valid_session() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", PW).await;
    let update = UserUpdate { status: Some(UserStatus::Deleted), ..UserUpdate::default() };
    h.store.users().update(id, update).await.expect("updated");
    h.advance(SESSION_CACHE_TTL_MS);
    assert_eq!(h.auth.validate_token(&token).await.unwrap(), None);
}
