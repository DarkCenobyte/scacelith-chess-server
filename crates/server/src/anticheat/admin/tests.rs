//! Tests of the administration commands, ported from anticheat.admin and the moderator part of
//! anticheat.refunds, against the real store (in memory, or a file for the backups).

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::anticheat::Anticheat;
use crate::anticheat::players::from_store_level;
use crate::anticheat::reports::review_priority;
use crate::anticheat::testing::*;
use crate::clock::ManualClock;
use crate::ids::GameId;
use crate::store::status::{BLACK_WINS, DRAW, WHITE_WINS};
use crate::store::tests::support::{LogCapture, TempDir};
use crate::store::{
    GameRecord, IntegrityLevel, IntegrityUpdate, JobStatus, NewAnomaly, NewReport, NewSession, NewSignup,
    Priority, RatingRecord, RefundScope, ReportCategory, ReportStatus, Severity, Source, UserUpdate,
};

/// The moderator of the commands when `--by` is not given.
const MODERATOR: &str = "mod-anna";

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| s.to_string()).collect()
}

/// What a command answered.
#[derive(Debug)]
struct Ran {
    code: i32,
    out: String,
    err: String,
}

impl Ran {
    /// Asserts success.
    fn ok(self) -> Ran {
        assert_eq!(self.code, 0, "{}", self.err);
        self
    }

    /// The `--json` answer of a command that succeeded.
    fn json(&self) -> Value {
        assert_eq!(self.code, 0, "{}", self.err);
        serde_json::from_str(&self.out).expect("JSON output")
    }
}

/// A store with the real rating rules, its clock, players and the commands' environment (its audit
/// lines logged under a component of its own).
struct World {
    config: Arc<Config>,
    clock: Arc<ManualClock>,
    store: Store,
    env: AdminEnv,
    component: String,
    ids: Vec<UserId>,
}

impl World {
    async fn new(overrides: &[(&str, &str)], names: &[&str]) -> World {
        static N: AtomicUsize = AtomicUsize::new(0);
        let config = config(overrides);
        let clock = ManualClock::new(0.0, NOW);
        let store = store(&config, &clock).await;
        let ids = users(&store, names).await;
        let config = Arc::new(config);
        let component = format!("admin-test-{}", N.fetch_add(1, Ordering::Relaxed));
        let env = AdminEnv {
            store: store.clone(),
            config: config.clone(),
            clock: clock.clone(),
            moderator: MODERATOR.into(),
            logger: Logger::root().child(&component),
            hash_token: crate::security::keys::sha256_hex,
            euid: None,
        };
        World { config, clock, store, env, component, ids }
    }

    /// The players of the refund scenarios ([`REFUND_PLAYERS`]) with their rated records.
    async fn refunds(overrides: &[(&str, &str)]) -> World {
        let w = World::new(overrides, &REFUND_PLAYERS).await;
        seed_refund_ratings(&w.store, &w.ids).await;
        w
    }

    async fn run(&self, argv: &[&str]) -> Ran {
        run_with(&self.env, argv).await
    }

    /// Runs a command at another time (the store keeps its own clock).
    async fn run_at(&self, now: i64, argv: &[&str]) -> Ran {
        run_with(&AdminEnv { clock: ManualClock::new(0.0, now), ..self.env.clone() }, argv).await
    }

    async fn user(&self, name: &str) -> Option<crate::store::User> {
        self.store.users().by_username(name.into()).await.unwrap()
    }

    async fn rating(&self, user: UserId) -> RatingRecord {
        self.store.ratings().get(user, "3+2".into()).await.unwrap()
    }

    async fn active_ban(&self, user: UserId, now: i64) -> Option<crate::store::Sanction> {
        self.store.sanctions().active_ban(user, now).await.unwrap()
    }

    async fn level(&self, user: UserId) -> IntegrityLevel {
        self.store.integrity().get(user).await.unwrap().level
    }

    async fn set_integrity(&self, user: UserId, level: IntegrityLevel, score: f64, evidence: Value) {
        let update = IntegrityUpdate {
            level: Some(level),
            score: Some(score),
            evidence: Some(Some(evidence)),
            updated_at: Some(NOW),
            ..IntegrityUpdate::default()
        };
        self.store.integrity().set(user, update).await.unwrap();
    }

    async fn report(
        &self,
        by: UserId,
        of: UserId,
        game: GameId,
        category: ReportCategory,
        weight: f64,
        at: i64,
    ) -> i64 {
        self.report_with(by, of, game, category, None, weight, at).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn report_with(
        &self,
        by: UserId,
        of: UserId,
        game: GameId,
        category: ReportCategory,
        comment: Option<&str>,
        weight: f64,
        at: i64,
    ) -> i64 {
        let report = NewReport {
            reporter_id: by,
            reported_id: of,
            game_id: (game != 0).then_some(game),
            category,
            comment: comment.map(Into::into),
            weight,
            at,
        };
        self.store.reports().create(report).await.unwrap()
    }

    async fn session(
        &self,
        user: UserId,
        hash: &str,
        created_at: i64,
        expires_at: i64,
        idle_expires_at: i64,
    ) {
        let session = NewSession {
            user_id: user,
            token_hash: hash.into(),
            created_at,
            expires_at,
            idle_expires_at: Some(idle_expires_at),
            client_label: None,
            ip: None,
        };
        self.store.sessions().create(session).await.unwrap();
    }

    async fn live_sessions(&self, user: UserId) -> usize {
        let sessions = self.store.sessions().list_for_user(user).await.unwrap();
        sessions.iter().filter(|s| s.revoked_at.is_none()).count()
    }

    /// The details of the `moderator_action` events, in order.
    async fn moderator_actions(&self) -> Vec<Value> {
        let list = query_json(
            &self.store,
            "SELECT json_group_array(json(detail)) FROM (SELECT detail FROM security_events
             WHERE kind = 'moderator_action' ORDER BY id)",
        )
        .await;
        list.as_array().cloned().unwrap_or_default()
    }

