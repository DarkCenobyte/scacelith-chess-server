//! The commit gate's view of write failures: the write-batch override, `failed_writes`,
//! `has_unwritten`, the errors handed to every flush of a failed batch, the metric and the log.

use std::io::ErrorKind;
use std::sync::Arc;

use parking_lot::Mutex;

use super::support::*;
use crate::journal::format::parse_segment;
use crate::journal::metrics::ERRORS;
use crate::journal::{JournalError, JournalOptions, RecordKind};
use crate::log::Logger;

use RecordKind::{Created, Ended, Move};

#[tokio::test]
async fn a_failed_batch_fails_every_flush_waiting_for_it_and_is_counted_and_logged() {
    let dir = TempDir::new("failures");
    let component = "journal-failed-write-test";
    let options = JournalOptions { logger: Logger::root().child(component), ..opts(dir.path()) };
    let j = open(options.clone()).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.flush().await.unwrap();
    let errors = ERRORS.get();
    let logs = super::LogCapture::start();
    // The failing write waits until both flushes wait for it.
    let (go, gate) = std::sync::mpsc::channel::<()>();
    let mut fail = fail_once(ErrorKind::Other, "EIO: i/o error, write");
    j.set_write_batch_override(Some(Box::new(move |batch: &[u8]| {
        let _ = gate.recv_timeout(std::time::Duration::from_secs(5));
        fail(batch)
    })));
    j.append(Move, 1, b"m1", 2.0).unwrap();
    let (a, b) = (j.flush(), j.flush());
    go.send(()).unwrap();
    drop(go); // later batches pass the gate at once
    let (a, b) = (a.await.unwrap_err(), b.await.unwrap_err());
    assert_eq!(a, b, "every flush of the batch gets its error");
    assert!(
        matches!(&a, JournalError::Io { kind: ErrorKind::Other, message } if message.contains("EIO")),
        "{a}"
    );
    assert_eq!(j.failed_writes(), 1);
    assert!(!j.has_unwritten(), "the lost records are not written later (a snapshot heals the game)");
    assert!(ERRORS.get() > errors);
    let lines = logs.records(component);
    drop(logs);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        (lines[0]["level"].as_str(), lines[0]["msg"].as_str()),
        (Some("error"), Some("journal write failed"))
    );
    assert_eq!(lines[0]["bytes"], 2 + crate::journal::format::RECORD_OVERHEAD as u64);
    assert!(lines[0]["err"].as_str().unwrap().contains("EIO"));
    // The next batch is written normally (to a new segment), its flushes succeed.
    j.append(Move, 1, b"m2", 3.0).unwrap();
    j.flush().await.unwrap();
    assert_eq!(j.failed_writes(), 1);
    assert_eq!(segments(dir.path(), 0), names(&[1, 2]));
    j.close().await.unwrap();
    let c = open(options).await;
    assert_eq!(texts(&c.recover()[&1]), vec![(Created, "c1".into()), (Move, "m2".into())]);
    c.close().await.unwrap();
}

#[tokio::test]
async fn the_override_sees_each_batch_and_an_ok_lets_it_be_written() {
    let dir = TempDir::new("failures");
    let j = open(opts(dir.path())).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    j.set_write_batch_override(Some(Box::new(move |batch: &[u8]| {
        let mut games = Vec::new();
        parse_segment(batch, |r| games.push(r.game));
        s.lock().push(games);
        Ok(())
    })));
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.append(Created, 2, b"c2", 1.0).unwrap();
    j.flush().await.unwrap();
    j.append(Move, 2, b"m", 2.0).unwrap();
    j.flush().await.unwrap();
    assert_eq!(*seen.lock(), vec![vec![1, 2], vec![2]]);
    assert_eq!(j.failed_writes(), 0);
    j.close().await.unwrap();
    let c = open(opts(dir.path())).await;
    assert_eq!(keys(&c), vec![1, 2]);
    c.close().await.unwrap();
}

#[tokio::test]
async fn failures_in_a_row_count_one_by_one_and_the_records_appended_meanwhile_wait() {
    let dir = TempDir::new("failures");
    let j = open(opts(dir.path())).await;
    j.set_write_batch_override(Some(Box::new(|_| Err(io_error(ErrorKind::StorageFull, "ENOSPC")))));
    for i in 0..3u32 {
        j.append(Move, 1, &i.to_le_bytes(), f64::from(i)).unwrap();
        assert!(j.has_unwritten());
        assert!(j.flush().await.is_err());
        assert_eq!(j.failed_writes(), u64::from(i) + 1);
    }
    // The commit gate: a game committed while the disk is full keeps its committed record
    // pending until a write succeeds.
    j.append(Ended, 1, b"", 9.0).unwrap();
    j.committed(1).unwrap();
    assert!(j.flush().await.is_err());
    assert!(j.has_unwritten(), "its committed record is appended again");
    assert_eq!(j.stats().pending_bytes, crate::journal::format::RECORD_OVERHEAD);
    j.set_write_batch_override(None);
    j.flush().await.unwrap();
    assert!(!j.has_unwritten());
    assert_eq!(j.failed_writes(), 4);
    j.close().await.unwrap();
    let c = open(opts(dir.path())).await;
    assert!(c.recover().is_empty(), "nothing of game 1 but its committed record reached the disk");
    c.close().await.unwrap();
}

#[tokio::test]
async fn a_failure_during_the_close_is_returned_and_the_committed_records_are_not_appended_again() {
    let dir = TempDir::new("failures");
    let j = open(opts(dir.path())).await;
    j.append(Created, 1, b"c1", 1.0).unwrap();
    j.flush().await.unwrap();
    j.committed(1).unwrap();
    j.set_write_batch_override(Some(fail_once(ErrorKind::Other, "EIO")));
    let err = j.close().await.unwrap_err();
    assert!(err.to_string().contains("EIO"));
    assert!(!j.has_unwritten(), "nothing re-appended while closing");
    assert_eq!(j.append(Move, 1, b"m", 2.0), Err(JournalError::Closed));
    j.close().await.unwrap();
    // The game comes back at the next start (its commit record was lost); the database commit
    // of a game it already has is ignored there.
    let c = open(opts(dir.path())).await;
    assert_eq!(keys(&c), vec![1]);
    c.close().await.unwrap();
}
