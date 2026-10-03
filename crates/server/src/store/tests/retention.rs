//! Port of store.retention.test.js (the purge against the real schema on a file: old rows go,
//! recent rows stay, IPs are erased in place, the chunked runner does what the one-shot run does)
//! and of cluster.retention.test.js (the scheduler: first run after a delay, then an interval
//! after each run, never two at once, counts logged and counted, a clean stop). The scheduler
//! tests use short real delays (no paused tokio clock in this build).

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::Connection;
use tokio::sync::oneshot;

use super::support::*;
use crate::clock::ManualClock;
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::store::retention::{
    AbortFlag, MsClock, PauseFn, PurgeFn, PurgeOptions, RetentionError, RetentionPolicy, RetentionScheduler,
    SchedulerOptions,
};
use crate::store::{
    ConductKind, ErrorKind, GameRecord, NewAnomaly, NewSecurityEvent, NewSession, NewToken, PurgeCounts,
    Severity, Store, StoreError, status,
};

const DAY: i64 = 86_400_000;
const POLICY: RetentionPolicy = RetentionPolicy { security_days: 90, ip_days: 30 };

/// The purge's `now`: the queue times (real clock) are then long ago.
fn purge_now() -> i64 {
    crate::clock::wall_ms() + 400 * DAY
}

/// ipErased: the IPs of the 40-day-old live session and of the 31-day-old event, and those of the
/// expired session and of the 91-day-old event, erased before their rows are deleted.
const EXPECTED: PurgeCounts = PurgeCounts {
    sessions: 3,
    tokens: 1,
    security_events: 1,
    anomalies: 2,
    conduct_events: 1,
    analysis_jobs: 1,
    ip_erased: 4,
};

/// A migrated store on a temporary file, and a raw connection to inspect and prepare rows.
struct Db {
    store: Store,
    raw: Connection,
    _dir: TempDir,
}

async fn temp_store() -> Db {
    let dir = TempDir::new("retention");
    let store = file_store(&dir).await;
    let raw = Connection::open(dir.file("scacelith.db")).unwrap();
    raw.busy_timeout(Duration::from_secs(5)).unwrap();
    Db { store, raw, _dir: dir }
}

struct Jobs {
    failed_old: GameId,
    failed_new: GameId,
    done_old: GameId,
    queued_old: GameId,
    running: GameId,
}

fn game(id: GameId, white: UserId, black: UserId) -> GameRecord {
    let t = crate::clock::wall_ms();
    GameRecord {
        started_at: Some(t - 600_000),
        ended_at: Some(t),
        status: status::WHITE_WINS,
        moves: vec![0; 40],
        ..record(id, white, black)
    }
}

