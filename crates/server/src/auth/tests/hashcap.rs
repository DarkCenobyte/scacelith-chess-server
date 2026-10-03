//! The cap on password hashes (`PASSWORD_HASH_*`) seen from the API: a burst beyond the queue is
//! answered 503 `server_busy` with a Retry-After while the cap holds, for every endpoint that
//! hashes or checks a password, a refused request changes nothing, one client source cannot fill
//! the queue, and the races between logins, rehashes, resets and changes end with the newest
//! password (auth.hashcap.test.js).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use http::Method;
use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::{Harness, NEW_PW, PW, Setup, link_in, test_hasher, token_of};
use crate::http::testing::{TestRequest, TestResponse};
use crate::ids::UserId;
use crate::log::{Level, capture_logs};
use crate::metrics;
use crate::security::password::{
    Argon2Hasher, Argon2Params, HashFailure, HashLimiter, PasswordHasher, Verified,
};
use crate::store::{NewUser, User};

const LOGIN: &str = "/api/v1/auth/login";
const FORM: &str = "application/x-www-form-urlencoded";

/// One server process, as the per-worker defaults of the former server: 1 hash at a time, 32
/// waiting, 2 per client source.
const ONE_WORKER: (&str, &str) = ("WORKERS", "1");

/// The tests that read the process-wide refusal counters or capture the logs run one at a time
/// (the limiter's own unit tests may still move the counters meanwhile: deltas are lower bounds).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The hashes the limiter refused for `reason`, process-wide.
fn rejected(reason: &str) -> u64 {
    metrics::counter_vec(
        "scacelith_password_hash_rejected_total",
        "Password hashes refused (queue_full, timeout: 503 server_busy; source_limit: 429 rate_limited)",
        &["reason"],
    )
    .with(&[reason])
    .get()
}

/// A real (cheap) hasher whose calls can be held: every call while the gate is closed, or the
/// calls of one password while it is held. A held call blocks its blocking thread as a slow hash
/// would, so it keeps its hash slot. Counts the calls running at once and their peak.
struct GatedHasher {
    inner: Arc<dyn PasswordHasher>,
    gates: Mutex<Gates>,
    changed: Condvar,
    running: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
    blocked: AtomicUsize,
    warmed: AtomicBool,
}

#[derive(Default)]
struct Gates {
    closed: bool,
    held: HashSet<String>,
}

impl Gates {
    fn shut(&self, password: &str) -> bool {
        self.closed || self.held.contains(password)
    }
}

impl GatedHasher {
    fn new() -> Arc<GatedHasher> {
        Arc::new(GatedHasher {
            inner: test_hasher(),
            gates: Mutex::new(Gates::default()),
            changed: Condvar::new(),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            blocked: AtomicUsize::new(0),
            warmed: AtomicBool::new(false),
        })
    }

    fn close(&self) {
        self.gates.lock().closed = true;
    }

    fn open(&self) {
        self.gates.lock().closed = false;
        self.changed.notify_all();
    }

    fn hold(&self, password: &str) {
        self.gates.lock().held.insert(password.to_owned());
    }

    fn release(&self, password: &str) {
        self.gates.lock().held.remove(password);
        self.changed.notify_all();
    }

    fn release_all(&self) {
        let mut gates = self.gates.lock();
        gates.closed = false;
        gates.held.clear();
        drop(gates);
        self.changed.notify_all();
    }

    /// Starts counting the calls and the peak afresh.
    fn reset(&self) {
        self.peak.store(self.running.load(Ordering::SeqCst), Ordering::SeqCst);
        self.calls.store(0, Ordering::SeqCst);
    }

    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Calls waiting at a gate now.
    fn blocked(&self) -> usize {
        self.blocked.load(Ordering::SeqCst)
    }

