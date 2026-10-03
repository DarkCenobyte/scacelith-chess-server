//! Port of store.accounts.test.js: users, MFA recovery codes, sessions, tokens, SSO identities,
//! and single-use consumption raced by several stores (writers) on one file.

use serde_json::json;

use super::support::*;
use crate::store::{
    ErrorKind, NewSecurityEvent, NewSession, NewToken, NewUser, Store, UserStatus, UserUpdate,
    normalize_email,
};

const DAY: i64 = 86_400_000;

#[tokio::test]
async fn users_create_lookups_uniqueness_normalization() {
    let store = memory_store().await;
    let id = store
        .users()
        .create(NewUser {
            username: "Magnus".into(),
            email: Some("  First.Last@GMail.com ".into()),
            password_hash: Some("scrypt$x".into()),
            email_verified: false,
            accept_challenges: true,
            created_at: 1_700_000_000_000,
        })
        .await
        .unwrap();
    let u = store.users().by_id(id).await.unwrap().unwrap();
    assert_eq!(u.username, "Magnus");
    assert_eq!(u.email.as_deref(), Some("First.Last@GMail.com"));
    assert!(!u.email_verified);
    assert_eq!(u.password_hash.as_deref(), Some("scrypt$x"));
    assert_eq!(u.status, UserStatus::Active);
    assert!(u.accept_challenges);
    assert!(!u.mfa_enabled);
    assert_eq!(u.mfa_secret_enc, None);
    assert_eq!(u.last_login_at, None);
    assert_eq!(u.created_at, 1_700_000_000_000);

    let users = store.users();
    assert_eq!(users.by_username("MAGNUS".into()).await.unwrap().unwrap().id, id);
    assert_eq!(users.by_email("first.last@gmail.com".into()).await.unwrap().unwrap().id, id);
    assert_eq!(users.by_email("firstlast@gmail.com".into()).await.unwrap(), None, "Gmail dots are kept");
    assert_eq!(users.by_login("magnus".into()).await.unwrap().unwrap().id, id);
    assert_eq!(users.by_login(" FIRST.LAST@gmail.COM".into()).await.unwrap().unwrap().id, id);
    assert_eq!(users.by_login("nobody".into()).await.unwrap(), None);
    assert_eq!(users.by_id(9999).await.unwrap(), None);
    assert_eq!(normalize_email(" A@B.c ").as_deref(), Some("a@b.c"));

    let e = users.create(new_user("magnus", Some("other@example.org"))).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::UsernameTaken);
    assert_eq!(e.code(), "username_taken");
    let e = users.create(new_user("Hikaru", Some("FIRST.LAST@gmail.com"))).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::EmailTaken);
    let e = users.create(new_user("", Some("empty@example.org"))).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);
    let e = users.create(new_user(&"x".repeat(65), Some("long@example.org"))).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);

    // SSO-only account: no password hash. Accounts without an address do not clash.
    let sso = users
        .create(NewUser {
            password_hash: None,
            email_verified: true,
            ..new_user("Sso", Some("sso@example.org"))
        })
        .await
        .unwrap();
    let u = users.by_id(sso).await.unwrap().unwrap();
    assert_eq!(u.password_hash, None);
    assert!(u.email_verified);
    users.create(new_user("NoMail1", None)).await.unwrap();
    users.create(new_user("NoMail2", Some("   "))).await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn users_update_mfa_fields_advance_mfa_step_anonymize() {
    let store = memory_store().await;
    let users = store.users();
    let id = users.create(new_user("Judit", Some("j@example.org"))).await.unwrap();
    let other = users.create(new_user("Other", Some("o@example.org"))).await.unwrap();
    let secret = "v1.c2VjcmV0LXNlYWxlZA".to_string();
    let changed = users
        .update(
            id,
            UserUpdate {
                email_verified: Some(true),
                password_hash: Some(Some("h2".into())),
                last_login_at: Some(Some(1234)),
                accept_challenges: Some(false),
                pending_mfa_secret_enc: Some(Some(secret.clone())),
                ..UserUpdate::default()
            },
        )
        .await
        .unwrap();
    assert!(changed);
    let u = users.by_id(id).await.unwrap().unwrap();
    assert!(u.email_verified);
    assert_eq!(u.password_hash.as_deref(), Some("h2"));
    assert_eq!(u.last_login_at, Some(1234));
    assert!(!u.accept_challenges);
    assert_eq!(u.pending_mfa_secret_enc.as_deref(), Some(secret.as_str()));
    users
        .update(
            id,
            UserUpdate {
                mfa_enabled: Some(true),
                mfa_secret_enc: Some(Some(secret.clone())),
                pending_mfa_secret_enc: Some(None),
                ..UserUpdate::default()
            },
        )
        .await
        .unwrap();
    let u = users.by_id(id).await.unwrap().unwrap();
    assert!(u.mfa_enabled);
    assert_eq!(u.mfa_secret_enc.as_deref(), Some(secret.as_str()));
    assert_eq!(u.pending_mfa_secret_enc, None);

    let clash = |f: UserUpdate| users.update(id, f);
    let e = clash(UserUpdate { email: Some(Some("O@example.org".into())), ..UserUpdate::default() })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::EmailTaken);
    let e = clash(UserUpdate { username: Some("other".into()), ..UserUpdate::default() }).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::UsernameTaken);
    let e = clash(UserUpdate { username: Some(String::new()), ..UserUpdate::default() }).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);
    users
        .update(id, UserUpdate { email: Some(Some("New@Example.org".into())), ..UserUpdate::default() })
        .await
        .unwrap();
    assert_eq!(users.by_email("new@example.org".into()).await.unwrap().unwrap().id, id);
    let none = UserUpdate { password_hash: Some(Some("x".into())), ..UserUpdate::default() };
    assert!(!users.update(424_242, none).await.unwrap());
    assert!(users.update(id, UserUpdate::default()).await.unwrap(), "empty update of an existing account");
    assert!(!users.update(424_242, UserUpdate::default()).await.unwrap());

    assert!(users.advance_mfa_step(id, 100).await.unwrap());
    assert!(!users.advance_mfa_step(id, 100).await.unwrap(), "replayed step refused");
    assert!(!users.advance_mfa_step(id, 99).await.unwrap());
    assert!(users.advance_mfa_step(id, 101).await.unwrap());
    assert_eq!(users.by_id(id).await.unwrap().unwrap().mfa_last_step, 101);

    // Anonymization erases personal data and revokes everything, keeps the id.
    let now = 1_800_000_000_000;
    let s1 = sha("s1");
    store
        .sessions()
        .create(NewSession {
            user_id: id,
            token_hash: s1.clone(),
            created_at: now,
            expires_at: now + 1_000_000,
            idle_expires_at: Some(now + 1_000_000),
            client_label: None,
            ip: Some("203.0.113.9".into()),
        })
        .await
        .unwrap();
    store
        .tokens()
        .create(NewToken {
            kind: "reset".into(),
            token_hash: sha("t1"),
            user_id: Some(id),
            data: None,
            created_at: now,
            expires_at: now + 1_000_000,
        })
        .await
        .unwrap();
    store.sso().link(id, "google".into(), "sub-1".into(), Some("j@gmail.com".into()), now).await.unwrap();
    store.mfa().replace_recovery_codes(id, vec!["a".into(), "b".into()], now).await.unwrap();
    store
        .security()
        .insert_batch(vec![NewSecurityEvent {
            kind: "login_failed".into(),
            user_id: Some(id),
            ip: Some("203.0.113.9".into()),
            at: Some(now),
            detail: None,
        }])
        .await
        .unwrap();
    let res = users.anonymize(id, now + 5).await.unwrap();
    assert_eq!(res.token_hashes, vec![s1.clone()]);
    let u = users.by_id(id).await.unwrap().unwrap();
    assert_eq!(u.status, UserStatus::Deleted);
    assert_eq!(u.username, format!("deleted#{id}"));
    assert_eq!(u.email, None);
    assert_eq!(u.password_hash, None);
    assert!(!u.mfa_enabled);
    assert_eq!(u.mfa_secret_enc, None);
    assert_eq!(u.deleted_at, Some(now + 5));
    assert_eq!(users.by_email("new@example.org".into()).await.unwrap(), None);
    assert_eq!(users.by_username("judit".into()).await.unwrap(), None);
    assert_eq!(store.sessions().by_token_hash(s1).await.unwrap(), None);
    assert_eq!(store.tokens().get("reset".into(), sha("t1")).await.unwrap(), None);
    assert_eq!(store.sso().find("google".into(), "sub-1".into()).await.unwrap(), None);
    assert_eq!(store.mfa().count_recovery_codes(id).await.unwrap(), 0);
    assert_eq!(store.security().for_user(id, 10).await.unwrap()[0].ip, None);
    // The name and e-mail are free again.
    assert!(users.create(new_user("Judit", Some("new@example.org"))).await.unwrap() > other);
    assert_eq!(users.anonymize(987_654, now).await.unwrap_err().kind(), ErrorKind::NotFound);
    store.close().await;
}

