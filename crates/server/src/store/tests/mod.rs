//! Tests of the store, ported from the Node store suites (store.*.test.js) to the new schema.

mod account_api;
mod accounts;
mod analysis_queue;
mod games;
mod migrations;
mod moderation;
mod retention;
pub(crate) mod support;
mod writer;

mod smoke {
    use super::support::*;

    #[tokio::test]
    async fn opens_migrates_and_round_trips_a_user() {
        let store = memory_store().await;
        let id = store.users().create(new_user("alice", Some("Alice@Example.org"))).await.unwrap();
        let u = store.users().by_email("alice@example.org".into()).await.unwrap().unwrap();
        assert_eq!(u.id, id);
        assert_eq!(u.email.as_deref(), Some("Alice@Example.org"));
        store.close().await;
    }

    #[tokio::test]
    async fn db_cache_mb_is_shared_evenly_by_the_writer_and_the_readers() {
        let dir = TempDir::new("db-cache");
        let mut config = config();
        config.db_cache_mb = 100;
        let opts = crate::store::StoreOptions { readers: Some(4), ..options(None) };
        let store = file_store_with(&dir, &config, opts).await;
        // In KiB when negative: 100 MiB for 5 connections.
        let writer = store.write(|db| db.count("PRAGMA cache_size", [])).await.unwrap();
        let reader = store.read(|db| db.count("PRAGMA cache_size", [])).await.unwrap();
        assert_eq!((writer, reader), (-20 * 1024, -20 * 1024));
        store.close().await;
    }
}