    fn pass<T>(&self, password: &str, work: impl FnOnce() -> T) -> T {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        {
            let mut gates = self.gates.lock();
            if gates.shut(password) {
                self.blocked.fetch_add(1, Ordering::SeqCst);
                while gates.shut(password) {
                    self.changed.wait(&mut gates);
                }
                self.blocked.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let result = work();
        self.running.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

impl PasswordHasher for GatedHasher {
    fn algorithm(&self) -> &'static str {
        self.inner.algorithm()
    }

    fn hash(&self, password: &str) -> Result<String, HashFailure> {
        self.pass(password, || self.inner.hash(password))
    }

    fn verify(&self, stored: &str, password: &str) -> Result<Verified, HashFailure> {
        self.pass(password, || self.inner.verify(stored, password))
    }

    fn verify_dummy(&self, password: &str) -> Result<(), HashFailure> {
        self.pass(password, || self.inner.verify_dummy(password))
    }

    fn warm_up(&self) -> Result<f64, HashFailure> {
        let measured = self.inner.warm_up();
        self.warmed.store(true, Ordering::SeqCst);
        measured
    }
}

/// Lets every held call go when the test ends, even when it fails: the runtime waits for its
/// blocking threads.
struct ReleaseOnDrop(Arc<GatedHasher>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release_all();
    }
}

/// A test server on a [`GatedHasher`], once the start-up warm-up (which takes a slot) is over.
async fn gated(env: &[(&'static str, &str)]) -> (Harness, Arc<GatedHasher>, ReleaseOnDrop) {
    let g = GatedHasher::new();
    let mut pairs = vec![ONE_WORKER];
    pairs.extend_from_slice(env);
    let h = Harness::build(Setup { hasher: Some(g.clone()), ..Setup::env(&pairs) }).await;
    let warmed = g.clone();
    wait_for("the start-up warm-up to finish", || {
        warmed.warmed.load(Ordering::SeqCst) && limiter(&h).stats().active == 0
    })
    .await;
    let guard = ReleaseOnDrop(g.clone());
    (h, g, guard)
}

fn limiter(h: &Harness) -> &HashLimiter {
    h.auth.inner.hasher.limiter()
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !done() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn login_from(h: &Harness, ip: &str, login: &str, password: &str) -> TestRequest {
    h.call_from(ip, Method::POST, LOGIN).json(&json!({ "login": login, "password": password }))
}

fn spawn(request: TestRequest) -> JoinHandle<TestResponse> {
    tokio::spawn(request.send())
}

/// Spawns `request`; its answer also lands in `answered`.
fn spawn_into(request: TestRequest, answered: &Arc<Mutex<Vec<TestResponse>>>) -> JoinHandle<TestResponse> {
    let answered = answered.clone();
    tokio::spawn(async move {
        let r = request.send().await;
        answered.lock().push(r.clone());
        r
    })
}

fn retry_after_in_spread(r: &TestResponse) -> u64 {
    let secs = r.json()["retryAfter"].as_u64().unwrap_or_else(|| panic!("a retryAfter: {}", r.text()));
    assert!((5..=15).contains(&secs), "retryAfter {secs}");
    assert_eq!(r.header("retry-after"), Some(secs.to_string().as_str()));
    secs
}

fn assert_busy(r: &TestResponse) {
    assert_eq!(r.status, 503, "{}", r.text());
    let body = r.json();
    assert_eq!(body["error"], "server_busy");
    assert_eq!(body["message"], "The server is busy; try again in a few seconds.");
    retry_after_in_spread(r);
}

fn assert_source_limited(r: &TestResponse) {
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")), "{}", r.text());
    retry_after_in_spread(r);
}

async fn user_named(h: &Harness, name: &str) -> Option<User> {
    h.store.users().by_username(name.into()).await.unwrap()
}

/// The token of a new reset mail to `email`.
async fn reset_token(h: &Harness, email: &str) -> String {
    h.post("/api/v1/auth/password/forgot", json!({ "email": email })).await;
    let mail = h
        .sent()
        .await
        .into_iter()
        .rev()
        .find(|m| m.to == email && m.subject.contains("Reset your"))
        .expect("a reset mail");
    token_of(&link_in(&mail.text).unwrap()).unwrap()
}

fn reset_page_form(token: &str) -> String {
    let pw = NEW_PW.replace(' ', "%20");
    format!("token={token}&newPassword={pw}&confirmPassword={pw}")
}

async fn live_sessions(h: &Harness, id: UserId) -> usize {
    let all = h.store.read(move |db| db.sessions().all_for_user(id)).await.unwrap();
    all.iter().filter(|s| s.revoked_at.is_none()).count()
}

/// A hash of an older kind (less memory): it matches, and the login wants to upgrade it.
fn outdated_hash(password: &str) -> String {
    let old =
        Argon2Hasher::new(Argon2Params { memory_kib: 32, passes: 1, lanes: 1, ..Argon2Params::DEFAULT });
    old.hash(password).unwrap()
}

/// Inserts a verified account with this stored hash.
async fn user_with_hash(h: &Harness, name: &str, hash: &str) -> UserId {
    h.store
        .users()
        .create(NewUser {
            username: name.into(),
            email: Some(format!("{name}@example.com")),
            password_hash: Some(hash.into()),
            email_verified: true,
            accept_challenges: true,
            created_at: h.now(),
        })
        .await
        .unwrap()
}

fn matches(stored: &str, password: &str) -> bool {
    test_hasher().verify(stored, password).unwrap().ok
}

#[tokio::test]
async fn a_login_flood_the_cap_holds_and_the_queue_overflow_gets_503_server_busy_with_retry_after() {
    let _serial = SERIAL.lock().await;
    let (h, g, _release) =
        gated(&[("PASSWORD_HASH_CONCURRENCY", "1"), ("PASSWORD_HASH_QUEUE_MAX", "3")]).await;
    h.create_user("alice").await;
    g.reset();
    let full0 = rejected("queue_full");

    g.close();
    let answered = Arc::new(Mutex::new(Vec::new()));
    // Unknown accounts: their dummy verification goes through the same queue.
    let flood: Vec<_> = (0..12)
        .map(|i| {
            let ip = format!("203.0.113.{i}");
            spawn_into(login_from(&h, &ip, &format!("ghost{i}"), "not the password"), &answered)
        })
        .collect();
    wait_for("8 refusals and 3 waiters", || answered.lock().len() == 8 && limiter(&h).stats().waiting == 3)
        .await;
    assert_eq!(limiter(&h).stats().active, 1);
    assert_eq!(g.running(), 1, "one hash in flight while the others wait");
    for r in answered.lock().iter() {
        assert_busy(r);
    }
    g.open();
    let mut statuses = Vec::new();
    for task in flood {
        statuses.push(task.await.unwrap().status);
    }
    assert_eq!(statuses.iter().filter(|s| **s == 401).count(), 4);
    assert_eq!(statuses.iter().filter(|s| **s == 503).count(), 8);
    assert_eq!(g.peak(), 1, "never two hashes at once");
    assert_eq!(g.calls(), 4, "refused requests hashed nothing");
    assert!(rejected("queue_full") >= full0 + 8);
    let stats = limiter(&h).stats();
    assert_eq!((stats.active, stats.waiting), (0, 0));

    // Refusals are not failed logins: only the 4 checked passwords were counted.
    assert_eq!(h.event_kinds().await.iter().filter(|k| *k == "login_failed").count(), 4);
    assert_eq!(h.post(LOGIN, json!({ "login": "alice", "password": PW })).await.status, 200);
}

#[tokio::test]
async fn a_wait_longer_than_the_queue_timeout_is_answered_503_server_busy() {
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[("PASSWORD_HASH_QUEUE_TIMEOUT_MS", "100")]).await;
    h.create_user("alice").await;
    assert_eq!(
        limiter(&h).stats().to_json(),
        json!({ "active": 0, "waiting": 0, "concurrency": 1, "queueMax": 32, "queueTimeoutMs": 100 }),
        "the defaults of one worker"
    );
    let timeout0 = rejected("timeout");

    g.close();
    let first = spawn(login_from(&h, "203.0.113.10", "alice", PW));
    wait_for("the first login to hold the slot", || limiter(&h).stats().active == 1).await;
    let answered = Arc::new(Mutex::new(Vec::new()));
    let waiters: Vec<_> = (0..2)
        .map(|i| {
            spawn_into(login_from(&h, "203.0.113.10", &format!("ghost{i}"), "whatever it is"), &answered)
        })
        .collect();
    wait_for("the two waiters to give up", || answered.lock().len() == 2).await;
    for r in answered.lock().iter() {
        assert_busy(r);
    }
    assert_eq!(limiter(&h).stats().waiting, 0, "the expired waiters left the queue");
    assert!(rejected("timeout") >= timeout0 + 2);
    g.open();
    assert_eq!(first.await.unwrap().status, 200);
    for w in waiters {
        w.await.unwrap();
    }
}

#[tokio::test]
async fn every_password_endpoint_goes_through_the_cap_and_a_refused_request_changes_nothing() {
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[("PASSWORD_HASH_QUEUE_MAX", "0")]).await;
    let alice_id = h.create_user("alice").await;
    h.create_user("bob").await;
    let alice = h.token("alice", PW).await;
    let bob = h.token("bob", PW).await;
    let token = reset_token(&h, "alice@example.com").await;
    let alice_hash = h.user(alice_id).await.password_hash;

    // One login holds the only slot; with no queue, every other hash is refused at once.
    g.close();
    let holder = spawn(login_from(&h, "203.0.113.10", "ghost", "whatever it is"));
    wait_for("the holder to take the slot", || limiter(&h).stats().active == 1).await;

    assert_busy(&h.post(LOGIN, json!({ "login": "alice", "password": PW })).await);
    let carol =
        json!({ "username": "carol", "email": "carol@example.com", "password": "a fine passphrase of hers" });
    assert_busy(&h.post("/api/v1/auth/register", carol).await);
    assert!(user_named(&h, "carol").await.is_none(), "no account created");
    assert!(h.store.signups().by_username("carol".into()).await.unwrap().is_none(), "no signup pending");
    assert_busy(
        &h.post("/api/v1/auth/password/reset", json!({ "token": token, "newPassword": NEW_PW })).await,
    );
    let page = h.call(Method::POST, "/reset-password").body(FORM, reset_page_form(&token)).send().await;
    assert_eq!(page.status, 503);
    let secs: u64 = page.header("retry-after").unwrap().parse().unwrap();
    assert!((5..=15).contains(&secs));
    assert!(page.header("content-type").unwrap().starts_with("text/html"));
    assert!(page.text().contains("The server is busy; try again in a few seconds."));
    assert!(
        page.text().contains(&format!("<input type=\"hidden\" name=\"token\" value=\"{token}\">")),
        "the form is shown again"
    );
    let change = json!({ "currentPassword": PW, "newPassword": NEW_PW });
    assert_busy(&h.post_as(&alice, "/api/v1/account/password", change).await);
    assert_busy(&h.post_as(&alice, "/api/v1/account/mfa/totp/setup", json!({ "password": PW })).await);
    assert_busy(&h.post_as(&bob, "/api/v1/account/delete", json!({ "password": PW })).await);
    assert_eq!(h.user(alice_id).await.password_hash, alice_hash, "password unchanged");
    assert!(user_named(&h, "bob").await.is_some_and(|b| b.status == crate::store::UserStatus::Active));
    assert_eq!(h.me_status(&alice).await, 200, "sessions untouched");

    g.open();
    assert_eq!(holder.await.unwrap().status, 401);
    // The reset link survived the refusals.
    let r = h.post("/api/v1/auth/password/reset", json!({ "token": token, "newPassword": NEW_PW })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    h.login("alice", NEW_PW).await;
    assert_eq!(h.post_as(&bob, "/api/v1/account/delete", json!({ "password": PW })).await.status, 200);
}

#[tokio::test]
async fn a_password_change_waits_for_both_of_its_hashes_within_one_queue_timeout() {
    const Q: u64 = 1200;
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[("PASSWORD_HASH_QUEUE_TIMEOUT_MS", "1200")]).await;
    let id = h.create_user("alice").await;
    let alice = h.token("alice", PW).await;
    let other = h.token("alice", PW).await;
    let alice_hash = h.user(id).await.password_hash;
    let timeout0 = rejected("timeout");

    // Queue: [holder 1 (running), the change's check of the current password, holder 2].
    g.hold("holder one pw");
    g.hold("holder two pw");
    let h1 = spawn(login_from(&h, "203.0.113.1", "ghost1", "holder one pw"));
    wait_for("holder 1 to take the slot", || limiter(&h).stats().active == 1 && g.blocked() == 1).await;
    let t0 = Instant::now();
    let change = spawn(
        h.call_from("203.0.113.2", Method::POST, "/api/v1/account/password")
            .bearer(&alice)
            .json(&json!({ "currentPassword": PW, "newPassword": NEW_PW })),
    );
    wait_for("the change to wait", || limiter(&h).stats().waiting == 1).await;
    let h2 = spawn(login_from(&h, "203.0.113.3", "ghost2", "holder two pw"));
    wait_for("holder 2 to wait", || limiter(&h).stats().waiting == 2).await;
    // Half the budget is spent; then the current password is checked, and holder 2 takes the
    // slot before the new password's hash, which may only wait for what is left.
    tokio::time::sleep(Duration::from_millis(Q / 2)).await;
    g.release("holder one pw");
    let r = change.await.unwrap();
    let ms = t0.elapsed().as_millis();
    assert_busy(&r);
    // Two separate waits would answer after about 1.5 x Q.
    assert!(ms < u128::from(Q + 300), "answered after {ms} ms: the two waits must share the {Q} ms budget");
    assert!(rejected("timeout") > timeout0);
    assert_eq!(h.user(id).await.password_hash, alice_hash, "password unchanged");
    assert_eq!(h.me_status(&other).await, 200, "other sessions untouched");
    g.release("holder two pw");
    assert_eq!((h1.await.unwrap().status, h2.await.unwrap().status), (401, 401));
    // With a free queue the same change goes through.
    let ok = h
        .post_as(&alice, "/api/v1/account/password", json!({ "currentPassword": PW, "newPassword": NEW_PW }))
        .await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    h.login("alice", NEW_PW).await;
}

#[tokio::test]
async fn the_login_rehash_of_an_outdated_hash_does_not_wait_for_a_slot_and_is_not_an_error_when_skipped() {
    const P1: &str = "legacy passphrase one";
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[]).await;
    let old = outdated_hash(P1);
    let id = user_with_hash(&h, "legacy", &old).await;

    // The login's check holds the slot; a holder queues behind it and gets the slot next.
    g.hold(P1);
    g.hold("holder pw");
    let login = spawn(login_from(&h, "203.0.113.1", "legacy", P1));
    wait_for("the login check to hold the slot", || limiter(&h).stats().active == 1 && g.blocked() == 1)
        .await;
    let holder = spawn(login_from(&h, "203.0.113.2", "ghost", "holder pw"));
    wait_for("the holder to wait", || limiter(&h).stats().waiting == 1).await;
    let logs = capture_logs(Level::Warn);
    g.release(P1);
    let r = tokio::time::timeout(Duration::from_secs(3), login)
        .await
        .expect("the login answered while the slot was still taken (it did not wait again for the rehash)")
        .unwrap();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.user(id).await.password_hash.as_deref(), Some(old.as_str()), "rehash skipped: no free slot");
    let complaints: Vec<Value> = logs
        .records_of("auth")
        .into_iter()
        .filter(|l| l["msg"].as_str().is_some_and(|m| m.contains("rehash failed") || m.contains("saturated")))
        .collect();
    assert!(complaints.is_empty(), "a skipped rehash is no refusal: {complaints:?}");
    drop(logs);
    g.release("holder pw");
    assert_eq!(holder.await.unwrap().status, 401);

    // With a free slot, the next login upgrades the hash.
    wait_for("the queue to drain", || limiter(&h).stats().active == 0).await;
    h.login("legacy", P1).await;
    let upgraded = h.user(id).await.password_hash.unwrap();
    assert!(upgraded.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{upgraded}");
}

#[tokio::test]
async fn a_password_reset_that_lands_during_a_login_with_a_rehash_wins() {
    const P1: &str = "old leaked passphrase";
    const P2: &str = "fresh secret passphrase";
    let _serial = SERIAL.lock().await;
    for concurrency in ["1", "2"] {
        let (h, g, _release) = gated(&[("PASSWORD_HASH_CONCURRENCY", concurrency)]).await;
        let id = user_with_hash(&h, "alice", &outdated_hash(P1)).await;
        let token = reset_token(&h, "alice@example.com").await;

        // The login checks P1 (an outdated hash, so it will want to rehash); the reset runs while
        // the check is in its slot (concurrency 2), or right after it (concurrency 1).
        g.hold(P1);
        let login = spawn(login_from(&h, "203.0.113.9", "alice", P1));
        wait_for("the login check to start", || g.blocked() == 1).await;
        let reset = spawn(
            h.call(Method::POST, "/api/v1/auth/password/reset")
                .json(&json!({ "token": token, "newPassword": P2 })),
        );
        let reset = if concurrency == "2" {
            let r = reset.await.unwrap();
            assert_eq!(r.status, 200, "{}", r.text());
            None
        } else {
            wait_for("the reset to wait", || limiter(&h).stats().waiting == 1).await;
            Some(reset)
        };
        g.release(P1);
        let rl = login.await.unwrap();
        if let Some(reset) = reset {
            let rr = reset.await.unwrap();
            assert_eq!(rr.status, 200, "{}", rr.text());
        }
        assert!(rl.status == 200 || rl.status == 401, "{}", rl.text());
        let stored = h.user(id).await.password_hash.unwrap();
        assert!(matches(&stored, P2), "the reset password is the one stored (concurrency {concurrency})");
        assert!(!matches(&stored, P1), "the old password no longer works");
        assert_eq!(
            live_sessions(&h, id).await,
            0,
            "no session opened with the old password survives the reset"
        );
        assert_eq!(h.post(LOGIN, json!({ "login": "alice", "password": P1 })).await.status, 401);
        h.login("alice", P2).await;
    }
}

#[tokio::test]
async fn a_password_reset_that_lands_while_a_password_change_hashes_the_new_password_wins() {
    const P2: &str = "the reset passphrase";
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[("PASSWORD_HASH_CONCURRENCY", "2")]).await;
    let id = h.create_user("alice").await;
    let alice = h.token("alice", PW).await;
    let token = reset_token(&h, "alice@example.com").await;

    g.hold(NEW_PW);
    let change = spawn(
        h.call(Method::POST, "/api/v1/account/password")
            .bearer(&alice)
            .json(&json!({ "currentPassword": PW, "newPassword": NEW_PW })),
    );
    wait_for("the change to hash the new password", || g.blocked() == 1).await;
    let rr = h.post("/api/v1/auth/password/reset", json!({ "token": token, "newPassword": P2 })).await;
    assert_eq!(rr.status, 200);
    g.release(NEW_PW);
    let r = change.await.unwrap();
    assert_eq!((r.status, r.json()["error"].clone()), (403, json!("invalid_password")), "{}", r.text());
    let stored = h.user(id).await.password_hash.unwrap();
    assert!(matches(&stored, P2), "the reset password is the one stored");
    assert!(!matches(&stored, NEW_PW));
}

#[tokio::test]
async fn a_concurrent_login_that_upgraded_the_hash_does_not_make_another_login_of_the_same_password_fail() {
    const P1: &str = "legacy passphrase one";
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[("PASSWORD_HASH_CONCURRENCY", "2")]).await;
    let old = outdated_hash(P1);
    let id = user_with_hash(&h, "legacy", &old).await;
    // Login A checks P1 and waits; login B checks, rehashes and stores the new hash; A then finds
    // another hash than the one it checked, and checks the password again against it.
    g.hold(P1);
    let a = spawn(login_from(&h, "203.0.113.1", "legacy", P1));
    wait_for("login A to start", || g.blocked() == 1).await;
    let upgraded = test_hasher().hash(P1).unwrap(); // what login B stored
    let update =
        crate::store::UserUpdate { password_hash: Some(Some(upgraded.clone())), ..Default::default() };
    h.store.users().update(id, update).await.unwrap();
    g.release(P1);
    let r = a.await.unwrap();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.user(id).await.password_hash, Some(upgraded), "A's rehash did not replace B's");
    assert_eq!(live_sessions(&h, id).await, 1);
}

