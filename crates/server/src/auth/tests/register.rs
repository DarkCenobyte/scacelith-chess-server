//! Registration: pending signups, the confirmation link and its pages, username and password
//! rules, existing addresses, resends, proof of work and limits (auth.register.test.js).

use http::Method;
use serde_json::{Value, json};

use super::{CountingHasher, Harness, Setup, link_in, token_of};
use crate::http::testing::TestResponse;
use crate::mail::OutgoingMail;
use crate::security::pow::{POW_TTL_MS, solve_pow};
use crate::store::{NewUser, RetentionPolicy};

const REG: &str = "/api/v1/auth/register";
const GOOD_PW: &str = "ivory rook takes e5";

fn good(over: Value) -> Value {
    let mut body = json!({ "username": "Alice_1", "email": "Alice@Example.com", "password": GOOD_PW });
    if let (Some(b), Some(o)) = (body.as_object_mut(), over.as_object()) {
        b.extend(o.clone());
    }
    body
}

fn mail_token(mail: &OutgoingMail) -> String {
    token_of(&link_in(&mail.text).expect("a link")).expect("a token")
}

async fn confirm_post(h: &Harness, token: &str) -> TestResponse {
    h.call(Method::POST, "/verify-email")
        .body("application/x-www-form-urlencoded", format!("token={token}"))
        .send()
        .await
}

async fn confirm_page(h: &Harness, token: &str) -> TestResponse {
    h.call(Method::GET, &format!("/verify-email?token={token}")).send().await
}

/// Follows the link of a confirmation mail: the GET page, then the POST.
async fn verify_from_mail(h: &Harness, mail: &OutgoingMail) -> String {
    let link = link_in(&mail.text).expect("a link");
    assert!(
        link.starts_with("http://chess.example.org:8443/verify-email?"),
        "TLS_MODE=off in tests: http; {link}"
    );
    let token = token_of(&link).unwrap();
    let page = confirm_page(h, &token).await;
    assert_eq!(page.status, 200);
    assert!(page.text().contains("<form method=\"post\" action=\"/verify-email\">"));
    assert!(page.header("content-security-policy").unwrap().contains("default-src 'none'"));
    let done = confirm_post(h, &token).await;
    assert_eq!(done.status, 200);
    assert!(done.text().contains("confirmed"));
    token
}

async fn user_by_name(h: &Harness, name: &str) -> Option<crate::store::User> {
    h.store.users().by_username(name.into()).await.unwrap()
}

async fn user_by_email(h: &Harness, email: &str) -> Option<crate::store::User> {
    h.store.users().by_email(email.into()).await.unwrap()
}

async fn signup_by_name(h: &Harness, name: &str) -> Option<crate::store::Signup> {
    h.store.signups().by_username(name.into()).await.unwrap()
}

async fn signup_by_email(h: &Harness, email: &str) -> Option<crate::store::Signup> {
    h.store.signups().by_email(email.into()).await.unwrap()
}

/// What a client sees of an answer: status, body and headers.
fn seen(r: &TestResponse) -> (u16, String, Vec<(String, String)>) {
    let headers =
        r.headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned())).collect();
    (r.status, r.text().to_owned(), headers)
}

#[tokio::test]
async fn register_answers_202_and_mails_a_link_and_the_account_exists_only_once_it_is_used() {
    let h = Harness::new().await;
    let r = h.post(REG, good(json!({}))).await;
    assert_eq!((r.status, r.json()), (202, json!({ "status": "verification_sent" })));
    assert!(user_by_name(&h, "alice_1").await.is_none(), "no account before the link is used");
    let p = signup_by_name(&h, "alice_1").await.unwrap();
    assert_eq!(p.email, "alice@example.com", "stored in lower case");
    assert!(p.password_hash.starts_with("$argon2id$"));
    assert_eq!(p.expires_at, h.now() + 24 * 3_600_000, "the life of the link");
    let sent = h.sent().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "alice@example.com");
    assert!(sent[0].subject.contains("Confirm your e-mail address"));

    // Before confirmation there is no account: the right password is answered as an unknown one.
    for password in [GOOD_PW, "wrong password!"] {
        let l = h.post("/api/v1/auth/login", json!({ "login": "alice_1", "password": password })).await;
        assert_eq!((l.status, l.json()["error"].clone()), (401, json!("invalid_credentials")), "{password}");
    }

    let token = verify_from_mail(&h, &sent[0]).await;
    let u = user_by_name(&h, "Alice_1").await.unwrap();
    assert_eq!(
        (u.username.as_str(), u.email.as_deref(), u.email_verified, u.password_hash.as_deref()),
        ("Alice_1", Some("alice@example.com"), true, Some(p.password_hash.as_str()))
    );
    assert!(signup_by_name(&h, "alice_1").await.is_none(), "the pending signup is gone");
    assert_eq!(confirm_post(&h, &token).await.status, 400, "single use");
    assert_eq!(confirm_page(&h, &token).await.status, 400);
    let ok = h.login("ALICE@example.com", GOOD_PW).await;
    assert!(ok["token"].as_str().unwrap().starts_with("sct_"));
    assert_eq!(ok["user"]["username"], "Alice_1");
    let registered: Vec<_> =
        h.events().await.into_iter().filter(|e| e.kind == "register").map(|e| e.user_id).collect();
    assert_eq!(registered, [Some(u.id)]);
}

