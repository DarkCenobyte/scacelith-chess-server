//! Port of store.migrations.test.js (the data migrations of the Node schema have no equivalent:
//! the Rust schema starts afresh at version 1).

use rusqlite::{Connection, OpenFlags};

use super::support::*;
use crate::store::migrate::{Migration, embedded};
use crate::store::{ErrorKind, Store, StoreOptions};

const TABLES: &[&str] = &[
    "meta",
    "users",
    "mfa_recovery_codes",
    "sessions",
    "tokens",
    "sso_identities",
    "ratings",
    "games",
    "conduct_events",
    "conduct_state",
    "sanctions",
    "anomalies",
    "security_events",
    "analysis_jobs",
    "player_integrity",
    "population_stats",
    "reports",
    "rating_refunds",
    "pending_signups",
    "schema_migrations",
];

fn raw(path: &str) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

#[tokio::test]
async fn migrate_creates_every_table_and_records_the_migration() {
    let dir = TempDir::new("mig");
    let file = dir.file("fresh.db");
    let store = Store::open(&config(), options(Some(file.clone()))).await.unwrap();
    let res = store.migrate().await.unwrap();
    let all: Vec<i64> = embedded().iter().map(|m| m.version).collect();
    assert_eq!(res.applied, all);
    assert_eq!(res.version, *all.last().unwrap());
    let id = store.server_id().await.unwrap().unwrap();
    assert_eq!(id.len(), 36);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));

    let raw = raw(&file);
    let names: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for t in TABLES {
        assert!(names.iter().any(|n| n == t), "table {t}");
    }
    // Every table of the schema is STRICT.
    let strict: Vec<(String, i64)> = raw
        .prepare(
            "SELECT name, strict FROM pragma_table_list WHERE schema = 'main' AND name NOT LIKE 'sqlite_%'",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (name, s) in &strict {
        assert_eq!(*s, 1, "table {name} is STRICT");
    }
    let (name, checksum): (String, String) = raw
        .query_row("SELECT name, checksum FROM schema_migrations WHERE version = 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(name, "001_initial");
    assert_eq!(checksum, embedded()[0].checksum());
    let mode: String = raw.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    drop(raw);
    store.close().await;
}

#[tokio::test]
async fn an_in_memory_store_works_and_close_is_idempotent() {
    let store = memory_store().await;
    assert_eq!(store.path(), ":memory:");
    store.meta().set("k".into(), "v".into()).await.unwrap();
    assert_eq!(store.meta().get("k".into()).await.unwrap().as_deref(), Some("v"));
    store.close().await;
    store.close().await;
    let e = store.meta().get("k".into()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Closed);
    let e = store.meta().set("k".into(), "w".into()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Closed);
}

#[tokio::test]
async fn migrate_is_idempotent_keeps_the_server_id_and_the_file_survives_reopening() {
    let dir = TempDir::new("mig");
    let file = dir.path().join("db").join("test.db").display().to_string();
    let store = Store::open(&config(), options(Some(file.clone()))).await.unwrap();
    store.migrate().await.unwrap();
    let id = store.server_id().await.unwrap();
    assert_eq!(store.migrate().await.unwrap().applied, Vec::<i64>::new());
    store.meta().set("k".into(), "42".into()).await.unwrap();
    store.close().await;

    let store = Store::open(&config(), options(Some(file))).await.unwrap();
    assert_eq!(store.migrate().await.unwrap().applied, Vec::<i64>::new());
    assert_eq!(store.server_id().await.unwrap(), id);
    assert_eq!(store.meta().get("k".into()).await.unwrap().as_deref(), Some("42"));
    assert_eq!(store.meta().get("missing".into()).await.unwrap(), None);
    store.close().await;
}

#[tokio::test]
async fn a_changed_applied_migration_is_refused_and_crlf_is_not_a_change() {
    let dir = TempDir::new("mig");
    let file = dir.file("x.db");
    let sql = embedded()[0].sql.clone();
    let cfg = config();
    let open = || Store::open(&cfg, options(Some(file.clone())));

    let store = open().await.unwrap();
    store.migrate_with(vec![Migration::new(1, "001_initial", sql.clone())], 1).await.unwrap();
    store.close().await;

    let store = open().await.unwrap();
    let crlf = Migration::new(1, "001_initial", sql.replace('\n', "\r\n"));
    assert_eq!(store.migrate_with(vec![crlf], 2).await.unwrap().applied, Vec::<i64>::new());
    store.close().await;

    let store = open().await.unwrap();
    let edited = Migration::new(1, "001_initial", format!("{sql}\n-- edited\n"));
    let e = store.migrate_with(vec![edited], 3).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::MigrationChecksum);
    store.close().await;
}

