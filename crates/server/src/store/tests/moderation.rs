//! Port of store.moderation.test.js: conduct, sanctions, anomalies and security events, integrity
//! and population statistics, reports, the retention purge, and atomic multi-call jobs.

use serde_json::json;

use super::support::*;
use crate::ids::UserId;
use crate::store::retention::RetentionPolicy;
use crate::store::{
    ConductCounts, ConductKind, Cooldown, ErrorKind, Integrity, IntegrityLevel, IntegrityUpdate, NewAnomaly,
    NewReport, NewSanction, NewSecurityEvent, NewSession, NewToken, PopulationUpdate, PurgeCounts,
    ReportCategory, ReportStatus, Sample, SanctionKind, Severity, Source, Store, StoreError,
};

const DAY: i64 = 86_400_000;
const T: i64 = 1_800_000_000_000;

async fn setup() -> (Store, [UserId; 4]) {
    let store = memory_store().await;
    let mut ids = [0; 4];
    for (i, name) in ["Ann", "Ben", "Cid", "Dee"].iter().enumerate() {
        ids[i] = store.users().create(new_user(name, Some(&format!("{name}@example.org")))).await.unwrap();
    }
    (store, ids)
}

fn counts(abandon: i64, abort: i64, noshow: i64) -> ConductCounts {
    ConductCounts { abandon, abort, noshow }
}

#[tokio::test]
async fn conduct_events_counts_since_cooldown_state() {
    let (store, [a, b, ..]) = setup().await;
    let conduct = store.conduct();
    conduct.record(a, ConductKind::Abandon, T).await.unwrap();
    conduct.record(a, ConductKind::Abandon, T + 1000).await.unwrap();
    conduct.record(a, ConductKind::NoShow, T + 2000).await.unwrap();
    conduct.record(a, ConductKind::Abort, T - DAY).await.unwrap();
    conduct.record(b, ConductKind::Abort, T).await.unwrap();
    assert_eq!(conduct.count_since(a, T - 1).await.unwrap(), counts(2, 0, 1));
    assert_eq!(conduct.count_since(a, T - DAY).await.unwrap(), counts(2, 1, 1));
    assert_eq!(conduct.count_since(a, T - DAY).await.unwrap().total(), 4);
    assert_eq!(conduct.count_since(b, T + 1).await.unwrap(), counts(0, 0, 0));
    assert_eq!(ConductKind::parse("rage"), None, "unknown kinds are the caller's to refuse");
    assert_eq!(conduct.cooldown(a).await.unwrap(), Cooldown { until: 0, level: 0, updated_at: 0 });
    conduct.set_cooldown(a, T + 900_000, 1, T).await.unwrap();
    assert_eq!(conduct.cooldown(a).await.unwrap(), Cooldown { until: T + 900_000, level: 1, updated_at: T });
    conduct.set_cooldown(a, T + 3_600_000, 2, T + 5).await.unwrap();
    assert_eq!(
        conduct.cooldown(a).await.unwrap(),
        Cooldown { until: T + 3_600_000, level: 2, updated_at: T + 5 }
    );
    store.close().await;
}

fn sanction(user_id: UserId, kind: SanctionKind, reason: &str, source: Source) -> NewSanction {
    NewSanction {
        user_id,
        kind,
        reason: Some(reason.into()),
        source,
        game_id: None,
        starts_at: T,
        ends_at: None,
        created_by: None,
        created_at: T,
    }
}