#[tokio::test]
async fn a_session_opens_only_while_the_password_hash_its_login_proved_is_still_stored() {
    let h = Harness::new().await;
    let id = h.create_user("alice").await;
    let user = h.user(id).await;
    let sessions = &h.auth.inner.sessions;
    let stored = user.password_hash.clone().unwrap();
    let stale = test_hasher().hash(PW).unwrap();
    assert!(sessions.create_if_current(&user, None, None, &stale).await.unwrap().is_none());
    assert_eq!(live_sessions(&h, id).await, 0);
    let opened = sessions.create_if_current(&user, None, None, &stored).await.unwrap().expect("a session");
    assert_eq!(h.me_status(&opened.token).await, 200);
}

#[tokio::test]
async fn once_the_queue_is_half_full_one_source_has_at_most_2_hashes_waiting_and_the_next_gets_429() {
    let _serial = SERIAL.lock().await;
    // A queue of 6: the per-source cap applies from 3 waiting hashes on.
    let (h, g, _release) = gated(&[("PASSWORD_HASH_QUEUE_MAX", "6")]).await;
    h.create_user("alice").await;
    assert_eq!(limiter(&h).per_source_max(), Some(2), "2 per source for one worker");
    let token = reset_token(&h, "alice@example.com").await;
    let src0 = rejected("source_limit");
    g.hold("holder pw");
    let holder = spawn(login_from(&h, "192.0.2.1", "ghost", "holder pw"));
    wait_for("the holder to take the slot", || limiter(&h).stats().active == 1 && g.blocked() == 1).await;

    let send = |ip: &str| login_from(&h, ip, "nobody", "not the password");
    let server = &h;
    let waiting = |n: usize| move || limiter(server).stats().waiting == n;
    let mut queued = Vec::new();
    // Another address of the same /24 is another source.
    queued.push(spawn(send("198.51.100.8")));
    wait_for("1 waiter", waiting(1)).await;
    // One IPv4 address: 2 waiting; the queue is then half full, and its third is refused.
    queued.push(spawn(send("198.51.100.7")));
    queued.push(spawn(send("198.51.100.7")));
    wait_for("3 waiters", waiting(3)).await;
    assert_eq!(limiter(&h).waiting_from("198.51.100.7"), 2);
    assert_source_limited(&send("198.51.100.7").send().await);
    // The reset page shows its form again (the link stays valid), with the same status.
    let page = h
        .call_from("198.51.100.7", Method::POST, "/reset-password")
        .body(FORM, reset_page_form(&token))
        .send()
        .await;
    assert_eq!(page.status, 429);
    let secs: u64 = page.header("retry-after").unwrap().parse().unwrap();
    assert!((5..=15).contains(&secs));
    assert!(
        page.text().contains(&format!("<input type=\"hidden\" name=\"token\" value=\"{token}\">")),
        "the form is shown again"
    );
    // IPv6: the /64s of one /48 are one source.
    queued.push(spawn(send("2001:db8:1:1::1")));
    queued.push(spawn(send("2001:db8:1:2::1")));
    wait_for("5 waiters", waiting(5)).await;
    let carol =
        json!({ "username": "carol", "email": "carol@example.com", "password": "a fine passphrase of hers" });
    let r6 =
        h.call_from("2001:db8:1:ffff::5", Method::POST, "/api/v1/auth/register").json(&carol).send().await;
    assert_source_limited(&r6);
    assert!(user_named(&h, "carol").await.is_none(), "nothing changed");
    assert!(h.store.signups().by_username("carol".into()).await.unwrap().is_none());
    queued.push(spawn(send("2001:db8:2::1"))); // another /48
    wait_for("6 waiters", waiting(6)).await;
    assert!(rejected("source_limit") >= src0 + 3);

    g.release("holder pw");
    assert_eq!(holder.await.unwrap().status, 401);
    for q in queued {
        assert_eq!(q.await.unwrap().status, 401);
    }
    // Once its waiters are served, the source may queue again; the reset link survived.
    assert_eq!(login_from(&h, "198.51.100.7", "alice", PW).send().await.status, 200);
    let reset = h
        .call_from("198.51.100.7", Method::POST, "/api/v1/auth/password/reset")
        .json(&json!({ "token": token, "newPassword": NEW_PW }))
        .send()
        .await;
    assert_eq!(reset.status, 200);
}

