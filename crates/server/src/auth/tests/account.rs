//! The account endpoints: `GET /account/me`, preferences, deletion (auth.account.test.js,
//! auth.account-view.test.js).

use http::Method;
use serde_json::{Value, json};

use super::{Harness, PW, link_in, token_of};
use crate::security::totp::{base32_decode, totp};
use crate::store::{NewSanction, NewUser, RatingRecord, SanctionKind, Source, UserStatus};

const FORM: &str = "application/x-www-form-urlencoded";

fn sanction(user_id: u32, kind: SanctionKind, reason: &str, starts_at: i64, ends_at: i64) -> NewSanction {
    NewSanction {
        user_id,
        kind,
        reason: Some(reason.into()),
        source: Source::Moderator,
        game_id: None,
        starts_at,
        ends_at: Some(ends_at),
        created_by: None,
        created_at: starts_at,
    }
}

fn record(rating: i64, games: i64, wins: i64, draws: i64, losses: i64, peak: i64) -> RatingRecord {
    RatingRecord {
        rating,
        games,
        wins,
        draws,
        losses,
        peak,
        rated: true,
        counted_games: games,
        ..RatingRecord::initial(1500)
    }
}

#[tokio::test]
async fn me_shows_the_account_ratings_and_active_sanctions_never_the_integrity_level() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let now = h.now();
    let (blitz, rapid) = (record(1612, 42, 20, 5, 17, 1650), record(1500, 3, 1, 1, 1, 1510));
    h.store
        .write(move |db| {
            db.ratings().put(id, "3+2", &blitz, now)?;
            db.ratings().put(id, "10+0", &rapid, now)?;
            db.sanctions().create(&sanction(
                id,
                SanctionKind::MmBlock,
                "abandons",
                now - 1000,
                now + 60_000,
            ))?;
            db.sanctions().create(&sanction(id, SanctionKind::Warning, "old", now - 9000, now - 1000))
        })
        .await
        .unwrap();
    let token = h.token("alice", PW).await;
    let r = h.get_as(&token, "/api/v1/account/me").await;
    assert_eq!(r.status, 200);
    let me = r.json();
    assert_eq!(super::keys(&me), ["user", "ratings", "sanctions", "ban"]);
    assert_eq!(me["user"]["username"], "alice");
    assert_eq!(me["user"]["email"], "alice@example.com");
    // The store lists the categories in their order.
    assert_eq!(
        me["ratings"],
        json!([
            { "category": "10+0", "rating": 1500, "games": 3, "wins": 1, "draws": 1, "losses": 1, "peak": 1510, "provisional": true },
            { "category": "3+2", "rating": 1612, "games": 42, "wins": 20, "draws": 5, "losses": 17, "peak": 1650, "provisional": false },
        ])
    );
    assert_eq!(
        me["sanctions"],
        json!([{ "kind": "mm_block", "reason": "abandons", "startsAt": now - 1000, "endsAt": now + 60_000 }])
    );
    assert_eq!(me["ban"], Value::Null);
    let text = r.text();
    assert!(!text.contains("integrity") && !text.contains("passwordHash") && !text.contains("argon2"));
    assert_eq!(h.call(Method::GET, "/api/v1/account/me").send().await.status, 401);
}

#[tokio::test]
async fn me_keeps_every_field_and_adds_last_login_at_and_pending_email_as_the_login_answer_does() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let login = h.login("alice", PW).await;
    let token = login["token"].as_str().unwrap().to_owned();
    let login_at = h.now();
    assert_eq!(login["user"]["lastLoginAt"], json!(login_at));
    assert_eq!(login["user"]["pendingEmail"], Value::Null);
    h.advance(5000);
    let me = h.get_as(&token, "/api/v1/account/me").await.json();
    let mut fields = super::keys(&me["user"]);
    fields.sort();
    assert_eq!(
        fields,
        [
            "acceptChallenges",
            "createdAt",
            "email",
            "emailVerified",
            "googleLinked",
            "hasPassword",
            "id",
            "lastLoginAt",
            "mfaEnabled",
            "pendingEmail",
            "username"
        ]
    );
    assert_eq!(me["user"]["id"], json!(id));
    assert_eq!(me["user"]["lastLoginAt"], json!(login_at));
    assert_eq!(me["user"]["acceptChallenges"], "all");
    assert!(me["user"]["createdAt"].as_i64().unwrap() > 0);

    let r = h
        .post_as(&token, "/api/v1/account/email", json!({ "newEmail": "Nora@Example.org", "password": PW }))
        .await;
    assert_eq!(r.status, 202);
    let me = h.get_as(&token, "/api/v1/account/me").await.json();
    assert_eq!(me["user"]["pendingEmail"], "nora@example.org");
    assert_eq!(me["user"]["email"], "alice@example.com");
}

#[tokio::test]
async fn preferences_are_stored_as_the_store_boolean_and_shown_as_all_or_none() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let put = |value: &str| {
        h.call(Method::PUT, "/api/v1/account/preferences")
            .bearer(&token)
            .json(&json!({ "acceptChallenges": value }))
            .send()
    };
    let r = put("none").await;
    assert_eq!((r.status, r.json()), (200, json!({ "preferences": { "acceptChallenges": "none" } })));
    assert!(!h.user(id).await.accept_challenges);
    assert_eq!(h.get_as(&token, "/api/v1/account/me").await.json()["user"]["acceptChallenges"], "none");
    let r = put("all").await;
    assert_eq!((r.status, r.json()), (200, json!({ "preferences": { "acceptChallenges": "all" } })));
    assert!(h.user(id).await.accept_challenges);
    assert_eq!(h.get_as(&token, "/api/v1/account/me").await.json()["user"]["acceptChallenges"], "all");
    assert_eq!(put("friends").await.status, 400);
    let r = h.post_as(&token, "/api/v1/account/preferences", json!({ "acceptChallenges": "all" })).await;
    assert_eq!((r.status, r.header("allow")), (405, Some("PUT, OPTIONS")));
}

