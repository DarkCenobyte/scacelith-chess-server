//! Changes of credentials as one transaction: a failure injected at each step of a password reset
//! or change changes nothing (and publishes nothing), the same link used twice at once works once,
//! two password changes at once end as one after the other, and a password stored while an
//! account change was re-authenticated wins over that change.

use std::sync::LazyLock;

use serde_json::{Value, json};

use super::{Harness, new_pw, pw};
use crate::auth::SESSION_CACHE_TTL_MS;
use crate::ids::UserId;
use crate::security::testing::random_password;
use crate::security::totp::{base32_decode, totp};
use crate::store::{StoreError, UserStatus};

const RESET: &str = "/api/v1/auth/password/reset";
const CHANGE: &str = "/api/v1/account/password";
/// A third valid password, drawn at random once per test run.
fn third_pw() -> &'static str {
    static THIRD_PW: LazyLock<String> = LazyLock::new(random_password);
    &THIRD_PW
}

/// Every write step of a password reset, each made to fail in turn: the trigger's name and event.
const RESET_STEPS: [(&str, &str); 4] = [
    ("fail_link_use", "UPDATE OF consumed_at ON tokens"),
    ("fail_password", "UPDATE OF password_hash ON users"),
    ("fail_link_end", "DELETE ON tokens"),
    ("fail_revocation", "UPDATE OF revoked_at ON sessions"),
];

/// A new reset link of alice.
async fn reset_link(h: &Harness) -> String {
    h.advance(5 * 60_000 + 1); // one link mail per address every 5 minutes
    let r = h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    assert_eq!(r.status, 202);
    h.last_link_token().await
}

/// A pending change of alice's address to new@example.com.
async fn request_email_change(h: &Harness, token: &str) {
    let r = h
        .post_as(token, "/api/v1/account/email", json!({ "newEmail": "new@example.com", "password": pw() }))
        .await;
    assert_eq!(r.status, 202, "{}", r.text());
}

/// The pending address shown to a session (and the session's status).
async fn pending_email(h: &Harness, token: &str) -> (u16, Value) {
    let r = h.get_as(token, "/api/v1/account/me").await;
    let pending = if r.status == 200 { r.json()["user"]["pendingEmail"].clone() } else { Value::Null };
    (r.status, pending)
}

async fn login_status(h: &Harness, password: &str) -> u16 {
    h.post("/api/v1/auth/login", json!({ "login": "alice", "password": password })).await.status
}

/// Whether a mail told alice of a new password.
async fn told_of_new_password(h: &Harness) -> bool {
    h.sent().await.iter().any(|m| m.subject.contains("password was changed"))
}

#[tokio::test]
async fn a_reset_that_fails_at_any_step_changes_nothing_and_its_link_still_works() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let old = h.token("alice", pw()).await;
    request_email_change(&h, &old).await;
    let token = reset_link(&h).await;
    let hash = h.user(id).await.password_hash;
    for (name, event) in RESET_STEPS {
        h.break_writes(name, event).await;
        let r = h.post(RESET, json!({ "token": token, "newPassword": new_pw() })).await;
        assert_eq!(r.status, 500, "{name}: {}", r.text());
        h.mend(name).await;
        assert_eq!(h.user(id).await.password_hash, hash, "{name}: the password stays");
        assert_eq!(h.unrevoked_sessions(id).await, 1, "{name}: the session stays");
        assert!(h.revoked.calls().is_empty(), "{name}: no connection closed");
        h.advance(SESSION_CACHE_TTL_MS + 1); // the session is read from the store again
        assert_eq!(pending_email(&h, &old).await, (200, json!("new@example.com")), "{name}");
        assert!(h.auth.peek_reset_token(&token).await.unwrap(), "{name}: the link is not used");
        assert!(!told_of_new_password(&h).await, "{name}: no mail");
    }
    assert!(!h.event_kinds().await.contains(&"password_reset".to_owned()));

    // The same link, once the store works.
    let r = h.post(RESET, json!({ "token": token, "newPassword": new_pw() })).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "password_reset" })));
    assert_eq!(h.revoked.calls(), [(id, None)], "the connection is closed once, after the commit");
    assert_eq!(h.unrevoked_sessions(id).await, 0);
    assert_eq!(h.me_status(&old).await, 401);
    assert_eq!(login_status(&h, pw()).await, 401);
    assert_eq!(login_status(&h, new_pw()).await, 200);
    assert!(told_of_new_password(&h).await);
    let r = h.post(RESET, json!({ "token": token, "newPassword": third_pw() })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_token")));
}