#[tokio::test]
async fn mfa_recovery_codes_replace_single_use_count() {
    let store = memory_store().await;
    let id = store.users().create(new_user("Mfa", Some("m@example.org"))).await.unwrap();
    let mfa = store.mfa();
    mfa.replace_recovery_codes(id, vec!["h1".into(), "h2".into(), "h3".into(), "h3".into()], 1)
        .await
        .unwrap();
    assert_eq!(mfa.count_recovery_codes(id).await.unwrap(), 3, "duplicates collapse");
    assert!(mfa.consume_recovery_code(id, "h2".into()).await.unwrap());
    assert!(!mfa.consume_recovery_code(id, "h2".into()).await.unwrap());
    assert!(!mfa.consume_recovery_code(id, "zz".into()).await.unwrap());
    assert_eq!(mfa.count_recovery_codes(id).await.unwrap(), 2);
    mfa.replace_recovery_codes(id, vec![sha("x"), sha("y")], 2).await.unwrap();
    assert_eq!(mfa.count_recovery_codes(id).await.unwrap(), 2);
    assert!(!mfa.consume_recovery_code(id, "h1".into()).await.unwrap(), "old codes are gone");
    assert!(mfa.consume_recovery_code(id, sha("x")).await.unwrap());
    let other = store.users().create(new_user("Other", Some("o@example.org"))).await.unwrap();
    assert!(!mfa.consume_recovery_code(other, sha("y")).await.unwrap(), "codes belong to their account");
    store.close().await;
}

