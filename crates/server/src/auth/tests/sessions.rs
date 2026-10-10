//! Sessions: validation, listing, revocation and its broadcasts, expiry, the validation cache
//! (auth.sessions.test.js).

use http::Method;
use serde_json::{Value, json};

use super::{DAY_MS, Harness, Setup, new_pw, pw, sha256_hex};
use crate::auth::{SESSION_CACHE_TTL_MS, SESSION_TOUCH_EVERY_MS, SessionInfo};
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
    let token = h.token("alice", pw()).await;
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
    let a = h.login_with("alice", pw(), json!({ "clientLabel": "Laptop" })).await;
    h.advance(1000);
    let b = h.login_with("alice", pw(), json!({ "clientLabel": "Desktop" })).await;
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
    let c = h.token("bob", pw()).await;
    let bobs = h.get_as(&c, "/api/v1/auth/sessions").await.json()["sessions"][0].clone();
    let path = format!("/api/v1/auth/sessions/{}", bobs["id"]);
    assert_eq!(h.call(Method::DELETE, &path).bearer(a).send().await.status, 404);
    assert_eq!(h.me_status(&c).await, 200);
}

#[tokio::test]
async fn logout_revokes_the_session_everywhere() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", pw()).await;
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
    let a = h.login_with("alice", pw(), json!({ "clientLabel": "Laptop" })).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = h.login_with("alice", pw(), json!({ "clientLabel": "Desktop" })).await["token"]
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
    let token = h.token("alice", pw()).await;
    assert_eq!(h.post_as(&token, "/api/v1/auth/logout-all", json!({})).await.status, 200);
    assert_eq!(h.revoked.calls(), [(id, None)]);

    h.token("alice", pw()).await;
    h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    let reset = h.last_link_token().await;
    let r = h.post("/api/v1/auth/password/reset", json!({ "token": reset, "newPassword": new_pw() })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.revoked.calls()[1..], [(id, None)]);

    let token = h.token("alice", new_pw()).await;
    assert_eq!(
        h.post_as(&token, "/api/v1/account/delete", json!({ "password": new_pw() })).await.status,
        200
    );
    assert_eq!(h.revoked.calls()[2..], [(id, None)]);
}

#[tokio::test]
async fn logout_all() {
    let (h, _) = setup(&[]).await;
    let a = h.token("alice", pw()).await;
    let b = h.token("alice", pw()).await;
    assert_eq!(h.post_as(&a, "/api/v1/auth/logout-all", json!({})).await.status, 200);
    for t in [a, b] {
        assert_eq!(h.me_status(&t).await, 401);
    }
}

#[tokio::test]
async fn idle_expiry_sliding_with_use_and_the_absolute_maximum() {
    let (h, _) = setup(&[("SESSION_IDLE_DAYS", "2"), ("SESSION_MAX_DAYS", "5")]).await;
    let idle = h.token("alice", pw()).await;
    h.advance(2 * DAY_MS + 1);
    assert_eq!(h.me_status(&idle).await, 401, "unused for 2 days");

    let used = h.token("alice", pw()).await;
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
    let token = h.token("alice", pw()).await;
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
    let token = h.token("alice", pw()).await;
    assert!(h.auth.validate_token(&token).await.unwrap().is_some());
    revoke_in_store(&h, id).await;
    assert!(h.auth.validate_token(&token).await.unwrap().is_some(), "still cached");
    h.advance(SESSION_CACHE_TTL_MS);
    assert_eq!(h.auth.validate_token(&token).await.unwrap(), None, "reloaded after 30 s");

    let b = h.token("alice", pw()).await;
    assert!(h.auth.validate_token(&b).await.unwrap().is_some());
    revoke_in_store(&h, id).await;
    h.auth.invalidate(Some(id), &[sha256_hex(&b)]);
    assert_eq!(h.auth.validate_token(&b).await.unwrap(), None, "dropped by hash");

    let c = h.token("alice", pw()).await;
    assert!(h.auth.validate_token(&c).await.unwrap().is_some());
    revoke_in_store(&h, id).await;
    h.auth.invalidate(Some(id), &[]);
    assert_eq!(h.auth.validate_token(&c).await.unwrap(), None, "dropped for the whole user");
}