    async fn actions(&self) -> Vec<String> {
        self.moderator_actions()
            .await
            .iter()
            .map(|d| d["action"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// The security lines the commands logged: (message, fields).
    fn security(&self, logs: &LogCapture) -> Vec<(String, Value)> {
        logs.records(&self.component)
            .into_iter()
            .filter(|r| r["level"] == "security")
            .map(|r| {
                let mut fields = r.as_object().cloned().unwrap_or_default();
                let msg = fields.get("msg").and_then(Value::as_str).unwrap_or("").to_string();
                for k in ["t", "level", "c", "msg"] {
                    fields.remove(k);
                }
                (msg, Value::Object(fields))
            })
            .collect()
    }

    async fn refunds_of(&self, scope: RefundScope) -> Vec<crate::store::Refund> {
        self.store.refunds().list(scope, 100).await.unwrap()
    }

    /// Finishes a game of the scenario's kind (3+2, 40 plies) now.
    async fn finish(&self, games: Vec<GameRecord>) -> Vec<crate::store::CommitEntry> {
        self.store.finish_batch(games).await.expect("games stored")
    }
}

async fn run_with(env: &AdminEnv, argv: &[&str]) -> Ran {
    let (mut out, mut err) = (String::new(), String::new());
    let code = run_admin(&strings(argv), env, &mut out, &mut err).await;
    Ran { code, out, err }
}

/// The JSON text that the single row and column of a query gives, parsed.
async fn query_json(store: &Store, sql: &'static str) -> Value {
    let text: String = store
        .write(move |db| db.connection().query_row(sql, [], |r| r.get(0)).map_err(StoreError::from))
        .await
        .expect("query runs");
    serde_json::from_str(&text).expect("JSON")
}

/// Bob (rated 1620 in 5+0) and Eve (two live sessions), the world of the former tests.
async fn bob_and_eve() -> (World, UserId, UserId) {
    let w = World::new(&[], &["bob", "eve"]).await;
    let (bob, eve) = (w.ids[0], w.ids[1]);
    seed_rating(&w.store, bob, "5+0", 1620, 44).await;
    for hash in ["h1", "h2"] {
        w.session(eve, hash, NOW - 1000, NOW + 1_000_000_000, NOW + 1_000_000_000).await;
    }
    (w, bob, eve)
}

/// A finished game of two players, its analysis job completed with these features.
async fn analysed(w: &World, white: UserId, black: UserId, features: impl FnOnce(GameId) -> Value) -> GameId {
    let g = game(white, black, WHITE_WINS, NOW);
    let id = g.id;
    w.finish(vec![g]).await;
    w.store.analysis().enqueue(id, NOW).await.unwrap();
    assert!(w.store.analysis().complete(id, Some(features(id)), NOW).await.unwrap());
    id
}

/// The whitespace-separated cells of the output line that starts with `first`.
fn row(out: &str, first: &str) -> Vec<String> {
    let line =
        out.lines().find(|l| l.starts_with(&format!("{first} "))).unwrap_or_else(|| panic!("no row {first}"));
    line.split_whitespace().map(str::to_string).collect()
}

/// A line of the output holds these cells in a row.
fn has_cells(out: &str, cells: &[&str]) -> bool {
    out.lines().any(|l| {
        let words: Vec<&str> = l.split_whitespace().collect();
        words.windows(cells.len()).any(|w| w == cells)
    })
}

// ---- arguments and usage -----------------------------------------------------------------------

#[test]
fn arguments_are_positionals_options_with_values_and_boolean_options() {
    let a = parse_args(&strings(&["user", "ban", "eve", "--hours", "5", "--reason=spam here", "--json"]));
    assert_eq!(a.positional, ["user", "ban", "eve"]);
    let flags = HashMap::from([
        ("hours".to_string(), Flag::Value("5".into())),
        ("reason".to_string(), Flag::Value("spam here".into())),
        ("json".to_string(), Flag::On),
    ]);
    assert_eq!(a.flags, flags);
    let b = parse_args(&strings(&["bench-accounts", "--i-know-this-is-a-test-server", "--count", "3"]));
    let flags = HashMap::from([
        ("i-know-this-is-a-test-server".to_string(), Flag::On),
        ("count".to_string(), Flag::Value("3".into())),
    ]);
    assert_eq!(b.flags, flags);
    // An option followed by another option or by nothing has no value; a later one wins.
    let c = parse_args(&strings(&["stats", "--limit", "--by", "x", "--by", "y", "--last"]));
    assert_eq!(c.flags["limit"], Flag::On);
    assert_eq!(c.flags["by"], Flag::Value("y".into()));
    assert_eq!(c.flags["last"], Flag::On);
    // Every command of the help is one.
    let commands = [
        "user show",
        "user ban",
        "user unban",
        "user reset-mfa",
        "user verify-email",
        "user revoke-sessions",
        "integrity list",
        "integrity show",
        "integrity confirm",
        "integrity clear",
        "refunds apply",
        "refunds list",
        "analysis queue",
        "reports list",
        "reports resolve",
        "anomalies",
        "stats",
        "backup",
        "bench-accounts",
    ];
    for c in commands {
        let mut words = c.split(' ');
        assert!(Command::of(words.next(), words.next()).is_some(), "{c}");
        assert!(USAGE.contains(&format!("  {c}")), "{c} in the help");
    }
}

#[tokio::test]
async fn usage_and_unknown_commands() {
    let (w, ..) = bob_and_eve().await;
    let r = w.run(&["nope"]).await;
    assert_eq!((r.code, r.err.as_str(), r.out.as_str()), (2, USAGE, ""));
    for name in ["constructor", "toString", "__proto__", "hasOwnProperty", "user", "integrity nope"] {
        let argv: Vec<&str> = name.split(' ').collect();
        assert_eq!(w.run(&argv).await.code, 2, "{name}");
        let mut help = argv.clone();
        help.push("--help");
        assert_eq!(w.run(&help).await.code, 2, "{name} --help");
    }
    let help = w.run(&["user", "show", "--help"]).await;
    assert_eq!((help.code, help.out.as_str()), (0, USAGE));
    let e = w.run(&["user", "show", "nobody"]).await;
    assert_eq!((e.code, e.err.as_str()), (1, "error: no user named \"nobody\"\n"));
    assert_eq!(w.run(&["user", "show"]).await.err, "error: a user name is required\n");
}

// ---- accounts ----------------------------------------------------------------------------------

#[tokio::test]
async fn user_show_hides_secrets() {
    let (w, bob, _) = bob_and_eve().await;
    let secrets = UserUpdate {
        password_hash: Some(Some("scrypt$secret".into())),
        mfa_secret_enc: Some(Some("enc".into())),
        ..UserUpdate::default()
    };
    w.store.users().update(bob, secrets).await.unwrap();
    let r = w.run(&["user", "show", "bob", "--json"]).await;
    let j = r.json();
    assert_eq!(j["user"]["username"], "bob");
    assert_eq!(j["ratings"][0]["rating"], 1620);
    assert!(!r.out.contains("scrypt$secret"));
    assert!(!r.out.contains("\"enc\""));
    let t = w.run(&["user", "show", "bob"]).await.ok();
    assert!(t.out.contains("integrity none"), "{}", t.out);
    assert!(!t.out.contains("scrypt$secret"));
}

#[tokio::test]
async fn user_show_counts_the_sessions_that_are_neither_expired_nor_idle_expired() {
    let (w, _, eve) = bob_and_eve().await;
    w.session(eve, "idle", NOW - 40 * DAY, NOW + 1_000_000_000, NOW - 1).await;
    w.session(eve, "old", NOW - 100 * DAY, NOW - 1, NOW + 1_000_000_000).await;
    let r = w.run(&["user", "show", "eve", "--json"]).await;
    assert_eq!(r.json()["activeSessions"], 2, "the two live sessions of the world");
}

#[tokio::test]
async fn user_ban_and_unban_are_audited() {
    let (w, _, eve) = bob_and_eve().await;
    let logs = LogCapture::start();
    let no_reason = w.run(&["user", "ban", "eve", "--hours", "5"]).await;
    assert_eq!((no_reason.code, no_reason.err.as_str()), (1, "error: --reason TEXT is required\n"));
    let bad_hours = w.run(&["user", "ban", "eve", "--hours", "x", "--reason", "r"]).await;
    assert_eq!((bad_hours.code, bad_hours.err.as_str()), (1, "error: --hours expects an integer\n"));
    let r = w
        .run(&["user", "ban", "eve", "--hours", "5", "--reason", "harassment", "--revoke-sessions"])
        .await
        .ok();
    assert!(r.out.contains(" 2 sessions revoked."), "{}", r.out);
    let ban = w.active_ban(eve, NOW).await.unwrap();
    assert_eq!(ban.ends_at, Some(NOW + 5 * HOUR));
    assert_eq!(ban.source, Source::Moderator);
    assert_eq!(ban.created_by.as_deref(), Some(MODERATOR));
    assert_eq!(w.live_sessions(eve).await, 0);
    assert_eq!(w.actions().await, ["ban"]);
    let lines = w.security(&logs);
    assert_eq!(
        lines,
        [(
            "moderator.action".to_string(),
            json!({ "userId": eve, "action": "ban", "moderator": MODERATOR, "hours": 5, "reason": "harassment",
                "sanctionId": ban.id, "until": NOW + 5 * HOUR, "revokedSessions": 2 })
        )]
    );
    drop(logs);
    w.run(&["user", "unban", "eve", "--by", "mod-ben"]).await.ok();
    assert!(w.active_ban(eve, NOW).await.is_none());
    let sanctions = w.store.sanctions().list(eve).await.unwrap();
    assert_eq!(sanctions[0].lifted_by.as_deref(), Some("mod-ben"));
    assert_eq!(w.moderator_actions().await[1]["moderator"], "mod-ben");
    assert!(w.run(&["user", "unban", "eve"]).await.ok().out.contains("no active ban"));
    assert_eq!(w.actions().await, ["ban", "unban"], "nothing lifted, nothing audited");
}

#[tokio::test]
async fn user_reset_mfa_verify_email_and_revoke_sessions() {
    let (w, _, eve) = bob_and_eve().await;
    let enrolled = UserUpdate {
        mfa_enabled: Some(true),
        mfa_secret_enc: Some(Some("x".into())),
        email_verified: Some(false),
        ..UserUpdate::default()
    };
    w.store.users().update(eve, enrolled).await.unwrap();
    w.store.mfa().replace_recovery_codes(eve, vec!["a".into(), "b".into()], NOW).await.unwrap();
    w.run(&["user", "reset-mfa", "eve"]).await.ok();
    let u = w.user("eve").await.unwrap();
    assert!(!u.mfa_enabled);
    assert_eq!(u.mfa_secret_enc, None);
    assert_eq!(w.store.mfa().count_recovery_codes(eve).await.unwrap(), 0);
    assert_eq!(w.live_sessions(eve).await, 0);
    w.run(&["user", "verify-email", "eve"]).await.ok();
    assert!(w.user("eve").await.unwrap().email_verified);
    w.session(eve, "h3", NOW, NOW + 1_000_000_000, NOW + 1_000_000_000).await;
    let r = w.run(&["user", "revoke-sessions", "eve", "--json"]).await;
    assert_eq!(r.json(), json!({ "revokedSessions": 1 }));
    assert_eq!(w.actions().await, ["reset_mfa", "verify_email", "revoke_sessions"]);
}

#[tokio::test]
async fn user_show_and_verify_email_reach_the_pending_signup_that_holds_a_name() {
    let w = World::new(&[], &[]).await;
    let signup = |username: &str, email: &str, link: bool, expires_at: i64| NewSignup {
        username: username.into(),
        email: email.into(),
        password_hash: format!("scrypt${username}"),
        token_hash: link.then(|| format!("link-{username}")),
        created_at: NOW - HOUR,
        expires_at,
    };
    w.store.signups().create(signup("Alice_1", "alice@example.org", true, NOW + HOUR)).await.unwrap();
    let show = w.run(&["user", "show", "alice_1", "--json"]).await;
    assert_eq!(
        show.json(),
        json!({ "pendingSignup": { "username": "Alice_1", "email": "alice@example.org", "createdAt": NOW - HOUR,
            "expiresAt": NOW + HOUR, "link": true } })
    );
    assert!(!show.out.contains("scrypt$"));
    let text = w.run(&["user", "show", "alice_1"]).await.ok().out;
    assert!(
        text.starts_with("Pending signup Alice_1 (no account yet)\n  e-mail alice@example.org, link stored"),
        "{text}"
    );

    // The account is created as the link would: its address verified, the signup's password, the
    // signup gone.
    let r = w.run(&["user", "verify-email", "alice_1", "--by", "mod-ben"]).await.ok();
    let u = w.user("alice_1").await.unwrap();
    assert_eq!(
        (u.username.as_str(), u.email.as_deref(), u.email_verified, u.password_hash.as_deref(), u.created_at),
        ("Alice_1", Some("alice@example.org"), true, Some("scrypt$Alice_1"), NOW)
    );
    assert!(r.out.contains(&format!("Account Alice_1 (#{}) created", u.id)), "{}", r.out);
    assert_eq!(w.store.signups().by_username("alice_1".into()).await.unwrap(), None);
    let mut events: Vec<(String, Value, Value)> = w
        .store
        .security()
        .for_user(u.id, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|e| {
            let d = e.detail.unwrap_or(Value::Null);
            (e.kind, d["action"].clone(), d["moderator"].clone())
        })
        .collect();
    events.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        events,
        [
            ("moderator_action".to_string(), json!("confirm_signup"), json!("mod-ben")),
            ("register".to_string(), Value::Null, Value::Null)
        ]
    );
    // Then the account itself, as before.
    let again = w.run(&["user", "verify-email", "alice_1"]).await.ok();
    assert!(again.out.contains("E-mail address of Alice_1 marked verified"));
    assert_eq!(w.run(&["user", "show", "alice_1", "--json"]).await.json()["user"]["id"], u.id);

    // A signup whose address has an account (no link), or whose name or address another account
    // took meanwhile: dropped, nothing created.
    w.store.signups().create(signup("Bob_2", "Alice@Example.org", false, NOW + HOUR)).await.unwrap();
    let shown = w.run(&["user", "show", "bob_2"]).await.ok();
    assert!(shown.out.contains("no link: the address had an account"), "{}", shown.out);
    assert_eq!(
        w.run(&["user", "verify-email", "bob_2", "--json"]).await.json(),
        json!({ "status": "taken" })
    );
    assert!(w.user("bob_2").await.is_none());
    assert_eq!(w.store.signups().by_username("bob_2".into()).await.unwrap(), None, "the name is free again");
    // Without a link even when no account has the address: its link could never confirm it either.
    w.store.signups().create(signup("Dan_4", "dan@example.org", false, NOW + HOUR)).await.unwrap();
    assert_eq!(
        w.run(&["user", "verify-email", "dan_4", "--json"]).await.json(),
        json!({ "status": "taken" })
    );
    assert!(w.user("dan_4").await.is_none());

    // An expired signup holds nothing: no such user.
    w.store.signups().create(signup("Cleo_3", "cleo@example.org", true, NOW)).await.unwrap();
    for cmd in ["show", "verify-email"] {
        let x = w.run(&["user", cmd, "cleo_3"]).await;
        assert_eq!((x.code, x.err.as_str()), (1, "error: no user named \"cleo_3\"\n"), "{cmd}");
    }
    assert!(w.user("cleo_3").await.is_none());
    assert!(
        w.store.signups().by_username("cleo_3".into()).await.unwrap().is_some(),
        "left to the retention purge"
    );
}

// ---- integrity reviews -------------------------------------------------------------------------

#[tokio::test]
async fn integrity_list_show_confirm_and_clear() {
    let (w, bob, eve) = bob_and_eve().await;
    let statistics = json!({ "statistics": { "model": 1, "computedAt": NOW, "level": "suspected", "score": 3.8,
        "groups": { "Q": 3.8, "E": 2, "J": 0, "T": 0.4 }, "reasons": ["Move quality Q=3.80 over 12 games"],
        "windows": { "all": { "games": 12 } } } });
    w.set_integrity(bob, IntegrityLevel::Suspected, 3.8, statistics).await;
    w.set_integrity(eve, IntegrityLevel::HighConfidence, 4.5, json!({})).await;
    let g = analysed(&w, bob, eve, |id| {
        json!({ "v": 1, "gameId": id, "category": "5+0",
            "white": { "userId": bob, "rating": 1600, "n": 30, "accuracy": 97.2, "acpl": 9, "t1Deep": 0.9,
                "t1Fast": 0.8, "t1Complex": 0.85, "nComplex": 12, "timeCorr": 0.01, "timeCv": 0.3 },
            "black": { "userId": eve, "n": 30 } })
    })
    .await;
    let anomaly = NewAnomaly {
        user_id: Some(bob),
        game_id: Some(g),
        kind: "clock_implausible".into(),
        severity: Severity::Suspicious,
        at: Some(NOW),
        detail: Some(json!({ "thinkMs": 9000 })),
    };
    w.store.anomalies().insert_batch(vec![anomaly]).await.unwrap();
    let report = w.report_with(eve, bob, g, ReportCategory::Cheating, Some("engine"), 1.0, NOW).await;

    let list = w.run(&["integrity", "list", "--json"]).await.json();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|r| r["username"].as_str().unwrap()).collect();
    assert_eq!(names, ["eve", "bob"], "sorted by review priority");
    assert_eq!(list[1]["reports30d"], 1);
    assert_eq!(list[1]["games"], 12);
    let hc = w.run(&["integrity", "list", "--level", "high_confidence", "--json"]).await.json();
    assert_eq!(hc.as_array().unwrap().iter().map(|r| r["username"].clone()).collect::<Vec<_>>(), ["eve"]);
    assert_eq!(w.run(&["integrity", "list", "--level", "none"]).await.code, 1);

    let show = w.run(&["integrity", "show", "bob"]).await.ok();
    for text in ["Move quality Q=3.80", "97.2", "clock_implausible", "engine"] {
        assert!(show.out.contains(text), "{text} in {}", show.out);
    }
    assert_eq!(
        row(&show.out, &g.to_string())[..5],
        [g.to_string(), "5+0".into(), "1600".into(), "30".into(), "97.2".into()]
    );

    let confirm =
        ["integrity", "confirm", "bob", "--reason", "engine moves confirmed by review", "--hours", "720"];
    w.run(&confirm).await.ok();
    let ib = w.store.integrity().get(bob).await.unwrap();
    assert_eq!(ib.level, IntegrityLevel::Confirmed);
    assert_eq!(ib.reviewed_by.as_deref(), Some(MODERATOR));
    let evidence = ib.evidence.unwrap();
    assert_eq!(evidence["reviews"][0]["action"], "confirm");
    assert_eq!(evidence["statistics"]["score"], 3.8, "evidence kept");
    assert_eq!(w.active_ban(bob, NOW).await.unwrap().ends_at, Some(NOW + 720 * HOUR));
    let reports = w.store.reports().for_reported(bob, 10).await.unwrap();
    let r = reports.iter().find(|r| r.id == report).unwrap();
    assert_eq!(r.status, ReportStatus::Actioned, "open cheating reports actioned");

    w.run(&["integrity", "clear", "eve", "--reason", "strong club player, verified"]).await.ok();
    let ie = w.store.integrity().get(eve).await.unwrap();
    assert_eq!(ie.level, IntegrityLevel::None);
    assert_eq!(ie.reviewed_by.as_deref(), Some(MODERATOR));
    assert_eq!(ie.evidence.unwrap()["review"]["clearedScore"], 4.5);
    assert_eq!(w.actions().await, ["integrity_confirm", "integrity_clear"]);
}

#[tokio::test]
async fn integrity_show_prints_no_rates_for_a_side_without_scored_moves() {
    let (w, bob, eve) = bob_and_eve().await;
    // The analyzer's empty side: no scored move, every rate null.
    let empty = |user: UserId| {
        json!({ "userId": user, "rating": 1600, "n": 0, "accuracy": null, "acpl": null, "t1Deep": null,
            "t1Fast": null, "nComplex": 0, "t1Complex": null, "timeCorr": null, "timeCv": null })
    };
    let g = analysed(
        &w,
        bob,
        eve,
        |id| json!({ "v": 1, "gameId": id, "category": "5+0", "white": empty(bob), "black": empty(eve) }),
    )
    .await;
    let show = w.run(&["integrity", "show", "bob"]).await.ok();
    assert_eq!(
        row(&show.out, &g.to_string()),
        [g.to_string().as_str(), "5+0", "1600", "0", "-", "-", "-", "-", "-", "-", "-"]
    );
}

#[tokio::test]
async fn integrity_show_a_report_comment_cannot_break_the_table_or_forge_lines() {
    let (w, bob, eve) = bob_and_eve().await;
    let plain = w.run(&["integrity", "show", "bob"]).await.ok().out;
    let comment = "x\nSanctions\n(none)\u{9b}\u{202e}abc\u{2028}";
    w.report_with(eve, bob, 1, ReportCategory::Cheating, Some(comment), 1.0, NOW).await;
    let show = w.run(&["integrity", "show", "bob"]).await.ok().out;
    assert_eq!(
        show.split('\n').count(),
        plain.split('\n').count() + 2,
        "one report row and its table header"
    );
    assert!(show.contains("x\\u000aSanctions\\u000a(none)\\u009b\\u202eabc\\u2028"), "{show}");
    assert!(!show.chars().any(|c| matches!(c, '\u{80}'..='\u{9f}' | '\u{2028}' | '\u{202e}')));
}

#[tokio::test]
async fn integrity_confirm_leaves_the_level_as_it_was_when_the_ban_cannot_be_stored() {
    let (w, bob, _) = bob_and_eve().await;
    w.set_integrity(bob, IntegrityLevel::Suspected, 3.8, json!({})).await;
    exec(
        &w.store,
        "CREATE TRIGGER no_bans BEFORE INSERT ON sanctions BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
    )
    .await;
    let r = w.run(&["integrity", "confirm", "bob", "--reason", "engine", "--no-refund"]).await;
    assert_eq!((r.code, r.err.as_str()), (1, "admin: database is locked\n"));
    assert_eq!(w.level(bob).await, IntegrityLevel::Suspected);
    assert_eq!(w.moderator_actions().await, Vec::<Value>::new());
}

// ---- reports, anomalies, stats -----------------------------------------------------------------

#[tokio::test]
async fn reports_list_and_resolve_anomalies_and_stats() {
    let (w, bob, eve) = bob_and_eve().await;
    let r1 = w.report(eve, bob, 1, ReportCategory::Cheating, 0.8, NOW).await;
    w.report(bob, eve, 1, ReportCategory::Abuse, 0.1, NOW).await;
    w.set_integrity(eve, IntegrityLevel::Suspected, 3.6, json!({})).await;
    let list = w.run(&["reports", "list", "--json"]).await.json();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|r| r["username"].as_str().unwrap()).collect();
    assert_eq!(names, ["eve", "bob"]);
    assert_eq!(list[1]["ids"], json!([r1]));
    let id = r1.to_string();
    let maybe = w.run(&["reports", "resolve", &id, "maybe"]).await;
    assert_eq!((maybe.code, maybe.err.as_str()), (1, "error: the outcome is actioned or dismissed\n"));
    w.run(&["reports", "resolve", &id, "dismissed"]).await.ok();
    let stored = w.store.reports().for_reported(bob, 10).await.unwrap();
    assert_eq!(
        (stored[0].status, stored[0].resolved_by.as_deref()),
        (ReportStatus::Dismissed, Some(MODERATOR))
    );
    let again = w.run(&["reports", "resolve", &id, "actioned"]).await;
    assert_eq!((again.code, again.err), (1, format!("error: report #{r1} not found or already resolved\n")));
    let anomaly = NewAnomaly {
        user_id: Some(bob),
        game_id: Some(5),
        kind: "bad_seq".into(),
        severity: Severity::Suspicious,
        at: Some(NOW),
        detail: Some(json!("{\"count\":3}")),
    };
    w.store.anomalies().insert_batch(vec![anomaly]).await.unwrap();
    let an = w.run(&["anomalies", "bob", "--json"]).await.json();
    assert_eq!(an[0]["detail"]["count"], 3);
    assert!(w.run(&["anomalies", "bob"]).await.ok().out.contains("{\"count\":3}"));
    let st = w.run(&["stats", "--json"]).await.json();
    assert_eq!(st["integrity"], json!({ "suspected": 1, "high_confidence": 0, "confirmed": 0 }));
    assert_eq!(st["openReports"], 1);
    assert_eq!(w.actions().await, ["report_resolve"]);
}

