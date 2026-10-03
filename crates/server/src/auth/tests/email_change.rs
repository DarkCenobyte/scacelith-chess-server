//! E-mail change (`POST /account/email`, `GET`/`POST /confirm-email-change`): with and without
//! e-mail confirmation, enumeration resistance, the confirmation page, the notices and their
//! masked addresses, and what a change or a new password cancels (auth.email-change.test.js).

use std::sync::Arc;

use http::Method;
use serde_json::{Value, json};

use super::{CountingHasher, Harness, NEW_PW, PW, Setup, link_in, token_of};
use crate::http::testing::TestResponse;
use crate::ids::UserId;
use crate::mail::OutgoingMail;
use crate::net::tls::tests::TempDir;
use crate::security::totp::{base32_decode, totp};
use crate::store::{NewToken, UserUpdate};

const EMAIL: &str = "/api/v1/account/email";
const FORM: &str = "application/x-www-form-urlencoded";

async fn setup(env: &[(&'static str, &str)]) -> (Harness, UserId, String) {
    let h = Harness::with_env(env).await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    (h, id, token)
}

async fn mails_since(h: &Harness, from: usize) -> Vec<OutgoingMail> {
    h.sent().await.split_off(from)
}

async fn sent_count(h: &Harness) -> usize {
    h.sent().await.len()
}

fn link_token(mail: &OutgoingMail) -> String {
    let link = link_in(&mail.text).expect("a link");
    assert!(link.contains("/confirm-email-change?"), "{link}");
    token_of(&link).unwrap()
}

fn to<'a>(mails: &'a [OutgoingMail], address: &str) -> &'a OutgoingMail {
    mails.iter().find(|m| m.to == address).unwrap_or_else(|| panic!("a mail to {address}"))
}

async fn page(h: &Harness, token: &str) -> TestResponse {
    h.call(Method::GET, &format!("/confirm-email-change?token={token}")).send().await
}

async fn confirm(h: &Harness, token: &str) -> TestResponse {
    h.call(Method::POST, "/confirm-email-change").body(FORM, format!("token={token}")).send().await
}

async fn me(h: &Harness, token: &str) -> Value {
    h.get_as(token, "/api/v1/account/me").await.json()["user"].clone()
}

async fn change(h: &Harness, token: &str, body: Value) -> TestResponse {
    h.post_as(token, EMAIL, body).await
}

async fn events_of(h: &Harness, kind: &str) -> Vec<crate::store::SecurityEvent> {
    h.events().await.into_iter().filter(|e| e.kind == kind).collect()
}

async fn live_changes(h: &Harness, id: UserId) -> usize {
    let now = h.now();
    let live = h.store.read(move |db| db.tokens().live_for_user(id, "email_change", now)).await.unwrap();
    usize::from(live.is_some())
}

/// The token of the last reset mail.
async fn reset_link(h: &Harness, from: usize) -> String {
    let mails = mails_since(h, from).await;
    let mail = mails.iter().find(|m| m.subject.contains("Reset your")).expect("a reset mail");
    token_of(&link_in(&mail.text).unwrap()).unwrap()
}