#[tokio::test]
async fn the_auth_rate_limit_also_applies_to_each_ipv6_48_as_a_whole() {
    let h = Harness::with_env(&[("AUTH_RATE_PER_IP", "3"), ("AUTH_RATE_PER_PREFIX", "5")]).await;
    let send = |ip: &str| {
        h.call_from(ip, Method::POST, "/api/v1/auth/password/forgot")
            .json(&json!({ "email": "someone@example.com" }))
            .send()
    };
    // 5 requests from 5 /64s of one /48 pass; the next /64 is refused although it sent nothing.
    for i in 0..5 {
        assert_eq!(send(&format!("2001:db8:5:{i}::1")).await.status, 202, "/64 number {i}");
    }
    let r = send("2001:db8:5:99::1").await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")), "{}", r.text());
    let secs = r.json()["retryAfter"].as_u64().unwrap();
    assert!(secs >= 1);
    assert_eq!(r.header("retry-after"), Some(secs.to_string().as_str()));
    // Another /48 and IPv4 addresses are not concerned (IPv4 is limited per address only).
    assert_eq!(send("2001:db8:6::1").await.status, 202);
    for i in 1..=6 {
        assert_eq!(send(&format!("192.0.2.{i}")).await.status, 202);
    }
    // The per-/64 limit still holds on its own.
    for _ in 0..3 {
        assert_eq!(send("2001:db8:7:1::1").await.status, 202);
    }
    assert_eq!(send("2001:db8:7:1::2").await.status, 429);
}

