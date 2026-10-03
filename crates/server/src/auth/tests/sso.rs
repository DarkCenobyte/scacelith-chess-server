//! Google sign-in (API.md "Google sign-in"): Google sends the browser back to the game's own
//! listener (`http://127.0.0.1:<port>/oauth2/google/<the server's origin tag>`), the game posts
//! the code to finish, and an account that already has the Google address is linked only after
//! its password, and its second factor when on, typed in the game (auth.sso.test.js).

use std::collections::HashMap;
use std::sync::Arc;

use http::Method;
use serde_json::{Value, json};

use super::fake_oidc::{FakeOidc, ISSUER, Redirect, Signing};
use super::{CountingHasher, Harness, PW, START_MS, Setup, link_in, token_of};
use crate::auth::oidc::{OidcClient, form_urlencode, pkce_challenge};
use crate::clock::{ManualClock, SharedClock};
use crate::config::{sso_origin_tag, test_config};
use crate::http::testing::TestResponse;
use crate::http::url::parse_urlencoded;
use crate::ids::UserId;
use crate::log::{Level, capture_logs};
use crate::mail::OutgoingMail;
use crate::mail::message::utc_string;
use crate::security::keys::{random_token, sha256_hex};
use crate::security::password::{Argon2Hasher, Argon2Params, PasswordHasher};
use crate::security::pow::solve_pow;
use crate::security::totp::{base32_decode, totp};
use crate::store::{NewSanction, NewToken, NewUser, SanctionKind, SecurityEvent, Source, UserUpdate};

const CLIENT_ID: &str = "1234-abc.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "GOCSPX-test-secret";
const PORT: u16 = 50123;
const START: &str = "/api/v1/auth/sso/google/start";
const FINISH: &str = "/api/v1/auth/sso/google/finish";
const LINK: &str = "/api/v1/auth/sso/google/link";
const COMPLETE: &str = "/api/v1/auth/sso/complete";
const MFA: &str = "/api/v1/auth/login/mfa";
const LOGIN: &str = "/api/v1/auth/login";
const FORM: &str = "application/x-www-form-urlencoded";
const ATTACKER: &str = "203.0.113.7";
const VICTIM: &str = "198.51.100.9";
const DEFAULT_IP: &str = "203.0.113.10";

/// The origin tag of the test servers (chess.example.org, API port 8443).
fn tag() -> String {
    sso_origin_tag("chess.example.org:8443")
}

fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/oauth2/google/{}", tag())
}

/// A PKCE pair of the game.
struct Pkce {
    verifier: String,
    challenge: String,
}

fn pkce() -> Pkce {
    let verifier = random_token("");
    Pkce { challenge: pkce_challenge(&verifier), verifier }
}

/// A started attempt: the game's verifier and the start's answer.
struct Attempt {
    verifier: String,
    attempt_id: String,
    auth_url: String,
    state: String,
}

/// A test server with Google sign-in and a fake Google.
struct Sso {
    h: Harness,
    idp: Arc<FakeOidc>,
}

/// What differs from the default Google sign-in server.
#[derive(Default)]
struct SsoSetup<'a> {
    env: Vec<(&'static str, &'a str)>,
    /// The provider, store and clock of another server of the test.
    shared: Option<&'a Sso>,
    hasher: Option<Arc<dyn PasswordHasher>>,
    real_mailer: bool,
}

impl Sso {
    async fn new(env: &[(&'static str, &str)]) -> Sso {
        Sso::with(SsoSetup { env: env.to_vec(), ..SsoSetup::default() }).await
    }

    async fn with(setup: SsoSetup<'_>) -> Sso {
        let (clock, idp) = match setup.shared {
            Some(other) => (other.h.clock.clone(), other.idp.clone()),
            None => {
                let clock = ManualClock::new(1_000_000.0, START_MS);
                (clock.clone(), Arc::new(FakeOidc::start(CLIENT_ID, CLIENT_SECRET, clock).await))
            }
        };
        let mut env: Vec<(&'static str, String)> = vec![
            ("SSO_GOOGLE_ENABLED", "1".into()),
            ("GOOGLE_CLIENT_ID", CLIENT_ID.into()),
            ("GOOGLE_CLIENT_SECRET", CLIENT_SECRET.into()),
        ];
        env.extend(setup.env.iter().map(|(k, v)| (*k, (*v).to_owned())));
        let h = Harness::build(Setup {
            env,
            oidc: Some(idp.options()),
            hasher: setup.hasher,
            clock: Some(clock),
            store: setup.shared.map(|o| o.h.store.clone()),
            real_mailer: setup.real_mailer,
            ..Setup::default()
        })
        .await;
        Sso { h, idp }
    }

    async fn post_from(&self, ip: &str, path: &str, body: Value) -> TestResponse {
        self.h.call_from(ip, Method::POST, path).json(&body).send().await
    }

    async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.post_from(DEFAULT_IP, path, body).await
    }

    /// The game's start, its listener on `port`.
    async fn start_at(&self, port: u16, ip: &str) -> Attempt {
        let p = pkce();
        let r =
            self.post_from(ip, START, json!({ "codeChallenge": p.challenge, "redirectPort": port })).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let b = r.json();
        let text = |k: &str| b[k].as_str().unwrap().to_owned();
        Attempt {
            verifier: p.verifier,
            attempt_id: text("attemptId"),
            auth_url: text("authUrl"),
            state: text("state"),
        }
    }

    async fn start(&self) -> Attempt {
        self.start_at(PORT, DEFAULT_IP).await
    }

    /// What the game posts once its listener got Google's redirect query `q`.
    async fn finish_from(&self, ip: &str, a: &Attempt, q: &Redirect, over: Value) -> TestResponse {
        let mut body = json!({
            "attemptId": a.attempt_id, "codeVerifier": a.verifier, "state": q.state, "code": q.code,
        });
        if let Some(iss) = &q.iss {
            body["iss"] = iss.as_str().into();
        }
        if let (Some(b), Some(o)) = (body.as_object_mut(), over.as_object()) {
            b.extend(o.clone());
        }
        self.post_from(ip, FINISH, body).await
    }

    async fn finish(&self, a: &Attempt, q: &Redirect, over: Value) -> TestResponse {
        self.finish_from(DEFAULT_IP, a, q, over).await
    }

    /// Start, the consent at Google, finish.
    async fn sign_in_from(&self, ip: &str, claims: Value) -> (Attempt, Redirect, TestResponse) {
        let a = self.start_at(PORT, ip).await;
        let q = self.idp.authorize(&a.auth_url, claims);
        let r = self.finish_from(ip, &a, &q, json!({})).await;
        (a, q, r)
    }

    async fn sign_in(&self, claims: Value) -> TestResponse {
        self.sign_in_from(DEFAULT_IP, claims).await.2
    }

    async fn link_from(&self, ip: &str, ticket: &str, password: &str, over: Value) -> TestResponse {
        let mut body = json!({ "linkTicket": ticket, "password": password });
        if let (Some(b), Some(o)) = (body.as_object_mut(), over.as_object()) {
            b.extend(o.clone());
        }
        self.post_from(ip, LINK, body).await
    }

    async fn link(&self, ticket: &str, password: &str) -> TestResponse {
        self.link_from(DEFAULT_IP, ticket, password, json!({})).await
    }
}

fn claims(over: Value) -> Value {
    let mut c = json!({
        "sub": "1098765", "email": "Magnus@Gmail.com", "email_verified": true, "name": "Magnus Hansen",
        "given_name": "Magnus",
    });
    if let (Some(b), Some(o)) = (c.as_object_mut(), over.as_object()) {
        b.extend(o.clone());
    }
    c
}

async fn events_of(h: &Harness, kind: &str) -> Vec<SecurityEvent> {
    h.events().await.into_iter().filter(|e| e.kind == kind).collect()
}

async fn last_login_method(h: &Harness) -> Value {
    events_of(h, "login").await.pop().expect("a login event").detail.unwrap()["method"].clone()
}

/// The account the Google subject `sub` is linked to.
async fn google_link(h: &Harness, sub: &str) -> Option<UserId> {
    h.store.sso().find("google".into(), sub.into()).await.unwrap().map(|l| l.user_id)
}

async fn live_sessions(h: &Harness, id: UserId) -> usize {
    let all = h.store.read(move |db| db.sessions().all_for_user(id)).await.unwrap();
    all.iter().filter(|s| s.revoked_at.is_none()).count()
}

async fn link_identity(h: &Harness, id: UserId, sub: &str, email: &str) {
    h.store.sso().link(id, "google".into(), sub.into(), Some(email.into()), h.now()).await.unwrap();
}

async fn ban(h: &Harness, id: UserId, ends_at: i64) {
    let sanction = NewSanction {
        user_id: id,
        kind: SanctionKind::Ban,
        reason: Some("test".into()),
        source: Source::Moderator,
        game_id: None,
        starts_at: h.now() - 1,
        ends_at: Some(ends_at),
        created_by: None,
        created_at: h.now() - 1,
    };
    h.store.sanctions().create(sanction).await.unwrap();
}

/// Turns two-step verification on for a password account; returns its secret.
async fn enable_mfa(h: &Harness, username: &str) -> Vec<u8> {
    let token = h.token(username, PW).await;
    let st = h.post_as(&token, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await;
    let secret = base32_decode(st.json()["secret"].as_str().unwrap()).unwrap();
    let en =
        h.post_as(&token, "/api/v1/account/mfa/totp/enable", json!({ "code": totp(&secret, h.now()) })).await;
    assert_eq!(en.status, 200, "{}", en.text());
    h.advance(30_000); // the next code is of a step not used yet
    secret
}

fn query_of(url: &str) -> Vec<(String, String)> {
    parse_urlencoded(url.split_once('?').map_or("", |(_, q)| q))
}

fn is_token(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|t| {
        t.len() == 43 && t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    })
}

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut keys = super::keys(v);
    keys.sort();
    keys
}