#[tokio::test]
async fn integrity_show_and_reports_list_print_the_report_dates_and_outcomes() {
    let w = World::new(&[], &["bob", "eve", "ann"]).await;
    let [bob, eve, ann] = w.ids[..] else { unreachable!() };
    let r1 = w.report_with(eve, bob, 0, ReportCategory::Cheating, Some("first"), 1.0, NOW - 2 * HOUR).await;
    let r2 = w.report_with(ann, bob, 0, ReportCategory::Cheating, Some("second"), 0.5, NOW - HOUR).await;
    w.store.reports().resolve(r1, ReportStatus::Dismissed, Some("mod".into()), NOW - 60_000).await.unwrap();
    let show = w.run(&["integrity", "show", "bob"]).await.ok().out;
    let cells = |id: i64| {
        let r = row(&show, &id.to_string());
        (r[1].clone(), r[5].clone())
    };
    assert_eq!(cells(r1), (text::iso(Some(NOW - 2 * HOUR)), "dismissed".to_string()));
    assert_eq!(cells(r2), (text::iso(Some(NOW - HOUR)), "open".to_string()));
    assert!(!text::iso(Some(NOW)).contains(".000"));
    let list = w.run(&["reports", "list", "--json"]).await.json();
    assert_eq!(
        list,
        json!([{ "reportedId": bob, "username": "bob", "level": "none", "score": 0, "priority": list[0]["priority"],
        "open": 1, "weight": 0.5, "categories": "cheating", "ids": [r2], "latest": NOW - HOUR }])
    );
}

