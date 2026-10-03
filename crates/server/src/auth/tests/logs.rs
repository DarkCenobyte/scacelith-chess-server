//! Every flow of the module at the debug log level: no password, token, TOTP secret, recovery
//! code, PKCE verifier, Google code or state, e-mail address or full client address reaches the
//! logs, and no secret reaches the stored security events (auth.logs.test.js).

use http::Method;
use serde_json::{Value, json};

use super::fake_oidc::FakeOidc;
use super::{Harness, START_MS, Setup, link_in, token_of};
use crate::auth::oidc::{form_urlencode, pkce_challenge};
use crate::clock::ManualClock;
use crate::http::testing::TestResponse;
use crate::log::{Level, capture_logs};
use crate::security::keys::random_token;
use crate::security::pow::solve_pow;
use crate::security::totp::{base32_decode, totp};

const CLIENT_ID: &str = "cid.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "GOCSPX-log-test";
/// The client of every request: only its /24 may appear in the logs.
const CLIENT: &str = "198.51.100.23";
const FORM: &str = "application/x-www-form-urlencoded";

/// The secrets each step of the flow produced.
#[derive(Default)]
struct Secrets(Vec<String>);

impl Secrets {
    /// Keeps `x` (every step produces its secret) and returns it.
    fn keep(&mut self, x: &str) -> String {
        assert!(x.len() >= 6, "every step of the flow produced its secret: {x:?}");
        self.0.push(x.to_owned());
        x.to_owned()
    }

    /// Keeps the text of `v`.
    fn keep_json(&mut self, v: &Value) -> String {
        self.keep(v.as_str().unwrap_or_default())
    }
}

struct Flow {
    h: Harness,
    idp: FakeOidc,
}