fn outdated_hash(password: &str) -> String {
    let old =
        Argon2Hasher::new(Argon2Params { memory_kib: 32, passes: 1, lanes: 1, ..Argon2Params::DEFAULT });
    old.hash(password).unwrap()
}

// ---- the loopback flow ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_unless_enabled_and_the_poll_route_and_callback_page_of_the_browser_flow_are_gone() {
    let h = Harness::new().await;
    let tok = format!("sso_{}", "a".repeat(43));
    let cases = [
        (START, json!({ "codeChallenge": "a".repeat(43), "redirectPort": PORT })),
        (
            FINISH,
            json!({ "attemptId": tok, "codeVerifier": "v".repeat(43), "state": "s".repeat(43), "code": "c" }),
        ),
        (LINK, json!({ "linkTicket": tok, "password": PW })),
    ];
    for (path, body) in cases {
        let r = h.post(path, body).await;
        assert_eq!((r.status, r.json()["error"].clone()), (404, json!("sso_disabled")), "{path}");
    }
    let x = Sso::new(&[]).await;
    let poll = x
        .post("/api/v1/auth/sso/google/poll", json!({ "attemptId": tok, "codeVerifier": "v".repeat(43) }))
        .await;
    assert_eq!(poll.status, 404);
    assert_eq!(x.h.call(Method::GET, "/auth/sso/google/callback?code=x&state=y").send().await.status, 404);
}

#[tokio::test]
async fn start_returns_to_the_posted_loopback_port_under_this_servers_origin_tag_with_the_state() {
    let x = Sso::new(&[]).await;
    assert_eq!(
        (x.h.config.sso_origin.as_str(), x.h.config.sso_redirect_tag.clone()),
        ("chess.example.org:8443", tag())
    );
    let p = pkce();
    let r = x.post(START, json!({ "codeChallenge": p.challenge, "redirectPort": PORT })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    assert_eq!(sorted_keys(&b), ["attemptId", "authUrl", "expiresIn", "state"]);
    assert!(is_token(b["attemptId"].as_str().unwrap(), "sso_"));
    assert!(is_token(b["state"].as_str().unwrap(), ""));
    assert_eq!(b["expiresIn"], 600);
    let url = b["authUrl"].as_str().unwrap();
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{url}");
    let pairs = query_of(url);
    let q: HashMap<&str, &str> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    for k in [
        "client_id",
        "redirect_uri",
        "response_type",
        "scope",
        "state",
        "nonce",
        "code_challenge",
        "code_challenge_method",
        "prompt",
    ] {
        assert_eq!(pairs.iter().filter(|(n, _)| n == k).count(), 1, "{k}");
    }
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], redirect_uri(PORT));
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["scope"], "openid email profile");
    assert_eq!(q["state"], b["state"]);
    assert!(is_token(q["nonce"], ""));
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(is_token(q["code_challenge"], ""));
    assert_ne!(q["code_challenge"], p.challenge, "the server uses its own PKCE pair with Google");
    assert_eq!(q["prompt"], "select_account");
    for port in [1024, 65535] {
        let a = x.start_at(port, DEFAULT_IP).await;
        let pairs = query_of(&a.auth_url);
        let uri = pairs.iter().find(|(k, _)| k == "redirect_uri").map(|(_, v)| v.clone());
        assert_eq!(uri, Some(redirect_uri(port)));
    }
}

#[tokio::test]
async fn the_origin_tags_of_the_contract_vectors_and_the_client_takes_only_a_loopback_redirect_uri() {
    for (origin, expected) in [
        ("play.scacelith.example:443", "IhcScoV7eDOzTEcSnqPUPt"),
        ("localhost:8443", "TFGx7zQ_8QlGZW5zpqznCr"),
        ("[::1]:8443", "XToJm0DG5PjciEVmZa9Cho"),
        ("127.0.0.1:50443", "3r653wM5ZjYsHcAJljmCwY"),
    ] {
        assert_eq!(sso_origin_tag(origin), expected, "{origin}");
    }
    let clock = ManualClock::new(1_000_000.0, START_MS);
    let idp = FakeOidc::start(CLIENT_ID, CLIENT_SECRET, clock.clone()).await;
    let shared: SharedClock = clock;
    let oidc = OidcClient::new(CLIENT_ID, CLIENT_SECRET, idp.endpoints(), shared, true);
    let url = |uri: &str| oidc.authorization_url(&"s".repeat(43), &"n".repeat(43), &"c".repeat(43), uri);
    let good = redirect_uri(PORT);
    assert!(url(&good).unwrap().contains(&form_urlencode(&[("redirect_uri", &good)])));
    let tagged = |host_port: &str, path: &str, scheme: &str| format!("{scheme}://{host_port}{path}");
    let path = format!("/oauth2/google/{}", tag());
    let lp = format!("127.0.0.1:{PORT}");
    for bad in [
        tagged(&format!("localhost:{PORT}"), &path, "http"),
        tagged(&format!("[::1]:{PORT}"), &path, "http"),
        tagged(&lp, &path, "https"),
        tagged("127.0.0.1:80", &path, "http"),
        tagged("127.0.0.1:1023", &path, "http"),
        tagged("127.0.0.1:65536", &path, "http"),
        tagged(&lp, &format!("{path}x"), "http"),
        tagged(&lp, &format!("{path}/"), "http"),
        tagged(&lp, "/callback", "http"),
        String::new(),
    ] {
        assert_eq!(url(&bad).unwrap_err().reason, "bad_redirect_uri", "{bad}");
        let exchange = oidc.exchange_code("code", &"v".repeat(43), &bad).await;
        assert_eq!(exchange.unwrap_err().reason, "bad_redirect_uri", "{bad}");
    }
    assert!(idp.token_calls().is_empty(), "nothing sent to Google");
}