#[tokio::test]
async fn integrity_confirm_and_clear_resolve_every_open_cheating_report() {
    let w = World::new(&[], &["bob", "eve", "ann"]).await;
    let [bob, eve, ann] = w.ids[..] else { unreachable!() };
    // Per reported player: 20 old open cheating reports, then 200 newer ones, resolved or about
    // abuse.
    w.store
        .write(move |db| {
            for target in [bob, eve] {
                let file = |game: GameId, category, at| {
                    db.reports().create(&NewReport {
                        reporter_id: ann,
                        reported_id: target,
                        game_id: Some(game),
                        category,
                        comment: None,
                        weight: 0.1,
                        at,
                    })
                };
                for i in 0..20 {
                    file(1000 + i, ReportCategory::Cheating, NOW - 10 * DAY + i as i64)?;
                }
                for i in 0..200 {
                    let category = if i % 2 == 1 { ReportCategory::Abuse } else { ReportCategory::Cheating };
                    let id = file(2000 + i, category, NOW - DAY + i as i64)?;
                    db.reports().resolve(id, ReportStatus::Dismissed, Some("mod"), NOW - DAY + i as i64)?;
                }
            }
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let open = |user: UserId| {
        let store = w.store.clone();
        async move {
            let open = store.reports().list_open(1000).await.unwrap();
            open.into_iter().filter(|r| r.reported_id == user).count()
        }
    };
    let resolved = |user: UserId, status: ReportStatus| {
        let store = w.store.clone();
        async move {
            let filed = store.reports().for_reporter(ann, 1000).await.unwrap();
            filed
                .iter()
                .filter(|r| {
                    r.reported_id == user && r.status == status && r.resolved_by.as_deref() == Some(MODERATOR)
                })
                .count()
        }
    };
    assert_eq!(open(eve).await, 20);
    let clr = w.run(&["integrity", "clear", "eve", "--dismiss-reports", "--json"]).await.json();
    assert_eq!(clr["reportsDismissed"], 20);
    assert_eq!(open(eve).await, 0);
    assert_eq!(resolved(eve, ReportStatus::Dismissed).await, 20);
    let conf =
        w.run(&["integrity", "confirm", "bob", "--reason", "engine", "--no-refund", "--json"]).await.json();
    assert_eq!(conf["reportsActioned"], 20);
    assert_eq!(open(bob).await, 0);
    assert_eq!(resolved(bob, ReportStatus::Actioned).await, 20);
}

#[tokio::test]
async fn the_commands_count_and_weigh_every_report() {
    let w = World::new(&[], &["bob", "ann"]).await;
    let [bob, ann] = w.ids[..] else { unreachable!() };
    w.set_integrity(bob, IntegrityLevel::Suspected, 2.0, json!({})).await;
    // One open report older than 30 days, then 250 of weight 0.1 (25 in all), the 30 oldest open.
    w.report(ann, bob, 1, ReportCategory::Cheating, 1.0, NOW - 40 * DAY).await;
    w.store
        .write(move |db| {
            for i in 0..250 {
                let id = db.reports().create(&NewReport {
                    reporter_id: ann,
                    reported_id: bob,
                    game_id: Some(100 + i),
                    category: ReportCategory::Cheating,
                    comment: None,
                    weight: 0.1,
                    at: NOW - 20 * DAY + i as i64 * 60_000,
                })?;
                if i >= 30 {
                    db.reports().resolve(id, ReportStatus::Dismissed, Some("mod"), NOW)?;
                }
            }
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let priority = review_priority(from_store_level(IntegrityLevel::Suspected), 2.0, 25.0);
    let user = w.run(&["user", "show", "bob", "--json"]).await.json();
    assert_eq!(user["reports"], json!({ "total": 251, "open": 31, "weight30d": 25 }));
    assert!(w.run(&["user", "show", "bob"]).await.out.contains("reports received: 251 (31 open)"));
    let list = w.run(&["integrity", "list", "--json"]).await.json();
    assert_eq!(
        list,
        json!([{ "userId": bob, "username": "bob", "level": "suspected", "score": 2, "priority": priority,
        "reports30d": 25, "games": "-", "updatedAt": NOW, "reviewedBy": null }])
    );
    assert_eq!(w.run(&["integrity", "show", "bob", "--json"]).await.json()["priority"], priority);
    let open = w.run(&["reports", "list", "--json"]).await.json();
    let rows: Vec<Value> = open
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["username"], r["open"], r["weight"], r["priority"]]))
        .collect();
    assert_eq!(rows, [json!(["bob", 31, 4, priority])]);
}

// ---- analysis requests -------------------------------------------------------------------------

/// The status and priority of the analysis jobs of games.
async fn jobs(store: &Store, games: &[&GameRecord]) -> Vec<(JobStatus, Priority)> {
    let mut out = Vec::new();
    for g in games {
        let j = store.analysis().job(g.id).await.unwrap().expect("a job");
        out.push((j.status, j.priority));
    }
    out
}

#[tokio::test]
async fn analysis_queue_puts_the_game_first_and_leaves_a_game_being_or_already_analysed_alone() {
    use JobStatus::{Done, Failed, Queued, Running};
    let w = World::new(&[], &["ann", "ben"]).await;
    let [ann, ben] = w.ids[..] else { unreachable!() };
    let rated = || game(ann, ben, WHITE_WINS, NOW);
    // Rated games are queued at the ordinary priority; the casual and the short one are left out.
    let (running, done, failed, ordinary, reported) = (rated(), rated(), rated(), rated(), rated());
    let mut casual = rated();
    casual.rated = false;
    let mut short = rated();
    short.moves = vec![0; 8];
    short.spent_ms = Some(vec![0; 8]);
    short.clock_ms = Some(vec![0; 8]);
    let ids = |games: &[&GameRecord]| games.iter().map(|g| g.id).collect::<Vec<_>>();
    let batch = [&running, &done, &failed, &ordinary, &reported, &casual, &short];
    w.finish(batch.iter().map(|g| (*g).clone()).collect()).await;
    let analysis = w.store.analysis();
    let claimed = analysis.next(3, Some("w".into()), NOW).await.unwrap();
    assert_eq!(claimed.iter().map(|j| j.game_id).collect::<Vec<_>>(), ids(&[&running, &done, &failed]));
    analysis.complete(done.id, Some(json!({ "gameId": done.id })), NOW).await.unwrap();
    for _ in 0..3 {
        if analysis.fail(failed.id, Some("engine crashed".into()), NOW).await.unwrap()
            == Some(JobStatus::Queued)
        {
            assert_eq!(analysis.next(1, Some("w".into()), NOW).await.unwrap()[0].game_id, failed.id);
        }
    }
    assert!(analysis.request(reported.id, Priority::Report, NOW).await.unwrap());
    assert_eq!(
        jobs(&w.store, &[&running, &done, &failed, &ordinary, &reported]).await,
        [
            (Running, Priority::Ordinary),
            (Done, Priority::Ordinary),
            (Failed, Priority::Ordinary),
            (Queued, Priority::Ordinary),
            (Queued, Priority::Report)
        ]
    );
    assert!(analysis.job(casual.id).await.unwrap().is_none());
    assert!(analysis.job(short.id).await.unwrap().is_none());

    let bad_args: [&[&str]; 5] = [&[], &["abc"], &["0"], &["-3"], &["1.5"]];
    for bad in bad_args {
        let mut argv = vec!["analysis", "queue"];
        argv.extend_from_slice(bad);
        let r = w.run(&argv).await;
        assert_eq!(
            (r.code, r.err.as_str()),
            (1, "error: analysis queue <gameId>: the number of a game\n"),
            "{bad:?}"
        );
    }
    let missing = w.run(&["analysis", "queue", "424242"]).await;
    assert_eq!((missing.code, missing.err.as_str()), (1, "error: no game #424242\n"));

    let logs = LogCapture::start();
    let first = w.run(&["analysis", "queue", &casual.id.to_string(), "--json"]).await;
    assert_eq!(first.json(), json!({ "gameId": casual.id, "queued": true, "previous": null }));
    // The log record leaves the null fields (userId, previousStatus) out.
    assert_eq!(
        w.security(&logs),
        [(
            "moderator.action".to_string(),
            json!({ "action": "analysis_queue", "moderator": MODERATOR, "gameId": casual.id })
        )]
    );
    let audit = w.moderator_actions().await;
    assert_eq!(
        audit,
        [
            json!({ "action": "analysis_queue", "moderator": MODERATOR, "gameId": casual.id, "previousStatus": null })
        ]
    );
    let said = |r: Ran, text: &str| assert!(r.ok().out.contains(text), "{text}");
    said(
        w.run(&["analysis", "queue", &short.id.to_string()]).await,
        "queued for engine analysis before every other game; it was not in the queue (a casual or short game",
    );
    said(
        w.run(&["analysis", "queue", &ordinary.id.to_string()]).await,
        "; it was waiting at priority ordinary.",
    );
    said(
        w.run(&["analysis", "queue", &failed.id.to_string()]).await,
        "its analysis had failed (engine crashed)",
    );
    assert_eq!(analysis.job(failed.id).await.unwrap().unwrap().attempts, 0);
    assert_eq!(w.security(&logs).len(), 4);
    for (g, text) in [
        (&running, "is being analysed now"),
        (&done, "was already analysed"),
        (&casual, "is already queued before every other game"),
    ] {
        said(w.run(&["analysis", "queue", &g.id.to_string()]).await, text);
    }
    assert_eq!(w.security(&logs).len(), 4, "nothing changed, nothing audited");
    drop(logs);
    let manual = (Queued, Priority::Manual);
    assert_eq!(
        jobs(&w.store, &[&running, &done, &casual, &short, &ordinary, &failed]).await,
        [(Running, Priority::Ordinary), (Done, Priority::Ordinary), manual, manual, manual, manual]
    );
    // The engine takes the four requested games before the reported one.
    let mut next: Vec<GameId> =
        analysis.next(4, Some("w".into()), NOW).await.unwrap().iter().map(|j| j.game_id).collect();
    next.sort_unstable();
    let mut requested = ids(&[&ordinary, &failed, &casual, &short]);
    requested.sort_unstable();
    assert_eq!(next, requested);
    assert_eq!(jobs(&w.store, &[&reported]).await, [(Queued, Priority::Report)]);
    assert_eq!(w.actions().await.len(), 4);
}

// ---- bench accounts ----------------------------------------------------------------------------

fn is_token(t: &str) -> bool {
    t.len() == 47
        && t.starts_with("sct_")
        && t[4..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn mode(path: &str) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn hash_for_test(token: &str) -> String {
    format!("H:{}", crate::security::keys::sha256_hex(token))
}

#[tokio::test]
async fn bench_accounts_are_refused_without_the_test_server_flag_and_are_verified_with_sessions() {
    let (w, ..) = bob_and_eve().await;
    let dir = TempDir::new("bench");
    let out = dir.file("tokens.txt");
    let refused = w.run(&["bench-accounts", "--count", "3", "--out", &out]).await;
    assert_eq!(refused.code, 1);
    assert!(refused.err.contains("--i-know-this-is-a-test-server"), "{}", refused.err);
    assert!(!std::path::Path::new(&out).exists());

    let env = AdminEnv { hash_token: hash_for_test, ..w.env.clone() };
    let argv = [
        "bench-accounts",
        "--count",
        "3",
        "--prefix",
        "bench",
        "--out",
        &out,
        "--i-know-this-is-a-test-server",
    ];
    let r = run_with(&env, &argv).await.ok();
    assert!(r.out.contains("3 bench accounts ready (3 created, 0 reused)"), "{}", r.out);
    let file = std::fs::read_to_string(&out).unwrap();
    let tokens: Vec<&str> = file.trim().split('\n').collect();
    assert_eq!(tokens.len(), 3);
    assert!(tokens.iter().all(|t| is_token(t)), "{tokens:?}");
    assert_eq!(mode(&out), 0o600);
    let u = w.user("bench0002").await.unwrap();
    assert!(u.email_verified);
    assert_eq!(u.email.as_deref(), Some("bench0002@bench.invalid"));
    let sessions = w.store.sessions().list_for_user(u.id).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].expires_at > NOW);
    let auth =
        w.store.sessions().by_token_hash(hash_for_test(tokens[1])).await.unwrap().expect("the session");
    assert_eq!(auth.user_id, u.id);

    // Running again reuses the accounts and adds sessions; an existing file is made 600 too.
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644)).unwrap();
    let tsv = [
        "bench-accounts",
        "--count",
        "3",
        "--out",
        &out,
        "--format",
        "tsv",
        "--i-know-this-is-a-test-server",
    ];
    let again = w.run(&tsv).await.ok();
    assert!(again.out.contains("0 created, 3 reused"), "{}", again.out);
    let file = std::fs::read_to_string(&out).unwrap();
    assert!(file.starts_with("bench0001\tsct_"), "{file}");
    assert_eq!(file.trim().split('\n').count(), 3);
    assert_eq!(mode(&out), 0o600);
    // A real account with a matching name is never taken over, and nothing of the run is kept.
    users(&w.store, &["bench0004"]).await;
    let taken =
        w.run(&["bench-accounts", "--count", "4", "--out", &out, "--i-know-this-is-a-test-server"]).await;
    assert_eq!(
        (taken.code, taken.err.as_str()),
        (1, "error: \"bench0004\" exists and is not a bench account\n")
    );
    assert_eq!(w.live_sessions(u.id).await, 2, "the sessions of the refused run are not kept");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), file, "the token file is left alone");
    assert_eq!(w.actions().await, ["bench_accounts", "bench_accounts"]);
}