#[tokio::test]
async fn the_get_confirmation_page_does_not_use_the_token() {
    let h = Harness::new().await;
    h.post(REG, good(json!({}))).await;
    let token = mail_token(&h.last_mail().await);
    for _ in 0..3 {
        assert_eq!(confirm_page(&h, &token).await.status, 200);
    }
    assert!(user_by_name(&h, "Alice_1").await.is_none());
    h.advance(24 * 3_600_000 + 1);
    let expired = confirm_page(&h, &token).await;
    assert_eq!(expired.status, 400);
    assert!(
        expired
            .text()
            .contains("create your account again from Scacelith (the same username and address work)")
    );
    assert_eq!(confirm_post(&h, &token).await.status, 400, "expired after 24 h");
    assert!(user_by_name(&h, "Alice_1").await.is_none());
}

#[tokio::test]
async fn username_rules() {
    let h = Harness::new().await;
    h.create_user("bob").await;
    let cases = [
        ("ab", "invalid_username"),
        (&*"a".repeat(21), "invalid_username"),
        ("_alice", "invalid_username"),
        ("-x-y", "invalid_username"),
        ("al ice", "invalid_username"),
        ("alicé", "invalid_username"),
        ("admin", "invalid_username"),
        ("Admin_Joe", "invalid_username"),
        ("Stockfish", "invalid_username"),
        ("system", "invalid_username"),
        ("moderator2", "invalid_username"),
        ("SCACELITH", "invalid_username"),
        ("BOB", "username_taken"),
    ];
    for (i, (username, code)) in cases.into_iter().enumerate() {
        let r = h
            .post(REG, good(json!({ "username": username, "email": format!("probe{i}@example.com") })))
            .await;
        assert_eq!(r.json()["error"], code, "{username}");
        assert_eq!(r.status, if code == "username_taken" { 409 } else { 400 }, "{username}");
    }
    for username in ["abc", "9lives", "a_b-c", &"x".repeat(20)] {
        let r = h
            .post(REG, good(json!({ "username": username, "email": format!("{username}@example.com") })))
            .await;
        assert_eq!(r.status, 202, "{username}");
    }
}

#[tokio::test]
async fn email_and_password_checks() {
    let h = Harness::new().await;
    let long = format!("{}@e.com", "x".repeat(250));
    for email in ["nope", "a@b", "a@@example.com", "a b@example.com", &long, "a@example..com"] {
        let r = h.post(REG, good(json!({ "email": email }))).await;
        let code = if email.len() > 254 { "invalid_request" } else { "invalid_email" };
        assert_eq!(r.json()["error"], code, "{email}");
    }
    let reasons = [
        ("short", "too_short"),
        ("qwertyuiop", "too_common"),
        ("my alice_1 password", "contains_username"),
        ("xx alice xx yy", "contains_email"),
    ];
    for (password, reason) in reasons {
        let r = h.post(REG, good(json!({ "password": password }))).await;
        let body = r.json();
        assert_eq!(
            (r.status, &body["error"], &body["reason"]),
            (400, &json!("weak_password"), &json!(reason)),
            "{password}"
        );
    }
}