#[tokio::test]
async fn with_confirmation_a_link_goes_to_the_new_address_a_notice_to_the_current_one_then_the_page_confirms()
{
    let (h, id, token) = setup(&[]).await;
    let other = h.token("alice", PW).await;
    let before = sent_count(&h).await;

    let r = change(&h, &token, json!({ "newEmail": " Nora@Example.ORG ", "password": PW })).await;
    assert_eq!((r.status, r.json()), (202, json!({ "status": "verification_sent" })));
    // Nothing changes before the link is used; the pending address shows.
    let view = me(&h, &token).await;
    assert_eq!(
        (view["email"].clone(), view["pendingEmail"].clone()),
        (json!("alice@example.com"), json!("nora@example.org"))
    );
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));

    let mails = mails_since(&h, before).await;
    let mut recipients: Vec<&str> = mails.iter().map(|m| m.to.as_str()).collect();
    recipients.sort();
    assert_eq!(recipients, ["alice@example.com", "nora@example.org"]);
    let link = to(&mails, "nora@example.org");
    assert!(link.subject.contains("Confirm your new e-mail address for"), "{}", link.subject);
    assert!(link.text.contains("Hello alice"));
    assert!(link_in(&link.text).unwrap().starts_with("http://chess.example.org:8443/"));
    let notice = to(&mails, "alice@example.com");
    assert!(notice.subject.contains("e-mail address was requested"), "{}", notice.subject);
    assert!(notice.text.contains("n***@example.org"), "the new address, masked");
    assert!(!notice.text.contains("nora@example.org"), "never in full");
    assert!(!notice.text.contains("confirm-email-change"), "no link in the notice");
    assert_eq!(events_of(&h, "email_change_requested").await.len(), 1);

    // GET shows the address and a button, and does not use the token (link scanners).
    let tk = link_token(link);
    for _ in 0..2 {
        let p = page(&h, &tk).await;
        assert_eq!(p.status, 200);
        assert!(p.header("content-type").unwrap().starts_with("text/html"));
        assert!(p.header("content-security-policy").unwrap().contains("default-src 'none'"));
        assert!(p.text().contains("nora@example.org") && p.text().contains("alice"));
        assert!(p.text().contains("<form method=\"post\" action=\"/confirm-email-change\">"));
    }
    assert_eq!(me(&h, &token).await["pendingEmail"], "nora@example.org");

    let sent_before = sent_count(&h).await;
    let done = confirm(&h, &tk).await;
    assert_eq!(done.status, 200);
    assert!(done.text().contains("E-mail address changed") && done.text().contains("nora@example.org"));
    let row = h.user(id).await;
    assert_eq!((row.email.as_deref(), row.email_verified), (Some("nora@example.org"), true));

    // Both sessions stay signed in and read the new address; no connection is closed.
    for t in [&token, &other] {
        let view = me(&h, t).await;
        assert_eq!(
            (view["email"].clone(), view["pendingEmail"].clone()),
            (json!("nora@example.org"), Value::Null)
        );
    }
    assert!(h.revoked.calls().is_empty(), "no session revoked");
    let sessions = h.store.read(move |db| db.sessions().all_for_user(id)).await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().all(|s| s.revoked_at.is_none()));

    // The former address is told, with the new one masked.
    let after = mails_since(&h, sent_before).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].to, "alice@example.com");
    assert!(after[0].subject.contains("e-mail address was changed"), "{}", after[0].subject);
    assert!(after[0].text.contains("n***@example.org") && !after[0].text.contains("nora@example.org"));
    assert_eq!(events_of(&h, "email_changed").await.len(), 1);

    // Single use; the new address logs in, the old one no longer does.
    let again = confirm(&h, &tk).await;
    assert_eq!(again.status, 400);
    assert!(again.text().contains("invalid, was already used, or has expired"));
    assert_eq!(page(&h, &tk).await.status, 400);
    assert_eq!(
        h.post("/api/v1/auth/login", json!({ "login": "nora@example.org", "password": PW })).await.status,
        200
    );
    assert_eq!(
        h.post("/api/v1/auth/login", json!({ "login": "alice@example.com", "password": PW })).await.status,
        401
    );
}