#[tokio::test]
async fn sessions_create_lookup_touch_revoke_list_limit() {
    let store = memory_store().await;
    let uid = store.users().create(new_user("Sess", Some("s@example.org"))).await.unwrap();
    let other = store.users().create(new_user("Other", Some("o@example.org"))).await.unwrap();
    let sessions = store.sessions();
    let t0 = 1_800_000_000_000;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            sessions
                .create(NewSession {
                    user_id: uid,
                    token_hash: sha(&format!("tok{i}")),
                    created_at: t0 + i,
                    expires_at: t0 + 90 * DAY,
                    idle_expires_at: Some(t0 + 30 * DAY),
                    client_label: Some(format!("pc{i}")),
                    ip: Some("198.51.100.1".into()),
                })
                .await
                .unwrap(),
        );
    }
    let s = sessions.by_token_hash(sha("tok2")).await.unwrap().unwrap();
    assert_eq!(
        s,
        crate::store::SessionAuth {
            id: ids[2],
            user_id: uid,
            created_at: t0 + 2,
            last_seen_at: t0 + 2,
            expires_at: t0 + 90 * DAY,
            idle_expires_at: t0 + 30 * DAY,
            revoked_at: None,
        }
    );
    assert_eq!(sessions.by_token_hash(sha("nope")).await.unwrap(), None);
    let dup = NewSession { user_id: uid, token_hash: sha("tok0"), expires_at: t0, ..NewSession::default() };
    assert_eq!(sessions.create(dup).await.unwrap_err().kind(), ErrorKind::Duplicate);
    let orphan =
        NewSession { user_id: 4242, token_hash: sha("orphan"), expires_at: t0, ..NewSession::default() };
    assert_eq!(sessions.create(orphan).await.unwrap_err().kind(), ErrorKind::ForeignKey);
    let no_idle =
        NewSession { user_id: other, token_hash: sha("noidle"), expires_at: t0 + 7, ..NewSession::default() };
    sessions.create(no_idle).await.unwrap();
    assert_eq!(sessions.by_token_hash(sha("noidle")).await.unwrap().unwrap().idle_expires_at, t0 + 7);

    sessions.touch(ids[2], t0 + 1000, t0 + 1000 + DAY).await.unwrap();
    let s = sessions.by_token_hash(sha("tok2")).await.unwrap().unwrap();
    assert_eq!((s.last_seen_at, s.idle_expires_at), (t0 + 1000, t0 + 1000 + DAY));

    assert_eq!(sessions.revoke(ids[0], Some(other), t0 + 5).await.unwrap(), None, "not the owner");
    assert_eq!(sessions.revoke(ids[0], Some(uid), t0 + 5).await.unwrap(), Some(sha("tok0")));
    assert_eq!(sessions.revoke(ids[0], None, t0 + 6).await.unwrap(), None, "already revoked");
    assert_eq!(sessions.by_token_hash(sha("tok0")).await.unwrap().unwrap().revoked_at, Some(t0 + 5));
    let list = sessions.list_for_user(uid).await.unwrap();
    assert_eq!(list.len(), 4);
    assert_eq!(list[0].id, ids[2], "most recently seen first");
    assert_eq!(list[0].client_label.as_deref(), Some("pc2"));
    assert_eq!(list[0].ip.as_deref(), Some("198.51.100.1"));
    assert_eq!(sessions.all_for_user(uid).await.unwrap().len(), 5, "revoked sessions included");

    // Keep the 2 newest live sessions.
    let revoked = sessions.enforce_limit(uid, 2, t0 + 2000).await.unwrap();
    assert_eq!(revoked, vec![sha("tok2"), sha("tok1")]);
    let mut left: Vec<i64> = sessions.list_for_user(uid).await.unwrap().iter().map(|x| x.id).collect();
    left.sort_unstable();
    assert_eq!(left, vec![ids[3], ids[4]]);

    let all = sessions.revoke_all_for_user(uid, Some(ids[4]), t0 + 3000).await.unwrap();
    assert_eq!(all, vec![sha("tok3")]);
    let left: Vec<i64> = sessions.list_for_user(uid).await.unwrap().iter().map(|x| x.id).collect();
    assert_eq!(left, vec![ids[4]]);
    assert_eq!(sessions.revoke_all_for_user(uid, None, t0 + 3001).await.unwrap().len(), 1);
    assert!(sessions.list_for_user(uid).await.unwrap().is_empty());

    // Hashes are opaque text: stored and compared as given.
    let sid = sessions
        .create(NewSession {
            user_id: other,
            token_hash: "abc123hex".into(),
            expires_at: t0 + 1,
            ..NewSession::default()
        })
        .await
        .unwrap();
    assert_eq!(sessions.by_token_hash("abc123hex".into()).await.unwrap().unwrap().id, sid);
    assert_eq!(sessions.revoke(sid, None, t0).await.unwrap().as_deref(), Some("abc123hex"));
    store.close().await;
}