#[tokio::test]
async fn start_and_finish_validate_their_body() {
    let x = Sso::new(&[]).await;
    let challenge = pkce().challenge;
    for port in [Value::Null, json!(0), json!(80), json!(1023), json!(65536), json!(1.5), json!("50123")] {
        let r = x.post(START, json!({ "codeChallenge": challenge, "redirectPort": port })).await;
        assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_request")), "{port}");
    }
    let r = x.post(START, json!({ "codeChallenge": challenge })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_request")), "no port");
    assert_eq!(x.post(START, json!({ "codeChallenge": "short", "redirectPort": PORT })).await.status, 400);
    for extra in [json!({ "codeChallengeMethod": "S256" }), json!({ "redirectUri": redirect_uri(PORT) })] {
        let mut body = json!({ "codeChallenge": challenge, "redirectPort": PORT });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert_eq!(x.post(START, body).await.status, 400, "{extra}");
    }
    let a = x.start().await;
    let good =
        json!({ "attemptId": a.attempt_id, "codeVerifier": a.verifier, "state": a.state, "code": "abc" });
    let state = a.state.clone();
    for bad in [
        json!({ "codeVerifier": "short" }),
        json!({ "state": state[1..] }),
        json!({ "state": format!("{}!", &state[1..]) }),
        json!({ "code": "" }),
        json!({ "code": "a b" }),
        json!({ "code": "x".repeat(2049) }),
        json!({ "iss": "" }),
        json!({ "iss": "x".repeat(257) }),
        json!({ "clientLabel": "x".repeat(65) }),
        json!({ "redirectUri": redirect_uri(PORT) }),
    ] {
        let mut body = good.clone();
        body.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        assert_eq!(x.post(FINISH, body).await.status, 400, "{bad}");
    }
    assert_eq!(x.post(LINK, json!({ "linkTicket": "sso_x", "password": PW, "extra": 1 })).await.status, 400);
}

#[tokio::test]
async fn a_first_google_sign_in_finishes_with_the_games_verifier_then_a_username_and_the_next_finds_the_link()
{
    let x = Sso::new(&[]).await;
    let h = &x.h;
    let a = x.start().await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    assert_eq!(q.iss.as_deref(), Some(ISSUER));

    let wrong = x.finish(&a, &q, json!({ "codeVerifier": pkce().verifier })).await;
    assert_eq!(
        (wrong.status, wrong.json()["error"].clone()),
        (403, json!("invalid_verifier")),
        "the attempt id alone is useless"
    );
    assert!(x.idp.token_calls().is_empty(), "nothing exchanged, the attempt kept");
    let r = x.finish(&a, &q, json!({ "clientLabel": "Scacelith (test)" })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    assert_eq!(sorted_keys(&b), ["needsUsername", "ssoTicket", "suggestedUsername"]);
    let ticket = b["ssoTicket"].as_str().unwrap().to_owned();
    assert!(is_token(&ticket, "sso_"));
    assert_eq!(b["suggestedUsername"], "Magnus");
    let call = x.idp.token_calls()[0].clone();
    assert_eq!(call["redirect_uri"], redirect_uri(PORT), "the redirect URI of the attempt, byte for byte");
    assert_eq!(call["code_verifier"].len(), 43);
    assert_ne!(call["code_verifier"], a.verifier, "the server's own verifier");
    assert_eq!(x.finish(&a, &q, json!({})).await.status, 410, "an attempt finishes once");

    h.create_user_with("Magnus", Some("other@example.com"), Some(PW), true).await;
    let complete = |username: &str| x.post(COMPLETE, json!({ "ssoTicket": ticket, "username": username }));
    let c = complete("magnus").await;
    assert_eq!((c.status, c.json()["error"].clone()), (409, json!("username_taken")));
    assert_eq!(complete("_x").await.json()["error"], "invalid_username");
    // A username held by the pending signup of another address is taken too.
    let signup =
        json!({ "username": "Magnus_C", "email": "carlsen@example.com", "password": "ivory rook takes e5" });
    assert_eq!(x.post("/api/v1/auth/register", signup).await.status, 202);
    let c = complete("magnus_c").await;
    assert_eq!((c.status, c.json()["error"].clone()), (409, json!("username_taken")));
    let c = complete("MagnusH").await;
    assert_eq!(c.status, 200, "{}", c.text());
    let cb = c.json();
    assert!(cb["token"].as_str().unwrap().starts_with("sct_"));
    let u = &cb["user"];
    assert_eq!(
        [&u["username"], &u["email"], &u["emailVerified"], &u["googleLinked"], &u["hasPassword"]],
        [&json!("MagnusH"), &json!("magnus@gmail.com"), &json!(true), &json!(true), &json!(false)]
    );
    assert_eq!(complete("Other1").await.status, 410);

    // The next sign-in finds the link (by the Google subject, whatever the address now).
    let again = x.sign_in(claims(json!({ "email": "changed@gmail.com" }))).await;
    assert_eq!(again.status, 200, "{}", again.text());
    assert_eq!(again.json()["user"]["username"], "MagnusH");
    assert_eq!(last_login_method(h).await, "google");
    assert_eq!(events_of(h, "sso_login").await.len(), 1);
    assert_eq!(x.idp.jwks_fetches(), 1, "the keys are cached as their Cache-Control says");
}

#[tokio::test]
async fn a_linked_account_with_two_step_verification_gets_the_mfa_step_then_a_google_totp_session() {
    let x = Sso::new(&[]).await;
    let h = &x.h;
    let id = h.create_user_with("magnus", Some("magnus@gmail.com"), Some(PW), true).await;
    let secret = enable_mfa(h, "magnus").await;
    link_identity(h, id, "1098765", "magnus@gmail.com").await;
    let r = x.sign_in(claims(json!({}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!((r.json()["mfaRequired"].clone(), r.json().get("token").cloned()), (json!(true), None));
    let m = x.post(MFA, json!({ "mfaToken": r.json()["mfaToken"], "code": totp(&secret, h.now()) })).await;
    assert_eq!(m.status, 200, "{}", m.text());
    assert!(m.json()["token"].as_str().unwrap().starts_with("sct_"));
    assert_eq!(last_login_method(h).await, "google+totp");
}

#[tokio::test]
async fn finish_a_wrong_state_or_issuer_fails_and_uses_the_attempt_and_dead_attempts_are_410() {
    let x = Sso::new(&[]).await;
    let h = &x.h;
    let a = x.start().await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    let r = x.finish(&a, &q, json!({ "state": pkce().verifier })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (502, json!("sso_failed")));
    assert_eq!(x.finish(&a, &q, json!({})).await.status, 410, "the attempt was used");
    let a = x.start().await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    let r = x.finish(&a, &q, json!({ "iss": "https://evil.example" })).await;
    assert_eq!((r.status, r.json()["error"].clone()), (502, json!("sso_failed")));
    assert!(x.idp.token_calls().is_empty(), "neither was exchanged");
    let reasons: Vec<Value> =
        events_of(h, "sso_failed").await.into_iter().map(|e| e.detail.unwrap()["reason"].clone()).collect();
    assert_eq!(reasons, [json!("state_mismatch"), json!("bad_iss")]);
    // No iss in the redirect (it is optional): accepted.
    let a = x.start().await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    let r = x.finish(&a, &Redirect { iss: None, ..q }, json!({})).await;
    assert_eq!(r.json()["needsUsername"], true, "{}", r.text());

    let a = x.start().await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    h.advance(10 * 60_000 + 1);
    assert_eq!(x.finish(&a, &q, json!({})).await.status, 410, "expired");
    let unknown = json!({ "attemptId": format!("sso_{}", "z".repeat(43)), "codeVerifier": "v".repeat(43), "state": "s".repeat(43), "code": "c" });
    assert_eq!(x.post(FINISH, unknown).await.status, 410, "unknown");
    // An attempt row of the browser-callback flow (no stateHash, redirectUri or verifier).
    let stale = random_token("sso_");
    let p = pkce();
    let row = NewToken {
        kind: "sso_attempt".into(),
        token_hash: sha256_hex(&stale),
        user_id: None,
        data: Some(json!({ "challenge": p.challenge, "status": "pending" })),
        created_at: h.now(),
        expires_at: h.now() + 600_000,
    };
    h.store.tokens().create(row).await.unwrap();
    let r = x
        .post(
            FINISH,
            json!({ "attemptId": stale, "codeVerifier": p.verifier, "state": "s".repeat(43), "code": "c" }),
        )
        .await;
    assert_eq!(r.status, 410, "stale");
}

#[tokio::test]
async fn a_code_minted_for_one_attempt_fails_through_another_and_opens_no_session() {
    let x = Sso::new(&[]).await;
    let id = x.h.create_user_with("magnus", Some("magnus@gmail.com"), Some(PW), true).await;
    link_identity(&x.h, id, "1098765", "magnus@gmail.com").await;
    for port in [PORT, PORT + 1] {
        let a = x.start().await;
        let b = x.start_at(port, DEFAULT_IP).await;
        let qa = x.idp.authorize(&a.auth_url, claims(json!({})));
        let r = x.finish(&b, &Redirect { state: b.state.clone(), ..qa }, json!({})).await;
        assert_eq!((r.status, r.json()["error"].clone()), (502, json!("sso_failed")), "B on port {port}");
    }
    assert_eq!(live_sessions(&x.h, id).await, 0);
}

#[tokio::test]
async fn cross_device_phishing_whoever_started_the_attempt_never_gets_the_sign_in_of_the_browser_sent_to_google()
 {
    let x = Sso::new(&[]).await;
    let h = &x.h;
    let victim = h.create_user_with("victor", Some("victim@gmail.com"), Some(PW), true).await;
    link_identity(h, victim, "v-sub", "victim@gmail.com").await;
    // The attacker starts from a script and sends the genuine Google link to the victim.
    let a = x.start_at(PORT, ATTACKER).await;
    // The victim signs in at Google; her browser goes to her own 127.0.0.1, with the code.
    let q = x.idp.authorize(
        &a.auth_url,
        json!({ "sub": "v-sub", "email": "victim@gmail.com", "email_verified": true }),
    );
    let uri = query_of(&a.auth_url).into_iter().find(|(k, _)| k == "redirect_uri").unwrap().1;
    assert!(uri.starts_with("http://127.0.0.1:"));
    // The routes that delivered the result to the attacker are gone.
    let poll = x
        .post_from(
            ATTACKER,
            "/api/v1/auth/sso/google/poll",
            json!({ "attemptId": a.attempt_id, "codeVerifier": a.verifier }),
        )
        .await;
    assert_eq!(poll.status, 404);
    let callback =
        format!("/auth/sso/google/callback?{}", form_urlencode(&[("code", &q.code), ("state", &q.state)]));
    assert_eq!(h.call_from(VICTIM, Method::GET, &callback).send().await.status, 404);
    // Without the code, the attacker's finish fails and uses the attempt.
    let guessed = Redirect { code: "guessed-code".into(), state: a.state.clone(), iss: None };
    let r = x.finish_from(ATTACKER, &a, &guessed, json!({})).await;
    assert_eq!((r.status, r.json()["error"].clone()), (502, json!("sso_failed")));
    assert_eq!(x.finish_from(ATTACKER, &a, &q, json!({})).await.status, 410);
    assert_eq!(live_sessions(h, victim).await, 0);
    // A first sign-in (no account yet) gives the attacker no ticket either.
    let b = x.start_at(PORT, ATTACKER).await;
    x.idp.authorize(
        &b.auth_url,
        json!({ "sub": "new-sub", "email": "newcomer@gmail.com", "email_verified": true }),
    );
    let guessed = Redirect { code: "guessed-code".into(), state: b.state.clone(), iss: None };
    assert_eq!(x.finish_from(ATTACKER, &b, &guessed, json!({})).await.status, 502);
}

#[tokio::test]
async fn a_relay_through_another_server_has_another_tag_and_a_rewritten_redirect_uri_fails_at_the_exchange() {
    let x = Sso::new(&[]).await;
    let evil = test_config(&[("SERVER_PUBLIC_HOST", "evil.example"), ("API_PORT", "8443")]).unwrap();
    assert_eq!(evil.sso_redirect_tag, sso_origin_tag("evil.example:8443"));
    assert_ne!(evil.sso_redirect_tag, tag());
    // The hostile server forwards the game's start (its port) to the official one...
    let a = x.start().await;
    let mut pairs = query_of(&a.auth_url);
    let uri = pairs.iter_mut().find(|(k, _)| k == "redirect_uri").unwrap();
    assert_eq!(uri.1, redirect_uri(PORT), "the official server only hands out its own tag");
    // ... and rewrites the redirect URI to its own tag, which the game would accept.
    uri.1 = format!("http://127.0.0.1:{PORT}/oauth2/google/{}", evil.sso_redirect_tag);
    let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let rewritten = format!("https://accounts.google.com/o/oauth2/v2/auth?{}", form_urlencode(&refs));
    let q = x.idp.authorize(&rewritten, claims(json!({})));
    let r = x.finish(&a, &q, json!({})).await;
    assert_eq!((r.status, r.json()["error"].clone()), (502, json!("sso_failed")));
    let last = x.idp.token_calls().pop().unwrap();
    assert_eq!(last["redirect_uri"], redirect_uri(PORT), "the exchange sends the stored redirect URI");
}

#[tokio::test]
async fn no_secret_leaks_a_failed_sign_in_shows_no_provider_text_code_or_token_and_every_410_has_the_same_body()
 {
    let x = Sso::new(&[]).await;
    let logs = capture_logs(Level::Debug);
    let a = x.start().await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    // A code the provider refuses (invalid_grant), then an ID token with a bad signature.
    let refused = x.finish(&a, &Redirect { code: "never-issued-code".into(), ..q.clone() }, json!({})).await;
    let b = x.start().await;
    let qb = x.idp.authorize(&b.auth_url, claims(json!({})));
    x.idp.tamper(Some(Box::new(|s: &mut Signing| s.key = 2)));
    let forged = x.finish(&b, &qb, json!({})).await;
    x.idp.tamper(None);
    for r in [&refused, &forged] {
        assert_eq!(r.status, 502);
        assert_eq!(
            r.json(),
            json!({ "error": "sso_failed", "message": "Google sign-in could not be completed." })
        );
    }
    let secrets = [
        &q.code,
        &qb.code,
        "never-issued-code",
        &a.state,
        &b.state,
        &a.attempt_id,
        &b.attempt_id,
        &a.verifier,
        "invalid_grant",
    ];
    for v in secrets {
        assert!(!refused.text().contains(v) && !forged.text().contains(v), "{v}");
    }
    // Neither in the log (the token endpoint's error code, 60 characters at most, is).
    let logged = logs.lines().concat();
    for v in [&q.code, &qb.code, "never-issued-code", &a.state, &b.state, &a.verifier, &b.verifier] {
        assert!(!logged.contains(v), "logged: {v}");
    }
    drop(logs);

    let c = x.start().await;
    x.h.advance(10 * 60_000 + 1);
    let qc = x.idp.authorize(&c.auth_url, claims(json!({})));
    let unknown = format!("sso_{}", "z".repeat(43));
    let attempt = |id: &str| json!({ "attemptId": id, "codeVerifier": "v".repeat(43), "state": "s".repeat(43), "code": "c" });
    let answers = [
        x.finish(&a, &q, json!({})).await,
        x.finish(&c, &qc, json!({})).await,
        x.post(FINISH, attempt(&unknown)).await,
        x.post(FINISH, attempt("nope")).await,
        x.link(&unknown, PW).await,
        x.link("nope", PW).await,
        x.post(COMPLETE, json!({ "ssoTicket": unknown, "username": "Someone" })).await,
    ];
    for r in answers {
        assert_eq!(
            (r.status, r.json()),
            (
                410,
                json!({ "error": "sso_expired", "message": "This sign-in has expired; start again from Scacelith." })
            )
        );
    }
}

#[tokio::test]
async fn id_token_checks_signature_audience_issuer_nonce_expiry_and_algorithm() {
    let x = Sso::new(&[]).await;
    type Case = (&'static str, fn(&mut Signing));
    let cases: [Case; 8] = [
        ("signature", |s| s.key = 2),
        ("audience", |s| {
            s.claims.insert("aud".into(), "someone-else".into());
            s.claims.insert("azp".into(), "someone-else".into());
        }),
        ("issuer", |s| {
            s.claims.insert("iss".into(), "https://evil.example".into());
        }),
        ("nonce", |s| {
            s.claims.insert("nonce".into(), "x".repeat(43).into());
        }),
        ("expired", |s| {
            let iat = s.claims["iat"].as_i64().unwrap();
            s.claims.insert("exp".into(), (iat - 3600).into());
            s.claims.insert("iat".into(), (iat - 7200).into());
        }),
        ("future", |s| {
            let iat = s.claims["iat"].as_i64().unwrap();
            s.claims.insert("iat".into(), (iat + 3600).into());
        }),
        ("unknown key id", |s| s.kid = "nope".into()),
        ("alg none", |s| s.alg = "none"),
    ];
    for (name, tamper) in cases {
        x.idp.tamper(Some(Box::new(tamper)));
        let r = x.sign_in(claims(json!({}))).await;
        assert_eq!((r.status, r.json()["error"].clone()), (502, json!("sso_failed")), "{name}");
    }
    x.idp.tamper(None);
    assert_eq!(x.sign_in(claims(json!({}))).await.json()["needsUsername"], true);
    assert!(events_of(&x.h, "sso_failed").await.len() >= 8);
}

#[tokio::test]
async fn key_rotation_an_unknown_key_id_refreshes_the_keys() {
    let x = Sso::new(&[]).await;
    x.sign_in(claims(json!({}))).await;
    assert_eq!(x.idp.jwks_fetches(), 1);
    x.idp.rotate_key("k2");
    x.h.advance(61_000);
    let r = x.sign_in(claims(json!({ "sub": "222", "email": "other@gmail.com" }))).await;
    assert_eq!(r.json()["needsUsername"], true, "{}", r.text());
    assert_eq!(x.idp.jwks_fetches(), 2);
}

#[tokio::test]
async fn banned_accounts_closed_registration_and_addresses_google_has_not_confirmed() {
    let x = Sso::new(&[("REGISTRATION", "closed")]).await;
    let h = &x.h;
    let id = h.create_user_with("magnus", Some("magnus@gmail.com"), Some(PW), true).await;
    link_identity(h, id, "1098765", "magnus@gmail.com").await;
    ban(h, id, h.now() + 1000).await;
    let r = x.sign_in(claims(json!({}))).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("banned")));
    let r = x.sign_in(claims(json!({ "sub": "999", "email": "new@gmail.com" }))).await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("registration_closed")));
    let r = x
        .sign_in(claims(json!({ "sub": "998", "email": "unconfirmed@gmail.com", "email_verified": false })))
        .await;
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("sso_email_unverified")));
}

