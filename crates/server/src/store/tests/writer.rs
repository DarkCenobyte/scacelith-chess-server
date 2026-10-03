//! Port of store.writer.test.js (the close of the writer thread while another process holds the
//! write lock) and the concurrency guarantees of the store: submission order, read-your-writes,
//! snapshot reads, parallel readers, panics, the closed store.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::support::*;
use crate::store::{ErrorKind, GameRecord, Store, StoreError};

/// Another process's connection holding the write lock (`BEGIN IMMEDIATE`) until released.
struct LockHolder {
    release: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LockHolder {
    fn take(file: &str) -> LockHolder {
        let (release, released) = mpsc::channel::<()>();
        let (locked_tx, locked) = mpsc::channel();
        let file = file.to_string();
        let thread = std::thread::spawn(move || {
            let raw = rusqlite::Connection::open(file).unwrap();
            raw.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked_tx.send(()).unwrap();
            let _ = released.recv();
            raw.execute_batch("ROLLBACK").unwrap();
        });
        locked.recv().unwrap();
        LockHolder { release, thread: Some(thread) }
    }

    fn release(&mut self) {
        let _ = self.release.send(());
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        self.release();
    }
}

fn invalid(id: u64) -> GameRecord {
    GameRecord { status: 99, ..record(id, 1, 2) }
}

fn outcome<T>(r: Result<T, StoreError>) -> String {
    match r {
        Ok(_) => "resolved".into(),
        Err(e) => e.message().to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_fails_the_jobs_not_answered_when_it_times_out_instead_of_leaving_them_pending() {
    let dir = TempDir::new("writer");
    let file = dir.file("scacelith.db");
    let store = file_store(&dir).await;
    store.meta().set("up".into(), "1".into()).await.unwrap();
    // Another connection holds the write lock: each job waits busy_timeout (5 s) for it, so the
    // second one is still waiting when the close times out (7 s).
    let _lock = LockHolder::take(&file);
    let first = tokio::spawn(store.finish_batch(vec![invalid(9001)]));
    let second = tokio::spawn(store.finish_batch(vec![invalid(9002)]));
    let t0 = Instant::now();
    store.close().await;
    assert!(t0.elapsed() >= Duration::from_secs(6), "the close waited for the queued jobs");
    let second = second.await.unwrap().unwrap_err();
    assert_eq!(second.kind(), ErrorKind::Closed);
    assert_eq!(second.message(), "store writer closed before answering (outcome unknown)");
    let first = outcome(first.await.unwrap());
    assert!(first.contains("closed before answering") || first.contains("locked"), "{first}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_answered_after_one_busy_timeout_wait_settles_with_its_own_outcome() {
    let dir = TempDir::new("writer");
    let file = dir.file("scacelith.db");
    let store = file_store(&dir).await;
    store.meta().set("up".into(), "1".into()).await.unwrap();
    let busy0 = crate::store::metrics::BUSY.get();
    let mut lock = LockHolder::take(&file);
    // The first job waits busy_timeout (5 s) for the lock and fails; the lock is released 300 ms
    // later, so the second one is answered about 5.3 s after the close started.
    let first = store.finish_batch(vec![invalid(9001)]);
    let second = store.finish_batch(vec![invalid(9002)]);
    let releaser = tokio::spawn(async move {
        let r = first.await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || lock.release()).await.unwrap();
        r
    });
    let t0 = Instant::now();
    store.close().await;
    assert!(t0.elapsed() < Duration::from_secs(7), "answered before the close timeout");
    let first = releaser.await.unwrap().unwrap_err();
    assert_eq!(first.kind(), ErrorKind::Busy);
    assert!(first.message().contains("locked"), "{}", first.message());
    assert!(crate::store::metrics::BUSY.get() > busy0, "counted in scacelith_store_busy_total");
    let second = second.await.unwrap_err();
    assert_eq!(second.kind(), ErrorKind::InvalidRecord);
    assert_eq!(second.game_id(), Some(9002));
    assert!(second.message().starts_with("game 9002: invalid"), "{}", second.message());
}

#[tokio::test]
async fn jobs_run_in_submission_order_whatever_the_order_their_futures_are_awaited_in() {
    let store = memory_store().await;
    let append = |i: usize| {
        store.write(move |db| {
            let cur = db.meta().get("seq")?.unwrap_or_default();
            let next = if cur.is_empty() { i.to_string() } else { format!("{cur},{i}") };
            db.meta().set("seq", &next)
        })
    };
    let mut futures: Vec<_> = (0..50).map(append).collect();
    // A dropped future does not cancel its job: it was queued at the call.
    drop(futures.remove(10));
    futures.reverse();
    for f in futures {
        f.await.unwrap();
    }
    let expected: Vec<String> = (0..50).map(|i| i.to_string()).collect();
    assert_eq!(store.meta().get("seq".into()).await.unwrap(), Some(expected.join(",")));
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_started_after_a_write_answered_sees_it_and_writes_from_many_tasks_all_land() {
    let dir = TempDir::new("concurrency");
    let store = file_store(&dir).await;
    for i in 0..100 {
        store.meta().set("k".into(), i.to_string()).await.unwrap();
        assert_eq!(store.meta().get("k".into()).await.unwrap(), Some(i.to_string()));
    }
    let mut tasks = Vec::new();
    for t in 0..8 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let mut ids = Vec::new();
            for i in 0..25 {
                let name = format!("u{t}x{i}");
                ids.push(
                    store.users().create(new_user(&name, Some(&format!("{name}@e.org")))).await.unwrap(),
                );
            }
            ids
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.extend(t.await.unwrap());
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 200);
    let n = store
        .read(|db| {
            db.connection()
                .query_row("SELECT count(*) FROM users", [], |r| r.get::<_, i64>(0))
                .map_err(StoreError::from)
        })
        .await
        .unwrap();
    assert_eq!(n, 200);
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_job_sees_one_snapshot_and_the_readers_run_in_parallel() {
    let dir = TempDir::new("concurrency");
    let store = file_store(&dir).await;
    store.meta().set("k".into(), "before".into()).await.unwrap();
    // The read job reads, waits while a write commits, reads again: the same snapshot.
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let read = store.read(move |db| {
        let a = db.meta().get("k")?;
        let _ = started_tx.send(());
        let _ = go_rx.recv_timeout(Duration::from_secs(5));
        let b = db.meta().get("k")?;
        Ok::<_, StoreError>((a, b))
    });
    let read = tokio::spawn(read);
    started_rx.await.unwrap();
    store.meta().set("k".into(), "after".into()).await.unwrap();
    go_tx.send(()).unwrap();
    let (a, b) = read.await.unwrap().unwrap();
    assert_eq!((a.as_deref(), b.as_deref()), (Some("before"), Some("before")));
    assert_eq!(store.meta().get("k".into()).await.unwrap().as_deref(), Some("after"));

    // Four reads of 150 ms each on the four reader connections take about 150 ms, not 600.
    let t0 = Instant::now();
    let reads: Vec<_> = (0..4)
        .map(|_| {
            tokio::spawn(store.read(|_| {
                std::thread::sleep(Duration::from_millis(150));
                Ok::<_, StoreError>(())
            }))
        })
        .collect();
    for r in reads {
        r.await.unwrap().unwrap();
    }
    assert!(t0.elapsed() < Duration::from_millis(450), "parallel reads took {:?}", t0.elapsed());
    store.close().await;
}

#[tokio::test]
async fn a_panicking_job_fails_alone_and_the_store_goes_on() {
    let dir = TempDir::new("panic");
    let store = file_store(&dir).await;
    let e = store
        .write(|db| -> Result<(), StoreError> {
            db.meta().set("half", "written")?;
            panic!("job exploded")
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(store.meta().get("half".into()).await.unwrap(), None, "rolled back");
    store.meta().set("after".into(), "1".into()).await.unwrap();
    let e = store.read(|_| -> Result<(), StoreError> { panic!("read exploded") }).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    for _ in 0..8 {
        assert_eq!(store.meta().get("after".into()).await.unwrap().as_deref(), Some("1"));
    }
    store.close().await;
}

#[tokio::test]
async fn a_closed_store_refuses_every_call_with_closed() {
    let dir = TempDir::new("closed");
    let store = file_store(&dir).await;
    let queued = store.meta().set("k".into(), "v".into());
    store.close().await;
    queued.await.unwrap(); // queued before the close: answered
    assert!(store.is_closed());
    let closed = |e: StoreError| assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
    closed(store.meta().get("k".into()).await.unwrap_err());
    closed(store.meta().set("k".into(), "w".into()).await.unwrap_err());
    closed(store.finish_batch(vec![record(1, 1, 2)]).await.unwrap_err());
    closed(store.migrate().await.unwrap_err());
    // A purge stops at a closed store without an error: nothing deleted.
    let policy = crate::store::RetentionPolicy { security_days: 1, ip_days: 1 };
    let counts = store.retention().purge_security(0, policy).await.unwrap();
    assert_eq!(counts, crate::store::SecurityPurge { deleted: 0, ip_erased: 0 });
    // Clones share the state, and a second close is a no-op.
    let clone: Store = store.clone();
    closed(clone.users().by_id(1).await.unwrap_err());
    clone.close().await;
}

#[tokio::test]
async fn db_cache_mb_is_shared_by_the_writer_and_the_readers() {
    fn cache_size(db: &crate::store::Db<'_>) -> Result<i64, StoreError> {
        db.connection().query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0)).map_err(StoreError::from)
    }
    let mut c = config();
    c.db_cache_mb = 100;
    // The writer and the 4 readers of a file store: 20 MiB each (cache_size in KiB, negative).
    let dir = TempDir::new("cache");
    let store = file_store_with(&dir, &c, options(None)).await;
    assert_eq!(store.read(cache_size).await.unwrap(), -20 * 1024);
    assert_eq!(store.write(cache_size).await.unwrap(), -20 * 1024);
    store.close().await;
    // An in-memory store has its writer only.
    let memory = store_with(&c, options(None)).await;
    assert_eq!(memory.read(cache_size).await.unwrap(), -100 * 1024);
    memory.close().await;
}