#[tokio::test]
async fn tokens_create_get_update_single_use_consume_expiry() {
    let store = memory_store().await;
    let uid = store.users().create(new_user("Tok", Some("t@example.org"))).await.unwrap();
    let tokens = store.tokens();
    let now = 1_800_000_000_000;
    let token = |kind: &str, data, user_id| NewToken {
        kind: kind.into(),
        token_hash: sha("v"),
        user_id,
        data,
        created_at: now,
        expires_at: now + 1000,
    };
    tokens.create(token("verify", Some(json!({"email": "t@example.org"})), Some(uid))).await.unwrap();
    tokens.create(token("sso", Some(json!({"status": "pending"})), None)).await.unwrap();
    assert_eq!(tokens.create(token("verify", None, None)).await.unwrap_err().kind(), ErrorKind::Duplicate);
    let sso = tokens.get("sso".into(), sha("v")).await.unwrap().unwrap();
    assert_eq!(sso.data, Some(json!({"status": "pending"})));
    assert_eq!(sso.user_id, None);
    assert!(
        tokens.update("sso".into(), sha("v"), Some(json!({"status": "done", "userId": 5}))).await.unwrap()
    );
    let sso = tokens.get("sso".into(), sha("v")).await.unwrap().unwrap();
    assert_eq!(sso.data, Some(json!({"status": "done", "userId": 5})));
    assert!(!tokens.update("sso".into(), sha("nope"), Some(json!({}))).await.unwrap());

    let row = tokens.consume("verify".into(), sha("v"), now + 10).await.unwrap().unwrap();
    assert_eq!(row.user_id, Some(uid));
    assert_eq!(row.kind, "verify");
    assert_eq!(row.data, Some(json!({"email": "t@example.org"})));
    assert_eq!(row.consumed_at, Some(now + 10));
    assert_eq!(tokens.consume("verify".into(), sha("v"), now + 11).await.unwrap(), None, "single use");
    assert_eq!(tokens.get("verify".into(), sha("v")).await.unwrap().unwrap().consumed_at, Some(now + 10));
    assert_eq!(tokens.consume("sso".into(), sha("v"), now + 1000).await.unwrap(), None, "expired");
    assert_eq!(tokens.consume("reset".into(), sha("v"), now).await.unwrap(), None, "kind matters");
    store.close().await;
}