// ---- an account with the Google address: its password first --------------------------------------

const MODES: [&str; 2] = ["1", "0"];

fn mode(verification: &'static str) -> (&'static str, &'static str) {
    ("REQUIRE_EMAIL_VERIFICATION", verification)
}

#[tokio::test]
async fn an_account_with_the_google_address_is_linked_only_after_its_password_typed_in_the_game() {
    for verification in MODES {
        let x = Sso::new(&[mode(verification)]).await;
        let h = &x.h;
        let id = h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), false).await;
        let (_, _, r) = x.sign_in_from(VICTIM, claims(json!({}))).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let b = r.json();
        assert_eq!(sorted_keys(&b), ["expiresIn", "linkTicket", "needsPassword", "username"]);
        assert_eq!(
            (&b["needsPassword"], &b["username"], &b["expiresIn"]),
            (&json!(true), &json!("Magnus"), &json!(600))
        );
        let ticket = b["linkTicket"].as_str().unwrap().to_owned();
        assert!(is_token(&ticket, "sso_"));
        assert_eq!(google_link(h, "1098765").await, None, "nothing linked before the password");
        assert_eq!(live_sessions(h, id).await, 0);
        let required: Vec<_> =
            events_of(h, "sso_link_required").await.into_iter().map(|e| (e.user_id, e.ip)).collect();
        assert_eq!(required, [(Some(id), None)]);
        // A wrong password: 401, the ticket stays, the account's login counter counts it.
        let l = x.link_from(VICTIM, &ticket, "wrong password 1", json!({})).await;
        assert_eq!((l.status, l.json()["error"].clone()), (401, json!("invalid_credentials")));
        assert_eq!(h.auth.inner.failures.failures("l:magnus"), 1);
        let failed = events_of(h, "login_failed").await.pop().unwrap();
        assert_eq!(failed.detail.unwrap()["method"], "google_link");
        assert_eq!(google_link(h, "1098765").await, None);
        let l = x.link_from(VICTIM, &ticket, PW, json!({ "clientLabel": "Scacelith (test)" })).await;
        assert_eq!(l.status, 200, "{}", l.text());
        let lb = l.json();
        assert!(lb["token"].as_str().unwrap().starts_with("sct_"));
        assert_eq!(
            (&lb["user"]["id"], &lb["user"]["googleLinked"], &lb["user"]["emailVerified"]),
            (&json!(id), &json!(true), &json!(true))
        );
        assert_eq!(google_link(h, "1098765").await, Some(id));
        assert_eq!(h.auth.inner.failures.failures("l:magnus"), 0, "the right password resets the counter");
        let linked: Vec<_> =
            events_of(h, "sso_linked").await.into_iter().map(|e| (e.user_id, e.ip, e.detail)).collect();
        assert_eq!(
            linked,
            [(
                Some(id),
                Some(VICTIM.to_owned()),
                Some(json!({ "provider": "google", "method": "password" }))
            )]
        );
        assert_eq!(last_login_method(h).await, "google+password");
        assert_eq!(x.link(&ticket, PW).await.status, 410, "the ticket is used");
        // The next Google sign-in needs no password.
        let again = x.sign_in(claims(json!({}))).await;
        assert_eq!(again.status, 200, "{}", again.text());
        assert_eq!(again.json()["user"]["id"], json!(id));
    }
}