#[tokio::test]
async fn a_taken_address_gets_the_same_answer_and_its_owner_the_throttled_notice_never_a_link() {
    let (h, id, token) = setup(&[]).await;
    let bob = h.create_user_with("bob", Some("bob@example.org"), Some(PW), true).await;
    let free = change(&h, &token, json!({ "newEmail": "free@example.org", "password": PW })).await;
    let free_view = me(&h, &token).await;
    let before = sent_count(&h).await;
    let taken = change(&h, &token, json!({ "newEmail": "BOB@example.org", "password": PW })).await;
    assert_eq!((taken.status, taken.json()), (free.status, free.json()));
    assert_eq!(taken.json(), json!({ "status": "verification_sent" }));
    let view = me(&h, &token).await;
    assert_eq!(view["pendingEmail"], "bob@example.org", "pendingEmail as for a free address");
    assert_eq!(super::keys(&view), super::keys(&free_view));

    let mails = mails_since(&h, before).await;
    let mut recipients: Vec<&str> = mails.iter().map(|m| m.to.as_str()).collect();
    recipients.sort();
    assert_eq!(recipients, ["alice@example.com", "bob@example.org"]);
    let to_bob = to(&mails, "bob@example.org");
    assert!(to_bob.subject.contains("Someone tried to use your e-mail address"), "{}", to_bob.subject);
    assert!(to_bob.text.contains("Hello bob"));
    assert!(!to_bob.text.contains("alice"), "the requester is not named");
    assert_eq!(link_in(&to_bob.text), None, "no link to the owner of the address");
    let to_alice = to(&mails, "alice@example.com");
    assert!(to_alice.subject.contains("was requested"));
    assert!(to_alice.text.contains("b***@example.org"));
    assert_eq!(h.user(bob).await.email.as_deref(), Some("bob@example.org"));
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));

    // The requester's events are the same whatever the address; the owner's has no IP.
    let requested = events_of(&h, "email_change_requested").await;
    assert_eq!(requested.iter().filter(|e| e.user_id == Some(id)).count(), 2);
    let owner = events_of(&h, "email_change_existing_email").await;
    assert_eq!(owner.len(), 1);
    assert_eq!((owner[0].user_id, owner[0].ip.clone()), (Some(bob), None));

    // Within the hour, no second notice to the owner; the requester's notice still goes.
    let before = sent_count(&h).await;
    assert_eq!(
        change(&h, &token, json!({ "newEmail": "bob@example.org", "password": PW })).await.status,
        202
    );
    let mails = mails_since(&h, before).await;
    assert_eq!(mails.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["alice@example.com"]);
}

#[tokio::test]
async fn one_confirmation_mail_per_new_address_every_5_minutes_and_the_link_already_mailed_keeps_working() {
    let (h, id, token) = setup(&[]).await;
    h.create_user("bob").await;
    let bob = h.token("bob", PW).await;
    let from = sent_count(&h).await;
    for _ in 0..10 {
        let r = change(&h, &token, json!({ "newEmail": "victim@example.net", "password": PW })).await;
        assert_eq!((r.status, r.json()), (202, json!({ "status": "verification_sent" })));
        h.advance(1000);
    }
    // Another player asking for the same address within the 5 minutes: the same answer, no mail.
    let r = change(&h, &bob, json!({ "newEmail": "victim@example.net", "password": PW })).await;
    assert_eq!((r.status, r.json()), (202, json!({ "status": "verification_sent" })));
    assert_eq!(me(&h, &bob).await["pendingEmail"], "victim@example.net");
    let mails = mails_since(&h, from).await;
    let links: Vec<_> = mails.iter().filter(|m| m.to == "victim@example.net").collect();
    assert_eq!(links.len(), 1, "one confirmation mail for 11 requests");
    assert_eq!(
        mails.iter().filter(|m| m.to == "alice@example.com").count(),
        10,
        "the requester is told each time"
    );
    assert_eq!(me(&h, &token).await["pendingEmail"], "victim@example.net");
    let first = link_token(links[0]);
    assert_eq!(page(&h, &first).await.status, 200, "the link mailed first still works");

    // 5 minutes later, a request mails a new link, which replaces the first one.
    h.advance(5 * 60_000);
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "victim@example.net", "password": PW })).await;
    let mails = mails_since(&h, from).await;
    assert_eq!(mails.iter().filter(|m| m.to == "victim@example.net").count(), 1);
    let second = link_token(to(&mails, "victim@example.net"));
    assert_eq!(confirm(&h, &first).await.status, 400);
    assert_eq!(confirm(&h, &second).await.status, 200);
    assert_eq!(h.user(id).await.email.as_deref(), Some("victim@example.net"));
}