#[tokio::test]
async fn a_password_change_that_fails_at_any_step_changes_nothing() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let (current, other) = (h.token("alice", pw()).await, h.token("alice", pw()).await);
    request_email_change(&h, &current).await;
    let link = reset_link(&h).await;
    let hash = h.user(id).await.password_hash;
    let body = json!({ "currentPassword": pw(), "newPassword": new_pw() });
    for (name, event) in &RESET_STEPS[1..] {
        h.break_writes(name, event).await;
        let r = h.post_as(&current, CHANGE, body.clone()).await;
        assert_eq!(r.status, 500, "{name}: {}", r.text());
        h.mend(name).await;
        assert_eq!(h.user(id).await.password_hash, hash, "{name}: the password stays");
        assert_eq!(h.unrevoked_sessions(id).await, 2, "{name}: both sessions stay");
        assert!(h.revoked.calls().is_empty(), "{name}: no connection closed");
        h.advance(SESSION_CACHE_TTL_MS + 1);
        assert_eq!(pending_email(&h, &other).await, (200, json!("new@example.com")), "{name}");
        assert!(h.auth.peek_reset_token(&link).await.unwrap(), "{name}: the reset link stays");
        assert!(!told_of_new_password(&h).await, "{name}: no mail");
    }

    let r = h.post_as(&current, CHANGE, body).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "password_changed" })));
    let other_hash = super::sha256_hex(&other);
    assert_eq!(h.revoked.calls(), [(id, Some(vec![other_hash]))], "after the commit");
    assert_eq!(h.unrevoked_sessions(id).await, 1);
    assert_eq!(pending_email(&h, &current).await, (200, Value::Null), "this session stays, the change goes");
    assert_eq!(h.me_status(&other).await, 401);
    assert!(!h.auth.peek_reset_token(&link).await.unwrap());
    assert_eq!(login_status(&h, new_pw()).await, 200);
}

#[tokio::test]
async fn one_reset_link_sent_twice_at_once_works_once() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let old = h.token("alice", pw()).await;
    let token = reset_link(&h).await;
    let (a, b) = tokio::join!(
        h.post(RESET, json!({ "token": token, "newPassword": new_pw() })),
        h.post(RESET, json!({ "token": token, "newPassword": third_pw() })),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 400], "{} / {}", a.text(), b.text());
    let (kept, lost) = if a.status == 200 { (new_pw(), third_pw()) } else { (third_pw(), new_pw()) };
    assert_eq!(h.revoked.calls(), [(id, None)]);
    assert_eq!(h.me_status(&old).await, 401);
    assert_eq!(login_status(&h, lost).await, 401);
    assert_eq!(login_status(&h, kept).await, 200);
}