#[tokio::test]
async fn the_5th_wrong_password_ends_the_ticket_ten_at_once_cost_at_most_5_checks_and_login_shares_the_lockout()
 {
    for verification in MODES {
        let counter = CountingHasher::new();
        let x = Sso::with(SsoSetup {
            env: vec![mode(verification)],
            hasher: Some(counter.clone()),
            ..SsoSetup::default()
        })
        .await;
        let h = &x.h;
        h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), true).await;
        let r = x.sign_in(claims(json!({}))).await;
        let ticket = r.json()["linkTicket"].as_str().unwrap().to_owned();
        for i in 1..=4 {
            assert_eq!(x.link(&ticket, &format!("wrong password {i}")).await.status, 401, "try {i}");
        }
        let l = x.link(&ticket, "wrong password 5").await;
        assert_eq!((l.status, l.json()["error"].clone()), (410, json!("sso_expired")));
        assert_eq!(x.link(&ticket, PW).await.status, 410, "dead, even with the right password");
        assert_eq!(google_link(h, "1098765").await, None);
        // AUTH_FAILURES_PER_ACCOUNT (5) link failures: the password login waits as well.
        let l = x.post(LOGIN, json!({ "login": "magnus", "password": PW })).await;
        assert_eq!((l.status, l.json()["error"].clone()), (429, json!("too_many_attempts")));
        h.advance(2001);
        h.login("magnus", PW).await;

        let r = x.sign_in(claims(json!({}))).await;
        let ticket = r.json()["linkTicket"].as_str().unwrap().to_owned();
        let checks = counter.checks();
        let tasks: Vec<_> = (0..10)
            .map(|i| {
                let req = h
                    .call(Method::POST, LINK)
                    .json(&json!({ "linkTicket": ticket, "password": format!("parallel wrong {i}") }));
                tokio::spawn(req.send())
            })
            .collect();
        let mut statuses = Vec::new();
        for t in tasks {
            statuses.push(t.await.unwrap().status);
        }
        assert!(counter.checks() - checks <= 5, "{} password checks", counter.checks() - checks);
        // Past the ticket's tries 410, or first 429 once the failures counted reach the account's wait.
        assert!(statuses.iter().all(|s| [401, 410, 429].contains(s)), "{statuses:?}");
        assert!(statuses.iter().filter(|s| **s == 401).count() <= 4, "{statuses:?}");
        assert_eq!(x.link(&ticket, PW).await.status, 410);
        assert_eq!(google_link(h, "1098765").await, None);
    }
}

#[tokio::test]
async fn a_squatters_account_with_the_address_is_not_opened_by_the_address_owners_google() {
    for verification in MODES {
        // Registered without e-mail confirmation (or before the operator turned it on).
        let off = Sso::new(&[mode("0")]).await;
        let body =
            json!({ "username": "squatter", "email": "magnus@gmail.com", "password": "ivory rook takes e5" });
        let reg = off.post("/api/v1/auth/register", body).await;
        assert_eq!(reg.status, 201, "{}", reg.text());
        let on;
        let x = if verification == "0" {
            &off
        } else {
            on = Sso::with(SsoSetup {
                env: vec![mode(verification)],
                shared: Some(&off),
                ..SsoSetup::default()
            })
            .await;
            &on
        };
        let squatter = x.h.store.users().by_username("squatter".into()).await.unwrap().unwrap();
        let r = x.sign_in(claims(json!({}))).await;
        assert_eq!(
            (r.json()["needsPassword"].clone(), r.json()["username"].clone()),
            (json!(true), json!("squatter"))
        );
        let l = x.link(r.json()["linkTicket"].as_str().unwrap(), PW).await;
        assert_eq!((l.status, l.json()["error"].clone()), (401, json!("invalid_credentials")));
        assert_eq!(google_link(&x.h, "1098765").await, None);
        assert_eq!(live_sessions(&x.h, squatter.id).await, 0);
    }
}

