//! Tests of the anti-cheat service, ported from anticheat.anomalies, anticheat.writer,
//! anticheat.sanction and the service part of anticheat.refunds.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::anticheat::players::{apply_player_score, score_player_games};
use crate::anticheat::scoring::Population;
use crate::anticheat::testing::*;
use crate::clock::ManualClock;
use crate::store::status::WHITE_WINS;
use crate::store::tests::support::LogCapture;
use crate::store::{
    GameRecord, IntegrityLevel, IntegrityUpdate, NewSanction, Priority, RefundScope, SanctionKind, Source,
};

fn anomaly(user: UserId, game: GameId, kind: &'static str, detail: Value, pos_matched: bool) -> Anomaly {
    Anomaly { user, game, kind, detail, pos_matched }
}

struct World {
    config: Config,
    clock: Arc<ManualClock>,
    store: Store,
    ids: Vec<UserId>,
}

async fn world(overrides: &[(&str, &str)], names: &[&str]) -> World {
    let config = config(overrides);
    let clock = ManualClock::new(0.0, NOW);
    let store = store(&config, &clock).await;
    let ids = users(&store, names).await;
    World { config, clock, store, ids }
}

impl World {
    fn anticheat(&self) -> (Anticheat, Arc<Recorder>) {
        let ac = Anticheat::new(&self.config, self.store.clone(), self.clock.clone());
        let events = Recorder::new();
        ac.set_sanction_events(events.clone());
        (ac, events)
    }
}

fn anomaly_count(kind: &str, severity: &str) -> u64 {
    METRICS.anomalies.with(&[kind, severity]).get()
}

// ---- classification (anticheat.anomalies) ------------------------------------------------------

#[test]
fn the_classification_table_matches_design() {
    let expected = [
        ("malformed", "suspicious"),
        ("forged_type", "certain"),
        ("bad_seq", "suspicious"),
        ("flood", "suspicious"),
        ("foreign_game", "certain"),
        ("out_of_turn", "certain"),
        ("illegal_move", "certain"),
        ("repeated_desync", "suspicious"),
        ("clock_implausible", "suspicious"),
        ("stale_ply", "info"),
        ("desync", "info"),
        ("nothing_to_claim", "info"),
    ];
    let table: Vec<(&str, &str)> = ANOMALY_KINDS.iter().map(|(k, s)| (*k, s.as_str())).collect();
    assert_eq!(table, expected);
    for (kind, severity) in expected {
        let c = classify(kind, Some(true));
        assert_eq!(c.severity.as_str(), severity, "{kind}");
        assert_eq!(c.certain, severity == "certain", "{kind}");
        assert!(c.known);
    }
}

#[test]
fn unknown_kinds_are_info_and_position_cheats_need_a_synchronised_position() {
    assert_eq!(
        classify("made_up", None),
        Classification { severity: Severity::Info, certain: false, known: false }
    );
    assert_eq!(classify("toString", None).severity, Severity::Info);
    assert_eq!(classify("illegal_move", Some(false)).severity, Severity::Suspicious);
    assert!(!classify("out_of_turn", Some(false)).certain);
    assert!(classify("illegal_move", None).certain, "the caller did not say");
    assert!(classify("forged_type", Some(false)).certain, "not position dependent");
}

// ---- recording (anticheat.anomalies) -----------------------------------------------------------