#[tokio::test]
async fn bench_accounts_write_to_a_pipe_without_changing_its_mode_and_refuse_another_users_file() {
    let (w, ..) = bob_and_eve().await;
    let dir = TempDir::new("bench-fifo");
    // A FIFO stands for a pipe or /dev/null: written to, never made 600.
    let fifo = dir.file("tokens.fifo");
    let made = std::process::Command::new("mkfifo").args(["-m", "644", &fifo]).status().expect("mkfifo runs");
    assert!(made.success());
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .expect("reader opens");
    w.run(&["bench-accounts", "--count", "2", "--out", &fifo, "--i-know-this-is-a-test-server"]).await.ok();
    let mut buf = [0u8; 4096];
    let n = reader.read(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    let tokens: Vec<&str> = text.trim().split('\n').collect();
    assert_eq!(tokens.len(), 2);
    assert!(tokens.iter().all(|t| is_token(t)), "{tokens:?}");
    assert_eq!(mode(&fifo), 0o644);

    // A regular file of another owner cannot be made 600: refused up front.
    let out = dir.file("theirs.txt");
    std::fs::write(&out, "kept\n").unwrap();
    let env = AdminEnv { euid: Some(std::fs::metadata(&out).unwrap().uid() + 1), ..w.env.clone() };
    let argv = [
        "bench-accounts",
        "--count",
        "3",
        "--prefix",
        "other",
        "--out",
        &out,
        "--i-know-this-is-a-test-server",
    ];
    let refused = run_with(&env, &argv).await;
    assert_eq!(refused.code, 1);
    assert!(refused.err.contains("belongs to another user"), "{}", refused.err);
    assert!(w.user("other0001").await.is_none());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "kept\n");
}