#[tokio::test]
async fn with_two_step_verification_the_link_is_stored_only_once_the_code_passes() {
    for verification in MODES {
        let x = Sso::new(&[mode(verification)]).await;
        let h = &x.h;
        let id = h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), true).await;
        let secret = enable_mfa(h, "Magnus").await;
        let step = async || {
            let r = x.sign_in(claims(json!({}))).await;
            let l = x.link_from(VICTIM, r.json()["linkTicket"].as_str().unwrap(), PW, json!({})).await;
            assert_eq!(l.status, 200, "{}", l.text());
            let b = l.json();
            assert_eq!(
                (&b["mfaRequired"], b.get("token"), &b["expiresIn"]),
                (&json!(true), None, &json!(300))
            );
            assert_eq!(google_link(h, "1098765").await, None, "not before the code");
            b["mfaToken"].as_str().unwrap().to_owned()
        };
        let m = x
            .post(MFA, json!({ "mfaToken": step().await, "code": totp(&secret, h.now() - 3_600_000) }))
            .await;
        assert_eq!((m.status, m.json()["error"].clone()), (401, json!("invalid_code")));
        assert_eq!(google_link(h, "1098765").await, None);
        let late = step().await;
        h.advance(301_000);
        let m = x.post(MFA, json!({ "mfaToken": late, "code": totp(&secret, h.now()) })).await;
        assert_eq!((m.status, m.json()["error"].clone()), (401, json!("invalid_mfa_token")));
        assert_eq!(google_link(h, "1098765").await, None);
        // The address changed between the password and the code: 410, no link.
        let token = step().await;
        let elsewhere =
            UserUpdate { email: Some(Some("elsewhere@example.com".into())), ..UserUpdate::default() };
        h.store.users().update(id, elsewhere).await.unwrap();
        let m = x.post(MFA, json!({ "mfaToken": token, "code": totp(&secret, h.now()) })).await;
        assert_eq!((m.status, m.json()["error"].clone()), (410, json!("sso_expired")));
        assert_eq!(google_link(h, "1098765").await, None);
        let back = UserUpdate { email: Some(Some("magnus@gmail.com".into())), ..UserUpdate::default() };
        h.store.users().update(id, back).await.unwrap();
        h.advance(30_000);

        let token = step().await;
        let m = x.post_from(VICTIM, MFA, json!({ "mfaToken": token, "code": totp(&secret, h.now()) })).await;
        assert_eq!(m.status, 200, "{}", m.text());
        assert_eq!((&m.json()["user"]["id"], &m.json()["user"]["googleLinked"]), (&json!(id), &json!(true)));
        assert_eq!(google_link(h, "1098765").await, Some(id));
        assert_eq!(last_login_method(h).await, "google+totp");
        let linked: Vec<_> = events_of(h, "sso_linked").await.into_iter().map(|e| (e.ip, e.detail)).collect();
        assert_eq!(
            linked,
            [(Some(VICTIM.to_owned()), Some(json!({ "provider": "google", "method": "password+totp" })))]
        );
    }
}

#[tokio::test]
async fn a_password_change_between_the_password_and_the_code_ends_the_step_and_an_upgraded_hash_still_passes()
{
    for verification in MODES {
        let x = Sso::new(&[mode(verification)]).await;
        let h = &x.h;
        let id = h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), true).await;
        let secret = enable_mfa(h, "Magnus").await;
        let r = x.sign_in(claims(json!({}))).await;
        let l = x.link(r.json()["linkTicket"].as_str().unwrap(), PW).await;
        assert_eq!(l.json()["mfaRequired"], true, "{}", l.text());
        let other = UserUpdate {
            password_hash: Some(Some(h.hasher.hash("another password 12").unwrap())),
            ..Default::default()
        };
        h.store.users().update(id, other).await.unwrap();
        let m =
            x.post(MFA, json!({ "mfaToken": l.json()["mfaToken"], "code": totp(&secret, h.now()) })).await;
        assert_eq!((m.status, m.json()["error"].clone()), (401, json!("invalid_mfa_token")));
        assert_eq!(google_link(h, "1098765").await, None);
        // An outdated hash: the link's check upgrades it, and the step is bound to the new one.
        let old = UserUpdate { password_hash: Some(Some(outdated_hash(PW))), ..Default::default() };
        h.store.users().update(id, old).await.unwrap();
        let r = x.sign_in(claims(json!({}))).await;
        let l = x.link(r.json()["linkTicket"].as_str().unwrap(), PW).await;
        assert_eq!(l.json()["mfaRequired"], true, "{}", l.text());
        let stored = h.user(id).await.password_hash.unwrap();
        assert!(stored.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "upgraded: {stored}");
        let m =
            x.post(MFA, json!({ "mfaToken": l.json()["mfaToken"], "code": totp(&secret, h.now()) })).await;
        assert_eq!(m.status, 200, "{}", m.text());
        assert_eq!(google_link(h, "1098765").await, Some(id));
    }
}

#[tokio::test]
async fn an_account_that_changes_after_its_password_or_an_identity_linked_elsewhere_meanwhile_gets_no_link() {
    for verification in MODES {
        let x = Sso::new(&[mode(verification)]).await;
        let h = &x.h;
        let id = h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), true).await;
        let other = h.create_user_with("other", Some("other@example.com"), Some(PW), true).await;
        let before = h.user(id).await;
        let other_hash = h.hasher.hash("another password 12").unwrap();
        // `change` runs once the password matched, as the link ticket is used up, just before
        // the link is stored (a trigger on the writer connection).
        let race = async |change: String| {
            let r = x.sign_in(claims(json!({}))).await;
            assert_eq!(r.json()["needsPassword"], true, "{}", r.text());
            let trigger = format!(
                "CREATE TEMP TRIGGER race AFTER UPDATE OF consumed_at ON main.tokens
                 WHEN NEW.kind = 'sso_link' AND NEW.consumed_at IS NOT NULL
                 BEGIN {change}; END"
            );
            h.store.write(move |db| db.exec(&trigger, [])).await.unwrap();
            let l = x.link(r.json()["linkTicket"].as_str().unwrap(), PW).await;
            h.store.write(|db| db.exec("DROP TRIGGER temp.race", [])).await.unwrap();
            l
        };
        let changes = [
            (
                "e-mail",
                "UPDATE main.users SET email = 'new@example.com', email_normalized = 'new@example.com'"
                    .to_owned(),
            ),
            ("password", format!("UPDATE main.users SET password_hash = '{other_hash}'")),
            ("status", "UPDATE main.users SET status = 'deleted'".to_owned()),
            ("two-step verification switched on", "UPDATE main.users SET mfa_enabled = 1".to_owned()),
        ];
        for (name, change) in changes {
            let l = race(format!("{change} WHERE id = {id}")).await;
            assert_eq!((l.status, l.json()["error"].clone()), (410, json!("sso_expired")), "{name}");
            assert_eq!(google_link(h, "1098765").await, None, "{name}");
            assert_eq!(live_sessions(h, id).await, 0, "{name}");
            let restore = UserUpdate {
                email: Some(before.email.clone()),
                password_hash: Some(before.password_hash.clone()),
                status: Some(crate::store::UserStatus::Active),
                mfa_enabled: Some(false),
                ..UserUpdate::default()
            };
            h.store.users().update(id, restore).await.unwrap();
        }
        let l = race(format!(
            "INSERT INTO main.sso_identities (provider, subject, user_id, email, created_at) \
             VALUES ('google', '1098765', {other}, 'magnus@gmail.com', 0)"
        ))
        .await;
        assert_eq!((l.status, l.json()["error"].clone()), (409, json!("sso_already_linked")));
        assert_eq!(google_link(h, "1098765").await, Some(other));
        assert_eq!(live_sessions(h, id).await, 0);
    }
}