/// Old and recent rows of every kind; the game ids of the analysis jobs by state.
async fn seed(db: &Db, now: i64) -> (UserId, Jobs) {
    let store = &db.store;
    let a = store.users().create(new_user("Ann", Some("ann@example.org"))).await.unwrap();
    let b = store.users().create(new_user("Ben", Some("ben@example.org"))).await.unwrap();
    let sess = |h: &str, created_at, expires_at, idle, ip: Option<&str>| NewSession {
        user_id: a,
        token_hash: h.into(),
        created_at,
        expires_at,
        idle_expires_at: Some(idle),
        client_label: None,
        ip: ip.map(Into::into),
    };
    let s = store.sessions();
    let (live, idle) = (now + 50 * DAY, now + DAY);
    s.create(sess("s-expired", now - 100 * DAY, now - 1, idle, Some("198.51.100.1"))).await.unwrap();
    s.create(sess("s-idle", now - 10 * DAY, live, now - 1, Some("198.51.100.2"))).await.unwrap();
    let old = s.create(sess("s-revoked-old", now - 10 * DAY, live, idle, None)).await.unwrap();
    s.revoke(old, Some(a), now - 2 * DAY).await.unwrap();
    let new = s.create(sess("s-revoked-new", now - 10 * DAY, live, idle, None)).await.unwrap();
    s.revoke(new, Some(a), now - 1000).await.unwrap();
    s.create(sess("s-live-old-ip", now - 40 * DAY, live, idle, Some("198.51.100.3"))).await.unwrap();
    s.create(sess("s-live-new-ip", now - DAY, live, idle, Some("198.51.100.4"))).await.unwrap();
    let token = |kind: &str, h: &str, data, expires_at| NewToken {
        kind: kind.into(),
        token_hash: h.into(),
        user_id: Some(a),
        data,
        created_at: now - DAY,
        expires_at,
    };
    let t = store.tokens();
    let email = Some(serde_json::json!({"email": "ann@example.org"}));
    t.create(token("password_reset", "t-expired", email, now - 1)).await.unwrap();
    t.create(token("password_reset", "t-live", None, now + 3_600_000)).await.unwrap();
    t.create(token("email_verify", "t-consumed-live", None, now + 3_600_000)).await.unwrap();
    t.consume("email_verify".into(), "t-consumed-live".into(), now - 5000).await.unwrap().unwrap();
    let ev = |kind: &str, ip: Option<&str>, at| NewSecurityEvent {
        kind: kind.into(),
        user_id: Some(a),
        ip: ip.map(Into::into),
        at: Some(at),
        detail: None,
    };
    store
        .security()
        .insert_batch(vec![
            ev("sec-old", Some("203.0.113.1"), now - 91 * DAY),
            ev("sec-mid", Some("203.0.113.2"), now - 31 * DAY),
            ev("sec-mid-noip", None, now - 31 * DAY),
            ev("sec-new", Some("203.0.113.3"), now - DAY),
        ])
        .await
        .unwrap();
    let an = |kind: &str, severity, at| NewAnomaly {
        user_id: Some(a),
        game_id: None,
        kind: kind.into(),
        severity,
        at: Some(at),
        detail: None,
    };
    store
        .anomalies()
        .insert_batch(vec![
            an("desync", Severity::Info, now - 91 * DAY),
            an("bad_seq", Severity::Suspicious, now - 91 * DAY),
            an("illegal_move", Severity::Certain, now - 91 * DAY),
            an("flood", Severity::Suspicious, now - DAY),
        ])
        .await
        .unwrap();
    store.conduct().record(a, ConductKind::Abandon, now - 31 * DAY).await.unwrap();
    store.conduct().record(a, ConductKind::NoShow, now - DAY).await.unwrap();
    // Analysis jobs: failed long ago / recently, done long ago, queued long ago, running.
    let games: Vec<GameRecord> = (1..=5).map(|i| game(7_000_000_000_000 + i, a, b)).collect();
    store.finish_batch(games.clone()).await.unwrap();
    let ids: Vec<GameId> = games.iter().map(|g| g.id).collect();
    let jobs = Jobs {
        failed_old: ids[0],
        failed_new: ids[1],
        done_old: ids[2],
        queued_old: ids[3],
        running: ids[4],
    };
    let set = "UPDATE analysis_jobs SET status = ?1, finished_at = ?2, features = ?3, attempts = ?4 WHERE game_id = ?5";
    let features = serde_json::json!({"gameId": jobs.done_old}).to_string();
    db.raw
        .execute(set, rusqlite::params!["failed", now - 31 * DAY, None::<String>, 3, jobs.failed_old as i64])
        .unwrap();
    db.raw
        .execute(set, rusqlite::params!["failed", now - DAY, None::<String>, 3, jobs.failed_new as i64])
        .unwrap();
    db.raw
        .execute(set, rusqlite::params!["done", now - 100 * DAY, features, 1, jobs.done_old as i64])
        .unwrap();
    db.raw
        .execute(
            "UPDATE analysis_jobs SET status = 'running', started_at = ?1, attempts = 1 WHERE game_id = ?2",
            rusqlite::params![now - 60_000, jobs.running as i64],
        )
        .unwrap();
    (a, jobs)
}

/// Everything the purge may touch, as plain rows (to compare two databases).
#[derive(Debug, PartialEq)]
struct Dump {
    sessions: Vec<(String, Option<String>)>,
    tokens: Vec<String>,
    security: Vec<(String, Option<String>)>,
    anomalies: Vec<(String, String)>,
    conduct: Vec<String>,
    jobs: Vec<(i64, String)>,
}

fn rows<T>(raw: &Connection, sql: &str, f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>) -> Vec<T> {
    raw.prepare(sql).unwrap().query_map([], f).unwrap().collect::<Result<_, _>>().unwrap()
}

fn dump(raw: &Connection) -> Dump {
    Dump {
        sessions: rows(raw, "SELECT token_hash, ip FROM sessions ORDER BY id", |r| {
            Ok((r.get(0)?, r.get(1)?))
        }),
        tokens: rows(raw, "SELECT token_hash FROM tokens ORDER BY id", |r| r.get(0)),
        security: rows(raw, "SELECT kind, ip FROM security_events ORDER BY id", |r| {
            Ok((r.get(0)?, r.get(1)?))
        }),
        anomalies: rows(raw, "SELECT kind, severity FROM anomalies ORDER BY id", |r| {
            Ok((r.get(0)?, r.get(1)?))
        }),
        conduct: rows(raw, "SELECT kind FROM conduct_events ORDER BY id", |r| r.get(0)),
        jobs: rows(raw, "SELECT game_id, status FROM analysis_jobs ORDER BY game_id", |r| {
            Ok((r.get(0)?, r.get(1)?))
        }),
    }
}