#[tokio::test]
async fn an_idle_server_lets_10_simultaneous_logins_from_one_ipv4_address_in() {
    let _serial = SERIAL.lock().await;
    let (h, g, _release) = gated(&[("AUTH_RATE_PER_IP", "20")]).await;
    for i in 0..10 {
        h.create_user(&format!("pupil{i}")).await;
    }
    g.close();
    let logins: Vec<_> =
        (0..10).map(|i| spawn(login_from(&h, "198.51.100.7", &format!("pupil{i}"), PW))).collect();
    // One check runs, the 9 others wait (beyond the per-source cap of 2: the queue of 32 is far
    // from half full).
    wait_for("1 running and 9 waiting", || {
        let s = limiter(&h).stats();
        s.active == 1 && s.waiting == 9
    })
    .await;
    assert_eq!(limiter(&h).waiting_from("198.51.100.7"), 9);
    g.open();
    for login in logins {
        let r = login.await.unwrap();
        assert_eq!(r.status, 200, "{}", r.text());
    }
}

#[tokio::test]
async fn a_request_refused_for_its_source_gives_its_auth_rate_tokens_back() {
    let _serial = SERIAL.lock().await;
    // A queue of 2 (contended from 1 waiting on), 1 waiter per source, 3 attempts per 10 minutes.
    let (h, g, _release) = gated(&[
        ("AUTH_RATE_PER_IP", "3"),
        ("AUTH_RATE_PER_PREFIX", "3"),
        ("PASSWORD_HASH_QUEUE_MAX", "2"),
        ("PASSWORD_HASH_WAITERS_PER_SOURCE", "1"),
    ])
    .await;
    assert_eq!(limiter(&h).per_source_max(), Some(1));
    g.hold("holder pw");
    let holder = spawn(login_from(&h, "192.0.2.1", "ghost", "holder pw"));
    wait_for("the holder to take the slot", || limiter(&h).stats().active == 1 && g.blocked() == 1).await;
    // A new login each time, so that no account's failure delay starts.
    let n = AtomicUsize::new(0);
    let send = |ip: &str| {
        login_from(&h, ip, &format!("nobody{}", n.fetch_add(1, Ordering::SeqCst)), "not the password")
    };

    // IPv4: one login waits (1 of the 3 tokens); 4 more are refused for the source, and each
    // gives its token back, so none of them meets the rate limit (whose Retry-After is longer).
    let a = "198.51.100.7";
    let waiter_a = spawn(send(a));
    wait_for("the waiter of A", || limiter(&h).stats().waiting == 1).await;
    for _ in 0..4 {
        assert_source_limited(&send(a).send().await);
    }
    // IPv6: the /64s of one /48 are one source, and the /48 bucket is refunded as well.
    let waiter_x = spawn(send("2001:db8:9:1::1"));
    wait_for("the waiter of the /48", || limiter(&h).stats().waiting == 2).await;
    for _ in 0..4 {
        assert_source_limited(&send("2001:db8:9:2::1").send().await);
    }
    // A request refused by the queue itself (queue full) is no source refusal.
    assert_busy(&send("203.0.113.50").send().await);

    g.release("holder pw");
    assert_eq!(holder.await.unwrap().status, 401);
    assert_eq!((waiter_a.await.unwrap().status, waiter_x.await.unwrap().status), (401, 401));
    // A still has its 2 other attempts, the /48 too; then the limit of 3 applies.
    assert_eq!((send(a).send().await.status, send(a).send().await.status), (401, 401));
    let over = send(a).send().await;
    assert_eq!(over.status, 429);
    assert!(
        over.json()["retryAfter"].as_u64().unwrap() > 15,
        "the rate limit's own Retry-After: {}",
        over.text()
    );
    let pair = (send("2001:db8:9:3::1").send().await.status, send("2001:db8:9:4::1").send().await.status);
    assert_eq!(pair, (401, 401));
    let over48 = send("2001:db8:9:5::1").send().await;
    assert_eq!(over48.status, 429);
    assert!(
        over48.json()["retryAfter"].as_u64().unwrap() > 15,
        "the /48 limit's own Retry-After: {}",
        over48.text()
    );
}