#[tokio::test]
async fn new_migrations_are_applied_in_order_and_broken_or_unknown_ones_refused() {
    let dir = TempDir::new("mig");
    let file = dir.file("x.db");
    let first = embedded()[0].clone();
    let store = Store::open(&config(), options(Some(file.clone()))).await.unwrap();
    store.migrate_with(vec![first.clone()], 10).await.unwrap();

    let second = Migration::new(2, "002_second", "CREATE TABLE second (a INTEGER) STRICT;");
    let third = Migration::new(3, "003_third", "CREATE TABLE third (a INTEGER) STRICT;");
    let res = store.migrate_with(vec![first.clone(), third.clone(), second.clone()], 11).await.unwrap();
    assert_eq!((res.applied, res.version), (vec![2, 3], 3));

    // A failing migration is rolled back entirely and reported.
    let broken = Migration::new(4, "004_broken", "CREATE TABLE fourth (a INTEGER); CREATE TABLE broken (;");
    let all = vec![first.clone(), second.clone(), third.clone(), broken];
    let e = store.migrate_with(all, 12).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::MigrationFailed);
    let fixed = Migration::new(4, "004_fixed", "CREATE TABLE fourth (a INTEGER);");
    let all = vec![first.clone(), second.clone(), third.clone(), fixed];
    assert_eq!(store.migrate_with(all, 13).await.unwrap().applied, vec![4]);

    // Two migrations with one version are refused.
    let dup = vec![first.clone(), second.clone(), Migration::new(2, "002_other", "SELECT 1;")];
    assert_eq!(store.migrate_with(dup, 14).await.unwrap_err().kind(), ErrorKind::MigrationFailed);
    store.close().await;

    // An older server (fewer migrations) refuses a newer database.
    let store = Store::open(&config(), options(Some(file.clone()))).await.unwrap();
    let e = store.migrate_with(vec![first.clone(), second, third], 15).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::MigrationMissing);
    store.close().await;

    // A gap is filled: version 2 is applied below an applied version 3.
    let other = TempDir::new("mig");
    let store = Store::open(&config(), options(Some(other.file("y.db")))).await.unwrap();
    let three = Migration::new(3, "003_third", "CREATE TABLE third (a INTEGER);");
    store.migrate_with(vec![first.clone(), three.clone()], 20).await.unwrap();
    let two = Migration::new(2, "002_second", "CREATE TABLE second (a INTEGER);");
    assert_eq!(store.migrate_with(vec![first, two, three], 21).await.unwrap().applied, vec![2]);
    store.close().await;
}

#[tokio::test]
async fn a_read_only_store_reads_but_never_writes_nor_migrates() {
    let dir = TempDir::new("mig");
    let file = dir.file("x.db");
    let store = Store::open(&config(), options(Some(file.clone()))).await.unwrap();
    store.migrate().await.unwrap();
    store.users().create(new_user("Reader", Some("r@example.org"))).await.unwrap();

    let ro = Store::open(
        &config(),
        StoreOptions { path: Some(file.clone()), readonly: true, ..StoreOptions::default() },
    )
    .await
    .unwrap();
    assert!(ro.readonly());
    assert_eq!(ro.users().by_username("reader".into()).await.unwrap().unwrap().username, "Reader");
    let e = ro.users().create(new_user("Nope", Some("n@example.org"))).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ReadOnly);
    assert_eq!(ro.migrate().await.unwrap_err().kind(), ErrorKind::ReadOnly);
    // A write slipped into a read job fails on the query_only connection.
    let e = store.read(|db| db.meta().set("x", "y")).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ReadOnly);
    ro.close().await;
    store.close().await;
    // The last connection closed: checkpointed, the WAL file removed.
    assert!(!std::path::Path::new(&format!("{file}-wal")).exists());
}

#[tokio::test]
async fn pragmas_of_the_writer_and_the_readers() {
    let dir = TempDir::new("mig");
    let store = file_store(&dir).await;
    let writer = store
        .write(|db| {
            let c = db.connection();
            let q = |p: &str| c.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0)).unwrap();
            Ok::<_, crate::store::StoreError>((
                q("synchronous"),
                q("foreign_keys"),
                q("secure_delete"),
                q("busy_timeout"),
                q("journal_size_limit"),
            ))
        })
        .await
        .unwrap();
    assert_eq!(writer, (2, 1, 1, 5000, 64 * 1024 * 1024));
    let reader = store
        .read(|db| {
            let c = db.connection();
            let q = |p: &str| c.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0)).unwrap();
            Ok::<_, crate::store::StoreError>((q("query_only"), q("foreign_keys"), q("busy_timeout")))
        })
        .await
        .unwrap();
    assert_eq!(reader, (1, 1, 5000));
    store.close().await;
}