#[tokio::test]
async fn sso_find_link_relink_conflict() {
    let store = memory_store().await;
    let a = store.users().create(new_user("A", Some("a@example.org"))).await.unwrap();
    let b = store.users().create(new_user("B", Some("b@example.org"))).await.unwrap();
    let sso = store.sso();
    assert_eq!(sso.find("google".into(), "123".into()).await.unwrap(), None);
    sso.link(a, "google".into(), "123".into(), Some("a@gmail.com".into()), 10).await.unwrap();
    let found = sso.find("google".into(), "123".into()).await.unwrap().unwrap();
    assert_eq!(found, crate::store::SsoLink { user_id: a, email: Some("a@gmail.com".into()) });
    sso.link(a, "google".into(), "123".into(), Some("a2@gmail.com".into()), 20).await.unwrap();
    assert_eq!(
        sso.find("google".into(), "123".into()).await.unwrap().unwrap().email.as_deref(),
        Some("a2@gmail.com")
    );
    assert_eq!(sso.for_user(a).await.unwrap()[0].created_at, 10, "a re-link keeps the creation time");
    let e = sso.link(b, "google".into(), "123".into(), Some("b@gmail.com".into()), 30).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::SsoTaken);
    assert_eq!(sso.find("google".into(), "123".into()).await.unwrap().unwrap().user_id, a);
    sso.link(b, "google".into(), "456".into(), None, 40).await.unwrap();
    let ids = sso.for_user(b).await.unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].subject, "456");
    assert_eq!(ids[0].email, None);
    store.close().await;
}

/// Several stores (each with its own writer connection) on one file race to consume the same
/// tokens: each token is consumed exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_consume_is_single_use_across_writers_racing_on_one_file() {
    let dir = TempDir::new("race");
    let store = file_store(&dir).await;
    let uid = store.users().create(new_user("Race", Some("r@example.org"))).await.unwrap();
    let now = 1_800_000_000_000;
    let hashes: Vec<String> = (0..300).map(|i| sha(&format!("race{i}"))).collect();
    let all = hashes.clone();
    store
        .write(move |db| {
            for h in &all {
                db.tokens().create(&NewToken {
                    kind: "reset".into(),
                    token_hash: h.clone(),
                    user_id: Some(uid),
                    data: None,
                    created_at: now,
                    expires_at: now + 3_600_000,
                })?;
            }
            Ok::<_, crate::store::StoreError>(())
        })
        .await
        .unwrap();

    let mut racers = Vec::new();
    for _ in 0..4 {
        racers.push(Store::open(&config(), options(Some(dir.file("scacelith.db")))).await.unwrap());
    }
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(racers.len()));
    let mut tasks = Vec::new();
    for racer in &racers {
        let (racer, hashes, barrier) = (racer.clone(), hashes.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut won = Vec::new();
            for h in hashes {
                if racer.tokens().consume("reset".into(), h.clone(), now + 1).await.unwrap().is_some() {
                    won.push(h);
                }
            }
            won
        }));
    }
    let mut won = Vec::new();
    for t in tasks {
        won.extend(t.await.unwrap());
    }
    assert_eq!(won.len(), hashes.len(), "every token consumed exactly once");
    won.sort();
    won.dedup();
    assert_eq!(won.len(), hashes.len());
    for h in &hashes {
        assert!(store.tokens().get("reset".into(), h.clone()).await.unwrap().unwrap().consumed_at.is_some());
    }
    for racer in racers {
        racer.close().await;
    }
    store.close().await;
}