#[tokio::test]
async fn non_certain_anomalies_are_coalesced_while_their_job_waits_for_the_writer() {
    let w = world(&[], &["ann", "ben"]).await;
    let (a, b) = (w.ids[0], w.ids[1]);
    // A game id of its own: the logs of the other tests are captured too.
    let g = next_game_id();
    let (ac, _) = w.anticheat();
    let logs = LogCapture::start();
    let before = anomaly_count("bad_seq", "suspicious");
    let hold = hold_writer(&w.store);
    let c = ac.record_anomaly(&anomaly(a, g, "bad_seq", json!({ "expected": 4, "got": 9 }), true));
    assert_eq!(c, Classification { severity: Severity::Suspicious, certain: false, known: true });
    w.clock.advance(3.0);
    ac.record_anomaly(&anomaly(a, g, "bad_seq", Value::Null, true));
    w.clock.advance(4.0);
    ac.record_anomaly(&anomaly(a, g, "bad_seq", Value::Null, true));
    ac.record_anomaly(&anomaly(a, g, "stale_ply", Value::Null, true));
    ac.record_anomaly(&anomaly(b, g, "desync", Value::Null, false));
    assert_eq!(ac.pending_count(), 3, "the repeats wait in one row");
    assert_eq!(anomaly_count("bad_seq", "suspicious") - before, 3, "every anomaly is counted");
    drop(hold);
    barrier(&w.store).await;
    assert_eq!(ac.pending_count(), 0);

    let rows = w.store.anomalies().for_user(a, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    let seq = rows.iter().find(|r| r.kind == "bad_seq").unwrap();
    assert_eq!(
        seq.detail,
        Some(json!({ "expected": 4, "got": 9, "posMatched": true, "count": 3, "lastAt": NOW + 7 }))
    );
    assert_eq!(seq.at, NOW);
    assert_eq!(seq.game_id, Some(g));
    let desync = w.store.anomalies().for_user(b, 10).await.unwrap();
    assert_eq!(desync[0].detail, Some(json!({ "posMatched": false })));
    assert_eq!(desync[0].severity, Severity::Info);

    let records = logs.records("anticheat");
    let security: Vec<&Value> = records
        .iter()
        .filter(|r| r["level"] == "security" && r["msg"] == "anomaly" && r["userId"] == a && r["gameId"] == g)
        .collect();
    assert_eq!(security.len(), 1, "a repeat waiting in the buffer is not logged again");
    assert_eq!(security[0]["kind"], "bad_seq");
    assert!(
        !records.iter().any(|r| r["level"] == "security" && r["kind"] == "stale_ply" && r["gameId"] == g),
        "info anomalies are not security-logged"
    );
    assert!(records.iter().any(|r| r["level"] == "debug" && r["kind"] == "stale_ply" && r["gameId"] == g));
}

#[tokio::test]
async fn a_flood_of_repeats_costs_one_row_per_flush_period_even_with_an_idle_writer() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let g = next_game_id();
    let (ac, _) = w.anticheat();
    // A client that sends a pointless claim every 20 ms: the writer is idle between them.
    for _ in 0..50 {
        ac.record_anomaly(&anomaly(a, g, "nothing_to_claim", Value::Null, true));
        ac.record_anomaly(&anomaly(a, g, "bad_seq", Value::Null, true));
        barrier(&w.store).await;
        w.clock.advance(20.0);
    }
    // The first anomaly of each kind is written at once (the analysis queue policy sees it);
    // the repeats of the next second wait for one batch.
    let rows = |kind: &str, rows: &[crate::store::Anomaly]| rows.iter().filter(|r| r.kind == kind).count();
    let written = w.store.anomalies().for_user(a, 1000).await.unwrap();
    assert_eq!((rows("nothing_to_claim", &written), rows("bad_seq", &written)), (1, 1));
    assert!(eventually(|| ac.pending_count() == 0).await, "the timer writes the repeats");
    barrier(&w.store).await;
    let written = w.store.anomalies().for_user(a, 1000).await.unwrap();
    assert_eq!((rows("nothing_to_claim", &written), rows("bad_seq", &written)), (2, 2));
    let counted: u64 = written
        .iter()
        .filter(|r| r.kind == "bad_seq")
        .map(|r| r.detail.as_ref().and_then(|d| d.get("count")).and_then(Value::as_u64).unwrap_or(1))
        .sum();
    assert_eq!(counted, 50, "every repeat is counted");
    // A second later, the next anomaly of the kind is written at once again.
    w.clock.advance(f64::from(FLUSH_MS));
    ac.record_anomaly(&anomaly(a, g, "bad_seq", Value::Null, true));
    barrier(&w.store).await;
    assert_eq!(ac.pending_count(), 0);
    assert_eq!(rows("bad_seq", &w.store.anomalies().for_user(a, 1000).await.unwrap()), 3);
}

#[tokio::test]
async fn flush_writes_the_waiting_repeats_at_once() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let g = next_game_id();
    let (ac, _) = w.anticheat();
    ac.record_anomaly(&anomaly(a, g, "flood", Value::Null, false));
    barrier(&w.store).await;
    w.clock.advance(5.0);
    ac.record_anomaly(&anomaly(a, g, "flood", Value::Null, false));
    ac.record_anomaly(&anomaly(a, g, "flood", Value::Null, false));
    barrier(&w.store).await;
    assert_eq!(ac.pending_count(), 1, "the repeats wait for the timer");
    ac.flush();
    barrier(&w.store).await;
    assert_eq!(ac.pending_count(), 0);
    let rows = w.store.anomalies().for_user(a, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    let repeats = rows.iter().find_map(|r| r.detail.as_ref()?.get("count")).cloned();
    assert_eq!(repeats, Some(json!(2)));
}

#[test]
fn outside_a_runtime_the_repeats_are_written_with_the_next_writer_turn() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let w = rt.block_on(world(&[], &["ann"]));
    let a = w.ids[0];
    let g = next_game_id();
    let (ac, _) = w.anticheat();
    ac.record_anomaly(&anomaly(a, g, "bad_seq", Value::Null, false));
    rt.block_on(barrier(&w.store));
    ac.record_anomaly(&anomaly(a, g, "bad_seq", Value::Null, false));
    // The repeat waits for the writer job queued with it, which the writer thread may already
    // have run.
    assert!(ac.pending_count() <= 1);
    rt.block_on(barrier(&w.store));
    assert_eq!(ac.pending_count(), 0, "no timer to wait for");
    assert_eq!(rt.block_on(w.store.anomalies().for_user(a, 10)).unwrap().len(), 2);
}

#[tokio::test]
async fn certain_anomalies_get_a_job_of_their_own() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let (ac, _) = w.anticheat();
    let r = ac.record_anomaly(&anomaly(a, 99, "illegal_move", json!("e1e8"), true));
    assert_eq!(r, Classification { severity: Severity::Certain, certain: true, known: true });
    assert_eq!(ac.pending_count(), 0, "not buffered");
    barrier(&w.store).await;
    let rows = w.store.anomalies().for_user(a, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].detail, Some(json!({ "info": "e1e8", "posMatched": true })));
    assert_eq!(rows[0].severity, Severity::Certain);
    assert_eq!(rows[0].kind, "illegal_move");
}