#[tokio::test]
async fn sanctions_create_active_ban_list_lift() {
    let (store, [a, b, ..]) = setup().await;
    let s = store.sanctions();
    assert_eq!(s.active_ban(a, T).await.unwrap(), None);
    let w = s
        .create(NewSanction {
            created_by: Some("mod1".into()),
            ..sanction(a, SanctionKind::Warning, "abuse", Source::Moderator)
        })
        .await
        .unwrap();
    let b1 = s
        .create(NewSanction {
            game_id: Some(77),
            ends_at: Some(T + DAY),
            ..sanction(a, SanctionKind::Ban, "illegal_move", Source::Auto)
        })
        .await
        .unwrap();
    let b2 = s
        .create(NewSanction {
            ends_at: Some(T + 7 * DAY),
            created_at: T + 1,
            ..sanction(a, SanctionKind::Ban, "longer", Source::Moderator)
        })
        .await
        .unwrap();
    s.create(NewSanction {
        starts_at: T + DAY,
        ends_at: Some(T + 2 * DAY),
        ..sanction(b, SanctionKind::Ban, "future", Source::Moderator)
    })
    .await
    .unwrap();
    let ban = s.active_ban(a, T + 10).await.unwrap().unwrap();
    assert_eq!(ban.id, b2, "the longest ban");
    assert_eq!(ban.kind, SanctionKind::Ban);
    assert_eq!(ban.source, Source::Moderator);
    assert_eq!(s.active_ban(a, T + 7 * DAY).await.unwrap(), None, "expired");
    assert_eq!(s.active_ban(b, T).await.unwrap(), None, "not started");
    assert_eq!(s.active_ban(b, T + DAY).await.unwrap().unwrap().reason.as_deref(), Some("future"));
    assert!(s.lift(b2, Some("mod2".into()), T + 20).await.unwrap());
    assert!(!s.lift(b2, Some("mod2".into()), T + 21).await.unwrap());
    assert_eq!(s.active_ban(a, T + 30).await.unwrap().unwrap().id, b1);
    let perm = s.create(sanction(a, SanctionKind::Ban, "permanent", Source::Moderator)).await.unwrap();
    assert_eq!(s.active_ban(a, T + 100 * DAY).await.unwrap().unwrap().id, perm);
    assert_eq!(s.active_ban(a, T + 40).await.unwrap().unwrap().id, perm, "a permanent ban comes first");
    let list = s.list(a).await.unwrap();
    assert_eq!(list.len(), 4);
    let find = |id| list.iter().find(|x| x.id == id).unwrap();
    assert_eq!(find(b2).lifted_by.as_deref(), Some("mod2"));
    assert_eq!(find(b1).game_id, Some(77));
    assert_eq!(find(w).created_by.as_deref(), Some("mod1"));
    let mut active: Vec<i64> = s.active(a, T + 40).await.unwrap().iter().map(|x| x.id).collect();
    active.sort_unstable();
    assert_eq!(active, vec![w, b1, perm]);
    assert_eq!(SanctionKind::parse("jail"), None);
    store.close().await;
}

fn anomaly(user_id: Option<UserId>, kind: &str, severity: Severity, at: i64) -> NewAnomaly {
    NewAnomaly { user_id, game_id: None, kind: kind.into(), severity, at: Some(at), detail: None }
}