#[tokio::test]
async fn invalid_and_same_addresses_are_refused_before_the_password_is_checked() {
    let (h, _, token) = setup(&[]).await;
    let r =
        change(&h, &token, json!({ "newEmail": "not-an-address", "password": "wrong wrong wrong" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_email")));
    let r = change(&h, &token, json!({ "newEmail": " ALICE@example.com", "password": "wrong wrong wrong" }))
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("same_email")));
    assert!(events_of(&h, "reauth_failed").await.is_empty());
    let r =
        change(&h, &token, json!({ "newEmail": "nora@example.org", "password": "wrong wrong wrong" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")));
    assert_eq!(events_of(&h, "reauth_failed").await.len(), 1);
    let r = change(&h, &token, json!({ "newEmail": "nora@example.org" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_request")));
    assert_eq!(h.post(EMAIL, json!({ "newEmail": "nora@example.org", "password": PW })).await.status, 401);
    assert_eq!(me(&h, &token).await["pendingEmail"], Value::Null);
}

#[tokio::test]
async fn two_step_verification_needs_a_code_and_the_reauth_failures_count_with_the_deletion() {
    let (h, id, token) = setup(&[("AUTH_FAILURES_PER_ACCOUNT", "3")]).await;
    let s = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = base32_decode(s.json()["secret"].as_str().unwrap()).unwrap();
    let en =
        h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    h.advance(30_000);
    let r = change(&h, &token, json!({ "newEmail": "nora@example.org", "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("mfa_code_required")));
    let r = change(
        &h,
        &token,
        json!({ "newEmail": "nora@example.org", "password": PW, "code": totp(&secret, h.now()) }),
    )
    .await;
    assert_eq!(r.status, 202);
    let code = en.json()["recoveryCodes"][0].clone();
    let r =
        change(&h, &token, json!({ "newEmail": "nina@example.org", "password": PW, "recoveryCode": code }))
            .await;
    assert_eq!(r.status, 202);
    assert_eq!(h.store.mfa().count_recovery_codes(id).await.unwrap(), 9, "the recovery code is used up");
    assert_eq!(me(&h, &token).await["pendingEmail"], "nina@example.org");

    // Wrong passwords here and at deletion count together.
    for _ in 0..2 {
        let r =
            change(&h, &token, json!({ "newEmail": "x@example.org", "password": "nope nope nope" })).await;
        assert_eq!(r.json()["error"], "invalid_password");
    }
    let r = h.post_as(&token, "/api/v1/account/delete", json!({ "password": "nope nope nope" })).await;
    assert_eq!(r.json()["error"], "invalid_password");
    let r =
        change(&h, &token, json!({ "newEmail": "x@example.org", "password": PW, "code": "123456" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("too_many_attempts")));
}

#[tokio::test]
async fn a_new_request_replaces_the_pending_one_and_an_expired_link_does_nothing() {
    let (h, id, token) = setup(&[]).await;
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "first@example.org", "password": PW })).await;
    let first = link_token(to(&mails_since(&h, from).await, "first@example.org"));
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "second@example.org", "password": PW })).await;
    let second = link_token(to(&mails_since(&h, from).await, "second@example.org"));
    assert_eq!(me(&h, &token).await["pendingEmail"], "second@example.org");
    let rows: i64 = h
        .store
        .read(|db| {
            db.count("SELECT count(*) FROM tokens WHERE kind = 'email_change' AND consumed_at IS NULL", [])
        })
        .await
        .unwrap();
    assert_eq!(rows, 1, "one pending change per account");
    assert_eq!(page(&h, &first).await.status, 400);
    assert_eq!(confirm(&h, &first).await.status, 400);
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));

    h.advance(24 * 3_600_000 + 1);
    assert_eq!(me(&h, &token).await["pendingEmail"], Value::Null, "expired after 24 h");
    assert_eq!(page(&h, &second).await.status, 400);
    assert_eq!(confirm(&h, &second).await.status, 400);
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));
    assert_eq!(page(&h, &"A".repeat(43)).await.status, 400);
    assert_eq!(confirm(&h, "garbage").await.status, 400);
}

#[tokio::test]
async fn an_address_taken_between_the_request_and_the_confirmation_is_refused_and_nothing_changes() {
    let (h, id, token) = setup(&[]).await;
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "nora@example.org", "password": PW })).await;
    let tk = link_token(to(&mails_since(&h, from).await, "nora@example.org"));
    h.create_user_with("nora", Some("nora@example.org"), Some(PW), true).await;
    assert_eq!(page(&h, &tk).await.status, 200, "the page itself does not tell");
    let before = sent_count(&h).await;
    let r = confirm(&h, &tk).await;
    assert_eq!(r.status, 409);
    assert!(r.text().contains("Another account now uses this e-mail address"));
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));
    assert!(mails_since(&h, before).await.is_empty());
    assert_eq!(events_of(&h, "email_change_refused").await.len(), 1);
    assert_eq!(confirm(&h, &tk).await.status, 400, "the link is used up");
}