#[tokio::test]
async fn details_are_stored_as_objects() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let (ac, _) = w.anticheat();
    ac.record_anomaly(&anomaly(a, 1, "forged_type", json!({ "type": 0x80 }), false));
    ac.record_anomaly(&anomaly(a, 2, "flood", json!(12.5), false));
    ac.record_anomaly(&anomaly(a, 3, "malformed", json!(["a", 2]), false));
    ac.record_anomaly(&anomaly(a, 4, "clock_implausible", json!(true), false));
    barrier(&w.store).await;
    let rows = w.store.anomalies().for_user(a, 10).await.unwrap();
    let detail = |g: GameId| rows.iter().find(|r| r.game_id == Some(g)).unwrap().detail.clone().unwrap();
    assert_eq!(detail(1), json!({ "type": 128, "posMatched": false }));
    assert_eq!(detail(2), json!({ "info": "12.5", "posMatched": false }));
    assert_eq!(detail(3), json!({ "0": "a", "1": 2, "posMatched": false }));
    assert_eq!(detail(4), json!({ "info": "true", "posMatched": false }));
}

#[tokio::test]
async fn unknown_kinds_are_recorded_as_info_under_the_label_unknown() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let (ac, _) = w.anticheat();
    let long: &'static str = "x123456789x123456789x123456789x123456789-cut";
    ac.record_anomaly(&anomaly(a, 0, "weird\nkind", Value::Null, false));
    // One label for every unknown kind: written a batching period apart, they are not coalesced.
    barrier(&w.store).await;
    w.clock.advance(f64::from(FLUSH_MS));
    ac.record_anomaly(&anomaly(a, 0, long, Value::Null, false));
    barrier(&w.store).await;
    let rows = w.store.anomalies().for_user(a, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.kind, "unknown");
        assert_eq!(r.severity, Severity::Info);
        assert_eq!(r.game_id, None, "game 0 is stored as none");
    }
    let kinds: Vec<&Value> = rows.iter().map(|r| &r.detail.as_ref().unwrap()["reportedKind"]).collect();
    assert!(kinds.contains(&&json!("weird\nkind")));
    assert!(kinds.contains(&&json!(&long[..40])));
}

#[tokio::test]
async fn a_failing_store_loses_the_rows_without_failing_the_caller() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let (ac, _) = w.anticheat();
    exec(&w.store, "CREATE TRIGGER no_anomalies BEFORE INSERT ON anomalies BEGIN SELECT RAISE(ABORT, 'database is locked'); END;")
        .await;
    let logs = LogCapture::start();
    let before = METRICS.dropped.get();
    ac.record_anomaly(&anomaly(a, 2, "out_of_turn", Value::Null, true));
    ac.record_anomaly(&anomaly(a, 2, "flood", Value::Null, false));
    ac.record_anomaly(&anomaly(a, 2, "flood", Value::Null, false));
    barrier(&w.store).await;
    let errors = || {
        logs.records("anticheat")
            .into_iter()
            .filter(|r| r["level"] == "error")
            .map(|r| (r["msg"].as_str().unwrap_or("").to_string(), r["rows"].as_u64().unwrap_or(0)))
            .collect::<Vec<_>>()
    };
    let lost = ("anomaly batch lost".to_string(), 1);
    let certain = ("certain anomaly not persisted".to_string(), 1);
    assert!(eventually(|| errors().contains(&lost) && errors().contains(&certain)).await, "{:?}", errors());
    assert!(METRICS.dropped.get() - before >= 2);
    assert_eq!(ac.pending_count(), 0);

    // The next anomalies are written again once the store accepts them.
    exec(&w.store, "DROP TRIGGER no_anomalies;").await;
    ac.record_anomaly(&anomaly(a, 3, "flood", Value::Null, false));
    barrier(&w.store).await;
    let rows = w.store.anomalies().for_user(a, 10).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.game_id).collect::<Vec<_>>(), [Some(3)]);
}

#[tokio::test]
async fn a_closed_store_drops_the_buffered_rows() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let (ac, _) = w.anticheat();
    w.store.close().await;
    let logs = LogCapture::start();
    ac.record_anomaly(&anomaly(a, 2, "flood", Value::Null, false));
    ac.record_anomaly(&anomaly(a, 2, "bad_seq", Value::Null, false));
    let lost =
        || logs.records("anticheat").into_iter().any(|r| r["msg"] == "anomaly batch lost" && r["rows"] == 2);
    assert!(eventually(lost).await);
    assert_eq!(ac.pending_count(), 0, "a job that never ran does not keep the buffer");
    // A new job is queued for the next anomaly (it fails too, and is counted).
    ac.record_anomaly(&anomaly(a, 3, "flood", Value::Null, false));
    assert!(eventually(|| ac.pending_count() == 0).await);
}

#[tokio::test]
async fn the_buffer_is_bounded() {
    let w = world(&[], &["ann"]).await;
    let (ac, _) = w.anticheat();
    let before = METRICS.dropped.get();
    let hold = hold_writer(&w.store);
    for g in 0..MAX_PENDING as u64 {
        ac.record_anomaly(&anomaly(w.ids[0], g + 1, "stale_ply", Value::Null, true));
    }
    assert_eq!(ac.pending_count(), MAX_PENDING);
    ac.record_anomaly(&anomaly(w.ids[0], 999_999, "stale_ply", Value::Null, true));
    assert_eq!(ac.pending_count(), MAX_PENDING, "an info row past the first limit is dropped");
    ac.record_anomaly(&anomaly(w.ids[0], 999_999, "flood", Value::Null, true));
    assert_eq!(ac.pending_count(), MAX_PENDING + 1, "the others are kept up to twice as many");
    assert!(METRICS.dropped.get() > before);
    drop(hold);
    barrier(&w.store).await;
    assert_eq!(w.store.anomalies().for_user(w.ids[0], 10_000).await.unwrap().len(), MAX_PENDING + 1);
}