#[tokio::test]
async fn an_existing_email_gets_the_same_answer_work_and_traces_as_a_new_one() {
    // Two identical servers, except that one has an account with the address.
    let mut servers = Vec::new();
    for with_account in [false, true] {
        let counter = CountingHasher::new();
        let h = Harness::build(Setup { hasher: Some(counter.clone()), ..Setup::default() }).await;
        if with_account {
            h.create_user_with("owner", Some("target@example.com"), Some(super::PW), true).await;
        }
        servers.push((h, counter));
    }
    let mut answers = Vec::new();
    for (h, counter) in &servers {
        let before = counter.hashes();
        let r = h.post(REG, good(json!({ "username": "Prober", "email": "TARGET@example.com" }))).await;
        answers.push(seen(&r));
        // The same work: one password hash in both cases.
        assert_eq!(counter.hashes() - before, 1);
        assert!(user_by_name(h, "Prober").await.is_none(), "no account created");
        assert_eq!(
            signup_by_name(h, "prober").await.unwrap().email,
            "target@example.com",
            "the username is held"
        );
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0].0, 202);
    // What a prober could look at next answers the same on both servers.
    let mut probes = Vec::new();
    for (h, _) in &servers {
        let again = h.post(REG, good(json!({ "username": "prober", "email": "other@example.com" }))).await;
        let login = h.post("/api/v1/auth/login", json!({ "login": "Prober", "password": GOOD_PW })).await;
        probes.push((
            again.status,
            again.json()["error"].clone(),
            login.status,
            login.json()["error"].clone(),
        ));
    }
    assert_eq!(probes[0], (409, json!("username_taken"), 401, json!("invalid_credentials")));
    assert_eq!(probes[1], probes[0]);
}

#[tokio::test]
async fn an_existing_email_gets_the_same_answer_and_its_owner_a_notice() {
    let h = Harness::new().await;
    h.create_user_with("owner", Some("taken@example.com"), Some(super::PW), true).await;
    let r = h.post(REG, good(json!({ "username": "newcomer", "email": "TAKEN@example.com" }))).await;
    let fresh = h.post(REG, good(json!({ "username": "another", "email": "free@example.com" }))).await;
    assert_eq!((r.status, r.json()), (fresh.status, fresh.json()));
    assert!(user_by_name(&h, "newcomer").await.is_none());
    assert_eq!(
        signup_by_email(&h, "taken@example.com").await.unwrap().token_hash,
        None,
        "no link for an address with an account"
    );
    let notice = h.sent().await.into_iter().find(|m| m.to == "taken@example.com").unwrap();
    assert!(notice.subject.contains("Someone tried to register"), "{}", notice.subject);
    assert!(!notice.text.contains("verify-email"));
    // A second attempt within the hour does not mail the owner again.
    h.post(REG, good(json!({ "username": "newcomer2", "email": "taken@example.com" }))).await;
    assert_eq!(h.sent().await.iter().filter(|m| m.to == "taken@example.com").count(), 1);
    assert!(h.event_kinds().await.iter().any(|k| k == "register_existing_email"));
}

#[tokio::test]
async fn a_pending_signup_holds_its_username_for_the_life_of_its_link_and_the_purge_deletes_it() {
    let h = Harness::new().await;
    assert_eq!(h.post(REG, good(json!({}))).await.status, 202);
    let other = good(json!({ "email": "mallory@example.com" }));
    let mut upper = other.clone();
    upper["username"] = json!("ALICE_1");
    let r = h.post(REG, upper).await;
    assert_eq!(
        (r.status, r.json()["error"].clone()),
        (409, json!("username_taken")),
        "held, whatever the case"
    );
    h.advance(24 * 3_600_000 - 1);
    assert_eq!(h.post(REG, other.clone()).await.status, 409);
    h.advance(1);
    // Expired: the username is free at once, before the purge.
    assert_eq!(h.post(REG, other).await.status, 202);
    assert_eq!(signup_by_name(&h, "alice_1").await.unwrap().email, "mallory@example.com");
    assert!(signup_by_email(&h, "alice@example.com").await.is_none(), "the expired signup was replaced");
    // The retention purge deletes the expired ones and keeps the live ones.
    assert_eq!(
        h.post(REG, good(json!({ "username": "Bob_2", "email": "bob@example.com" }))).await.status,
        202
    );
    h.advance(12 * 3_600_000);
    assert_eq!(
        h.post(REG, good(json!({ "username": "Carol", "email": "carol@example.com" }))).await.status,
        202
    );
    h.advance(12 * 3_600_000);
    let policy = RetentionPolicy {
        security_days: h.config.retention_security_days,
        ip_days: h.config.retention_ip_days,
    };
    let counts = h.store.retention().run(h.now(), policy).await.unwrap();
    assert_eq!(counts.tokens, 2, "the two signups of the first day");
    assert!(signup_by_name(&h, "alice_1").await.is_none());
    assert!(signup_by_name(&h, "bob_2").await.is_none());
    assert!(signup_by_name(&h, "carol").await.is_some());
    assert!(user_by_name(&h, "carol").await.is_none());
}

