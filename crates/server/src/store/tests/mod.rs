//! Tests of the store, ported from the Node store suites (store.*.test.js) to the new schema.

mod account_api;
mod accounts;
mod analysis_queue;
mod games;
mod migrations;
mod moderation;
mod retention;
mod support;
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
}