#[tokio::test]
async fn bench_accounts_refuse_a_symbolic_link_before_any_account_is_created() {
    let (w, ..) = bob_and_eve().await;
    let dir = TempDir::new("bench-link");
    let target = dir.file("elsewhere.txt");
    std::fs::write(&target, "kept\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let link = dir.file("tokens.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let argv = ["bench-accounts", "--count", "2", "--out", &link, "--i-know-this-is-a-test-server"];
    let refused = w.run(&argv).await;
    assert_eq!(refused.code, 1);
    assert!(refused.err.contains("is a symbolic link"), "{}", refused.err);
    assert!(w.user("bench0001").await.is_none());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept\n");
    assert_eq!(mode(&target), 0o644);
    // A dangling link (the tokens would create the file it names) is refused the same way.
    std::fs::remove_file(&target).unwrap();
    let dangling = w.run(&argv).await;
    assert_eq!(dangling.code, 1);
    assert!(dangling.err.contains("is a symbolic link"), "{}", dangling.err);
    assert!(!std::path::Path::new(&target).exists());
}

// ---- backups and the process -------------------------------------------------------------------

#[tokio::test]
async fn backup_refuses_a_source_that_is_not_a_scacelith_database_and_creates_nothing() {
    let w = World::new(&[], &[]).await;
    let dir = TempDir::new("backup-src");
    let target = dir.file("copy.db");
    let backup = |db_path: String| {
        let env = AdminEnv { config: Arc::new(config(&[("DB_PATH", db_path.as_str())])), ..w.env.clone() };
        let target = target.clone();
        async move { run_with(&env, &["backup", &target, "--verify"]).await }
    };
    // A DB_PATH that does not exist (a wrong DATA_DIR, or a relative one run from another
    // directory): no empty database created there, no empty backup written.
    let missing = dir.file("data/scacelith.db");
    let r1 = backup(missing.clone()).await;
    assert_eq!(
        (r1.code, r1.err),
        (
            1,
            format!("error: no Scacelith database at {missing} (no such file); check DB_PATH and DATA_DIR\n")
        )
    );
    assert!(!dir.path().join("data").exists());
    assert!(!std::path::Path::new(&target).exists());
    // An SQLite database without the server's schema.
    let other = dir.file("other.db");
    rusqlite::Connection::open(&other).unwrap().execute_batch("CREATE TABLE notes (x TEXT)").unwrap();
    let r2 = backup(other.clone()).await;
    assert_eq!(
        (r2.code, r2.err),
        (
            1,
            format!(
                "error: no Scacelith database at {other} (no schema_migrations); check DB_PATH and DATA_DIR\n"
            )
        )
    );
    assert!(!std::path::Path::new(&target).exists());
    // A file that is not a database at all.
    let notes = dir.file("notes.txt");
    let body = "not a database\n".repeat(500);
    std::fs::write(&notes, &body).unwrap();
    let r3 = backup(notes.clone()).await;
    assert_eq!(r3.code, 1);
    assert!(r3.err.starts_with(&format!("error: no Scacelith database at {notes} (")), "{}", r3.err);
    assert!(!std::path::Path::new(&target).exists());
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), body, "the source is left alone");
    // The in-memory database of the tests is not a file.
    assert_eq!(w.run(&["backup", &target]).await.err, "error: no database file to back up (DB_PATH)\n");
}

#[tokio::test]
async fn backup_is_a_consistent_copy_of_mode_600_that_never_overwrites() {
    let dir = TempDir::new("backup");
    let file = dir.file("scacelith.db");
    let cfg = config(&[("DB_PATH", file.as_str())]);
    let store = Store::open(&cfg, StoreOptions::default()).await.unwrap();
    store.migrate().await.unwrap();
    let id = users(&store, &["Ann"]).await[0];
    let env = AdminEnv::new(store.clone(), Arc::new(cfg), MODERATOR.into());
    let target = dir.file("copy.db");
    let r = run_with(&env, &["backup", &target, "--verify", "--json"]).await;
    let j = r.json();
    assert_eq!((j["verified"].clone(), j["file"].clone()), (json!(true), json!(target)));
    assert!(j["bytes"].as_u64().unwrap() > 0);
    assert_eq!(mode(&target), 0o600);
    let copy =
        rusqlite::Connection::open_with_flags(&target, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let name: String =
        copy.query_row("SELECT username FROM users WHERE id = ?1", [id], |r| r.get(0)).unwrap();
    assert_eq!(name, "Ann");
    drop(copy);
    let text = run_with(&env, &["backup", &dir.file("text.db")]).await.ok().out;
    assert!(
        text.starts_with(&format!("Backup written to {} (", dir.file("text.db")))
            && text.ends_with(" ms).\n"),
        "{text}"
    );
    let again = run_with(&env, &["backup", &target]).await;
    assert_eq!((again.code, again.err), (1, format!("error: {target} exists: choose a new file\n")));
    let none = run_with(&env, &["backup"]).await;
    assert_eq!((none.code, none.err.as_str()), (1, "error: backup <file>: the new file to write\n"));
    store.close().await;
}

/// Runs the process entry point with a configuration.
fn process(args: &[&str], config: Result<Config, ConfigError>) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_process(&strings(args), move || config, false, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn the_process_prints_its_help_before_loading_the_configuration() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_process(&strings(&["--help"]), || panic!("not loaded"), false, &mut out, &mut err);
    assert_eq!((code, out.as_slice(), err.as_slice()), (0, USAGE.as_bytes(), &b""[..]));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(run_process(&strings(&["help"]), || panic!("not loaded"), false, &mut out, &mut err), 0);
    assert_eq!(run_process(&[], || panic!("not loaded"), false, &mut out, &mut err), 2);
    let invalid = ConfigError { errors: vec!["SERVER_SECRET is required".into()] };
    assert_eq!(
        process(&["stats"], Err(invalid)),
        (1, String::new(), "Invalid configuration:\n  - SERVER_SECRET is required\n".into())
    );
}

#[test]
fn the_process_refuses_a_missing_database_instead_of_creating_an_empty_one() {
    let dir = TempDir::new("admin-cwd");
    let db = dir.file("data/scacelith.db");
    let cfg = config(&[("DB_PATH", db.as_str())]);
    let copy = dir.file("copy.db");
    let runs: [&[&str]; 2] = [&["stats"], &["backup", &copy, "--verify"]];
    for args in runs {
        let (code, out, err) = process(args, Ok(cfg.clone()));
        assert_eq!((code, out.as_str()), (1, ""), "{args:?}");
        assert_eq!(
            err,
            format!(
                "error: no Scacelith database at {db}; check DB_PATH and DATA_DIR (a relative DATA_DIR is resolved \
                 from the current directory)\n"
            )
        );
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no data directory, no database, no backup"
    );
}

#[test]
fn the_process_runs_a_command_on_the_servers_database() {
    let dir = TempDir::new("admin-process");
    let file = dir.file("scacelith.db");
    let cfg = config(&[("DB_PATH", file.as_str())]);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let store = Store::open(&cfg, StoreOptions::default()).await.unwrap();
        store.migrate().await.unwrap();
        store.close().await;
    });
    drop(runtime);
    let (code, out, err) = process(&["stats", "--json"], Ok(cfg.clone()));
    assert_eq!((code, err.as_str()), (0, ""));
    let stats: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        stats,
        json!({ "integrity": { "suspected": 0, "high_confidence": 0, "confirmed": 0 }, "openReports": 0, "store": null })
    );
    let (code, _, err) = process(&["user", "show", "nobody"], Ok(cfg));
    assert_eq!((code, err.as_str()), (1, "error: no user named \"nobody\"\n"));
}

// ---- rating refunds (anticheat.refunds) --------------------------------------------------------

fn sorted(mut ids: Vec<GameId>) -> Vec<GameId> {
    ids.sort_unstable();
    ids
}

fn game_ids(list: &Value) -> Vec<GameId> {
    sorted(list.as_array().unwrap().iter().map(|r| r["gameId"].as_u64().unwrap()).collect())
}