/// A hasher whose verifications only sleep: stored hashes `slow:<password>` take `SLOW` ms (an
/// older, slower kind), everything else `FAST` ms. Its warm-up reports `warm_up_ms`.
struct SleepyHasher {
    warm_up_ms: u64,
}

const SLOW: u64 = 150;
const FAST: u64 = 20;

impl PasswordHasher for SleepyHasher {
    fn algorithm(&self) -> &'static str {
        "fast"
    }

    fn hash(&self, password: &str) -> Result<String, HashFailure> {
        std::thread::sleep(Duration::from_millis(FAST));
        Ok(format!("fast:{password}"))
    }

    fn verify(&self, stored: &str, password: &str) -> Result<Verified, HashFailure> {
        let ms = if stored.starts_with("slow:") { SLOW } else { FAST };
        std::thread::sleep(Duration::from_millis(ms));
        Ok(Verified { ok: stored.get(5..) == Some(password), needs_rehash: false })
    }

    fn verify_dummy(&self, _password: &str) -> Result<(), HashFailure> {
        std::thread::sleep(Duration::from_millis(FAST));
        Ok(())
    }

    fn warm_up(&self) -> Result<f64, HashFailure> {
        std::thread::sleep(Duration::from_millis(self.warm_up_ms));
        Ok(self.warm_up_ms as f64)
    }
}