// ---- ordering with the writer (anticheat.writer) -----------------------------------------------

#[tokio::test]
async fn anomalies_recorded_before_a_commit_are_seen_by_its_analysis_queue_policy() {
    let w = world(&[], &["ann", "ben"]).await;
    let (a, b) = (w.ids[0], w.ids[1]);
    let (ac, _) = w.anticheat();
    let flagged = game(a, b, WHITE_WINS, NOW);
    let plain = game(b, a, WHITE_WINS, NOW);
    ac.record_anomaly(&anomaly(a, flagged.id, "clock_implausible", json!({ "thinkMs": 1 }), true));
    // What a host does: record, then commit, without waiting in between.
    let entries = w.store.finish_batch(vec![flagged.clone(), plain.clone()]).await.unwrap();
    assert_eq!(entries.len(), 2);
    let jobs = w.store.analysis().next(10, Some("w".into()), NOW).await.unwrap();
    let priority = |g: GameId| jobs.iter().find(|j| j.game_id == g).map(|j| j.priority);
    assert_eq!(priority(flagged.id), Some(Priority::Signal), "the anomaly was stored before the commit");
    assert_eq!(priority(plain.id), Some(Priority::Ordinary));
    assert_eq!(w.store.anomalies().for_user(a, 10).await.unwrap()[0].kind, "clock_implausible");
}

#[tokio::test]
async fn the_first_anomaly_of_a_game_is_written_before_its_commit_even_right_after_another_game() {
    let w = world(&[], &["ann", "ben"]).await;
    let (a, b) = (w.ids[0], w.ids[1]);
    let (ac, _) = w.anticheat();
    let first = game(a, b, WHITE_WINS, NOW);
    let second = game(b, a, WHITE_WINS, NOW);
    ac.record_anomaly(&anomaly(a, first.id, "clock_implausible", Value::Null, true));
    barrier(&w.store).await;
    // The same kind by the same player a moment later, in another game: not a repeat.
    w.clock.advance(10.0);
    ac.record_anomaly(&anomaly(a, second.id, "clock_implausible", Value::Null, true));
    w.store.finish_batch(vec![second.clone()]).await.unwrap();
    let jobs = w.store.analysis().next(10, Some("w".into()), NOW).await.unwrap();
    let priority = jobs.iter().find(|j| j.game_id == second.id).map(|j| j.priority);
    assert_eq!(priority, Some(Priority::Signal), "the anomaly was stored before the commit");
}

#[tokio::test]
async fn a_certain_cheat_writes_anomaly_ban_integrity_and_refunds_and_makes_one_ban_per_game() {
    let w = world(&[], &["ann", "ben"]).await;
    let (a, b) = (w.ids[0], w.ids[1]);
    for u in [a, b] {
        seed_rating(&w.store, u, "3+2", 1500, 40).await;
    }
    // Ann (the cheater) beats Ben: Ben -10 (K 20).
    w.store.finish_batch(vec![game(a, b, WHITE_WINS, NOW - DAY)]).await.unwrap();
    assert_eq!(w.store.ratings().get(b, "3+2".into()).await.unwrap().rating, 1490);

    let (ac, events) = w.anticheat();
    assert!(ac.record_anomaly(&anomaly(a, 42, "illegal_move", Value::Null, true)).certain);
    let until = NOW + w.config.ban_duration_hours * HOUR;
    let first = ac.sanction(a, 42, "illegal_move");
    // The lobby holds Ann out before the ban is written.
    assert_eq!(events.pending(), [SanctionPending { user: a, until, conn: 0 }]);
    // The second certain anomaly of the game, while the writer has not answered yet.
    assert_eq!(
        ac.sanction(a, 42, "out_of_turn").await,
        SanctionResult { ban_until: until, applied: false, refunds: 0 }
    );
    assert_eq!(first.await, SanctionResult { ban_until: until, applied: true, refunds: 1 });
    assert_eq!(events.pending().len(), 1, "one hold per ban");

    assert_eq!(w.store.anomalies().for_user(a, 10).await.unwrap()[0].severity, Severity::Certain);
    let ban = w.store.sanctions().active_ban(a, NOW).await.unwrap().unwrap();
    assert_eq!(ban.reason.as_deref(), Some("certain_cheat:illegal_move"));
    assert_eq!((ban.source, ban.game_id, ban.ends_at), (Source::Auto, Some(42), Some(until)));
    assert_eq!(w.store.sanctions().list(a).await.unwrap().len(), 1);
    assert_eq!(w.store.integrity().get(a).await.unwrap().level, IntegrityLevel::Confirmed);
    assert_eq!(
        w.store.ratings().get(b, "3+2".into()).await.unwrap().rating,
        1500,
        "Ben got his 10 points back"
    );
    let refunds = w.store.refunds().list(RefundScope::All, 10).await.unwrap();
    assert_eq!(
        refunds.iter().map(|r| (r.victim_id, r.points, r.sanction_id)).collect::<Vec<_>>(),
        [(b, 10, Some(ban.id))]
    );
    let applied = events.applied();
    assert_eq!(applied.len(), 1);
    assert_eq!(
        applied[0],
        SanctionApplied { user: a, until, reason: "certain_cheat:illegal_move".into(), refunds: 1 }
    );
    assert_eq!(events.refunds_pending(), 1);
}