#[tokio::test]
async fn an_account_without_a_usable_password_is_not_named_409_sso_account_exists() {
    for verification in MODES {
        let x = Sso::new(&[mode(verification)]).await;
        let h = &x.h;
        h.create_user_with("NoPassword", Some("magnus@gmail.com"), None, true).await;
        h.store
            .users()
            .create(NewUser {
                username: "bench0001".into(),
                email: Some("bench0001@bench.invalid".into()),
                password_hash: Some("!bench-account-no-password".into()),
                email_verified: true,
                accept_challenges: true,
                created_at: h.now(),
            })
            .await
            .unwrap();
        for c in [claims(json!({})), claims(json!({ "sub": "77", "email": "bench0001@bench.invalid" }))] {
            let sub = c["sub"].as_str().unwrap().to_owned();
            let r = x.sign_in(c).await;
            assert_eq!((r.status, r.json()["error"].clone()), (409, json!("sso_account_exists")), "{sub}");
            let text = r.text().to_lowercase();
            assert!(!text.contains("nopassword") && !text.contains("bench0001"), "{text}");
            assert_eq!(google_link(h, &sub).await, None);
        }
    }
}

#[tokio::test]
async fn a_banned_account_gets_no_link_and_403_banned_only_after_the_right_password() {
    for verification in MODES {
        let x = Sso::new(&[mode(verification)]).await;
        let h = &x.h;
        let id = h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), true).await;
        let until = h.now() + 3_600_000;
        ban(h, id, until).await;
        let r = x.sign_in(claims(json!({}))).await;
        let ticket = r.json()["linkTicket"].as_str().unwrap().to_owned();
        let l = x.link(&ticket, "wrong password 1").await;
        assert_eq!((l.status, l.json()["error"].clone()), (401, json!("invalid_credentials")));
        let l = x.link(&ticket, PW).await;
        assert_eq!(
            (l.status, l.json()["error"].clone(), l.json()["until"].clone()),
            (403, json!("banned"), json!(until))
        );
        assert_eq!(google_link(h, "1098765").await, None);
    }
}

#[tokio::test]
async fn the_accounts_wait_and_a_login_proof_of_work_wave_apply_to_the_link_step_and_take_none_of_its_tries()
{
    for verification in MODES {
        // The sixth failed login of a minute turns the login proof of work on.
        let x = Sso::new(&[mode(verification), ("POW_LOGIN_BITS", "4"), ("POW_LOGIN_TRIGGER_PER_MIN", "6")])
            .await;
        let h = &x.h;
        let id = h.create_user_with("Magnus", Some("magnus@gmail.com"), Some(PW), true).await;
        for i in 1..=5 {
            x.post(LOGIN, json!({ "login": "magnus", "password": format!("wrong password {i}") })).await;
        }
        let r = x.sign_in(claims(json!({}))).await;
        let ticket = r.json()["linkTicket"].as_str().unwrap().to_owned();
        for _ in 0..6 {
            let l = x.link(&ticket, PW).await;
            assert_eq!((l.status, l.json()["error"].clone()), (429, json!("too_many_attempts")));
        }
        let row = h.store.tokens().get("sso_link".into(), sha256_hex(&ticket)).await.unwrap().unwrap();
        assert_eq!(row.data.unwrap()["tries"], 0);
        h.advance(2001);
        assert_eq!(x.link(&ticket, PW).await.status, 200);
        assert_eq!(google_link(h, "1098765").await, Some(id));

        let v = h.create_user_with("Hikaru", Some("hikaru@gmail.com"), Some(PW), true).await;
        let r = x.sign_in(claims(json!({ "sub": "2002", "email": "hikaru@gmail.com" }))).await;
        let ticket = r.json()["linkTicket"].as_str().unwrap().to_owned();
        assert_eq!(
            x.post(LOGIN, json!({ "login": "nobody", "password": "wrong password" })).await.status,
            401
        );
        assert!(h.auth.login_pow_active(), "the wave turned the proof of work on");
        // As the game does: each password first without a proof, then again with the proof solved.
        let with_pow = async |password: &str| {
            let l = x.link(&ticket, password).await;
            let b = l.json();
            assert_eq!((l.status, &b["error"], &b["pow"]["bits"]), (428, &json!("pow_required"), &json!(4)));
            let challenge = b["pow"]["challenge"].as_str().unwrap();
            let pow = json!({ "challenge": challenge, "nonce": solve_pow(challenge, 4) });
            x.link_from(DEFAULT_IP, &ticket, password, json!({ "pow": pow })).await
        };
        for i in 1..=4 {
            assert_eq!(with_pow(&format!("wrong password {i}")).await.status, 401, "try {i}");
        }
        let l = with_pow(PW).await;
        assert_eq!(l.status, 200, "{}", l.text());
        assert_eq!(google_link(h, "2002").await, Some(v));
    }
}

#[tokio::test]
async fn an_address_confirmed_for_someone_elses_account_is_not_linked_without_the_password() {
    let x = Sso::new(&[mode("1")]).await;
    let h = &x.h;
    let token_of_link = async |path: &str| {
        let mail = h
            .sent()
            .await
            .into_iter()
            .rev()
            .find(|m| link_in(&m.text).is_some_and(|l| l.contains(&format!("{path}?"))))
            .expect("a link mail");
        token_of(&link_in(&mail.text).unwrap()).unwrap()
    };
    // A signup with the victim's address: the victim opens the link mailed to her.
    let signup =
        json!({ "username": "deputy1", "email": "magnus@gmail.com", "password": "deputy password 1" });
    assert_eq!(x.post("/api/v1/auth/register", signup).await.status, 202);
    let token = token_of_link("/verify-email").await;
    assert_eq!(
        h.call(Method::POST, "/verify-email").body(FORM, format!("token={token}")).send().await.status,
        200
    );
    let r = x.sign_in(claims(json!({}))).await;
    assert_eq!(
        (r.json()["needsPassword"].clone(), r.json()["username"].clone()),
        (json!(true), json!("deputy1"))
    );
    assert_eq!(google_link(h, "1098765").await, None);
    // Another account's change to the victim's address, confirmed by the victim.
    h.create_user_with("deputy2", Some("deputy2@example.com"), Some("deputy password 2"), true).await;
    let session = h.token("deputy2", "deputy password 2").await;
    let ch = h
        .post_as(
            &session,
            "/api/v1/account/email",
            json!({ "newEmail": "victim2@gmail.com", "password": "deputy password 2" }),
        )
        .await;
    assert_eq!(ch.status, 202, "{}", ch.text());
    let token = token_of_link("/confirm-email-change").await;
    let done =
        h.call(Method::POST, "/confirm-email-change").body(FORM, format!("token={token}")).send().await;
    assert_eq!(done.status, 200);
    let r = x.sign_in(claims(json!({ "sub": "2002", "email": "victim2@gmail.com" }))).await;
    assert_eq!(
        (r.json()["needsPassword"].clone(), r.json()["username"].clone()),
        (json!(true), json!("deputy2"))
    );
    assert_eq!(google_link(h, "2002").await, None);
}

