//! Tests of the auth service and of its routes and pages, ported from the Node suites
//! (`auth.*.test.js`, `account.export.test.js`). Requests go through the API pipeline in process
//! ([`TestApi`]) on an in-memory store, a manual clock, a mailer that keeps its messages and a
//! cheap Argon2id hasher.

mod login;
mod mfa;
mod register;
mod sessions;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::Method;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{Auth, AuthDeps, OidcOptions};
use crate::clock::{Clock, ManualClock, SharedClock};
use crate::config::{Config, test_config};
use crate::events::SessionEvents;
use crate::http::pages::{self, PageDeps};
use crate::http::routes::auth::AuthRouteDeps;
use crate::http::routes::{self, AuthGroups, account::AccountRouteDeps, account_export::ExportRouteDeps};
use crate::http::testing::{TestApi, TestRequest, TestResponse};
use crate::http::{Api, Router};
use crate::ids::UserId;
use crate::log::Logger;
use crate::mail::{CustomTransport, Mailer, MailerOptions, OutgoingMail};
use crate::security::password::{Argon2Hasher, Argon2Params, HashFailure, PasswordHasher, Verified};
use crate::store::{GameSummary, NewUser, SecurityEvent, Store, StoreOptions, User};

/// The password of the test accounts.
pub(crate) const PW: &str = "correct horse battery";
/// Another valid password.
pub(crate) const NEW_PW: &str = "a brand new passphrase";
/// A day in milliseconds.
pub(crate) const DAY_MS: i64 = 86_400_000;
/// The wall clock at the start of a test: 2026-09-28 12:00:00 UTC.
pub(crate) const START_MS: i64 = 1_790_596_800_000;

/// The settings of every test server: limits high enough that only the tests of a limit meet
/// it, e-mail confirmation on, a public host for the links.
const TEST_DEFAULTS: [(&str, &str); 14] = [
    ("AUTH_RATE_PER_IP", "10000"),
    ("HTTP_RATE_PER_IP", "100000"),
    ("AUTH_FAILURES_PER_ACCOUNT", "5"),
    ("AUTH_REGISTER_PER_HOUR", "10000"),
    ("AUTH_MAIL_PER_HOUR", "10000"),
    ("AUTH_FORGOT_PER_HOUR", "10000"),
    ("AUTH_FORGOT_PER_DAY", "10000"),
    ("AUTH_RESET_PER_HOUR", "10000"),
    ("AUTH_MFA_PER_ACCOUNT", "10000"),
    ("AUTH_REAUTH_PER_USER", "10000"),
    ("USER_RATE_PER_MIN", "100000"),
    ("REQUIRE_EMAIL_VERIFICATION", "1"),
    ("SERVER_PUBLIC_HOST", "chess.example.org"),
    ("API_PORT", "8443"),
];

/// The revocations the service announced: `(user, token hashes in hex)`.
#[derive(Default)]
pub(crate) struct Revocations(Mutex<Vec<(UserId, Option<Vec<String>>)>>);

impl Revocations {
    /// Every call so far.
    pub(crate) fn calls(&self) -> Vec<(UserId, Option<Vec<String>>)> {
        self.0.lock().clone()
    }
}

impl SessionEvents for Revocations {
    fn sessions_revoked(&self, user: UserId, token_hashes: Option<Vec<[u8; 32]>>) {
        self.0.lock().push((user, token_hashes.map(|v| v.iter().map(hex::encode).collect())));
    }
}