#[tokio::test]
async fn a_ban_that_cannot_be_stored_writes_nothing_and_the_next_certain_anomaly_tries_again() {
    let w = world(&[], &["ann"]).await;
    let a = w.ids[0];
    let (ac, events) = w.anticheat();
    exec(&w.store, "CREATE TRIGGER no_bans BEFORE INSERT ON sanctions BEGIN SELECT RAISE(ABORT, 'database is locked'); END;")
        .await;
    let logs = LogCapture::start();
    assert_eq!(ac.sanction(a, 77, "illegal_move").await, SanctionResult::default());
    assert!(
        logs.records("anticheat")
            .iter()
            .any(|r| r["level"] == "error" && r["msg"] == "automatic ban not stored")
    );
    drop(logs);
    assert_eq!(
        w.store.integrity().get(a).await.unwrap().level,
        IntegrityLevel::None,
        "not confirmed without the ban"
    );
    assert!(w.store.security().for_user(a, 10).await.unwrap().is_empty());
    assert!(events.applied().is_empty());

    exec(&w.store, "DROP TRIGGER no_bans;").await;
    let r = ac.sanction(a, 77, "illegal_move").await;
    assert!(r.applied, "retried");
    assert_eq!(w.store.sanctions().list(a).await.unwrap().len(), 1);
    assert_eq!(w.store.integrity().get(a).await.unwrap().level, IntegrityLevel::Confirmed);
    assert_eq!(events.applied().len(), 1);
}

// ---- the automatic sanction (anticheat.sanction) -----------------------------------------------

#[tokio::test]
async fn a_certain_cheat_makes_one_ban_per_game_with_evidence_and_one_event() {
    let w = world(&[("BAN_DURATION_HOURS", "48")], &["cheat"]).await;
    let u = w.ids[0];
    let (ac, events) = w.anticheat();
    let a = ac.sanction(u, 77, "illegal_move").await;
    assert_eq!(a, SanctionResult { ban_until: NOW + 48 * HOUR, applied: true, refunds: 0 });
    w.clock.advance(10.0);
    let b = ac.sanction(u, 77, "out_of_turn").await;
    let c = ac.sanction(u, 77, "illegal_move").await;
    assert_eq!(b.ban_until, a.ban_until);
    assert!(!c.applied);
    let bans = w.store.sanctions().list(u).await.unwrap();
    assert_eq!(bans.len(), 1);
    assert_eq!(
        (bans[0].kind, bans[0].source, bans[0].game_id, bans[0].reason.as_deref()),
        (SanctionKind::Ban, Source::Auto, Some(77), Some("certain_cheat:illegal_move"))
    );
    let integ = w.store.integrity().get(u).await.unwrap();
    assert_eq!(integ.level, IntegrityLevel::Confirmed);
    assert_eq!(
        integ.evidence.unwrap()["certain"],
        json!([{ "kind": "illegal_move", "gameId": 77, "at": NOW, "banUntil": a.ban_until }])
    );
    assert_eq!(
        events.applied(),
        [SanctionApplied {
            user: u,
            until: a.ban_until,
            reason: "certain_cheat:illegal_move".into(),
            refunds: 0
        }]
    );
    assert_eq!(events.refunds_pending(), 0, "no refund given");
    let security = w.store.security().for_user(u, 10).await.unwrap();
    let auto: Vec<_> = security.iter().filter(|e| e.kind == "sanction_auto").collect();
    assert_eq!(auto.len(), 1);
    assert_eq!(auto[0].detail, Some(json!({ "kind": "illegal_move", "gameId": 77, "until": a.ban_until })));
}

#[tokio::test]
async fn another_service_reuses_the_ban_of_the_same_game() {
    let w = world(&[], &["cheat"]).await;
    let u = w.ids[0];
    let (shard1, _) = w.anticheat();
    let (shard2, events2) = w.anticheat();
    let a = shard1.sanction(u, 1, "forged_type").await;
    w.clock.advance(5.0);
    let b = shard2.sanction(u, 1, "foreign_game").await;
    assert_eq!(w.store.sanctions().list(u).await.unwrap().len(), 1);
    assert_eq!(b.ban_until, a.ban_until);
    assert!(!b.applied);
    assert!(events2.applied().is_empty());
    // Another game is another offence: a new ban (ending later).
    let c = shard2.sanction(u, 2, "illegal_move").await;
    assert!(c.applied);
    assert_eq!(w.store.sanctions().list(u).await.unwrap().len(), 2);
    let evidence = w.store.integrity().get(u).await.unwrap().evidence.unwrap();
    assert_eq!(evidence["certain"].as_array().unwrap().len(), 3, "every sanction adds evidence");
}