async fn sleepy(warm_up_ms: u64) -> Harness {
    let hasher: Arc<dyn PasswordHasher> = Arc::new(SleepyHasher { warm_up_ms });
    Harness::build(Setup { hasher: Some(hasher), ..Setup::default() }).await
}

/// The duration of a failed login, in ms.
async fn failed_login_ms(h: &Harness, login: &str) -> f64 {
    let t0 = Instant::now();
    let r = h.post(LOGIN, json!({ "login": login, "password": "a wrong one" })).await;
    assert_eq!(r.status, 401);
    t0.elapsed().as_secs_f64() * 1000.0
}

#[tokio::test]
async fn the_first_failed_login_is_padded_to_the_warm_up_baseline() {
    // The dummy is fast, a stored hash of the older kind slow; the warm-up reports the slowest
    // verification it timed.
    let h = sleepy(SLOW).await;
    user_with_hash(&h, "dormant", "slow:the right one").await;
    let floor = h.auth.inner.hasher.floor();
    wait_for("the warm-up", || floor.baseline_ms() > 0.0 && limiter(&h).stats().active == 0).await;
    // No earlier check: the very first failure of the server, then the slow one.
    let unknown = failed_login_ms(&h, "ghost@example.com").await;
    let known = failed_login_ms(&h, "dormant@example.com").await;
    assert!(
        unknown >= known - 30.0,
        "first failure: unknown account {unknown:.0} ms, slow stored hash {known:.0} ms"
    );
    assert!(unknown >= SLOW as f64 - 5.0, "unknown {unknown:.0} ms");
}