#[tokio::test]
async fn a_deleted_user_has_no_valid_session() {
    let (h, id) = setup(&[]).await;
    let token = h.token("alice", pw()).await;
    let update = UserUpdate { status: Some(UserStatus::Deleted), ..UserUpdate::default() };
    h.store.users().update(id, update).await.expect("updated");
    h.advance(SESSION_CACHE_TTL_MS);
    assert_eq!(h.auth.validate_token(&token).await.unwrap(), None);
}

// ---- renewals under pressure (audit A10) ------------------------------------------------------

/// A harness on a database file (reads on the reader connections, not behind the writer).
async fn on_file(
    dir: &crate::store::tests::support::TempDir,
    env: &[(&'static str, &str)],
) -> (Harness, UserId) {
    let mut setup = Setup::env(env);
    setup.db_path = Some(dir.file("auth.db"));
    let h = Harness::build(setup).await;
    let id = h.create_user("alice").await;
    (h, id)
}

/// Holds the database writer until the sender is dropped or sends (a stalled disk).
async fn stall_writer(h: &Harness) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (running_tx, running) = tokio::sync::oneshot::channel();
    let blocker = h.store.write(move |_| {
        let _ = running_tx.send(());
        let _ = held.recv();
        Ok::<_, crate::store::StoreError>(())
    });
    let task = tokio::spawn(async move {
        blocker.await.expect("the blocker ran");
    });
    running.await.expect("the writer runs the blocker");
    (release, task)
}

/// `n` validations of `token` at once: they all answer, within a second, whatever the writer.
async fn validate_many(h: &Harness, token: &str, n: usize) {
    let all = (0..n).map(|_| {
        let auth = h.auth.clone();
        let token = token.to_owned();
        tokio::spawn(async move { auth.validate_token(&token).await })
    });
    let all: Vec<_> = all.collect();
    for v in all {
        let v = tokio::time::timeout(std::time::Duration::from_secs(1), v)
            .await
            .expect("answered without the writer");
        assert!(v.expect("joined").expect("validated").is_some());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewal_due_is_queued_once_whatever_the_concurrent_validations_and_never_awaited() {
    use crate::auth::sessions::TouchOutcome as T;
    let dir = crate::store::tests::support::TempDir::new("touch");
    let (h, id) = on_file(&dir, &[]).await;
    let token = h.token("alice", pw()).await;
    h.auth.validate_token(&token).await.unwrap().expect("valid");
    h.auth.events().flush().await;
    let before = last_seen(&h, id).await;
    h.advance(SESSION_TOUCH_EVERY_MS);

    // The writer stalls: 50 validations at once, due for a renewal, all answer at once and queue
    // one renewal (the first one marks it in flight, and the cache renewed, before its write;
    // those that read the session before that find it in flight).
    let (release, writer) = stall_writer(&h).await;
    let backlog = h.store.write_backlog();
    validate_many(&h, &token, 50).await;
    let touches = h.auth.session_touches();
    assert_eq!((touches.get(T::Queued), touches.get(T::Deferred)), (1, 0));
    assert_eq!(h.store.write_backlog(), backlog + 1, "one renewal job");
    assert_eq!(h.auth.session_touches_in_flight(), 1);
    // The cache entry expires, or is dropped, while the renewal waits: the session is read again
    // (from before the renewal) and takes the renewal in flight, which is not queued again.
    h.advance(SESSION_CACHE_TTL_MS);
    validate_many(&h, &token, 20).await;
    h.auth.invalidate(None, &[sha256_hex(&token)]);
    validate_many(&h, &token, 20).await;
    assert_eq!((touches.get(T::Queued), touches.get(T::Deferred)), (1, 0));
    assert_eq!(h.store.write_backlog(), backlog + 1, "still one renewal job");
    assert_eq!(last_seen(&h, id).await, before, "not written while the writer stalls");

    // The writer is back: the renewal lands, once.
    release.send(()).unwrap();
    writer.await.unwrap();
    h.store.write(|_| Ok::<_, crate::store::StoreError>(())).await.unwrap(); // after the renewal
    assert_eq!(last_seen(&h, id).await, before + SESSION_TOUCH_EVERY_MS);
    for _ in 0..50 {
        if h.auth.session_touches_in_flight() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(h.auth.session_touches_in_flight(), 0);
    validate_many(&h, &token, 10).await;
    assert_eq!((touches.get(T::Queued), touches.get(T::Failed)), (1, 0), "renewed: nothing more is due");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewals_give_way_to_a_backlogged_writer_unless_the_session_is_about_to_expire() {
    use crate::auth::sessions::TouchOutcome as T;
    use crate::auth::{SESSION_TOUCH_DEFER_BACKLOG, SESSION_TOUCH_URGENT_MS};
    use crate::store::{StoreError, WRITE_QUEUE_MAX};
    let dir = crate::store::tests::support::TempDir::new("touch");
    let (h, id) = on_file(&dir, &[("SESSION_IDLE_DAYS", "1")]).await;
    let token = h.token("alice", pw()).await;
    h.auth.events().flush().await;
    let created = last_seen(&h, id).await;
    let touches = h.auth.session_touches();

    // A backlogged writer: the renewal due is deferred, the backlog does not grow.
    let (release, writer) = stall_writer(&h).await;
    let noop = |h: &Harness| tokio::spawn(h.store.write(|_| Ok::<_, StoreError>(())));
    let mut fillers: Vec<_> = (0..SESSION_TOUCH_DEFER_BACKLOG).map(|_| noop(&h)).collect();
    h.advance(SESSION_TOUCH_EVERY_MS);
    let backlog = h.store.write_backlog();
    for _ in 0..10 {
        validate_many(&h, &token, 5).await;
        h.advance(SESSION_CACHE_TTL_MS);
    }
    assert_eq!((touches.get(T::Queued), touches.get(T::Deferred)), (0, 50));
    assert_eq!(h.store.write_backlog(), backlog, "no renewal queued under pressure");

    // Its idle expiry less than an hour away, the session is renewed despite the backlog; with
    // the writer's queue full, the renewal fails, is logged, and the cache forgets it.
    let fill = WRITE_QUEUE_MAX - h.store.write_backlog();
    fillers.extend((0..fill).map(|_| noop(&h)));
    let idle_ms = 86_400_000;
    h.advance(idle_ms - SESSION_TOUCH_URGENT_MS / 2 - 10 * SESSION_CACHE_TTL_MS - SESSION_TOUCH_EVERY_MS);
    h.auth.validate_token(&token).await.unwrap().expect("valid: its idle expiry is half an hour away");
    assert_eq!(touches.get(T::Queued), 1, "urgent");
    for _ in 0..100 {
        if touches.get(T::Failed) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(touches.get(T::Failed), 1, "refused by the full queue");
    assert_eq!(h.store.write_backlog(), WRITE_QUEUE_MAX, "the queue did not grow");

    // The writer is back: the next validation renews the session (its mark was undone).
    release.send(()).unwrap();
    writer.await.unwrap();
    for f in fillers {
        f.await.unwrap().unwrap();
    }
    h.auth.validate_token(&token).await.unwrap().expect("valid");
    assert_eq!(touches.get(T::Queued), 2);
    h.store.write(|_| Ok::<_, StoreError>(())).await.unwrap(); // after the renewal
    let renewed = h.store.read(move |db| db.sessions().all_for_user(id)).await.unwrap().remove(0);
    assert_eq!(renewed.last_seen_at, h.now());
    assert!(renewed.last_seen_at > created);
    assert_eq!(renewed.idle_expires_at, h.now() + idle_ms, "the idle expiry slid");
}