fn opt(s: &str) -> Option<String> {
    Some(s.into())
}

fn no_pause() -> PauseFn {
    Arc::new(|| Box::pin(async {}))
}

#[tokio::test]
async fn retention_on_a_real_database_old_rows_go_recent_rows_stay_ips_are_erased_in_place() {
    let db = temp_store().await;
    let now = purge_now();
    let (a, jobs) = seed(&db, now).await;
    let counts = db.store.retention().run_async(now, POLICY, PurgeOptions::default()).await.unwrap();
    assert_eq!(counts, EXPECTED);
    let d = dump(&db.raw);
    // Expired, idle-expired and revoked (for more than a day) sessions are deleted; the live
    // session older than RETENTION_IP_DAYS keeps its row but loses its IP.
    assert_eq!(
        d.sessions,
        vec![
            ("s-revoked-new".into(), None),
            ("s-live-old-ip".into(), None),
            ("s-live-new-ip".into(), opt("198.51.100.4"))
        ]
    );
    // Expired tokens (with the e-mail address in their data) are deleted, live ones stay even consumed.
    assert_eq!(d.tokens, vec!["t-live".to_string(), "t-consumed-live".to_string()]);
    // Security events: deleted after RETENTION_SECURITY_DAYS, IP erased after RETENTION_IP_DAYS.
    assert_eq!(
        d.security,
        vec![("sec-mid".into(), None), ("sec-mid-noip".into(), None), ("sec-new".into(), opt("203.0.113.3"))]
    );
    // Anomalies: certain ones (evidence of a sanction) are kept, the others go with the security retention.
    assert_eq!(
        d.anomalies,
        vec![("illegal_move".into(), "certain".into()), ("flood".into(), "suspicious".into())]
    );
    assert_eq!(d.conduct, vec!["noshow".to_string()]);
    // Analysis jobs: only failed ones older than 30 days go; done jobs (the players' analysed
    // history), waiting and running jobs stay whatever their age.
    let status_of = |id: GameId| d.jobs.iter().find(|j| j.0 == id as i64).map(|j| j.1.clone());
    assert_eq!(status_of(jobs.failed_old), None);
    assert_eq!(status_of(jobs.failed_new), opt("failed"));
    assert_eq!(status_of(jobs.done_old), opt("done"));
    assert_eq!(status_of(jobs.queued_old), opt("queued"));
    assert_eq!(status_of(jobs.running), opt("running"));
    let mine = db.store.analysis().for_user(a, 10, false).await.unwrap();
    let done = mine.iter().find(|j| j.game_id == jobs.done_old).unwrap();
    assert_eq!(done.features.as_ref().unwrap()["gameId"], jobs.done_old);
    // Nothing is left to do.
    let again = db.store.retention().run_async(now, POLICY, PurgeOptions::default()).await.unwrap();
    assert_eq!(again, PurgeCounts::default());
    db.store.close().await;
}

#[tokio::test]
async fn run_and_run_async_execute_the_same_statements_with_the_same_result() {
    let (one, two) = (temp_store().await, temp_store().await);
    let now = purge_now();
    seed(&one, now).await;
    seed(&two, now).await;
    assert_eq!(one.store.retention().run(now, POLICY).await.unwrap(), EXPECTED);
    let opts = PurgeOptions { slice_ms: 0.0, ..PurgeOptions::default() };
    assert_eq!(two.store.retention().run_async(now, POLICY, opts).await.unwrap(), EXPECTED);
    assert_eq!(dump(&one.raw), dump(&two.raw));
    one.store.close().await;
    two.store.close().await;
}