impl Flow {
    async fn request(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> TestResponse {
        let mut req = self.h.call_from(CLIENT, method, path);
        if let Some(token) = token {
            req = req.bearer(token);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await
    }

    async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.request(Method::POST, path, None, Some(body)).await
    }

    async fn post_as(&self, token: &str, path: &str, body: Value) -> TestResponse {
        self.request(Method::POST, path, Some(token), Some(body)).await
    }

    async fn form(&self, path: &str, pairs: &[(&str, &str)]) -> TestResponse {
        self.h.call_from(CLIENT, Method::POST, path).body(FORM, form_urlencode(pairs)).send().await
    }

    async fn last_link_token(&self) -> String {
        let mail = self.h.last_mail().await;
        token_of(&link_in(&mail.text).expect("a link")).expect("a token")
    }
}

fn pow_of(r: &TestResponse) -> Value {
    assert_eq!(r.status, 428, "{}", r.text());
    let challenge = r.json()["pow"]["challenge"].as_str().unwrap().to_owned();
    json!({ "nonce": solve_pow(&challenge, 4), "challenge": challenge })
}

#[tokio::test]
async fn no_credential_token_code_or_e_mail_address_appears_in_the_logs() {
    let clock = ManualClock::new(1_000_000.0, START_MS);
    let idp = FakeOidc::start(CLIENT_ID, CLIENT_SECRET, clock.clone()).await;
    let env = [
        ("POW_REGISTER_BITS", "4"),
        ("POW_LOGIN_BITS", "4"),
        ("POW_LOGIN_TRIGGER_PER_MIN", "3"),
        ("SSO_GOOGLE_ENABLED", "1"),
        ("GOOGLE_CLIENT_ID", CLIENT_ID),
        ("GOOGLE_CLIENT_SECRET", CLIENT_SECRET),
    ];
    let h = Harness::build(Setup {
        env: env.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
        oidc: Some(idp.options()),
        clock: Some(clock),
        ..Setup::default()
    })
    .await;
    let f = Flow { h, idp };
    let h = &f.h;
    let logs = capture_logs(Level::Debug);
    let mut s = Secrets::default();
    let pw = s.keep("Sup3r secret pass phrase");
    let new = s.keep("An0ther secret pass phrase");
    let third = s.keep("Third secret pass phrase");
    for x in [
        CLIENT_SECRET,
        "alice@example.com",
        "ALICE@example.com",
        "ghost@example.com",
        "magnus@gmail.com",
        CLIENT,
    ] {
        s.keep(x);
    }

    // Registration with proof of work, e-mail confirmation.
    let register = |pow: Option<Value>| {
        let mut body = json!({ "username": "alice", "email": "ALICE@example.com", "password": pw });
        if let Some(pow) = pow {
            body["pow"] = pow;
        }
        f.post("/api/v1/auth/register", body)
    };
    let r = register(None).await;
    assert_eq!(register(Some(pow_of(&r))).await.status, 202);
    let verify = s.keep(&f.last_link_token().await);
    assert_eq!(f.form("/verify-email", &[("token", &verify)]).await.status, 200);

    // Failed logins (they turn the login proof of work on), a login, two-step enrolment.
    for i in 0..3 {
        let guess = s.keep(&format!("wrong guess {i}!"));
        f.post("/api/v1/auth/login", json!({ "login": "alice", "password": guess })).await;
    }
    let r = f.post("/api/v1/auth/login", json!({ "login": "alice", "password": pw })).await;
    let r =
        f.post("/api/v1/auth/login", json!({ "login": "alice", "password": pw, "pow": pow_of(&r) })).await;
    let session = s.keep_json(&r.json()["token"]);
    h.advance(5 * 60_000 + 1);
    let r = f.post_as(&session, "/api/v1/account/mfa/totp/setup", json!({ "password": pw })).await;
    let encoded = s.keep_json(&r.json()["secret"]);
    s.keep_json(&r.json()["uri"]);
    let secret = base32_decode(&encoded).unwrap();
    let code = s.keep(&totp(&secret, h.now()));
    let r = f.post_as(&session, "/api/v1/account/mfa/totp/enable", json!({ "code": code })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let codes: Vec<String> = r.json()["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    for c in &codes {
        s.keep(c);
        s.keep(&c.replace('-', ""));
    }
    h.advance(30_000);

    // Logins with a TOTP code and a recovery code, a wrong code.
    let r = f.post("/api/v1/auth/login", json!({ "login": "alice", "password": pw })).await;
    let mfa_token = s.keep_json(&r.json()["mfaToken"]);
    let wrong = s.keep("000111");
    f.post("/api/v1/auth/login/mfa", json!({ "mfaToken": mfa_token, "code": wrong })).await;
    let code = s.keep(&totp(&secret, h.now()));
    let r = f.post("/api/v1/auth/login/mfa", json!({ "mfaToken": mfa_token, "code": code })).await;
    s.keep_json(&r.json()["token"]);
    let r = f.post("/api/v1/auth/login", json!({ "login": "alice", "password": pw })).await;
    let mfa_token = s.keep_json(&r.json()["mfaToken"]);
    let r =
        f.post("/api/v1/auth/login/mfa", json!({ "mfaToken": mfa_token, "recoveryCode": codes[0] })).await;
    s.keep_json(&r.json()["token"]);
    h.advance(30_000);
    let body = json!({ "password": pw, "code": totp(&secret, h.now()) });
    let r = f.post_as(&session, "/api/v1/account/mfa/recovery-codes", body).await;
    for c in r.json()["recoveryCodes"].as_array().unwrap() {
        s.keep_json(c);
    }

    // Password reset, change; sessions; logout.
    f.post("/api/v1/auth/password/forgot", json!({ "email": "alice@example.com" })).await;
    f.post("/api/v1/auth/password/forgot", json!({ "email": "ghost@example.com" })).await;
    let reset = s.keep(&f.last_link_token().await);
    f.request(Method::GET, &format!("/reset-password?token={reset}"), None, None).await;
    let r = f
        .form("/reset-password", &[("token", &reset), ("newPassword", &new), ("confirmPassword", &new)])
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = f.post("/api/v1/auth/login", json!({ "login": "alice", "password": new })).await;
    h.advance(30_000);
    let mfa_token = s.keep_json(&r.json()["mfaToken"]);
    let r = f
        .post("/api/v1/auth/login/mfa", json!({ "mfaToken": mfa_token, "code": totp(&secret, h.now()) }))
        .await;
    let s2 = s.keep_json(&r.json()["token"]);
    let change = json!({ "currentPassword": new, "newPassword": third });
    assert_eq!(f.post_as(&s2, "/api/v1/account/password", change).await.status, 200);
    f.request(Method::GET, "/api/v1/auth/sessions", Some(&s2), None).await;
    let forged = s.keep(&format!("sct_{}", "Q".repeat(43)));
    assert_eq!(f.request(Method::GET, "/api/v1/account/me", Some(&forged), None).await.status, 401);
    f.request(Method::POST, "/api/v1/auth/logout", Some(&s2), None).await;

    // Google sign-in, first time: start, the redirect to the game's listener, finish, complete.
    let mut sso = async |claims: Value| {
        let verifier = s.keep(&random_token(""));
        let start = json!({ "codeChallenge": pkce_challenge(&verifier), "redirectPort": 50123 });
        let a = f.post("/api/v1/auth/sso/google/start", start).await.json();
        s.keep_json(&a["attemptId"]);
        s.keep_json(&a["state"]);
        let q = f.idp.authorize(a["authUrl"].as_str().unwrap(), claims);
        let body = json!({ "attemptId": a["attemptId"], "codeVerifier": verifier, "state": q.state, "code": s.keep(&q.code), "iss": q.iss });
        f.post("/api/v1/auth/sso/google/finish", body).await.json()
    };
    let r = sso(json!({ "sub": "42", "email": "magnus@gmail.com", "email_verified": true })).await;
    let ticket = r["ssoTicket"].as_str().unwrap().to_owned();
    let r = f.post("/api/v1/auth/sso/complete", json!({ "ssoTicket": ticket, "username": "magnus" })).await;
    let token = r.json()["token"].clone();
    // Google sign-in with the address of alice's account: her password (a wrong one first), then her code.
    let r = sso(json!({ "sub": "43", "email": "alice@example.com", "email_verified": true })).await;
    s.keep(&ticket);
    s.keep_json(&token);
    let link_ticket = s.keep_json(&r["linkTicket"]);
    let guess = s.keep("wrong link guess!");
    f.post("/api/v1/auth/sso/google/link", json!({ "linkTicket": link_ticket, "password": guess })).await;
    let r =
        f.post("/api/v1/auth/sso/google/link", json!({ "linkTicket": link_ticket, "password": third })).await;
    h.advance(30_000);
    let mfa_token = s.keep_json(&r.json()["mfaToken"]);
    let r = f
        .post("/api/v1/auth/login/mfa", json!({ "mfaToken": mfa_token, "code": totp(&secret, h.now()) }))
        .await;
    s.keep_json(&r.json()["token"]);
    for call in f.idp.token_calls() {
        s.keep(&call["code_verifier"]);
    }

    let stored = format!("{:?}", h.events().await);
    let logged = logs.lines().concat();
    drop(logs);
    for kind in ["login_failed", "mfa_enabled", "password_reset", "sso_account_created", "sso_linked"] {
        assert!(logged.contains(&format!("\"{kind}\"")), "security event {kind} is logged");
    }
    assert!(logged.contains("\"route\":\"POST /api/v1/auth/login\""), "access log at debug level");
    for x in &s.0 {
        assert!(!logged.contains(x.as_str()), "leaked into the logs: {}...", &x[..x.len().min(12)]);
    }
    assert!(logged.contains("\"198.51.100.0/24\""), "client addresses are truncated in the logs");
    // The stored security events carry no secret either.
    for x in s.0.iter().filter(|x| !x.contains('@') && x.as_str() != CLIENT) {
        assert!(!stored.contains(x.as_str()), "leaked into security events: {}...", &x[..x.len().min(12)]);
    }
}