#[tokio::test]
async fn the_unique_index_decides_when_the_address_is_taken_at_the_last_moment() {
    let (h, id, token) = setup(&[]).await;
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "nora@example.org", "password": PW })).await;
    let tk = link_token(to(&mails_since(&h, from).await, "nora@example.org"));
    // The lookup of the confirmation does not see the other account: it appears only when the
    // address is written (as if another process had won the race), and the index refuses it.
    h.store
        .write(|db| {
            db.exec(
                "CREATE TEMP TRIGGER last_moment BEFORE UPDATE OF email_normalized ON main.users
                 WHEN NEW.email_normalized = 'nora@example.org'
                 BEGIN
                     INSERT INTO main.users (username, username_lower, email, email_normalized, created_at)
                     VALUES ('nora', 'nora', 'nora@example.org', 'nora@example.org', 0);
                 END",
                [],
            )
        })
        .await
        .unwrap();
    let r = confirm(&h, &tk).await;
    assert_eq!(r.status, 409);
    assert!(r.text().contains("Another account now uses this e-mail address"));
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));
    assert_eq!(events_of(&h, "email_change_refused").await.len(), 1);
    h.store.write(|db| db.exec("DROP TRIGGER temp.last_moment", [])).await.unwrap();
    assert_eq!(confirm(&h, &tk).await.status, 400, "the link is used up");
}

#[tokio::test]
async fn a_new_password_cancels_a_pending_change_and_a_change_ends_the_links_of_the_former_address() {
    let (h, id, token) = setup(&[]).await;
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "nora@example.org", "password": PW })).await;
    let tk = link_token(to(&mails_since(&h, from).await, "nora@example.org"));
    let r = h
        .post_as(&token, "/api/v1/account/password", json!({ "currentPassword": PW, "newPassword": NEW_PW }))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(me(&h, &token).await["pendingEmail"], Value::Null);
    assert_eq!(confirm(&h, &tk).await.status, 400);
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));

    // A reset link sent to the former address dies with the change.
    let from = sent_count(&h).await;
    h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    let reset = reset_link(&h, from).await;
    h.advance(5 * 60_000); // one confirmation mail per address every 5 minutes
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "nora@example.org", "password": NEW_PW })).await;
    let tk2 = link_token(to(&mails_since(&h, from).await, "nora@example.org"));
    assert_eq!(confirm(&h, &tk2).await.status, 200);
    let rr = h
        .post(
            "/api/v1/auth/password/reset",
            json!({ "token": reset, "newPassword": "attacker chooses this one" }),
        )
        .await;
    assert_eq!((rr.status, rr.json()["error"].clone()), (400, json!("invalid_token")));

    // A password reset cancels a pending change too.
    change(&h, &token, json!({ "newEmail": "zoe@example.org", "password": NEW_PW })).await;
    let from = sent_count(&h).await;
    h.post("/api/v1/auth/password/forgot", json!({ "email": "nora@example.org" })).await;
    let reset2 = reset_link(&h, from).await;
    let r = h
        .post(
            "/api/v1/auth/password/reset",
            json!({ "token": reset2, "newPassword": "yet another passphrase" }),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(live_changes(&h, id).await, 0, "the pending change is gone");
}