#[tokio::test]
async fn a_second_signup_with_the_same_address_replaces_the_pending_one() {
    let h = Harness::new().await;
    assert_eq!(h.post(REG, good(json!({}))).await.status, 202);
    let first = mail_token(&h.last_mail().await);
    // Within 5 minutes: replaced, but no second link mail (the address is anyone's).
    assert_eq!(h.post(REG, good(json!({ "username": "Alice_2" }))).await.status, 202);
    assert_eq!(h.sent().await.len(), 1);
    assert!(signup_by_name(&h, "alice_1").await.is_none(), "the first username is free again");
    assert_eq!(confirm_page(&h, &first).await.status, 400, "the first link no longer works");
    let r = h.post(REG, good(json!({ "username": "Alice_1", "email": "someone@example.com" }))).await;
    assert_eq!(r.status, 202);
    // Later: replaced again, with a new link.
    h.advance(5 * 60_000);
    let r = h.post(REG, good(json!({ "username": "Alice_3", "password": "another ivory rook" }))).await;
    assert_eq!(r.status, 202);
    let mails: Vec<_> = h.sent().await.into_iter().filter(|m| m.to == "alice@example.com").collect();
    assert_eq!(mails.len(), 2);
    assert!(mails[1].text.contains("Alice_3"));
    assert_eq!(confirm_post(&h, &mail_token(&mails[1])).await.status, 200);
    assert_eq!(user_by_email(&h, "alice@example.com").await.unwrap().username, "Alice_3");
    assert!(user_by_name(&h, "Alice_2").await.is_none());
    h.login("alice_3", "another ivory rook").await;
}

#[tokio::test]
async fn a_signup_that_lost_a_race_for_its_username_leaves_the_link_mail_to_its_retry() {
    let counter = CountingHasher::new();
    let h = Harness::build(Setup { hasher: Some(counter.clone()), ..Setup::default() }).await;
    // Another account takes the username while the password is hashed: 409, nothing mailed.
    let (store, rt, now) = (h.store.clone(), tokio::runtime::Handle::current(), h.now());
    counter.before_next_hash(move || {
        let user = NewUser {
            username: "Alice_1".into(),
            email: Some("first@example.com".into()),
            password_hash: Some("x".into()),
            email_verified: true,
            accept_challenges: true,
            created_at: now,
        };
        rt.block_on(store.users().create(user)).unwrap();
    });
    let lost = h.post(REG, good(json!({}))).await;
    assert_eq!((lost.status, lost.json()["error"].clone()), (409, json!("username_taken")));
    // The retry, within 5 minutes, gets its link.
    h.advance(60_000);
    assert_eq!(h.post(REG, good(json!({ "username": "Alice_2" }))).await.status, 202);
    let sent = h.sent().await;
    assert_eq!(sent.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["alice@example.com"]);
    assert_eq!(confirm_post(&h, &mail_token(&sent[0])).await.status, 200);
    assert_eq!(user_by_email(&h, "alice@example.com").await.unwrap().username, "Alice_2");
}

#[tokio::test]
async fn resend_renews_the_link_of_a_pending_signup_and_says_nothing_about_the_address() {
    let h = Harness::new().await;
    assert_eq!(h.post(REG, good(json!({}))).await.status, 202);
    let first = mail_token(&h.last_mail().await);
    h.advance(20 * 3_600_000);
    for email in ["alice@example.com", "nobody@example.com"] {
        let r = h.post("/api/v1/auth/verify-email/resend", json!({ "email": email })).await;
        assert_eq!((r.status, r.json()), (202, json!({ "status": "accepted" })));
    }
    let sent = h.sent().await;
    assert_eq!(
        sent.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(),
        ["alice@example.com", "alice@example.com"]
    );
    let second = mail_token(&sent[1]);
    assert_eq!(confirm_page(&h, &first).await.status, 400, "the new link replaces the first one");
    assert_eq!(signup_by_email(&h, "alice@example.com").await.unwrap().expires_at, h.now() + 24 * 3_600_000);
    h.advance(10 * 3_600_000);
    assert_eq!(confirm_post(&h, &second).await.status, 200, "valid 24 h from the resend");
    assert!(user_by_name(&h, "Alice_1").await.unwrap().email_verified);
}