#[tokio::test]
async fn run_async_with_a_fixed_chunk_pauses_between_slices_and_stops_when_aborted() {
    let db = temp_store().await;
    let now = purge_now();
    let a = db.store.users().create(new_user("Cid", Some("cid@example.org"))).await.unwrap();
    let events: Vec<NewSecurityEvent> = (0..2500)
        .map(|_| NewSecurityEvent {
            kind: "login_failed".into(),
            user_id: Some(a),
            ip: None,
            at: Some(now - 100 * DAY),
            detail: None,
        })
        .collect();
    db.store.security().insert_batch(events).await.unwrap();
    db.store
        .write(move |db| {
            for _ in 0..1200 {
                db.conduct().record(a, ConductKind::Abort, now - 40 * DAY)?;
            }
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let left = |raw: &Connection| -> (i64, i64) {
        raw.query_row(
            "SELECT (SELECT count(*) FROM security_events), (SELECT count(*) FROM conduct_events)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };

    // Aborted at the 7th pause: 6 statements with nothing to do, then the first chunk of events.
    let abort = AbortFlag::new();
    let pauses = Arc::new(Mutex::new(0));
    let (p, flag) = (pauses.clone(), abort.clone());
    let pause: PauseFn = Arc::new(move || {
        *p.lock() += 1;
        if *p.lock() == 7 {
            flag.abort();
        }
        Box::pin(async {})
    });
    let opts = PurgeOptions {
        slice_ms: 0.0,
        chunk: Some(1000),
        abort: Some(abort),
        pause: Some(pause),
        clock: None,
    };
    let first = db.store.retention().run_async(now, POLICY, opts).await.unwrap();
    assert_eq!(*pauses.lock(), 7);
    assert_eq!(first.security_events, 1000);
    assert_eq!(left(&db.raw), (1500, 1200));

    // The next run finishes: one pause per statement with slice 0 (2 chunks of events, 2 of
    // conduct events, one statement for each other step).
    *pauses.lock() = 0;
    let p = pauses.clone();
    let counting: PauseFn = Arc::new(move || {
        *p.lock() += 1;
        Box::pin(async {})
    });
    let opts = PurgeOptions {
        slice_ms: 0.0,
        chunk: Some(1000),
        pause: Some(counting.clone()),
        ..PurgeOptions::default()
    };
    let done = db.store.retention().run_async(now, POLICY, opts).await.unwrap();
    assert_eq!((done.security_events, done.conduct_events), (1500, 1200));
    assert_eq!(left(&db.raw), (0, 0));
    assert_eq!(
        *pauses.lock(),
        6 + 2 + 1 + 2 + 1,
        "session IPs, event IPs, sessions x2, tokens, pending signups, events x2, anomalies, conduct x2, jobs"
    );

    // A large slice runs everything without pausing.
    *pauses.lock() = 0;
    let opts = PurgeOptions { slice_ms: 60_000.0, pause: Some(counting), ..PurgeOptions::default() };
    db.store.retention().run_async(now, POLICY, opts).await.unwrap();
    assert_eq!(*pauses.lock(), 0);
    db.store.close().await;
}

#[tokio::test]
async fn run_async_stops_once_the_store_is_closed() {
    let db = temp_store().await;
    let pauses = Arc::new(Mutex::new(0));
    let (p, store) = (pauses.clone(), db.store.clone());
    let pause: PauseFn = Arc::new(move || {
        *p.lock() += 1;
        let store = store.clone();
        Box::pin(async move { store.close().await })
    });
    let opts = PurgeOptions { slice_ms: 0.0, pause: Some(pause), ..PurgeOptions::default() };
    let counts = db.store.retention().run_async(purge_now(), POLICY, opts).await.unwrap();
    assert_eq!(*pauses.lock(), 1);
    assert_eq!(counts, PurgeCounts::default());
    // The one-shot run also refuses a closed store (nothing to do, nothing touched).
    assert_eq!(db.store.retention().run(purge_now(), POLICY).await.unwrap(), PurgeCounts::default());
}

/// A clock for run_async that charges `per_row_ms` for every security event deleted since it was
/// last read (counted on a raw connection), so statement times are deterministic; the rows each
/// statement deleted are recorded.
fn row_clock(file: &str, per_row_ms: f64) -> (MsClock, Arc<Mutex<Vec<i64>>>) {
    let raw = Connection::open(file).unwrap();
    let count = |raw: &Connection| -> i64 {
        raw.query_row("SELECT count(*) FROM security_events", [], |r| r.get(0)).unwrap()
    };
    let last = count(&raw);
    let state = Arc::new(Mutex::new((raw, 0.0f64, last)));
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let ch = chunks.clone();
    let clock: MsClock = Arc::new(move || {
        let mut st = state.lock();
        let n = count(&st.0);
        if n != st.2 {
            ch.lock().push(st.2 - n);
        }
        st.1 += (st.2 - n) as f64 * per_row_ms;
        st.2 = n;
        st.1
    });
    (clock, chunks)
}

#[tokio::test]
async fn run_async_adapts_its_chunk_so_that_one_statement_takes_about_half_a_slice_at_most_1000_rows() {
    let db = temp_store().await;
    let now = purge_now();
    let a = db.store.users().create(new_user("Dan", Some("dan@example.org"))).await.unwrap();
    let seed_events = || {
        let events: Vec<NewSecurityEvent> = (0..3000)
            .map(|_| NewSecurityEvent {
                kind: "login_failed".into(),
                user_id: Some(a),
                ip: None,
                at: Some(now - 100 * DAY),
                detail: None,
            })
            .collect();
        db.store.security().insert_batch(events)
    };
    let file = db.store.path();
    let run = |clock: MsClock| {
        let opts = PurgeOptions {
            slice_ms: 10.0,
            pause: Some(no_pause()),
            clock: Some(clock),
            ..PurgeOptions::default()
        };
        db.store.retention().run_async(now, POLICY, opts)
    };

    // Slow statements (0.1 ms per row, 10 ms slices): the first chunk of 200 rows takes 20 ms,
    // four times the 5 ms aimed at, so the next ones take 50 rows (5 ms).
    seed_events().await.unwrap();
    let (clock, chunks) = row_clock(&file, 0.1);
    assert_eq!(run(clock).await.unwrap().security_events, 3000);
    let chunks = chunks.lock().clone();
    assert_eq!(chunks[0], 200);
    assert!(chunks[1..].iter().all(|&n| n == 50), "chunks {chunks:?}");

    // Fast statements: the chunk doubles up to 1000 rows, never more.
    seed_events().await.unwrap();
    let (clock, chunks) = row_clock(&file, 0.001);
    assert_eq!(run(clock).await.unwrap().security_events, 3000);
    assert_eq!(*chunks.lock(), vec![200, 400, 800, 1000, 600]);

    // Very slow statements: never below 50 rows.
    seed_events().await.unwrap();
    let (clock, chunks) = row_clock(&file, 5.0);
    run(clock).await.unwrap();
    let chunks = chunks.lock().clone();
    assert_eq!(chunks[..3], [200, 50, 50]);
    assert_eq!(chunks.iter().sum::<i64>(), 3000);
    db.store.close().await;
}

#[tokio::test]
async fn run_async_pauses_for_slice_ms_between_slices_so_that_other_writers_get_the_lock() {
    let db = temp_store().await;
    // A clock on which every statement takes a whole slice: the run pauses after each of its 10
    // statements (nothing to purge), for slice_ms each time (a timer, not just a yield).
    let fake = Arc::new(Mutex::new(0.0f64));
    let clock: MsClock = Arc::new(move || {
        let mut f = fake.lock();
        *f += 12.5;
        *f
    });
    let t0 = Instant::now();
    let opts = PurgeOptions { slice_ms: 25.0, clock: Some(clock), ..PurgeOptions::default() };
    db.store.retention().run_async(purge_now(), POLICY, opts).await.unwrap();
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(10 * 25 - 10), "10 pauses of 25 ms took {took:?}");
    db.store.close().await;
}

#[tokio::test]
async fn erased_ips_deleted_sessions_and_anonymized_addresses_do_not_stay_readable_in_the_file() {
    let dir = TempDir::new("secdel");
    let store = file_store(&dir).await;
    let file = dir.file("scacelith.db");
    let now = purge_now();
    let ip = |k: u32, i: u32| format!("198.18.{k}.{i}"); // marker addresses (a range reserved for benchmarks)
    let a = store.users().create(new_user("Eva", Some("eva@example.org"))).await.unwrap();
    let mut sessions = Vec::new();
    for i in 0..250 {
        // Live sessions older than RETENTION_IP_DAYS (IP erased in place) and expired ones (rows
        // deleted, whole pages freed).
        let s = |h: String, expires_at, ip| NewSession {
            user_id: a,
            token_hash: h,
            created_at: now - 40 * DAY,
            expires_at,
            idle_expires_at: Some(expires_at),
            client_label: None,
            ip: Some(ip),
        };
        sessions.push(s(format!("live-{i}"), now + DAY, ip(1, i)));
        sessions.push(s(format!("dead-{i}"), now - 1, ip(2, i)));
        // Sessions that expired before their IP was due for erasure: deleted with it.
        sessions
            .push(NewSession { created_at: now - 10 * DAY, ..s(format!("short-{i}"), now - 1, ip(5, i)) });
    }
    store
        .write(move |db| {
            for s in &sessions {
                db.sessions().create(s)?;
            }
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let ev = |k, i, at| NewSecurityEvent {
        kind: "login".into(),
        user_id: Some(a),
        ip: Some(ip(k, i)),
        at: Some(at),
        detail: None,
    };
    store.security().insert_batch((0..250).map(|i| ev(3, i, now - 100 * DAY)).collect()).await.unwrap();
    store.security().insert_batch((0..250).map(|i| ev(4, i, now - 40 * DAY)).collect()).await.unwrap();
    let gone = store.users().create(new_user("Gus", Some("gus.secret-marker@example.org"))).await.unwrap();
    let raw = Connection::open(&file).unwrap();
    raw.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap(); // the rows are in the main file now
    drop(raw);
    let opts = PurgeOptions { slice_ms: 1000.0, ..PurgeOptions::default() };
    let counts = store.retention().run_async(now, POLICY, opts).await.unwrap();
    assert_eq!((counts.sessions, counts.security_events, counts.ip_erased), (500, 250, 1000));
    store.users().anonymize(gone, now).await.unwrap();
    store.close().await; // the last connection: checkpointed, WAL removed
    let mut bytes = std::fs::read(&file).unwrap();
    if let Ok(wal) = std::fs::read(format!("{file}-wal")) {
        bytes.extend(wal);
    }
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("198.18."), "no erased or deleted IP address left in the file");
    assert!(!text.contains("secret-marker"), "no anonymized e-mail address left in the file");
}

// ---- The scheduler ----

/// The scheduler tests run one at a time: they read deltas of the global retention metrics.
static SCHEDULER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn purged(kind: &str) -> u64 {
    crate::store::retention::PURGED.with(&[kind]).get()
}

fn ip_erased() -> u64 {
    crate::store::retention::IP_ERASED.get()
}

fn runs(result: &str) -> u64 {
    crate::store::retention::RUNS.with(&[result]).get()
}

type Outcome = Result<PurgeCounts, RetentionError>;

/// A purge whose runs the test settles by hand.
struct Call {
    now: i64,
    abort: AbortFlag,
    settle: Option<oneshot::Sender<Outcome>>,
    at: Instant,
}

#[derive(Clone, Default)]
struct FakePurge {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakePurge {
    fn purge_fn(&self) -> PurgeFn {
        let calls = self.calls.clone();
        Arc::new(move |now, abort| {
            let (tx, rx) = oneshot::channel();
            calls.lock().push(Call { now, abort, settle: Some(tx), at: Instant::now() });
            Box::pin(async move { rx.await.unwrap_or_else(|_| Ok(PurgeCounts::default())) })
        })
    }

    fn count(&self) -> usize {
        self.calls.lock().len()
    }

    /// Waits until `n` runs have started (at most 5 s).
    async fn wait_for(&self, n: usize) {
        let t0 = Instant::now();
        while self.count() < n {
            assert!(t0.elapsed() < Duration::from_secs(5), "run {n} did not start");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn settle(&self, i: usize, outcome: Outcome) {
        let tx = self.calls.lock()[i].settle.take().expect("not settled yet");
        let _ = tx.send(outcome);
    }
}

fn scheduler_options(first_delay_ms: u64, interval_ms: u64, logger: &Logger) -> SchedulerOptions {
    SchedulerOptions {
        first_delay: Duration::from_millis(first_delay_ms),
        interval: Duration::from_millis(interval_ms),
        slice_ms: 10.0,
        clock: ManualClock::new(0.0, 1234),
        logger: logger.clone(),
    }
}

async fn wait_idle(s: &RetentionScheduler) {
    let t0 = Instant::now();
    while s.running() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the run did not end");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn the_first_run_comes_after_the_first_delay_then_every_interval_after_the_previous_one_ends() {
    let _serial = SCHEDULER.lock().await;
    let logs = LogCapture::start();
    let logger = Logger::root().child("retention-scheduler-test-a");
    let fake = FakePurge::default();
    let started = Instant::now();
    let (ok0, sessions0, tokens0, events0, jobs0) = (
        runs("ok"),
        purged("sessions"),
        purged("tokens"),
        purged("security_events"),
        purged("analysis_jobs"),
    );
    let ip0 = ip_erased();
    let s = RetentionScheduler::start(fake.purge_fn(), scheduler_options(60, 150, &logger));
    assert_eq!(fake.count(), 0);
    fake.wait_for(1).await;
    assert!(fake.calls.lock()[0].at - started >= Duration::from_millis(55), "after the first delay");
    assert_eq!(fake.calls.lock()[0].now, 1234);
    assert!(s.running());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fake.count(), 1, "the next run is scheduled only when this one ends");

    let counts = PurgeCounts {
        sessions: 3,
        tokens: 2,
        security_events: 5,
        analysis_jobs: 1,
        ip_erased: 4,
        ..PurgeCounts::default()
    };
    fake.settle(0, Ok(counts));
    wait_idle(&s).await;
    let ended = Instant::now();
    // One info line with the counts only: numbers, no personal data, none of them redacted.
    let done = logs
        .records(logger.component())
        .into_iter()
        .find(|r| r["msg"] == "retention purge done")
        .expect("the run is logged");
    assert_eq!(done["level"], "info");
    let mut fields = done.as_object().unwrap().clone();
    for k in ["t", "level", "c", "msg"] {
        fields.remove(k);
    }
    assert!(fields.remove("ms").unwrap().is_u64());
    assert_eq!(
        serde_json::Value::Object(fields),
        serde_json::json!({"sessions": 3, "singleUse": 2, "securityEvents": 5, "anomalies": 0, "conductEvents": 0,
            "analysisJobs": 1, "ipErased": 4})
    );
    assert_eq!(purged("sessions") - sessions0, 3);
    assert_eq!(purged("tokens") - tokens0, 2);
    assert_eq!(purged("security_events") - events0, 5);
    assert_eq!(purged("analysis_jobs") - jobs0, 1);
    assert_eq!(ip_erased() - ip0, 4);
    assert_eq!(runs("ok") - ok0, 1);

    fake.wait_for(2).await;
    assert!(fake.calls.lock()[1].at - ended >= Duration::from_millis(140), "an interval after the end");
    fake.settle(1, Ok(PurgeCounts::default()));
    wait_idle(&s).await;
    assert_eq!(runs("ok") - ok0, 2);
    s.stop().await;
    assert_eq!(s.run_now().await, None);
}

#[tokio::test]
async fn an_interval_beyond_the_longest_timer_is_clamped_by_from_config() {
    let mut cfg = config();
    cfg.retention_interval_ms = 30 * DAY;
    assert_eq!(SchedulerOptions::from_config(&cfg).interval, Duration::from_millis(2_147_483_647));
    assert_eq!(SchedulerOptions::from_config(&config()).first_delay, Duration::from_secs(60));
    assert_eq!(SchedulerOptions::from_config(&config()).interval, Duration::from_millis(3_600_000));
}

#[tokio::test]
async fn runs_never_overlap_run_now_waits_for_the_run_in_progress_and_the_timer_for_run_now() {
    let _serial = SCHEDULER.lock().await;
    let logger = Logger::root().child("retention-scheduler-test-b");
    let fake = FakePurge::default();
    let s = Arc::new(RetentionScheduler::start(fake.purge_fn(), scheduler_options(10, 60, &logger)));
    fake.wait_for(1).await;
    let manual = tokio::spawn({
        let s = s.clone();
        async move { s.run_now().await }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(fake.count(), 1, "run_now waits");
    fake.settle(0, Ok(PurgeCounts::default()));
    fake.wait_for(2).await;
    // The interval timer comes due while run_now's run is in progress: it does not start another.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(fake.count(), 2);
    let counts = PurgeCounts { conduct_events: 7, ..PurgeCounts::default() };
    fake.settle(1, Ok(counts));
    assert_eq!(manual.await.unwrap(), Some(counts));
    // One next run, an interval later.
    fake.wait_for(3).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(fake.count(), 3);
    fake.settle(2, Ok(PurgeCounts::default()));
    s.stop().await;
}

#[tokio::test]
async fn a_failed_run_is_counted_and_logged_with_the_partial_counts_and_the_next_one_still_happens() {
    let _serial = SCHEDULER.lock().await;
    let logs = LogCapture::start();
    let logger = Logger::root().child("retention-scheduler-test-c");
    let fake = FakePurge::default();
    let (failed0, sessions0) = (runs("failed"), purged("sessions"));
    let s = RetentionScheduler::start(fake.purge_fn(), scheduler_options(10, 40, &logger));
    fake.wait_for(1).await;
    let busy = StoreError::new(ErrorKind::Busy, "database is locked");
    let partial = PurgeCounts { sessions: 1000, ..PurgeCounts::default() };
    fake.settle(0, Err(RetentionError { error: busy, counts: partial }));
    fake.wait_for(2).await;
    assert_eq!(runs("failed") - failed0, 1);
    assert_eq!(purged("sessions") - sessions0, 1000);
    let failures = |logs: &LogCapture| -> Vec<serde_json::Value> {
        logs.records(logger.component())
            .into_iter()
            .filter(|r| r["msg"].as_str().is_some_and(|m| m.starts_with("retention purge failed")))
            .collect()
    };
    let first = failures(&logs);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["level"], "warn", "a busy database is a warning");
    assert_eq!(first[0]["sessions"], 1000);
    assert_eq!(first[0]["err"], "database is locked");
    let io = StoreError::new(ErrorKind::Sqlite, "disk I/O error");
    fake.settle(1, Err(RetentionError { error: io, counts: PurgeCounts::default() }));
    fake.wait_for(3).await;
    assert_eq!(failures(&logs).last().unwrap()["level"], "error");
    assert_eq!(runs("failed") - failed0, 2);
    fake.settle(2, Ok(PurgeCounts::default()));
    s.stop().await;
}

#[tokio::test]
async fn stop_cancels_the_first_run_aborts_a_run_in_progress_and_waits_for_it() {
    let _serial = SCHEDULER.lock().await;
    let logs = LogCapture::start();
    let logger = Logger::root().child("retention-scheduler-test-d");
    // Before the first run.
    let fake = FakePurge::default();
    let s = RetentionScheduler::start(fake.purge_fn(), scheduler_options(10_000, 60_000, &logger));
    let t0 = Instant::now();
    s.stop().await;
    assert!(t0.elapsed() < Duration::from_secs(1));
    assert_eq!(s.run_now().await, None);
    assert_eq!(fake.count(), 0);

    // During a run.
    let fake = FakePurge::default();
    let (aborted0, anomalies0) = (runs("aborted"), purged("anomalies"));
    let s = Arc::new(RetentionScheduler::start(fake.purge_fn(), scheduler_options(10, 60_000, &logger)));
    fake.wait_for(1).await;
    let abort = fake.calls.lock()[0].abort.clone();
    assert!(!abort.is_aborted());
    let stopping = tokio::spawn({
        let s = s.clone();
        async move { s.stop().await }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(abort.is_aborted(), "the run is asked to stop between two statements");
    assert!(!stopping.is_finished(), "stop() waits for the run to end");
    fake.settle(0, Ok(PurgeCounts { anomalies: 2, ..PurgeCounts::default() }));
    stopping.await.unwrap();
    assert_eq!(runs("aborted") - aborted0, 1);
    assert_eq!(purged("anomalies") - anomalies0, 2);
    let msgs: Vec<serde_json::Value> =
        logs.records(logger.component()).into_iter().map(|r| r["msg"].clone()).collect();
    assert!(msgs.contains(&serde_json::json!("retention purge interrupted by the shutdown")), "{msgs:?}");
    assert_eq!(s.run_now().await, None);
    assert_eq!(fake.count(), 1, "no next run");
}

#[tokio::test]
async fn with_a_real_database_the_scheduled_run_purges_and_stop_then_close_leaves_nothing_running() {
    let _serial = SCHEDULER.lock().await;
    let logs = LogCapture::start();
    let logger = Logger::root().child("retention-scheduler-test-e");
    let dir = TempDir::new("retention-timer");
    let store = file_store(&dir).await;
    let now = crate::clock::wall_ms();
    let a = store.users().create(new_user("Ann", Some("ann@example.org"))).await.unwrap();
    let events: Vec<NewSecurityEvent> = (0..1500)
        .map(|_| NewSecurityEvent {
            kind: "login_failed".into(),
            user_id: Some(a),
            ip: Some("192.0.2.1".into()),
            at: Some(now - 200 * DAY),
            detail: None,
        })
        .collect();
    store.security().insert_batch(events).await.unwrap();
    let expired = NewToken {
        kind: "password_reset".into(),
        token_hash: "expired".into(),
        user_id: Some(a),
        data: None,
        created_at: now - DAY,
        expires_at: now - 1,
    };
    store.tokens().create(expired).await.unwrap();
    let (events0, tokens0) = (purged("security_events"), purged("tokens"));
    let opts = SchedulerOptions {
        slice_ms: 0.0,
        clock: crate::clock::system(),
        ..scheduler_options(10, 60_000, &logger)
    };
    let s = RetentionScheduler::for_store_with(&store, POLICY, opts);
    let t0 = Instant::now();
    while !s.running() && t0.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let t0 = Instant::now();
    while purged("security_events") - events0 < 1500 && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    wait_idle(&s).await;
    assert_eq!(purged("security_events") - events0, 1500);
    assert_eq!(purged("tokens") - tokens0, 1);
    assert!(store.security().for_user(a, 10).await.unwrap().is_empty());
    s.stop().await;
    store.close().await;
    assert_eq!(s.run_now().await, None, "no run after the stop");
    let bad: Vec<serde_json::Value> = logs
        .records(logger.component())
        .into_iter()
        .filter(|r| r["level"] == "error" || r["level"] == "warn")
        .collect();
    assert!(bad.is_empty(), "{bad:?}");
}