#[tokio::test]
async fn integrity_confirm_refunds_the_window_and_refunds_apply_and_list_give_and_show_the_rest() {
    let w = World::refunds(&[]).await;
    let [cheat, vic, _, _, vold, ..] = w.ids[..] else { unreachable!() };
    let g = play_refund_scenario(&w.store, &w.ids).await;
    let vold_rating = w.rating(vold).await.rating;

    // --no-refund: a ban without refunds; --refund-since and --no-refund exclude each other.
    let confirm = ["integrity", "confirm", "Cheat", "--reason", "r"];
    let both = w.run(&[&confirm[..], &["--no-refund", "--refund-since", "2026-01-01"]].concat()).await;
    assert_eq!(
        (both.code, both.err.as_str()),
        (1, "error: --refund-since and --no-refund exclude each other\n")
    );
    let future = w.run(&[&confirm[..], &["--refund-since", "2099-01-01"]].concat()).await;
    assert_eq!((future.code, future.err.as_str()), (1, "error: --refund-since is in the future\n"));
    assert_eq!(
        w.run(&[&confirm[..], &["--refund-since", "2026-02-29"]].concat()).await.code,
        1,
        "no such day"
    );
    assert_eq!(w.level(cheat).await, IntegrityLevel::None, "a refused command writes nothing");
    assert!(w.active_ban(cheat, NOW).await.is_none());
    let no_refund =
        w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--no-refund", "--json"]).await;
    let no_refund = no_refund.json();
    assert_eq!((no_refund["refunds"].clone(), no_refund["refundSince"].clone()), (json!([]), Value::Null));
    assert!(w.refunds_of(RefundScope::All).await.is_empty());

    // refunds apply: a confirmed cheater only; the window counts back from the latest ban for
    // cheating.
    let vic_apply = w.run(&["refunds", "apply", "Vic"]).await;
    assert_eq!(
        (vic_apply.code, vic_apply.err.as_str()),
        (1, "error: Vic is not a confirmed cheater (integrity confirm first)\n")
    );
    assert_eq!(
        w.run(&["refunds", "apply", "Cheat", "--since", "2099-01-01"]).await.code,
        1,
        "a date in the future"
    );
    assert_eq!(w.run(&["refunds", "apply", "Cheat", "--since", "yesterday"]).await.code, 1);
    // An impossible calendar date is refused, not rolled over into the next month.
    for typo in ["2026-02-31", "2026-04-31T10:00Z", "2026-00-10", "2026-06-00"] {
        let r = w.run(&["refunds", "apply", "Cheat", "--since", typo]).await;
        assert_eq!(r.code, 1, "{typo}");
        assert!(r.err.contains("expects a date"), "{}", r.err);
    }
    let apply = w.run_at(NOW + 3 * DAY, &["refunds", "apply", "Cheat", "--json"]).await.json();
    assert_eq!(apply["since"], NOW - 60 * DAY, "RATING_REFUND_DAYS before the ban, not before the command");
    assert_eq!(game_ids(&apply["refunds"]), sorted(vec![g.vic_loss.id, g.val_draw.id]));
    assert_eq!(w.rating(vold).await.rating, vold_rating, "outside the window");

    // --since reaches the older game; the games already refunded are skipped.
    let since = text::date_of(NOW - 80 * DAY);
    let wider = w.run(&["refunds", "apply", "Cheat", "--since", &since]).await.ok();
    assert!(wider.out.contains("1 game(s), 10 point(s) to 1 player(s)"), "{}", wider.out);
    assert_eq!(w.rating(vold).await.rating, vold_rating + 10);
    let rows = w.refunds_of(RefundScope::All).await;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.source == Source::Moderator && r.created_by.as_deref() == Some(MODERATOR)));

    // Audit: one moderator_action per command, one rating_refund event per refund.
    let kinds = query_json(
        &w.store,
        "SELECT json_group_array(coalesce(json_extract(detail, '$.action'), kind)) FROM
         (SELECT kind, detail FROM security_events ORDER BY id)",
    )
    .await;
    assert_eq!(
        kinds,
        json!([
            "integrity_confirm",
            "rating_refund",
            "rating_refund",
            "refunds_apply",
            "rating_refund",
            "refunds_apply"
        ])
    );

    // refunds list: by cheater, by victim, all.
    assert_eq!(w.run(&["refunds", "list", "Cheat", "--json"]).await.json().as_array().unwrap().len(), 3);
    let by_victim = w.run(&["refunds", "list", "--victim", "Vold", "--json"]).await.json();
    assert_eq!(by_victim[0]["gameId"], g.old.id);
    assert_eq!(
        (by_victim[0]["points"].clone(), by_victim[0]["cheaterName"].clone()),
        (json!(10), json!("Cheat"))
    );
    assert_eq!(by_victim.as_array().unwrap().len(), 1);
    let both = w.run(&["refunds", "list", "Cheat", "--victim", "Vold"]).await;
    assert_eq!((both.code, both.err.as_str()), (1, "error: give a cheater or --victim, not both\n"));
    let text = w.run(&["refunds", "list"]).await.ok().out;
    assert!(has_cells(&text, &["Vold", "3+2", "10", "moderator", MODERATOR, "not", "yet"]), "{text}");
    assert!(w.store.refunds().pending_for(vic).await.unwrap().points > 0);
}