#[tokio::test]
async fn resend_holds_the_username_24_h_more_whether_or_not_the_address_has_an_account() {
    let mut servers = Vec::new();
    for with_account in [false, true] {
        let h = Harness::new().await;
        if with_account {
            h.create_user_with("owner", Some("target@example.com"), Some(super::PW), true).await;
        }
        servers.push(h);
    }
    let mut probes = Vec::new();
    for h in &servers {
        let r = h.post(REG, good(json!({ "username": "Prober", "email": "target@example.com" }))).await;
        assert_eq!(r.status, 202);
        h.advance(23 * 3_600_000);
        let r = h.post("/api/v1/auth/verify-email/resend", json!({ "email": "TARGET@example.com" })).await;
        h.advance(2 * 3_600_000);
        let again = h.post(REG, good(json!({ "username": "prober", "email": "other@example.com" }))).await;
        probes.push((seen(&r), again.status, again.json()["error"].clone()));
    }
    assert_eq!(
        (probes[0].1, &probes[0].2),
        (409, &json!("username_taken")),
        "still held 2 h after the first 24 h"
    );
    assert_eq!(probes[1], probes[0]);
    assert_eq!((probes[0].0.0, probes[0].0.1.as_str()), (202, r#"{"status":"accepted"}"#));
}

#[tokio::test]
async fn the_link_of_a_pending_signup_whose_username_or_address_another_account_took() {
    let h = Harness::new().await;
    h.post(REG, good(json!({}))).await;
    h.post(REG, good(json!({ "username": "Bob_2", "email": "bob@example.com" }))).await;
    let tokens: Vec<String> = h.sent().await.iter().map(mail_token).collect();
    // Accounts made another way (a Google sign-in, for one) with that address, and that username.
    h.create_user_with("Alice_9", Some("alice@example.com"), Some(super::PW), true).await;
    h.create_user_with("bob_2", Some("robert@example.com"), Some(super::PW), true).await;
    for token in &tokens {
        let r = confirm_post(&h, token).await;
        assert_eq!(r.status, 409);
        assert!(r.text().contains("Another account took this username or this e-mail address"));
        assert_eq!(confirm_post(&h, token).await.status, 400, "the signup is dropped");
    }
    assert!(user_by_name(&h, "Alice_1").await.is_none());
    assert!(user_by_email(&h, "bob@example.com").await.is_none());
}

#[tokio::test]
async fn accounts_created_unconfirmed_before_pending_signups_keep_their_links_and_answers() {
    let h = Harness::new().await;
    let old = h.create_user_with("oldtimer", Some("old@example.com"), Some(super::PW), false).await;
    let l = h.post("/api/v1/auth/login", json!({ "login": "oldtimer", "password": super::PW })).await;
    assert_eq!((l.status, l.json()["error"].clone()), (403, json!("email_unverified")));
    // A signup with its address is the "existing address" case: a held username, no link, a notice.
    assert_eq!(h.post(REG, good(json!({ "email": "old@example.com" }))).await.status, 202);
    let r = h.post("/api/v1/auth/verify-email/resend", json!({ "email": "old@example.com" })).await;
    assert_eq!(r.status, 202);
    let sent = h.sent().await;
    let mut subjects: Vec<String> = sent
        .iter()
        .map(|m| m.subject.split(" for ").next().unwrap().split(" on ").next().unwrap().to_owned())
        .collect();
    subjects.sort();
    assert_eq!(subjects, ["Confirm your e-mail address", "Someone tried to register"]);
    let link = sent.iter().find(|m| m.subject.contains("Confirm")).unwrap();
    assert!(link.text.contains("oldtimer"));
    verify_from_mail(&h, link).await;
    assert!(h.user(old).await.email_verified);
    assert!(user_by_name(&h, "Alice_1").await.is_none(), "the held username made no account");
    assert_eq!(h.login("oldtimer", super::PW).await["user"]["id"], json!(old));
}

#[tokio::test]
async fn registration_closed() {
    let h = Harness::with_env(&[("REGISTRATION", "closed")]).await;
    let r = h.post(REG, good(json!({}))).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("registration_closed")));
}