#[tokio::test]
async fn deletion_needs_the_password_and_second_factor_anonymises_and_revokes_the_sessions() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let other = h.token("alice", PW).await;
    let setup = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = base32_decode(setup.json()["secret"].as_str().unwrap()).unwrap();
    h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    h.advance(30_000);
    let delete = "/api/v1/account/delete";
    assert_eq!(
        h.post_as(&token, delete, json!({ "password": "nope nope nope" })).await.json()["error"],
        "invalid_password"
    );
    let r = h.post_as(&token, delete, json!({ "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("mfa_code_required")));
    let r = h.post_as(&token, delete, json!({ "password": PW, "code": totp(&secret, h.now()) })).await;
    assert_eq!((r.status, r.json()), (200, json!({ "status": "deleted" })));
    let row = h.user(id).await;
    assert_eq!(row.status, UserStatus::Deleted);
    assert_eq!((row.email, row.password_hash), (None, None));
    for t in [&token, &other] {
        assert_eq!(h.me_status(t).await, 401);
    }
    assert_eq!(h.post("/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await.status, 401);
    assert!(h.events().await.iter().any(|e| e.kind == "account_deleted" && e.user_id == Some(id)));
}

#[tokio::test]
async fn the_deletion_erases_the_ip_addresses_of_the_security_events_the_pending_ones_included() {
    let h = Harness::new().await;
    let alice = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    assert_eq!(h.post_as(&token, "/api/v1/account/delete", json!({ "password": PW })).await.status, 200);

    // With a recovery code: its recovery_code_used event comes from the deletion's own request.
    let bob = h.create_user("bob").await;
    let token = h.token("bob", PW).await;
    let setup = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = base32_decode(setup.json()["secret"].as_str().unwrap()).unwrap();
    let enable =
        h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    let code = enable.json()["recoveryCodes"][0].clone();
    let r =
        h.post_as(&token, "/api/v1/account/delete", json!({ "password": PW, "recoveryCode": code })).await;
    assert_eq!(r.status, 200);

    let events = h.events().await;
    for id in [alice, bob] {
        let mine: Vec<_> = events.iter().filter(|e| e.user_id == Some(id)).collect();
        assert!(mine.iter().any(|e| e.kind == "login") && mine.iter().any(|e| e.kind == "account_deleted"));
        let with_ip: Vec<&str> = mine.iter().filter(|e| e.ip.is_some()).map(|e| e.kind.as_str()).collect();
        assert!(with_ip.is_empty(), "user {id}: {with_ip:?}");
    }
    assert!(events.iter().any(|e| e.user_id == Some(bob) && e.kind == "recovery_code_used"));
}

#[tokio::test]
async fn an_account_without_a_password_is_told_to_set_one_first() {
    let h = Harness::new().await;
    let id = h.create_user_with("googler", Some("googler@example.com"), None, true).await;
    let session = h.auth.create_session(&h.user(id).await, None, None).await.unwrap();
    let r = h
        .post_as(&session.token, "/api/v1/account/delete", json!({ "password": "anything goes here" }))
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("password_not_set")));
    assert_eq!(h.get_as(&session.token, "/api/v1/account/me").await.json()["user"]["hasPassword"], false);
}

#[tokio::test]
async fn an_email_change_logs_in_with_the_new_address_and_refuses_an_address_taken_meanwhile() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let link_to = async |to: &str| {
        let mail = h.sent().await.into_iter().rfind(|m| m.to == to).expect("a mail");
        token_of(&link_in(&mail.text).unwrap()).unwrap()
    };
    let confirm =
        |t: String| h.call(Method::POST, "/confirm-email-change").body(FORM, format!("token={t}")).send();

    let r = h
        .post_as(&token, "/api/v1/account/email", json!({ "newEmail": "nora@example.org", "password": PW }))
        .await;
    assert_eq!(r.status, 202);
    let tk = link_to("nora@example.org").await;
    assert_eq!(h.call(Method::GET, &format!("/confirm-email-change?token={tk}")).send().await.status, 200);
    let r = confirm(tk).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.user(id).await.email.as_deref(), Some("nora@example.org"));
    let found = h.store.users().by_email("NORA@example.org".into()).await.unwrap().unwrap();
    assert_eq!(found.id, id, "the normalised address follows");
    let now = h.now();
    let live = h.store.read(move |db| db.tokens().live_for_user(id, "email_change", now)).await.unwrap();
    assert!(live.is_none());
    assert_eq!(
        h.post("/api/v1/auth/login", json!({ "login": "nora@example.org", "password": PW })).await.status,
        200
    );

    // Taken between the request and the confirmation.
    let r = h
        .post_as(&token, "/api/v1/account/email", json!({ "newEmail": "zoe@example.org", "password": PW }))
        .await;
    assert_eq!(r.status, 202);
    let tk = link_to("zoe@example.org").await;
    let zoe = NewUser {
        username: "zoe".into(),
        email: Some("Zoe@Example.org".into()),
        password_hash: Some("x".into()),
        email_verified: true,
        accept_challenges: true,
        created_at: h.now(),
    };
    h.store.users().create(zoe).await.unwrap();
    assert_eq!(confirm(tk).await.status, 409);
    assert_eq!(h.user(id).await.email.as_deref(), Some("nora@example.org"));
}