#[tokio::test]
async fn anomalies_and_security_events_batches_in_one_transaction_and_reads() {
    let (store, [a, ..]) = setup().await;
    let n = store
        .anomalies()
        .insert_batch(vec![
            NewAnomaly {
                game_id: Some(5),
                detail: Some(json!({"move": 1234})),
                ..anomaly(Some(a), "illegal_move", Severity::Certain, T)
            },
            NewAnomaly {
                game_id: Some(5),
                detail: Some(json!("ply 7")),
                ..anomaly(Some(a), "desync", Severity::Info, T + 1)
            },
            anomaly(Some(0), "malformed", Severity::Suspicious, T + 2),
        ])
        .await
        .unwrap();
    assert_eq!(n, 3);
    let an = store.anomalies().for_user(a, 10).await.unwrap();
    assert_eq!(an.len(), 2);
    assert_eq!(an[0].kind, "desync");
    assert_eq!(an[0].detail, Some(json!("ply 7")));
    assert_eq!(an[0].game_id, Some(5));
    assert_eq!(an[1].detail, Some(json!({"move": 1234})));
    assert_eq!(store.anomalies().for_user(a, 1).await.unwrap().len(), 1);
    // A failing job rolls its whole batch back; a failed savepoint only its own rows.
    let e = store
        .write(move |db| {
            db.anomalies().insert_batch(&[anomaly(Some(a), "ok", Severity::Info, T)])?;
            Err::<(), _>(StoreError::invalid("bad row"))
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);
    assert_eq!(store.anomalies().for_user(a, 10).await.unwrap().len(), 2);
    store
        .write(move |db| {
            let inner: Result<(), StoreError> = db.transaction(|db| {
                db.anomalies().insert_batch(&[anomaly(Some(a), "undone", Severity::Info, T)])?;
                Err(StoreError::invalid("inner"))
            });
            assert!(inner.is_err());
            db.anomalies().insert_batch(&[anomaly(Some(a), "kept", Severity::Info, T + 3)])?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let kinds: Vec<String> =
        store.anomalies().for_user(a, 10).await.unwrap().into_iter().map(|x| x.kind).collect();
    assert_eq!(kinds, ["kept", "desync", "illegal_move"]);
    assert_eq!(store.anomalies().insert_batch(vec![]).await.unwrap(), 0);

    let n = store
        .security()
        .insert_batch(vec![
            NewSecurityEvent {
                kind: "login_failed".into(),
                user_id: Some(a),
                ip: Some("192.0.2.1".into()),
                at: Some(T),
                detail: Some(json!({"reason": "password"})),
            },
            NewSecurityEvent {
                kind: "login_failed".into(),
                user_id: None,
                ip: Some("192.0.2.2".into()),
                at: Some(T + 1),
                detail: None,
            },
        ])
        .await
        .unwrap();
    assert_eq!(n, 2);
    let sec = store.security().for_user(a, 10).await.unwrap();
    assert_eq!(sec.len(), 1);
    assert_eq!(sec[0].ip.as_deref(), Some("192.0.2.1"));
    assert_eq!(sec[0].detail, Some(json!({"reason": "password"})));
    store.close().await;
}

#[tokio::test]
async fn integrity_defaults_set_merge_flagged_list_population_statistics() {
    let (store, [a, b, c, _]) = setup().await;
    let integrity = store.integrity();
    assert_eq!(integrity.get(a).await.unwrap(), Integrity::default());
    let set = |user, f: IntegrityUpdate| integrity.set(user, f);
    set(
        a,
        IntegrityUpdate {
            level: Some(IntegrityLevel::Suspected),
            score: Some(2.5),
            evidence: Some(Some(json!({"signals": ["acpl"]}))),
            updated_at: Some(T),
            ..IntegrityUpdate::default()
        },
    )
    .await
    .unwrap();
    let level = |l, score| IntegrityUpdate {
        level: Some(l),
        score: Some(score),
        updated_at: Some(T),
        ..IntegrityUpdate::default()
    };
    set(b, level(IntegrityLevel::HighConfidence, 4.1)).await.unwrap();
    set(c, level(IntegrityLevel::None, 0.3)).await.unwrap();
    set(a, IntegrityUpdate { score: Some(3.0), updated_at: Some(T + 1), ..IntegrityUpdate::default() })
        .await
        .unwrap();
    let ia = integrity.get(a).await.unwrap();
    assert_eq!(ia.level, IntegrityLevel::Suspected, "fields not given are kept");
    assert_eq!(ia.score, 3.0);
    assert_eq!(ia.evidence, Some(json!({"signals": ["acpl"]})));
    assert_eq!(ia.updated_at, T + 1);
    set(
        b,
        IntegrityUpdate {
            level: Some(IntegrityLevel::Confirmed),
            reviewed_by: Some(Some("mod1".into())),
            reviewed_at: Some(Some(T + 2)),
            note: Some(Some("engine match".into())),
            ..IntegrityUpdate::default()
        },
    )
    .await
    .unwrap();
    let ib = integrity.get(b).await.unwrap();
    assert_eq!(ib.reviewed_by.as_deref(), Some("mod1"));
    assert_eq!(ib.score, 4.1, "kept");
    assert_eq!(IntegrityLevel::parse("guilty"), None);
    let flagged = integrity.list_flagged(IntegrityLevel::Suspected, 10).await.unwrap();
    let rows: Vec<_> = flagged.iter().map(|r| (r.user_id, r.integrity.level)).collect();
    assert_eq!(rows, vec![(b, IntegrityLevel::Confirmed), (a, IntegrityLevel::Suspected)]);
    assert_eq!(flagged[0].username, "Ben");
    let high = integrity.list_flagged(IntegrityLevel::HighConfidence, 10).await.unwrap();
    assert_eq!(high.iter().map(|r| r.user_id).collect::<Vec<_>>(), vec![b]);
    assert_eq!(
        integrity.list_flagged(IntegrityLevel::None, 10).await.unwrap().len(),
        2,
        "level none is never listed"
    );

    // Welford statistics: merging batches equals computing over all values.
    let values = [10.0, 12.0, 23.0, 23.0, 16.0, 23.0, 21.0, 16.0];
    let up = |key: &str, sample| PopulationUpdate { key: key.into(), sample };
    integrity
        .update_population(vec![up("3+2|1500|acpl", Sample::Values(values[..3].to_vec()))], T)
        .await
        .unwrap();
    integrity
        .update_population(vec![up("3+2|1500|acpl", Sample::Values(values[3..7].to_vec()))], T)
        .await
        .unwrap();
    integrity
        .update_population(
            vec![
                up("3+2|1500|acpl", Sample::Values(vec![values[7], f64::NAN])),
                up("3+2|1500|empty", Sample::Values(vec![])),
            ],
            T + 1,
        )
        .await
        .unwrap();
    integrity
        .update_population(vec![up("3+2|1500|agree", Sample::Stats { n: 4, mean: 0.5, m2: 0.2 })], T)
        .await
        .unwrap();
    integrity.update_population(vec![up("3+2|15000|acpl", Sample::Values(vec![99.0]))], T).await.unwrap();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (values.len() - 1) as f64;
    let pop = integrity.population_stats("3+2|1500".into()).await.unwrap();
    let mut keys: Vec<&String> = pop.keys().collect();
    keys.sort();
    assert_eq!(keys, ["acpl", "agree"]);
    let acpl = pop["acpl"];
    assert_eq!(acpl.n, 8);
    assert!((acpl.mean - mean).abs() < 1e-9);
    assert!((acpl.variance - variance).abs() < 1e-9);
    assert!((acpl.stdev - variance.sqrt()).abs() < 1e-9);
    assert_eq!(acpl.updated_at, T + 1);
    assert_eq!(pop["agree"].n, 4);
    assert!(integrity.population_stats("5+0|1500".into()).await.unwrap().is_empty());
    assert_eq!(integrity.population_stats("3+2|15000".into()).await.unwrap()["acpl"].n, 1);
    store.close().await;
}

fn report(from: UserId, to: UserId, game: Option<u64>, category: ReportCategory, at: i64) -> NewReport {
    NewReport { reporter_id: from, reported_id: to, game_id: game, category, comment: None, weight: 1.0, at }
}

#[tokio::test]
async fn reports_create_uniqueness_counts_open_list_resolution() {
    let (store, [a, b, c, _]) = setup().await;
    let reports = store.reports();
    let r1 = reports
        .create(NewReport {
            comment: Some("engine".into()),
            weight: 0.8,
            ..report(a, b, Some(11), ReportCategory::Cheating, T)
        })
        .await
        .unwrap();
    reports.create(report(c, b, Some(11), ReportCategory::Abuse, T + 1)).await.unwrap();
    reports.create(report(a, b, None, ReportCategory::Other, T + 2)).await.unwrap();
    let e = reports.create(report(a, b, Some(11), ReportCategory::Abuse, T + 3)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Duplicate);
    let e = reports.create(report(a, b, None, ReportCategory::Abuse, T + 3)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Duplicate, "one report without a game per pair");
    let e = reports.create(report(a, 9999, None, ReportCategory::Abuse, T + 3)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ForeignKey);
    assert!(reports.exists(a, b, Some(11)).await.unwrap());
    assert!(reports.exists(a, b, None).await.unwrap());
    assert!(!reports.exists(b, a, Some(11)).await.unwrap());
    assert!(!reports.exists(a, c, None).await.unwrap());
    assert_eq!(reports.count_by_reporter_since(a, T).await.unwrap(), 2);
    assert_eq!(reports.count_by_reporter_since(a, T + 1).await.unwrap(), 1);
    let open = reports.list_open(10).await.unwrap();
    assert_eq!(open.len(), 3);
    assert_eq!(open[0].id, r1);
    assert_eq!(open[0].reporter_name, "Ann");
    assert_eq!(open[0].reported_name, "Ben");
    assert_eq!(open[0].weight, 0.8);
    assert_eq!(open[2].game_id, None);
    assert!(reports.resolve(r1, ReportStatus::Actioned, Some("mod1".into()), T + 10).await.unwrap());
    assert!(!reports.resolve(r1, ReportStatus::Dismissed, Some("mod1".into()), T + 11).await.unwrap());
    let e = reports.resolve(r1, ReportStatus::Open, Some("mod1".into()), T).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);
    assert_eq!(reports.list_open(10).await.unwrap().len(), 2);
    let for_b = reports.for_reported(b, 50).await.unwrap();
    assert_eq!(for_b.len(), 3);
    let first = for_b.iter().find(|r| r.id == r1).unwrap();
    assert_eq!(first.status, ReportStatus::Actioned);
    assert_eq!(first.resolved_by.as_deref(), Some("mod1"));
    assert_eq!(first.resolved_at, Some(T + 10));
    let counts = reports.count_for(b).await.unwrap();
    assert_eq!((counts.total, counts.open), (3, 2));
    let w = reports.weight_since(b, T, 0.9).await.unwrap();
    assert!((w.total - 2.8).abs() < 1e-9 && (w.low - 0.8).abs() < 1e-9, "{w:?}");
    let resolved = reports
        .resolve_open_for(b, ReportCategory::Abuse, ReportStatus::Dismissed, None, T + 20)
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(reports.count_for(b).await.unwrap().open, 1);
    store.close().await;
}

/// The retention fixture: every kind of row on both sides of each cutoff.
async fn retention_fixture(store: &Store, a: UserId, now: i64) {
    let session = |h: &str, created_at, expires_at, idle, ip: Option<&str>| NewSession {
        user_id: a,
        token_hash: h.into(),
        created_at,
        expires_at,
        idle_expires_at: Some(idle),
        client_label: None,
        ip: ip.map(Into::into),
    };
    let s = store.sessions();
    s.create(session("s-exp", now - 100 * DAY, now - 1, now + DAY, Some("1.1.1.1"))).await.unwrap();
    s.create(session("s-idle", now - 10 * DAY, now + DAY, now - 1, Some("1.1.1.2"))).await.unwrap();
    let rev = s.create(session("s-rev", now - 10 * DAY, now + DAY, now + DAY, None)).await.unwrap();
    s.revoke(rev, Some(a), now - 2 * DAY).await.unwrap();
    let recent = s.create(session("s-rev2", now - 10 * DAY, now + DAY, now + DAY, None)).await.unwrap();
    s.revoke(recent, Some(a), now - 1000).await.unwrap();
    s.create(session("s-old", now - 40 * DAY, now + 50 * DAY, now + DAY, Some("1.1.1.3"))).await.unwrap();
    s.create(session("s-new", now - DAY, now + 50 * DAY, now + DAY, Some("1.1.1.4"))).await.unwrap();
    let token = |h: &str, expires_at| NewToken {
        kind: "reset".into(),
        token_hash: h.into(),
        user_id: Some(a),
        data: None,
        created_at: now - DAY,
        expires_at,
    };
    store.tokens().create(token("t-exp", now - 1)).await.unwrap();
    store.tokens().create(token("t-live", now + 1000)).await.unwrap();
    let ev = |kind: &str, user, ip: &str, at| NewSecurityEvent {
        kind: kind.into(),
        user_id: user,
        ip: Some(ip.into()),
        at: Some(at),
        detail: None,
    };
    store
        .security()
        .insert_batch(vec![
            ev("old", None, "2.2.2.1", now - 91 * DAY),
            ev("mid", Some(a), "2.2.2.2", now - 31 * DAY),
            ev("new", Some(a), "2.2.2.3", now - DAY),
        ])
        .await
        .unwrap();
    store
        .anomalies()
        .insert_batch(vec![
            anomaly(Some(a), "desync", Severity::Info, now - 91 * DAY),
            anomaly(Some(a), "bad_seq", Severity::Suspicious, now - 91 * DAY),
            anomaly(Some(a), "illegal_move", Severity::Certain, now - 91 * DAY),
            anomaly(Some(a), "desync", Severity::Info, now - DAY),
        ])
        .await
        .unwrap();
    store.conduct().record(a, ConductKind::Abandon, now - 31 * DAY).await.unwrap();
    store.conduct().record(a, ConductKind::Abandon, now - DAY).await.unwrap();
}

const POLICY: RetentionPolicy = RetentionPolicy { security_days: 90, ip_days: 30 };

#[tokio::test]
async fn retention_expired_sessions_and_tokens_old_events_and_anomalies_ip_erasure() {
    let (store, [a, ..]) = setup().await;
    let now = T + 200 * DAY;
    retention_fixture(&store, a, now).await;
    let got = store.retention().run(now, POLICY).await.unwrap();
    // IPs erased: the 40-day-old live session and the 31-day-old event, and those of the older rows
    // (the expired session, the 91-day-old event), erased before the rows are deleted.
    let expected = PurgeCounts {
        sessions: 3,
        tokens: 1,
        security_events: 1,
        anomalies: 2,
        conduct_events: 1,
        analysis_jobs: 0,
        ip_erased: 4,
    };
    assert_eq!(got, expected);
    let s = store.sessions();
    for gone in ["s-exp", "s-idle", "s-rev"] {
        assert_eq!(s.by_token_hash(gone.into()).await.unwrap(), None, "{gone}");
    }
    assert!(
        s.by_token_hash("s-rev2".into()).await.unwrap().is_some(),
        "recently revoked sessions are kept a day"
    );
    let ips: Vec<Option<String>> = s.list_for_user(a).await.unwrap().into_iter().map(|x| x.ip).collect();
    assert!(
        ips.contains(&None)
            && ips.contains(&Some("1.1.1.4".into()))
            && !ips.contains(&Some("1.1.1.3".into()))
    );
    assert_eq!(store.tokens().get("reset".into(), "t-exp".into()).await.unwrap(), None);
    assert!(store.tokens().get("reset".into(), "t-live".into()).await.unwrap().is_some());
    let sec: Vec<(String, Option<String>)> =
        store.security().for_user(a, 10).await.unwrap().into_iter().map(|e| (e.kind, e.ip)).collect();
    assert_eq!(sec, vec![("new".into(), Some("2.2.2.3".into())), ("mid".into(), None)]);
    let mut kinds: Vec<String> =
        store.anomalies().for_user(a, 10).await.unwrap().into_iter().map(|x| x.kind).collect();
    kinds.sort();
    assert_eq!(kinds, ["desync", "illegal_move"]);
    assert_eq!(store.conduct().count_since(a, 0).await.unwrap(), counts(1, 0, 0));
    // A second run has nothing left to do.
    assert_eq!(store.retention().run(now, POLICY).await.unwrap(), PurgeCounts::default());
    store.close().await;
}

#[tokio::test]
async fn retention_deletes_in_chunks_larger_than_one_statement() {
    let (store, [a, ..]) = setup().await;
    let events: Vec<NewSecurityEvent> = (0..2500)
        .map(|_| NewSecurityEvent {
            kind: "login_failed".into(),
            user_id: Some(a),
            ip: Some("192.0.2.9".into()),
            at: Some(T),
            detail: None,
        })
        .collect();
    store.security().insert_batch(events).await.unwrap();
    let res = store.retention().purge_security(T + 100 * DAY, POLICY).await.unwrap();
    assert_eq!(res.deleted, 2500);
    assert_eq!(res.ip_erased, 2500);
    assert!(store.security().for_user(a, 10).await.unwrap().is_empty());
    store.close().await;
}

#[tokio::test]
async fn one_job_is_atomic_across_tables_and_savepoints_nest() {
    let (store, [a, b, ..]) = setup().await;
    let e = store
        .write(move |db| {
            db.sanctions().create(&NewSanction {
                ends_at: Some(T + DAY),
                ..sanction(a, SanctionKind::Ban, "x", Source::Auto)
            })?;
            db.integrity().set(
                a,
                &IntegrityUpdate { level: Some(IntegrityLevel::Confirmed), ..IntegrityUpdate::default() },
            )?;
            Err::<(), _>(StoreError::invalid("abort"))
        })
        .await
        .unwrap_err();
    assert_eq!(e.message(), "abort");
    assert!(store.sanctions().list(a).await.unwrap().is_empty());
    assert_eq!(store.integrity().get(a).await.unwrap().level, IntegrityLevel::None);
    store
        .write(move |db| {
            db.conduct().record(b, ConductKind::Abort, T)?;
            let inner = db.transaction(|db| {
                db.conduct().record(b, ConductKind::NoShow, T)?;
                let innermost = db.transaction(|db| {
                    db.conduct().record(b, ConductKind::NoShow, T)?;
                    Err::<(), _>(StoreError::invalid("innermost"))
                });
                assert!(innermost.is_err());
                Err::<(), _>(StoreError::invalid("inner"))
            });
            assert!(inner.is_err());
            db.transaction(|db| db.conduct().record(b, ConductKind::Abandon, T))?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    assert_eq!(store.conduct().count_since(b, 0).await.unwrap(), counts(1, 1, 0));
    store.close().await;
}