#[tokio::test]
async fn without_email_confirmation_the_account_is_ready_at_once() {
    let h = Harness::with_env(&[("REQUIRE_EMAIL_VERIFICATION", "0")]).await;
    let r = h.post(REG, good(json!({}))).await;
    assert_eq!((r.status, r.json()), (201, json!({ "status": "ready" })));
    assert!(h.sent().await.is_empty());
    h.login("Alice_1", GOOD_PW).await;
    let dup = h.post(REG, good(json!({ "username": "other" }))).await;
    assert_eq!((dup.status, dup.json()["error"].clone()), (409, json!("email_taken")));
}

#[tokio::test]
async fn proof_of_work_on_registration_is_required_verified_single_use_expiring_and_bound_to_the_client() {
    let h = Harness::with_env(&[("POW_REGISTER_BITS", "8")]).await;
    let ip = "203.0.113.9";
    let reg = |body: Value, from: &str| h.call_from(from, Method::POST, REG).json(&body).send();
    let r = reg(good(json!({})), ip).await;
    assert_eq!((r.status, r.json()["error"].clone()), (428, json!("pow_required")));
    let pow = r.json()["pow"].clone();
    let challenge = pow["challenge"].as_str().unwrap().to_owned();
    assert_eq!(pow["bits"], 8);
    assert_eq!(pow["expiresAt"], json!(h.now() + POW_TTL_MS));
    let nonce = solve_pow(&challenge, 8);

    let r = reg(good(json!({ "pow": { "challenge": challenge, "nonce": nonce } })), "198.51.100.20").await;
    assert_eq!(
        (r.status, r.json()["reason"].clone()),
        (428, json!("network")),
        "another client cannot use it"
    );
    let wrong = (nonce.parse::<u64>().unwrap() + 1).to_string();
    let r = reg(good(json!({ "pow": { "challenge": challenge, "nonce": wrong } })), ip).await;
    assert_eq!(r.status, 428);
    let r = reg(good(json!({ "pow": { "challenge": challenge, "nonce": nonce } })), ip).await;
    assert_eq!(r.status, 202);
    let body = good(
        json!({ "username": "Bob_2", "email": "bob@example.com", "pow": { "challenge": challenge, "nonce": nonce } }),
    );
    let r = reg(body, ip).await;
    assert_eq!((r.status, r.json()["reason"].clone()), (428, json!("replayed")));

    let carol = good(json!({ "username": "Carol", "email": "carol@example.com" }));
    let c2 = reg(carol.clone(), ip).await.json()["pow"]["challenge"].as_str().unwrap().to_owned();
    h.advance(POW_TTL_MS + 1);
    let mut late = carol;
    late["pow"] = json!({ "challenge": c2, "nonce": solve_pow(&c2, 8) });
    let r = reg(late, ip).await;
    assert_eq!((r.status, r.json()["reason"].clone()), (428, json!("expired")));
    assert!(r.json()["pow"]["challenge"].is_string(), "a fresh challenge comes with the refusal");

    // Cheap checks come before the work: an invalid username is answered without a challenge.
    assert_eq!(reg(good(json!({ "username": "_bad" })), ip).await.status, 400);
}

#[tokio::test]
async fn per_address_limit_on_the_auth_endpoints() {
    let h = Harness::with_env(&[("AUTH_RATE_PER_IP", "3")]).await;
    let reg = |i: u32, ip: &'static str| {
        h.call_from(ip, Method::POST, REG)
            .json(&good(json!({ "username": format!("user{i}"), "email": format!("u{i}@example.com") })))
            .send()
    };
    for i in 0..3 {
        assert_eq!(reg(i, "192.0.2.50").await.status, 202);
    }
    let r = reg(9, "192.0.2.50").await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
    assert!(r.json()["retryAfter"].as_u64().unwrap() > 0);
    assert_eq!(reg(8, "192.0.2.51").await.status, 202);
}