#[tokio::test]
async fn the_link_step_counts_its_tries_in_the_store_and_the_next_sign_in_finds_the_link() {
    let x = Sso::new(&[mode("0"), ("AUTH_FAILURES_PER_ACCOUNT", "20")]).await;
    let h = &x.h;
    let reg = x
        .post(
            "/api/v1/auth/register",
            json!({ "username": "Magnus", "email": "magnus@gmail.com", "password": PW }),
        )
        .await;
    assert_eq!(reg.status, 201);
    let r = x.sign_in(claims(json!({}))).await;
    assert_eq!(r.json()["needsPassword"], true, "{}", r.text());
    let ticket = r.json()["linkTicket"].as_str().unwrap().to_owned();
    for i in 0..2 {
        assert_eq!(x.link(&ticket, &format!("wrong password {i}")).await.status, 401);
    }
    let row = |h: &Harness| h.store.tokens().get("sso_link".into(), sha256_hex(&ticket));
    assert_eq!(row(h).await.unwrap().unwrap().data.unwrap()["tries"], 2);
    let r = x.link(&ticket, PW).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["user"]["id"].as_u64().unwrap() as UserId;
    assert_eq!(google_link(h, "1098765").await, Some(id));
    assert!(row(h).await.unwrap().unwrap().consumed_at.is_some());
    let r = x.sign_in(claims(json!({}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["user"]["id"], json!(id));
}

// ---- security notices: a Google-made account, Google added to an account --------------------------

fn is_notice(m: &OutgoingMail) -> bool {
    let created =
        m.subject.starts_with("A ") && m.subject.ends_with(" account was created with your Google account");
    let added = m.subject.starts_with("Google sign-in was added to your ") && m.subject.ends_with(" account");
    created || added
}

async fn notices(h: &Harness) -> Vec<OutgoingMail> {
    h.sent().await.into_iter().filter(is_notice).collect()
}

async fn notices_to(h: &Harness, to: &str) -> Vec<String> {
    notices(h).await.into_iter().filter(|m| m.to == to).map(|m| m.subject).collect()
}

#[tokio::test]
async fn notices_when_google_creates_an_account_or_is_added_after_the_password_none_on_a_plain_sign_in() {
    let x = Sso::new(&[]).await;
    let h = &x.h;
    let name = h.config.server_name.clone();
    let a = x.start_at(PORT, VICTIM).await;
    let q = x.idp.authorize(&a.auth_url, claims(json!({})));
    let r = x.finish_from(VICTIM, &a, &q, json!({})).await;
    assert!(notices_to(h, "magnus@gmail.com").await.is_empty(), "nothing before the account exists");
    let ticket = r.json()["ssoTicket"].as_str().unwrap().to_owned();
    let c = x.post_from(VICTIM, COMPLETE, json!({ "ssoTicket": ticket, "username": "MagnusH" })).await;
    assert_eq!(c.status, 200, "{}", c.text());
    assert_eq!(
        notices_to(h, "magnus@gmail.com").await,
        [format!("A {name} account was created with your Google account")]
    );
    let created = notices(h).await[0].text.clone();
    for part in [
        "Hello MagnusH".to_owned(),
        "\"MagnusH\"".into(),
        utc_string(h.now()),
        "\"Sign out everywhere\"".into(),
        format!("administrator of {name}"),
    ] {
        assert!(created.contains(&part), "{part}");
    }
    for _ in 0..2 {
        assert_eq!(x.sign_in(claims(json!({}))).await.status, 200);
    }
    assert_eq!(notices_to(h, "magnus@gmail.com").await.len(), 1, "a plain Google sign-in sends nothing");

    // Google added to a password account: once its password passes.
    h.create_user_with("Judit", Some("judit@gmail.com"), Some(PW), true).await;
    let (_, _, j) =
        x.sign_in_from(VICTIM, claims(json!({ "sub": "2001", "email": "judit@gmail.com" }))).await;
    assert_eq!(j.json()["needsPassword"], true, "{}", j.text());
    let link_ticket = j.json()["linkTicket"].as_str().unwrap().to_owned();
    assert_eq!(x.link(&link_ticket, "wrong password 1").await.status, 401);
    assert!(notices_to(h, "judit@gmail.com").await.is_empty(), "not for a wrong password");
    let l = x.link_from(VICTIM, &link_ticket, PW, json!({})).await;
    assert_eq!(l.status, 200, "{}", l.text());
    assert_eq!(
        notices_to(h, "judit@gmail.com").await,
        [format!("Google sign-in was added to your {name} account")]
    );
    let added = notices(h).await.pop().unwrap().text;
    for part in [
        "Hello Judit".to_owned(),
        "\"Judit\"".into(),
        utc_string(h.now()),
        "\"Forgot password\"".into(),
        "\"Sign out everywhere\"".into(),
        format!("administrator of {name}"),
    ] {
        assert!(added.contains(&part), "{part}");
    }
    assert_eq!(x.sign_in(claims(json!({ "sub": "2001", "email": "judit@gmail.com" }))).await.status, 200);
    assert_eq!(notices_to(h, "judit@gmail.com").await.len(), 1, "the next Google sign-in sends nothing");

    // With two-step verification: only once the code passes.
    h.create_user_with("Hou", Some("hou@gmail.com"), Some(PW), true).await;
    let secret = enable_mfa(h, "Hou").await;
    let hr = x.sign_in(claims(json!({ "sub": "2002", "email": "hou@gmail.com" }))).await;
    let step = x.link(hr.json()["linkTicket"].as_str().unwrap(), PW).await;
    assert_eq!(step.json()["mfaRequired"], true, "{}", step.text());
    let mfa_token = step.json()["mfaToken"].as_str().unwrap().to_owned();
    let wrong =
        x.post(MFA, json!({ "mfaToken": mfa_token, "code": totp(&secret, h.now() - 3_600_000) })).await;
    assert_eq!(wrong.status, 401);
    assert!(notices_to(h, "hou@gmail.com").await.is_empty(), "not before the code");
    let m = x.post(MFA, json!({ "mfaToken": mfa_token, "code": totp(&secret, h.now()) })).await;
    assert_eq!(m.status, 200, "{}", m.text());
    assert_eq!(notices_to(h, "hou@gmail.com").await.len(), 1);

    // Never a code, state, ticket, token, password, PKCE verifier, link or IP address.
    let secrets = [
        q.code.clone(),
        q.state.clone(),
        a.attempt_id.clone(),
        a.verifier.clone(),
        ticket,
        c.json()["token"].as_str().unwrap().to_owned(),
        link_ticket,
        l.json()["token"].as_str().unwrap().to_owned(),
        mfa_token,
        m.json()["token"].as_str().unwrap().to_owned(),
        PW.to_owned(),
        VICTIM.to_owned(),
        "198.51.100".to_owned(),
    ];
    let all = notices(h).await;
    assert_eq!(all.len(), 3);
    for mail in &all {
        for v in &secrets {
            assert!(
                !mail.text.contains(v.as_str()) && !mail.subject.contains(v.as_str()),
                "{}: {v}",
                mail.subject
            );
        }
        for marker in ["sct_", "sso_", "mfa_", "http://", "https://"] {
            assert!(!mail.text.contains(marker), "{}: {marker}", mail.subject);
        }
        let dotted_quad = mail.text.split(|c: char| !(c.is_ascii_digit() || c == '.')).any(|w| {
            let parts: Vec<&str> = w.split('.').collect();
            parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3)
        });
        assert!(!dotted_quad, "{}: an IP address", mail.subject);
    }
}

#[tokio::test]
async fn notices_an_smtp_failure_never_fails_the_google_sign_in_and_with_no_transport_nothing_is_sent() {
    // An SMTP server that drops every connection.
    let smtp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = smtp.local_addr().unwrap().port().to_string();
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = connections.clone();
    let dropper = tokio::spawn(async move {
        while let Ok((stream, _)) = smtp.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    let count = || connections.load(std::sync::atomic::Ordering::SeqCst);
    for transport in ["smtp", "none"] {
        let env =
            vec![("MAIL_TRANSPORT", transport), ("SMTP_HOST", "127.0.0.1"), ("SMTP_PORT", port.as_str())];
        let x = Sso::with(SsoSetup { env, real_mailer: true, ..SsoSetup::default() }).await;
        let h = &x.h;
        let logs = capture_logs(Level::Warn);
        let connected = count();
        let r = x.sign_in(claims(json!({}))).await;
        let c = x.post(COMPLETE, json!({ "ssoTicket": r.json()["ssoTicket"], "username": "MagnusH" })).await;
        assert_eq!(c.status, 200, "{transport}: {}", c.text());
        h.create_user_with("Judit", Some("judit@gmail.com"), Some(PW), true).await;
        let j = x.sign_in(claims(json!({ "sub": "2001", "email": "judit@gmail.com" }))).await;
        let l = x.link(j.json()["linkTicket"].as_str().unwrap(), PW).await;
        assert_eq!(l.status, 200, "{transport}: {}", l.text());
        h.mailer.idle().await;
        let failed: Vec<String> =
            logs.lines().into_iter().filter(|l| l.contains("e-mail not sent")).collect();
        if transport == "smtp" {
            assert!(count() >= connected + 2, "both notices went to the SMTP server");
            for template in ["ssoAccountCreated", "ssoLinked"] {
                assert!(failed.iter().any(|l| l.contains(template)), "{template}: {failed:?}");
            }
        } else {
            assert_eq!(count(), connected, "MAIL_TRANSPORT=none");
            assert!(failed.is_empty(), "{failed:?}");
        }
    }
    dropper.abort();
}