#[tokio::test]
async fn an_analysis_update_does_not_overwrite_a_ban_committed_meanwhile() {
    let w = world(&[], &["cheat"]).await;
    let u = w.ids[0];
    let (ac, _) = w.anticheat();
    let population = Population::new(None, w.clock.clone());
    // The scoring reads the player's games...
    let (games, result) = w
        .store
        .read({
            let population = Population::new(None, w.clock.clone());
            move |db| Ok::<_, StoreError>(score_player_games(db, u, &population))
        })
        .await
        .unwrap();
    // ...the player is banned meanwhile...
    assert!(ac.sanction(u, 7, "illegal_move").await.applied);
    // ...and the new level is applied to the record as it is now.
    let update = w
        .store
        .write(move |db| apply_player_score(db, u, &population, &games, &result, NOW + 1))
        .await
        .unwrap();
    assert_eq!(update.level, crate::anticheat::integrity::IntegrityLevel::Confirmed);
    let integ = w.store.integrity().get(u).await.unwrap();
    assert_eq!(integ.level, IntegrityLevel::Confirmed);
    let evidence = integ.evidence.unwrap();
    assert_eq!(evidence["certain"][0]["kind"], "illegal_move", "the evidence of the ban is kept");
    assert_eq!(evidence["statistics"]["games"], 0);
}

#[tokio::test]
async fn a_longer_ban_stands_for_the_automatic_one_only_when_it_refunds() {
    for (reason, reused) in
        [("confirmed: engine", true), ("confirmed, no refund: engine", false), ("abusive chat", false)]
    {
        let w = world(&[], &["cheat"]).await;
        let u = w.ids[0];
        let ends_at = NOW + 30 * DAY;
        w.store
            .sanctions()
            .create(NewSanction {
                user_id: u,
                kind: SanctionKind::Ban,
                reason: Some(reason.into()),
                source: Source::Moderator,
                game_id: None,
                starts_at: NOW - 1000,
                ends_at: Some(ends_at),
                created_by: Some("mod".into()),
                created_at: NOW - 1000,
            })
            .await
            .unwrap();
        let (ac, _) = w.anticheat();
        let r = ac.sanction(u, 8, "illegal_move").await;
        let bans = w.store.sanctions().list(u).await.unwrap().len();
        assert_eq!((r.applied, bans), if reused { (false, 1) } else { (true, 2) }, "{reason}");
        assert_eq!(r.ban_until, if reused { ends_at } else { NOW + 24 * HOUR }, "{reason}");
    }
}