#[tokio::test]
async fn a_failed_login_takes_as_long_for_an_unknown_account_as_for_a_stored_hash_of_a_slower_kind() {
    let h = sleepy(0).await;
    for i in 0..8 {
        user_with_hash(&h, &format!("user{i}"), "slow:the right one").await;
    }
    let (mut known, mut unknown) = (Vec::new(), Vec::new());
    for i in 0..8 {
        known.push(failed_login_ms(&h, &format!("user{i}@example.com")).await);
        unknown.push(failed_login_ms(&h, &format!("ghost{i}@example.com")).await);
    }
    let median = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(f64::total_cmp);
        v[4]
    };
    let detail = format!("known {known:.0?} / unknown {unknown:.0?} ms");
    assert!(
        median(&unknown) >= SLOW as f64 - 10.0,
        "unknown accounts are padded to the slow check: {detail}"
    );
    assert!((median(&known) - median(&unknown)).abs() < 50.0, "{detail}");
    // A successful login is not padded (and never needed to be).
    let t0 = Instant::now();
    assert_eq!(h.post(LOGIN, json!({ "login": "user0", "password": "the right one" })).await.status, 200);
    assert!(t0.elapsed() < Duration::from_millis(SLOW + 400));
}

#[tokio::test]
async fn password_hash_waiters_per_source_reaches_the_hash_limiter() {
    let h = Harness::with_env(&[("PASSWORD_HASH_WAITERS_PER_SOURCE", "6")]).await;
    assert_eq!(limiter(&h).per_source_max(), Some(6));
}