#[tokio::test]
async fn two_password_changes_at_once_end_as_one_after_the_other() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let (a, b) = (h.token("alice", pw()).await, h.token("alice", pw()).await);
    let body = |new: &str| json!({ "currentPassword": pw(), "newPassword": new });
    let (ra, rb) =
        tokio::join!(h.post_as(&a, CHANGE, body(new_pw())), h.post_as(&b, CHANGE, body(third_pw())));
    let ((winner, kept), (loser, lost), refused) = if ra.status == 200 {
        ((&a, new_pw()), (&b, third_pw()), &rb)
    } else {
        ((&b, third_pw()), (&a, new_pw()), &ra)
    };
    // The second, written over a password that is no longer the stored one, is refused (or its
    // session was revoked before it was checked).
    assert!(
        [(403, "invalid_password"), (401, "invalid_token")]
            .contains(&(refused.status, refused.json()["error"].as_str().unwrap_or(""))),
        "{} / {}",
        ra.text(),
        rb.text()
    );
    assert_eq!(h.unrevoked_sessions(id).await, 1);
    assert_eq!(h.me_status(winner).await, 200);
    assert_eq!(h.me_status(loser).await, 401);
    assert_eq!(login_status(&h, lost).await, 401);
    assert_eq!(login_status(&h, kept).await, 200);
}

/// Alice, signed in, with two-step verification on: the server, the account, the session and
/// the TOTP secret.
async fn with_mfa() -> (Harness, UserId, String, Vec<u8>) {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", pw()).await;
    let r = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": pw() })).await;
    let secret = base32_decode(r.json()["secret"].as_str().unwrap()).unwrap();
    let r =
        h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    h.advance(30_000); // the next code is a new step
    (h, id, token, secret)
}

/// Stores the hash of [`new_pw`] for alice in the transaction that uses her next authenticator
/// code: a reset that lands after the password of a request was checked, before its change.
async fn reset_with_the_next_code(h: &Harness, id: UserId) {
    let hash = h.hasher.hash(new_pw()).unwrap();
    let event = format!(
        "UPDATE OF mfa_last_step ON users WHEN NEW.id = {id} BEGIN UPDATE users SET password_hash = '{hash}' \
         WHERE id = {id}; END"
    );
    let sql = format!("CREATE TRIGGER reset_meanwhile AFTER {event}");
    h.store.write(move |db| db.connection().execute_batch(&sql).map_err(StoreError::from)).await.unwrap();
}

/// The stored hashes of the account's recovery codes, sorted.
async fn recovery_hashes(h: &Harness, id: UserId) -> Vec<String> {
    h.store
        .read(move |db| {
            let mut stmt = db
                .connection()
                .prepare("SELECT code_hash FROM mfa_recovery_codes WHERE user_id = ?1 ORDER BY code_hash")?;
            let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok::<_, StoreError>(rows)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_reset_that_lands_during_a_reauthenticated_change_wins_over_it() {
    // The deletion of the account.
    let (h, id, token, secret) = with_mfa().await;
    reset_with_the_next_code(&h, id).await;
    let r = h
        .post_as(
            &token,
            "/api/v1/account/delete",
            json!({ "password": pw(), "code": totp(&secret, h.now()) }),
        )
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")), "{}", r.text());
    let u = h.user(id).await;
    assert_eq!((u.status, u.username.as_str()), (UserStatus::Active, "alice"));
    assert_eq!(h.unrevoked_sessions(id).await, 1, "the reset of the test revoked nothing");

    // The end of two-step verification.
    let (h, id, token, secret) = with_mfa().await;
    reset_with_the_next_code(&h, id).await;
    let r = h
        .post_as(
            &token,
            "/api/v1/account/mfa/totp/disable",
            json!({ "password": pw(), "code": totp(&secret, h.now()) }),
        )
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")), "{}", r.text());
    assert!(h.user(id).await.mfa_enabled);

    // New recovery codes.
    let (h, id, token, secret) = with_mfa().await;
    let codes = recovery_hashes(&h, id).await;
    assert_eq!(codes.len(), 10);
    reset_with_the_next_code(&h, id).await;
    let r = h
        .post_as(
            &token,
            "/api/v1/account/mfa/recovery-codes",
            json!({ "password": pw(), "code": totp(&secret, h.now()) }),
        )
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")), "{}", r.text());
    assert_eq!(recovery_hashes(&h, id).await, codes, "the old codes stay");
}