#[tokio::test]
async fn a_new_game_after_the_ban_expired_gets_a_new_ban() {
    let w = world(&[("BAN_DURATION_HOURS", "1")], &["cheat"]).await;
    let u = w.ids[0];
    let (ac, _) = w.anticheat();
    ac.sanction(u, 10, "illegal_move").await;
    w.clock.advance((2 * HOUR) as f64);
    assert!(ac.sanction(u, 11, "illegal_move").await.applied);
    assert_eq!(w.store.sanctions().list(u).await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_sanction_is_off_without_auto_sanction_certain_cheats() {
    let w = world(&[("AUTO_SANCTION_CERTAIN_CHEATS", "false")], &["cheat"]).await;
    let u = w.ids[0];
    let (ac, events) = w.anticheat();
    assert_eq!(ac.sanction(u, 2, "illegal_move").await, SanctionResult::default());
    AnomalySink::sanction_certain(&ac, u, 3, "forged_type", 7);
    barrier(&w.store).await;
    assert!(w.store.sanctions().list(u).await.unwrap().is_empty());
    assert_eq!(w.store.integrity().get(u).await.unwrap().level, IntegrityLevel::None);
    assert!(events.pending().is_empty() && events.applied().is_empty());
}

#[tokio::test]
async fn confirmed_keeps_the_statistical_evidence_already_present() {
    let w = world(&[], &["cheat"]).await;
    let u = w.ids[0];
    w.store
        .integrity()
        .set(
            u,
            IntegrityUpdate {
                level: Some(IntegrityLevel::Suspected),
                score: Some(3.9),
                evidence: Some(Some(json!({ "statistics": { "score": 3.9 } }))),
                updated_at: Some(1),
                ..IntegrityUpdate::default()
            },
        )
        .await
        .unwrap();
    let (ac, _) = w.anticheat();
    ac.sanction(u, 5, "out_of_turn").await;
    let i = w.store.integrity().get(u).await.unwrap();
    assert_eq!(i.level, IntegrityLevel::Confirmed);
    assert_eq!(i.score, 3.9);
    let evidence = i.evidence.unwrap();
    assert_eq!(evidence["statistics"]["score"], 3.9);
    assert_eq!(evidence.as_object().unwrap().keys().collect::<Vec<_>>(), ["statistics", "certain"]);
}

#[tokio::test]
async fn the_sink_spawns_the_follow_up() {
    let w = world(&[], &["cheat"]).await;
    let u = w.ids[0];
    let (ac, events) = w.anticheat();
    let sink: Arc<dyn AnomalySink> = Arc::new(ac.clone());
    sink.record(anomaly(u, 4, "forged_type", json!({ "type": 200 }), false));
    sink.sanction_certain(u, 4, "forged_type", 7);
    // The hold is announced before the call returns, with the connection the cheat came from;
    // the ban follows once written.
    let until = NOW + w.config.ban_duration_hours * HOUR;
    assert_eq!(events.pending(), [SanctionPending { user: u, until, conn: 7 }]);
    assert!(events.applied().is_empty());
    assert!(eventually(|| events.applied().len() == 1).await);
    assert_eq!(events.applied()[0].until, until, "the hold announced the ban's end");
    assert_eq!(events.applied()[0].reason, "certain_cheat:forged_type");
    let anomalies = w.store.anomalies().for_user(u, 10).await.unwrap();
    assert_eq!(anomalies.len(), 1);
    assert_eq!(anomalies[0].kind, "forged_type");
}

/// Stalls the post of the first hold until the test says the repeat returned, or for 200 ms.
struct StalledHold {
    log: Mutex<Vec<&'static str>>,
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    repeat_returned: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl SanctionEvents for StalledHold {
    fn sanction_pending(&self, _: SanctionPending) {
        if let Some(tx) = self.entered.lock().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.repeat_returned.lock().take() {
            let _ = rx.recv_timeout(Duration::from_millis(200));
        }
        self.log.lock().push("hold");
    }

    fn sanction_applied(&self, _: SanctionApplied) {}

    fn refunds_pending(&self) {}
}

#[tokio::test]
async fn a_repeat_returns_only_once_the_first_sanction_of_the_game_has_posted_its_hold() {
    let w = world(&[], &["cheat"]).await;
    let u = w.ids[0];
    let ac = Anticheat::new(&w.config, w.store.clone(), w.clock.clone());
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (returned, returned_rx) = std::sync::mpsc::channel();
    let events = Arc::new(StalledHold {
        log: Mutex::new(Vec::new()),
        entered: Mutex::new(Some(entered_tx)),
        repeat_returned: Mutex::new(Some(returned_rx)),
    });
    ac.set_sanction_events(events.clone());
    // The shard's sanction (illegal_move) stalls in the post of its hold...
    let first = std::thread::spawn({
        let ac = ac.clone();
        move || ac.sanction(u, 9, "illegal_move")
    });
    entered.recv().unwrap();
    // ...while the connection task's sanction of the same game (forged_type) comes in: it waits
    // for the hold (the stall then lasts its 200 ms), or its caller's 4302 could reach the client
    // before the hold reaches the lobby.
    let repeat = ac.sanction(u, 9, "forged_type");
    events.log.lock().push("repeat");
    let _ = returned.send(());
    assert_eq!(*events.log.lock(), ["hold", "repeat"]);
    assert!(!repeat.await.applied);
    assert!(first.join().unwrap().await.applied);
}

// ---- refunds of an automatic ban (anticheat.refunds) -------------------------------------------

/// The players of the refund scenarios ([`REFUND_PLAYERS`]) with their rated records.
async fn refund_world(overrides: &[(&str, &str)]) -> World {
    let w = world(overrides, &REFUND_PLAYERS).await;
    seed_refund_ratings(&w.store, &w.ids).await;
    w
}

async fn play(w: &World) -> RefundScenario {
    play_refund_scenario(&w.store, &w.ids).await
}

async fn lost(w: &World, g: &GameRecord, white: bool) -> i64 {
    rating_lost(&w.store, g, white).await
}

async fn rating(w: &World, user: UserId) -> crate::store::RatingRecord {
    w.store.ratings().get(user, "3+2".into()).await.unwrap()
}

#[tokio::test]
async fn an_automatic_ban_gives_back_what_each_victim_lost_on_their_current_rating() {
    let w = refund_world(&[]).await;
    let [cheat, vic, val, ..] = w.ids[..] else { unreachable!() };
    let g = play(&w).await;
    let vic_lost = lost(&w, &g.vic_loss, false).await;
    let val_lost = lost(&w, &g.val_draw, true).await;
    assert_eq!(vic_lost, 10, "equal ratings, K 20: 20 x 0.5");
    assert!(val_lost > 0, "a draw against a lower-rated player costs points");
    assert!(lost(&w, &g.nova_first, false).await > 0, "Nova's first rating is below the working rating");
    assert!(lost(&w, &g.old, false).await > 0);
    let mut before = Vec::new();
    for &u in &w.ids {
        before.push(rating(&w, u).await);
    }

    let (ac, events) = w.anticheat();
    let r = ac.sanction(cheat, 999, "illegal_move").await;
    assert!(r.applied);
    assert_eq!(r.refunds, 2, "Vic and Val");
    assert_eq!(rating(&w, vic).await.rating, before[1].rating + vic_lost, "added to the current rating");
    assert_eq!(rating(&w, val).await.rating, before[2].rating + val_lost);
    for i in [0, 3, 4, 5, 6] {
        assert_eq!(rating(&w, w.ids[i]).await, before[i], "player {i} untouched");
    }
    assert_eq!(rating(&w, vic).await.games, before[1].games, "the games still happened");

    let mut rows: Vec<_> = w
        .store
        .refunds()
        .list(RefundScope::Cheater(cheat), 10)
        .await
        .unwrap()
        .into_iter()
        .map(|x| (x.game_id, x.victim_name.clone(), x.points, x.source, x.category.clone(), x))
        .collect();
    rows.sort_by_key(|r| r.0);
    assert_eq!(
        rows.iter().map(|r| (r.0, r.1.as_str(), r.2, r.3, r.4.as_str())).collect::<Vec<_>>(),
        [
            (g.vic_loss.id, "Vic", vic_lost, Source::Auto, "3+2"),
            (g.val_draw.id, "Val", val_lost, Source::Auto, "3+2")
        ]
    );
    let ban = w.store.sanctions().active_ban(cheat, NOW).await.unwrap().unwrap();
    assert!(rows.iter().all(|r| r.5.sanction_id == Some(ban.id)
        && r.5.created_at == NOW
        && r.5.notified_at.is_none()
        && r.5.created_by.is_none()));
    let vic_refund = rows.iter().find(|r| r.5.victim_id == vic).unwrap().5.id;
    let pending = w.store.refunds().pending_for(vic).await.unwrap();
    assert_eq!((pending.ids, pending.points), (vec![vic_refund], vic_lost));
    assert_eq!(
        events.applied(),
        [SanctionApplied {
            user: cheat,
            until: ban.ends_at.unwrap(),
            reason: "certain_cheat:illegal_move".into(),
            refunds: 2
        }]
    );
    assert_eq!(events.refunds_pending(), 1);
    // The audit trail: one rating_refund event per refund.
    let audit = w.store.security().for_user(vic, 10).await.unwrap();
    let refund_event = audit.iter().find(|e| e.kind == "rating_refund").unwrap();
    assert_eq!(
        refund_event.detail,
        Some(json!({ "refundId": vic_refund, "gameId": g.vic_loss.id, "cheaterId": cheat, "category": "3+2",
            "points": 10, "source": "auto", "sanctionId": ban.id, "by": null }))
    );
}

#[tokio::test]
async fn refunds_are_given_once_per_game_and_victim() {
    let w = refund_world(&[]).await;
    let [cheat, vic, ..] = w.ids[..] else { unreachable!() };
    let g = play(&w).await;
    let (ac, events) = w.anticheat();
    assert_eq!(ac.sanction(cheat, 1, "illegal_move").await.refunds, 2);
    let vic_rating = rating(&w, vic).await.rating;
    // A later cheat in another game, after the first ban expired: a new ban, no new refund.
    w.clock.advance((2 * DAY) as f64);
    let again = ac.sanction(cheat, 2, "out_of_turn").await;
    assert!(again.applied);
    assert_eq!(again.refunds, 0);
    assert_eq!(events.refunds_pending(), 1, "only the first ban gave refunds");
    assert_eq!(rating(&w, vic).await.rating, vic_rating);
    assert_eq!(w.store.refunds().list(RefundScope::All, 10).await.unwrap().len(), 2);
    // The store itself refuses a second refund of a game.
    let request = crate::store::CheaterRefunds {
        cheater_id: cheat,
        since: 0,
        now: NOW + 2 * DAY,
        sanction_id: None,
        source: Source::Moderator,
        by: Some("x".into()),
    };
    let given = w.store.refunds().apply_for_cheater(request.clone()).await.unwrap();
    assert_eq!(
        given.iter().map(|r| r.game_id).collect::<Vec<_>>(),
        [g.old.id],
        "only the game outside the window"
    );
    assert!(w.store.refunds().apply_for_cheater(request).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_victims_peak_rises_with_a_refund_and_a_lifted_ban_takes_nothing_back() {
    let w = refund_world(&[]).await;
    let [cheat, vic, ..] = w.ids[..] else { unreachable!() };
    play(&w).await;
    assert_eq!(rating(&w, vic).await.peak, 1500, "the seeded peak");
    let (ac, _) = w.anticheat();
    ac.sanction(cheat, 1, "illegal_move").await;
    let after = rating(&w, vic).await;
    assert!(after.rating > 1500, "{}", after.rating);
    assert_eq!(after.peak, after.rating.max(1500));
    let ban = w.store.sanctions().active_ban(cheat, NOW).await.unwrap().unwrap();
    assert!(w.store.sanctions().lift(ban.id, Some("mod".into()), NOW + 1000).await.unwrap());
    assert_eq!(rating(&w, vic).await.rating, after.rating);
}

#[tokio::test]
async fn no_automatic_refunds_with_rating_refund_days_0() {
    let w = refund_world(&[("RATING_REFUND_DAYS", "0")]).await;
    play(&w).await;
    let (ac, events) = w.anticheat();
    assert_eq!(ac.sanction(w.ids[0], 1, "illegal_move").await.refunds, 0);
    assert!(w.store.refunds().list(RefundScope::All, 10).await.unwrap().is_empty());
    assert_eq!(events.refunds_pending(), 0);
}

#[tokio::test]
async fn a_certain_cheat_under_a_ban_that_does_not_refund_gets_a_ban_of_its_own() {
    for reason in ["abusive chat", "confirmed, no refund: engine"] {
        let w = refund_world(&[]).await;
        let [cheat, vic, ..] = w.ids[..] else { unreachable!() };
        w.store
            .sanctions()
            .create(NewSanction {
                user_id: cheat,
                kind: SanctionKind::Ban,
                reason: Some(reason.into()),
                source: Source::Moderator,
                game_id: None,
                starts_at: NOW,
                ends_at: Some(NOW + 240 * HOUR),
                created_by: Some("mod".into()),
                created_at: NOW,
            })
            .await
            .unwrap();
        let (ac, _) = w.anticheat();
        assert!(ac.sanction(cheat, 5, "illegal_move").await.applied, "{reason}");
        let auto =
            w.store.sanctions().list(cheat).await.unwrap().into_iter().find(|s| s.source == Source::Auto);
        let auto = auto.unwrap();
        assert_eq!(auto.reason.as_deref(), Some("certain_cheat:illegal_move"));
        // A game on its way to the database at the ban is refunded as it is recorded.
        w.store.finish_batch(vec![game(cheat, vic, WHITE_WINS, NOW)]).await.unwrap();
        let rows = w.store.refunds().list(RefundScope::All, 10).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| (r.victim_name.as_str(), r.points, r.sanction_id)).collect::<Vec<_>>(),
            [("Vic", 10, Some(auto.id))],
            "{reason}"
        );
    }
}