/// A server whose hasher can land a change right after a request checked the password.
async fn hooked(env: &[(&'static str, &str)]) -> (Harness, Arc<CountingHasher>, UserId, String) {
    let counter = CountingHasher::new();
    let h = Harness::build(Setup { hasher: Some(counter.clone()), ..Setup::env(env) }).await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    (h, counter, id, token)
}

/// Stores `password`'s hash for `id` right after the next password check.
fn store_after_next_check(h: &Harness, counter: &CountingHasher, id: UserId, password: &str) {
    let (store, rt) = (h.store.clone(), tokio::runtime::Handle::current());
    let hash = h.hasher.hash(password).unwrap();
    counter.after_next_verify(move || {
        let update = UserUpdate { password_hash: Some(Some(hash)), ..UserUpdate::default() };
        rt.block_on(store.users().update(id, update)).unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_password_stored_right_after_the_check_stops_the_change_and_a_rehash_does_not() {
    for (password, refused) in [(NEW_PW, true), (PW, false)] {
        let (h, counter, id, token) = hooked(&[]).await;
        store_after_next_check(&h, &counter, id, password);
        let before = sent_count(&h).await;
        let r = change(&h, &token, json!({ "newEmail": "evil@attacker.example", "password": PW })).await;
        let mails = mails_since(&h, before).await;
        if !refused {
            // A sign-in's rehash of the same password is not a new password.
            assert_eq!((r.status, r.json()), (202, json!({ "status": "verification_sent" })));
            assert_eq!(live_changes(&h, id).await, 1);
            continue;
        }
        assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")));
        assert_eq!(live_changes(&h, id).await, 0, "no pending change");
        assert!(mails.iter().all(|m| m.to != "evil@attacker.example"), "no link");
        assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_confirmation_a_new_password_stored_after_the_check_stops_the_change_too() {
    let (h, counter, id, token) = hooked(&[("REQUIRE_EMAIL_VERIFICATION", "0")]).await;
    store_after_next_check(&h, &counter, id, NEW_PW);
    let r = change(&h, &token, json!({ "newEmail": "evil@attacker.example", "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")));
    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));
}

#[tokio::test]
async fn a_reset_link_stored_for_the_former_address_after_the_change_does_not_work() {
    let (h, id, token) = setup(&[]).await;
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "nora@example.org", "password": PW })).await;
    let tk = link_token(to(&mails_since(&h, from).await, "nora@example.org"));
    assert_eq!(confirm(&h, &tk).await.status, 200);
    // A "forgot password" for the former address that read the account before the change stores
    // its link only now (another process's race).
    let reset = "R".repeat(43);
    let link = NewToken {
        kind: "password_reset".into(),
        token_hash: super::sha256_hex(&reset),
        user_id: Some(id),
        data: Some(json!({ "email": "alice@example.com" })),
        created_at: h.now(),
        expires_at: h.now() + 3_600_000,
    };
    h.store.tokens().create(link).await.unwrap();
    // Whoever reads the former mailbox cannot use it: not on the page, not through the API.
    assert_eq!(h.call(Method::GET, &format!("/reset-password?token={reset}")).send().await.status, 400);
    let rr = h
        .post(
            "/api/v1/auth/password/reset",
            json!({ "token": reset, "newPassword": "attacker chooses this one" }),
        )
        .await;
    assert_eq!((rr.status, rr.json()["error"].clone()), (400, json!("invalid_token")));
    assert_eq!(
        h.post("/api/v1/auth/login", json!({ "login": "nora@example.org", "password": PW })).await.status,
        200
    );
}

#[tokio::test]
async fn a_busy_store_answers_503_server_busy_and_changes_nothing_and_the_links_still_work_afterwards() {
    let dir = TempDir::new("auth-busy");
    let file = dir.0.join("scacelith.db").display().to_string();
    let h = Harness::build(Setup { db_path: Some(file.clone()), ..Setup::default() }).await;
    let id = h.create_user("alice").await;
    let token = h.token("alice", PW).await;
    let from = sent_count(&h).await;
    change(&h, &token, json!({ "newEmail": "nora@example.org", "password": PW })).await;
    let tk = link_token(to(&mails_since(&h, from).await, "nora@example.org"));
    let from = sent_count(&h).await;
    h.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    let reset = reset_link(&h, from).await;

    // Another process holds the write lock past the busy timeout (shortened for the test).
    h.store.write(|db| db.count("PRAGMA busy_timeout = 20", [])).await.unwrap();
    let other = rusqlite::Connection::open(&file).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let r = confirm(&h, &tk).await;
    assert_eq!((r.status, r.header("retry-after")), (503, Some("1")));
    assert!(r.text().contains("busy"), "{}", r.text());
    let again = change(&h, &token, json!({ "newEmail": "zoe@example.org", "password": PW })).await;
    let body = again.json();
    assert_eq!((again.status, &body["error"], &body["retryAfter"]), (503, &json!("server_busy"), &json!(1)));
    let rr = h.post("/api/v1/auth/password/reset", json!({ "token": reset, "newPassword": NEW_PW })).await;
    assert_eq!((rr.status, rr.json()["error"].clone()), (503, json!("server_busy")));
    other.execute_batch("ROLLBACK").unwrap();

    assert_eq!(h.user(id).await.email.as_deref(), Some("alice@example.com"));
    assert_eq!(me(&h, &token).await["pendingEmail"], "nora@example.org");
    assert_eq!(
        h.post("/api/v1/auth/login", json!({ "login": "alice", "password": PW })).await.status,
        200,
        "password unchanged"
    );
    assert_eq!(confirm(&h, &tk).await.status, 200);
    assert_eq!(h.user(id).await.email.as_deref(), Some("nora@example.org"));
}

#[tokio::test]
async fn without_confirmation_the_address_changes_at_once_409_email_taken_and_the_former_address_is_told() {
    let (h, id, token) = setup(&[("REQUIRE_EMAIL_VERIFICATION", "0")]).await;
    let bob = h.create_user_with("bob", Some("bob@example.org"), Some(PW), true).await;
    let from = sent_count(&h).await;
    let r = change(&h, &token, json!({ "newEmail": "Bob@Example.org", "password": PW })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (409, json!("email_taken")));
    let mails = mails_since(&h, from).await;
    assert_eq!(
        mails.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(),
        ["bob@example.org"],
        "the owner still gets the notice"
    );
    assert!(mails[0].subject.contains("Someone tried to use your e-mail address"));

    let from = sent_count(&h).await;
    let r = change(&h, &token, json!({ "newEmail": "Nora@Example.org", "password": PW })).await;
    assert_eq!(
        (r.status, r.json()),
        (200, json!({ "status": "email_changed", "email": "nora@example.org" }))
    );
    let view = me(&h, &token).await;
    assert_eq!(
        (view["email"].clone(), view["emailVerified"].clone(), view["pendingEmail"].clone()),
        (json!("nora@example.org"), json!(true), Value::Null)
    );
    let mails = mails_since(&h, from).await;
    assert_eq!(mails.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["alice@example.com"]);
    assert!(mails[0].subject.contains("e-mail address was changed"));
    assert!(mails[0].text.contains("n***@example.org") && !mails[0].text.contains("nora@example.org"));
    let all = h.sent().await;
    assert!(
        !all.iter().any(|m| link_in(&m.text).is_some_and(|l| l.contains("confirm-email-change"))),
        "no confirmation link"
    );
    assert_eq!(h.user(bob).await.email.as_deref(), Some("bob@example.org"));
    assert_eq!(events_of(&h, "email_changed").await.iter().filter(|e| e.user_id == Some(id)).count(), 1);
}

#[tokio::test]
async fn an_account_without_a_password_is_told_to_set_one_first() {
    let h = Harness::new().await;
    let id = h.create_user_with("googler", Some("googler@example.com"), None, true).await;
    let session = h.auth.create_session(&h.user(id).await, None, None).await.unwrap();
    let r =
        change(&h, &session.token, json!({ "newEmail": "other@example.org", "password": "anything at all" }))
            .await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("password_not_set")));
}