#[tokio::test]
async fn integrity_confirm_refunds_rating_refund_days_back_by_default_or_from_refund_since() {
    let w = World::refunds(&[]).await;
    let cheat = w.ids[0];
    let g = play_refund_scenario(&w.store, &w.ids).await;
    let since = crate::log::iso_time(NOW - 80 * DAY);
    let conf =
        w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--refund-since", &since]).await.ok();
    assert!(conf.out.contains(": 3 game(s)"), "{}", conf.out);
    let ban = w.active_ban(cheat, NOW).await.unwrap();
    let rows = w.refunds_of(RefundScope::Cheater(cheat)).await;
    assert_eq!(
        sorted(rows.iter().map(|r| r.game_id).collect()),
        sorted(vec![g.old.id, g.vic_loss.id, g.val_draw.id])
    );
    assert!(rows.iter().all(|r| r.sanction_id == Some(ban.id) && r.source == Source::Moderator));

    let w2 = World::refunds(&[]).await;
    play_refund_scenario(&w2.store, &w2.ids).await;
    let def = w2.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--json"]).await.json();
    assert_eq!(def["refundSince"], NOW - 60 * DAY);
    assert_eq!(def["refunds"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn refunds_that_fail_leave_the_ban_standing_and_audited_and_say_how_to_give_them_later() {
    let w = World::refunds(&[]).await;
    let cheat = w.ids[0];
    let g = play_refund_scenario(&w.store, &w.ids).await;
    exec(
        &w.store,
        "CREATE TRIGGER no_refunds BEFORE INSERT ON rating_refunds BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
    )
    .await;
    let logs = LogCapture::start();
    let conf = w.run(&["integrity", "confirm", "Cheat", "--reason", "engine"]).await;
    assert_eq!(conf.code, 1);
    assert!(conf.err.starts_with("error: Cheat: integrity confirmed and banned until "), "{}", conf.err);
    assert!(
        conf.err.ends_with(
            ", but the rating refunds failed (database is locked): give them with `refunds apply Cheat`\n"
        ),
        "{}",
        conf.err
    );
    let lines = w.security(&logs);
    assert_eq!(lines.len(), 1, "the ban is audited: {lines:?}");
    assert_eq!(
        (lines[0].0.as_str(), &lines[0].1["refundError"]),
        ("moderator.action", &json!("database is locked"))
    );
    drop(logs);
    assert!(w.active_ban(cheat, NOW).await.is_some());
    assert_eq!(w.level(cheat).await, IntegrityLevel::Confirmed);
    let actions = w.moderator_actions().await;
    assert_eq!(actions.len(), 1);
    let d = &actions[0];
    assert_eq!(
        (d["action"].clone(), d["refundError"].clone(), d["refunds"].clone()),
        (json!("integrity_confirm"), json!("database is locked"), json!(0))
    );
    // The failure names the window of --refund-since.
    let since =
        w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--refund-since", "2026-06-01"]).await;
    assert!(since.err.ends_with("`refunds apply Cheat --since 2026-06-01T00:00:00Z`\n"), "{}", since.err);
    exec(&w.store, "DROP TRIGGER no_refunds;").await;
    let later = w.run(&["refunds", "apply", "Cheat", "--json"]).await.json();
    assert_eq!(game_ids(&later["refunds"]), sorted(vec![g.vic_loss.id, g.val_draw.id]));
}

#[tokio::test]
async fn a_game_recorded_against_a_confirmed_cheater_under_an_active_ban_is_refunded_as_it_is_recorded() {
    let w = World::refunds(&[]).await;
    let [cheat, vic, val, vera, vold, omar, _] = w.ids[..] else { unreachable!() };
    // The moderator confirms while games against the cheater are still being played or on their
    // way to the database.
    let conf = w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--json"]).await.json();
    assert_eq!(conf["refunds"], json!([]), "no game recorded yet");
    let ban = w.active_ban(cheat, NOW).await.unwrap();
    let late = game(cheat, vic, WHITE_WINS, NOW - 1000); // ended before the confirm, recorded after it
    let in_play = game(vold, cheat, BLACK_WINS, NOW + 1000); // in progress at the confirm
    let val_draw = game(val, cheat, DRAW, NOW + 2000); // the higher-rated Val loses points
    let vera_win = game(vera, cheat, WHITE_WINS, NOW + 3000); // a win: untouched
    let vic_omar = game(omar, vic, WHITE_WINS, NOW + 4000); // a loss to someone else: untouched
    let logs = LogCapture::start();
    let batch = vec![late.clone(), in_play.clone(), val_draw.clone(), vera_win, vic_omar];
    // Other tests log too (with the same user ids): the game ids are unique to this one.
    let ours: Vec<Value> = batch.iter().map(|g| json!(g.id)).collect();
    let res = w.finish(batch).await;
    let refund_logs: Vec<(Value, Value, Value)> = logs
        .records("store")
        .into_iter()
        .filter(|r| r["msg"] == "rating.refund" && ours.contains(&r["gameId"]))
        .map(|r| (r["cheaterId"].clone(), r["gameId"].clone(), r["points"].clone()))
        .collect();
    drop(logs);
    let lost_in = |i: usize, white: bool| {
        let r = res[i].ratings.as_ref().unwrap();
        let side = if white { &r.white } else { &r.black };
        side.before - side.after
    };
    assert_eq!(lost_in(0, false), 10, "the games keep their rating changes as played");
    assert!(lost_in(1, true) > 0 && lost_in(2, true) > 0);
    assert_eq!(
        w.rating(vic).await.rating,
        1490,
        "the loss to the cheater is given back, the loss to Omar stands"
    );
    assert_eq!(w.rating(vold).await.rating, 1500);
    assert_eq!(w.rating(val).await.rating, 1700);
    assert!(w.rating(vera).await.rating > 1500);

    let mut rows: Vec<_> = w
        .refunds_of(RefundScope::Cheater(cheat))
        .await
        .into_iter()
        .map(|x| (x.game_id, x.victim_name, x.points, x.source, x.sanction_id, x.created_by))
        .collect();
    rows.sort_by_key(|r| r.0);
    let auto = |g: &GameRecord, name: &str, points| {
        (g.id, name.to_string(), points, Source::Auto, Some(ban.id), None)
    };
    assert_eq!(
        rows,
        [
            auto(&late, "Vic", 10),
            auto(&in_play, "Vold", lost_in(1, true)),
            auto(&val_draw, "Val", lost_in(2, true))
        ]
    );
    // The victims are told as for any refund (Notice{RatingRestored}, out of a game).
    assert_eq!(w.store.refunds().pending_for(vic).await.unwrap().points, 10);
    let events = query_json(
        &w.store,
        "SELECT json_group_array(json_array(user_id, json_extract(detail, '$.gameId'), json_extract(detail, '$.source'),
         json_extract(detail, '$.sanctionId'))) FROM (SELECT user_id, detail FROM security_events
         WHERE kind = 'rating_refund' ORDER BY id)",
    )
    .await;
    assert_eq!(
        events,
        json!([
            [vic, late.id, "auto", ban.id],
            [vold, in_play.id, "auto", ban.id],
            [val, val_draw.id, "auto", ban.id]
        ])
    );
    assert_eq!(
        refund_logs,
        [
            (json!(cheat), json!(late.id), json!(10)),
            (json!(cheat), json!(in_play.id), json!(lost_in(1, true))),
            (json!(cheat), json!(val_draw.id), json!(lost_in(2, true)))
        ]
    );

    // Nothing twice: a game committed again after a crash, or a moderator's later refunds apply.
    assert!(w.finish(vec![late]).await[0].duplicate);
    let again = w.run(&["refunds", "apply", "Cheat", "--json"]).await.json();
    assert_eq!(again["refunds"], json!([]));
    assert_eq!(w.refunds_of(RefundScope::All).await.len(), 3);
    assert_eq!(w.rating(vic).await.rating, 1490);
}

#[tokio::test]
async fn no_refund_as_a_game_is_recorded_without_a_confirmed_level_and_an_active_ban_nor_with_rating_refund_days_0()
 {
    let w = World::refunds(&[]).await;
    let [cheat, vic, _, vera, ..] = w.ids[..] else { unreachable!() };
    // A ban for something else (`user ban`): not a confirmed cheater.
    w.run(&["user", "ban", "Cheat", "--hours", "24", "--reason", "abuse"]).await.ok();
    w.finish(vec![game(cheat, vic, WHITE_WINS, NOW)]).await;
    // Confirmed, then unbanned: no active ban.
    w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--no-refund"]).await.ok();
    w.run(&["user", "unban", "Cheat"]).await.ok();
    w.finish(vec![game(cheat, vera, WHITE_WINS, NOW)]).await;
    assert!(w.refunds_of(RefundScope::All).await.is_empty());
    assert_eq!(w.rating(vic).await.rating, 1490);
    assert!(w.rating(vera).await.rating < 1500);

    // RATING_REFUND_DAYS=0 turns these refunds off with the others.
    let off = World::refunds(&[("RATING_REFUND_DAYS", "0")]).await;
    off.run(&["integrity", "confirm", "Cheat", "--reason", "engine"]).await.ok();
    off.finish(vec![game(off.ids[0], off.ids[1], WHITE_WINS, NOW)]).await;
    assert!(off.refunds_of(RefundScope::All).await.is_empty());
    assert_eq!(off.rating(off.ids[1]).await.rating, 1490);
}

#[tokio::test]
async fn no_refund_as_a_game_is_recorded_under_a_ban_for_something_else_nor_after_confirm_no_refund() {
    let w = World::refunds(&[]).await;
    let [cheat, vic, _, vera, ..] = w.ids[..] else { unreachable!() };
    // Confirmed with refunds ten days ago: that ban (24 h) is over, the level stays confirmed.
    w.run_at(NOW - 10 * DAY, &["integrity", "confirm", "Cheat", "--reason", "engine"]).await.ok();
    assert!(w.active_ban(cheat, NOW).await.is_none());
    // A `user ban` cannot pass for a ban for cheating.
    for reason in ["confirmed: engine", "confirmed, no refund: engine"] {
        let refused = w.run(&["user", "ban", "Cheat", "--hours", "24", "--reason", reason]).await;
        assert_eq!(refused.code, 1, "{reason}");
        assert!(refused.err.contains("marks a ban for cheating: use integrity confirm"), "{}", refused.err);
    }
    assert!(w.active_ban(cheat, NOW).await.is_none(), "a refused command writes nothing");
    // A ban for something else: the game in progress at it, recorded during it, stands.
    w.run(&["user", "ban", "Cheat", "--hours", "24", "--reason", "abusive chat"]).await.ok();
    w.finish(vec![game(cheat, vic, WHITE_WINS, NOW)]).await;
    // A new confirm with --no-refund: not even the game in progress at it is refunded.
    let conf = w
        .run(&["integrity", "confirm", "Cheat", "--reason", "engine again", "--no-refund", "--json"])
        .await
        .json();
    let sanctions = w.store.sanctions().list(cheat).await.unwrap();
    let ban = sanctions.iter().find(|s| json!(s.id) == conf["sanctionId"]).unwrap();
    assert_eq!(ban.reason.as_deref(), Some("confirmed, no refund: engine again"));
    w.finish(vec![game(cheat, vera, WHITE_WINS, NOW)]).await;
    assert!(w.refunds_of(RefundScope::All).await.is_empty());
    assert_eq!(w.rating(vic).await.rating, 1490);
    assert!(w.rating(vera).await.rating < 1500);
}

#[tokio::test]
async fn a_certain_cheat_under_a_moderators_ban_that_does_not_refund_gets_a_ban_of_its_own() {
    let others: [&[&str]; 2] = [
        &["user", "ban", "Cheat", "--hours", "240", "--reason", "abusive chat"],
        &["integrity", "confirm", "Cheat", "--reason", "engine", "--hours", "240", "--no-refund"],
    ];
    for other in others {
        let w = World::refunds(&[]).await;
        let [cheat, vic, ..] = w.ids[..] else { unreachable!() };
        w.run(other).await.ok();
        let ac = Anticheat::new(&w.config, w.store.clone(), w.clock.clone());
        assert!(ac.sanction(cheat, 5, "illegal_move").await.applied, "{}", other[0]);
        let sanctions = w.store.sanctions().list(cheat).await.unwrap();
        let auto = sanctions.iter().find(|s| s.source == Source::Auto).unwrap();
        assert_eq!(auto.reason.as_deref(), Some("certain_cheat:illegal_move"));
        // On its way to the database at the ban: refunded as it is recorded.
        w.finish(vec![game(cheat, vic, WHITE_WINS, NOW)]).await;
        let rows = w.refunds_of(RefundScope::All).await;
        assert_eq!(
            rows.iter().map(|x| (x.victim_name.as_str(), x.points, x.sanction_id)).collect::<Vec<_>>(),
            [("Vic", 10, Some(auto.id))],
            "{}",
            other[0]
        );
    }
}

#[tokio::test]
async fn refunds_apply_counts_back_from_the_latest_ban_for_cheating_not_from_a_later_ban_for_something_else()
{
    let w = World::refunds(&[]).await;
    let g = play_refund_scenario(&w.store, &w.ids).await;
    let conf =
        w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--no-refund", "--json"]).await.json();
    let later = NOW + 100 * DAY;
    w.run_at(later, &["user", "ban", "Cheat", "--hours", "24", "--reason", "abusive chat"]).await.ok();
    let apply = w.run_at(later, &["refunds", "apply", "Cheat", "--json"]).await.json();
    assert_eq!(
        (apply["since"].clone(), apply["sanctionId"].clone()),
        (json!(NOW - 60 * DAY), conf["sanctionId"].clone())
    );
    assert_eq!(game_ids(&apply["refunds"]), sorted(vec![g.vic_loss.id, g.val_draw.id]));
}

#[tokio::test]
async fn an_unban_takes_no_refund_back() {
    let w = World::refunds(&[]).await;
    let [cheat, vic, ..] = w.ids[..] else { unreachable!() };
    play_refund_scenario(&w.store, &w.ids).await;
    let conf = w.run(&["integrity", "confirm", "Cheat", "--reason", "engine", "--json"]).await.json();
    assert_eq!(conf["refunds"].as_array().unwrap().len(), 2);
    let refunded = w.rating(vic).await;
    assert!(refunded.rating > 1500 && refunded.peak == refunded.rating, "{refunded:?}");
    let unban = w.run_at(NOW + 1000, &["user", "unban", "Cheat"]).await.ok();
    assert!(unban.out.starts_with("Lifted 1 ban(s) of Cheat: #"), "{}", unban.out);
    assert!(w.active_ban(cheat, NOW + 1000).await.is_none());
    assert_eq!(w.rating(vic).await, refunded);
}