/// What differs from the default test server.
#[derive(Default)]
pub(crate) struct Setup {
    /// Configuration keys over [`TEST_DEFAULTS`].
    pub env: Vec<(&'static str, String)>,
    /// The Google sign-in endpoints (a fake provider).
    pub oidc: Option<OidcOptions>,
    /// Replaces the cheap Argon2id hasher.
    pub hasher: Option<Arc<dyn PasswordHasher>>,
    /// The clock (shared by two servers of one test).
    pub clock: Option<Arc<ManualClock>>,
}

impl Setup {
    /// A setup with these configuration keys.
    pub(crate) fn env(pairs: &[(&'static str, &str)]) -> Setup {
        Setup { env: pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(), ..Setup::default() }
    }
}

/// The summary of a game in the export (the real one belongs to the routes module).
fn test_history_summary(g: &GameSummary, user: UserId) -> Value {
    json!({ "id": g.id, "category": g.category, "color": if g.white_id == user { "white" } else { "black" } })
}

/// A test server: the auth service, its routes and pages, and what they talk to.
pub(crate) struct Harness {
    pub config: Arc<Config>,
    pub clock: Arc<ManualClock>,
    pub store: Store,
    pub auth: Auth,
    pub api: TestApi,
    pub mailer: Mailer,
    pub revoked: Arc<Revocations>,
    pub hasher: Arc<dyn PasswordHasher>,
    mails: Arc<Mutex<Vec<OutgoingMail>>>,
}

/// The cheap hasher of the tests.
pub(crate) fn test_hasher() -> Arc<dyn PasswordHasher> {
    Arc::new(Argon2Hasher::new(Argon2Params { memory_kib: 64, passes: 1, lanes: 1, ..Argon2Params::DEFAULT }))
}

/// A hasher that counts its hashes and can run a hook once, before its next hash (to land a
/// change while a request hashes).
pub(crate) struct CountingHasher {
    inner: Arc<dyn PasswordHasher>,
    hashes: AtomicUsize,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl CountingHasher {
    /// Counts the hashes of the cheap test hasher.
    pub(crate) fn new() -> Arc<CountingHasher> {
        Arc::new(CountingHasher { inner: test_hasher(), hashes: AtomicUsize::new(0), hook: Mutex::new(None) })
    }

    /// New hashes so far.
    pub(crate) fn hashes(&self) -> usize {
        self.hashes.load(Ordering::SeqCst)
    }

    /// Runs `hook` on the blocking thread of the next hash, before it.
    pub(crate) fn before_next_hash(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock() = Some(Box::new(hook));
    }
}

impl PasswordHasher for CountingHasher {
    fn algorithm(&self) -> &'static str {
        self.inner.algorithm()
    }

    fn hash(&self, password: &str) -> Result<String, HashFailure> {
        self.hashes.fetch_add(1, Ordering::SeqCst);
        let hook = self.hook.lock().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.hash(password)
    }

    fn verify(&self, stored: &str, password: &str) -> Result<Verified, HashFailure> {
        self.inner.verify(stored, password)
    }

    fn verify_dummy(&self, password: &str) -> Result<(), HashFailure> {
        self.inner.verify_dummy(password)
    }
}

impl Harness {
    /// The default test server.
    pub(crate) async fn new() -> Harness {
        Harness::build(Setup::default()).await
    }

    /// A test server with these configuration keys.
    pub(crate) async fn with_env(pairs: &[(&'static str, &str)]) -> Harness {
        Harness::build(Setup::env(pairs)).await
    }

    /// A test server.
    pub(crate) async fn build(setup: Setup) -> Harness {
        let mut pairs: Vec<(&str, &str)> = TEST_DEFAULTS.to_vec();
        pairs.extend(setup.env.iter().map(|(k, v)| (*k, v.as_str())));
        let config = Arc::new(test_config(&pairs).expect("a valid test configuration"));
        let clock = setup.clock.unwrap_or_else(|| ManualClock::new(1_000_000.0, START_MS));
        let shared: SharedClock = clock.clone() as Arc<dyn Clock>;
        let store = Store::open(
            &config,
            StoreOptions {
                path: Some(":memory:".into()),
                clock: Some(shared.clone()),
                ..StoreOptions::default()
            },
        )
        .await
        .expect("an in-memory store");
        store.migrate().await.expect("migrations");

        let mails = Arc::new(Mutex::new(Vec::new()));
        let kept = mails.clone();
        let transport: CustomTransport = Arc::new(move |m: OutgoingMail| {
            kept.lock().push(m);
            Box::pin(async { Ok(()) })
        });
        let mailer = Mailer::with_options(
            &config,
            Logger::root().child("mail"),
            MailerOptions { transport: Some(transport), clock: shared.clone(), ..MailerOptions::default() },
        );

        let revoked = Arc::new(Revocations::default());
        let hasher = setup.hasher.unwrap_or_else(test_hasher);
        let mut deps = AuthDeps::new(config.clone(), store.clone(), mailer.clone(), revoked.clone());
        deps.clock = shared.clone();
        deps.password_hasher = Some(hasher.clone());
        if let Some(oidc) = setup.oidc {
            deps.oidc = oidc;
        }
        let auth = Auth::new(deps).expect("the auth service");

        let mut router = Router::new();
        let groups = AuthGroups {
            auth: AuthRouteDeps { config: config.clone(), auth: auth.clone() },
            account: AccountRouteDeps { config: config.clone(), auth: auth.clone() },
            sso: AuthRouteDeps { config: config.clone(), auth: auth.clone() },
            export: ExportRouteDeps {
                config: config.clone(),
                store: store.clone(),
                auth: auth.clone(),
                history_summary: test_history_summary,
                log: Logger::root().child("http"),
            },
        };
        routes::register(&mut router, groups);
        pages::register(&mut router, PageDeps { config: config.clone(), auth: auth.clone() });
        let api = Api::builder(config.clone(), router)
            .clock(shared)
            .authenticator(auth.clone())
            .page_renderer(pages::layout::error_page_renderer(config.server_name.clone()))
            .build();
        Harness { config, clock, store, auth, api: TestApi::new(api), mailer, revoked, hasher, mails }
    }

    /// Moves both clocks forward.
    pub(crate) fn advance(&self, ms: i64) {
        self.clock.advance(ms as f64);
    }

    /// The wall clock.
    pub(crate) fn now(&self) -> i64 {
        self.clock.wall_ms()
    }

    /// A request from the default address (203.0.113.10).
    pub(crate) fn call(&self, method: Method, path: &str) -> TestRequest {
        self.api.request(method, path)
    }

    /// A request from `ip`.
    pub(crate) fn call_from(&self, ip: &str, method: Method, path: &str) -> TestRequest {
        self.api.clone().at(ip).request(method, path)
    }

    /// `POST path` with a JSON body.
    pub(crate) async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.call(Method::POST, path).json(&body).send().await
    }

    /// `POST path` with a JSON body and a session.
    pub(crate) async fn post_as(&self, token: &str, path: &str, body: Value) -> TestResponse {
        self.call(Method::POST, path).bearer(token).json(&body).send().await
    }

    /// `GET path` with a session.
    pub(crate) async fn get_as(&self, token: &str, path: &str) -> TestResponse {
        self.call(Method::GET, path).bearer(token).send().await
    }

    /// `GET /account/me` with a session: its status.
    pub(crate) async fn me_status(&self, token: &str) -> u16 {
        self.get_as(token, "/api/v1/account/me").await.status
    }

    /// Inserts a verified account `<name>@example.com` with the password [`PW`].
    pub(crate) async fn create_user(&self, name: &str) -> UserId {
        self.create_user_with(name, Some(&format!("{}@example.com", name.to_lowercase())), Some(PW), true)
            .await
    }

    /// Inserts an account directly (no password when `password` is `None`).
    pub(crate) async fn create_user_with(
        &self,
        name: &str,
        email: Option<&str>,
        password: Option<&str>,
        verified: bool,
    ) -> UserId {
        let password_hash = password.map(|p| self.hasher.hash(p).expect("a hash"));
        self.store
            .users()
            .create(NewUser {
                username: name.into(),
                email: email.map(str::to_owned),
                password_hash,
                email_verified: verified,
                accept_challenges: true,
                created_at: self.now(),
            })
            .await
            .expect("a new account")
    }

    /// An account row.
    pub(crate) async fn user(&self, id: UserId) -> User {
        self.store.users().by_id(id).await.expect("a read").expect("the account")
    }

    /// Logs in with a password (panics unless 200): the answer.
    pub(crate) async fn login(&self, login: &str, password: &str) -> Value {
        self.login_with(login, password, json!({})).await
    }

    /// Logs in with extra body fields (panics unless 200): the answer.
    pub(crate) async fn login_with(&self, login: &str, password: &str, extra: Value) -> Value {
        let mut body = json!({ "login": login, "password": password });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        let r = self.post("/api/v1/auth/login", body).await;
        assert_eq!(r.status, 200, "login failed: {}", r.text());
        r.json()
    }

    /// The session token of a password login.
    pub(crate) async fn token(&self, login: &str, password: &str) -> String {
        self.login(login, password).await["token"].as_str().expect("a token").to_owned()
    }

    /// Every security event, oldest first, once the pending ones are saved.
    pub(crate) async fn events(&self) -> Vec<SecurityEvent> {
        self.auth.events().flush().await;
        self.store
            .read(|db| {
                db.all("SELECT id, kind, user_id, ip, at, detail FROM security_events ORDER BY id", [], |r| {
                    let detail: Option<String> = r.get(5)?;
                    Ok(SecurityEvent {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        user_id: r.get(2)?,
                        ip: r.get(3)?,
                        at: r.get(4)?,
                        detail: detail.map(|t| serde_json::from_str(&t).unwrap_or(Value::String(t))),
                    })
                })
            })
            .await
            .expect("a read")
    }

    /// The kinds of every security event, oldest first.
    pub(crate) async fn event_kinds(&self) -> Vec<String> {
        self.events().await.into_iter().map(|e| e.kind).collect()
    }

    /// The messages sent so far (once the mailer is idle).
    pub(crate) async fn sent(&self) -> Vec<OutgoingMail> {
        self.mailer.idle().await;
        self.mails.lock().clone()
    }

    /// The last message sent.
    pub(crate) async fn last_mail(&self) -> OutgoingMail {
        self.sent().await.pop().expect("a message")
    }

    /// The `token` of the first link of the last message.
    pub(crate) async fn last_link_token(&self) -> String {
        let mail = self.last_mail().await;
        let link = link_in(&mail.text).expect("a link");
        token_of(&link).expect("a token in the link")
    }
}

/// The first `http(s)://` link of a text.
pub(crate) fn link_in(text: &str) -> Option<String> {
    let at = text.find("https://").or_else(|| text.find("http://"))?;
    Some(text[at..].split(char::is_whitespace).next().unwrap_or_default().to_owned())
}

/// The `token` query parameter of a link.
pub(crate) fn token_of(link: &str) -> Option<String> {
    let query = link.split_once('?')?.1;
    query.split('&').find_map(|p| p.strip_prefix("token=")).map(str::to_owned)
}

/// The lower-hex SHA-256 of a text.
pub(crate) fn sha256_hex(s: &str) -> String {
    crate::security::keys::sha256_hex(s)
}

/// The keys of a JSON object, in order.
pub(crate) fn keys(v: &Value) -> Vec<String> {
    v.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}
