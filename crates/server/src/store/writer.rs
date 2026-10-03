//! The writer thread: the only writable connection of the process, running jobs one at a time in
//! the order they were submitted (RUST-PORT 6.1, 6.2).
//!
//! A job is queued when it is submitted, not when its future is first polled: two jobs submitted
//! one after the other by the same task run in that order, whatever happens to their futures.
//! The anti-cheat relies on it (anomalies written before the batch that queues the analysis).

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use rusqlite::Connection;
use tokio::sync::oneshot;

use super::conn::{self, DbPath, Role, Tuning};
use super::db::{Db, SharedCtx};
use super::error::{Result, StoreError};
use crate::{log_error, log_warn};

/// How long [`Writer::close`] waits for the jobs already queued: longer than `busy_timeout` (5 s),
/// so that a last job that waits once for another process's write lock is still answered.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_millis(7000);

type Task = Box<dyn FnOnce(&mut WriterConn) + Send>;
type Cancel = Box<dyn FnOnce() + Send>;

/// A queued job: what it runs, and how to answer it when the writer gives up on it.
struct Job {
    run: Task,
    cancel: Cancel,
}

enum Msg {
    Job(Job),
    Close(oneshot::Sender<()>),
}

#[derive(Default)]
struct State {
    jobs: VecDeque<Msg>,
    /// Set by close(): later submissions fail with `closed`.
    closing: bool,
    /// Set when the close timed out: the thread exits after its current job.
    abandoned: bool,
    /// Answers the job running now with "outcome unknown" (taken by the close timeout).
    current: Option<Cancel>,
}

struct Queue {
    state: Mutex<State>,
    ready: Condvar,
}

/// The writer connection, as a job sees it.
pub(crate) struct WriterConn {
    conn: Connection,
    ctx: SharedCtx,
}

impl WriterConn {
    /// The connection, outside any transaction.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Runs `f` in one `BEGIN IMMEDIATE` transaction, committed when it returns `Ok`, rolled back
    /// otherwise. Also returns the duration of the transaction in milliseconds.
    pub(crate) fn transact<R, E>(&mut self, f: impl FnOnce(&Db<'_>) -> Result<R, E>) -> (Result<R, E>, f64)
    where
        E: From<StoreError>,
    {
        let t0 = Instant::now();
        let out = (|| {
            self.conn.execute_batch("BEGIN IMMEDIATE").map_err(StoreError::from)?;
            let out = f(&Db::in_transaction(&self.conn, &self.ctx));
            match out {
                Ok(r) => match self.conn.execute_batch("COMMIT") {
                    Ok(()) => Ok(r),
                    Err(e) => {
                        self.rollback();
                        Err(StoreError::from(e).into())
                    }
                },
                Err(e) => {
                    self.rollback();
                    Err(e)
                }
            }
        })();
        (out, t0.elapsed().as_secs_f64() * 1000.0)
    }

    /// Runs `f` in a read transaction with `query_only` set (the reads of an in-memory store,
    /// which has no reader connection).
    pub(crate) fn read<R, E>(&mut self, f: impl FnOnce(&Db<'_>) -> Result<R, E>) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        self.conn.execute_batch("PRAGMA query_only = ON; BEGIN").map_err(StoreError::from)?;
        let out = f(&Db::in_transaction(&self.conn, &self.ctx));
        let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
        let done = self.conn.execute_batch(end);
        self.reset();
        match (out, done) {
            (Ok(r), Ok(())) => Ok(r),
            (Ok(_), Err(e)) => Err(StoreError::from(e).into()),
            (Err(e), _) => Err(e),
        }
    }

    fn rollback(&self) {
        if !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }

    /// Back to a clean state after a job: no open transaction, writes allowed.
    fn reset(&self) {
        self.rollback();
        let _ = self.conn.execute_batch("PRAGMA query_only = OFF");
    }
}

/// Handle on the writer thread.
pub(crate) struct Writer {
    queue: Arc<Queue>,
}

impl Writer {
    /// Opens the writer connection on a new thread and starts it. Blocks until the connection is
    /// open (call from a blocking context).
    pub(crate) fn start(path: DbPath, tuning: Tuning, ctx: SharedCtx) -> Result<Writer> {
        let queue = Arc::new(Queue { state: Mutex::new(State::default()), ready: Condvar::new() });
        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let q = queue.clone();
        std::thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || match conn::open(&path, Role::Writer, tuning) {
                Ok(conn) => {
                    let _ = opened_tx.send(Ok(()));
                    run(q, WriterConn { conn, ctx });
                }
                Err(e) => {
                    let _ = opened_tx.send(Err(e));
                }
            })
            .map_err(|e| {
                StoreError::new(super::ErrorKind::Sqlite, format!("cannot start the store writer: {e}"))
            })?;
        match opened_rx.recv() {
            Ok(Ok(())) => Ok(Writer { queue }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(StoreError::new(super::ErrorKind::Sqlite, "store writer thread exited at start")),
        }
    }

    /// Queues `f` (now, before the future is polled) and returns its outcome. Fails with
    /// [`ErrorKind::Closed`](super::ErrorKind::Closed) when the writer is closing, or when it closed
    /// before answering.
    pub(crate) fn submit<R, E, F>(
        &self,
        f: F,
    ) -> impl Future<Output = Result<R, E>> + Send + 'static + use<R, E, F>
    where
        F: FnOnce(&mut WriterConn) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, E>>();
        let slot = Arc::new(Mutex::new(Some(tx)));
        let answer = slot.clone();
        let job = Job {
            run: Box::new(move |w| {
                let out = f(w);
                if let Some(tx) = answer.lock().take() {
                    let _ = tx.send(out);
                }
            }),
            cancel: Box::new(move || {
                if let Some(tx) = slot.lock().take() {
                    let _ = tx.send(Err(StoreError::unanswered().into()));
                }
            }),
        };
        let accepted = {
            let mut st = self.queue.state.lock();
            if st.closing {
                false
            } else {
                st.jobs.push_back(Msg::Job(job));
                true
            }
        };
        if accepted {
            self.queue.ready.notify_one();
        }
        async move {
            if !accepted {
                return Err(StoreError::closed().into());
            }
            match rx.await {
                Ok(out) => out,
                // The job panicked (logged by the thread) and was rolled back.
                Err(_) => Err(StoreError::panicked().into()),
            }
        }
    }

    /// Runs the jobs already queued, then `PRAGMA optimize` and closes the connection. Jobs not
    /// answered within `timeout` (one waiting for another process's lock) are answered with
    /// "outcome unknown" and the thread is left to exit on its own. Later submissions fail.
    pub(crate) async fn close(&self, timeout: Duration, logger: &crate::log::Logger) {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.queue.state.lock();
            if st.closing {
                return;
            }
            st.closing = true;
            st.jobs.push_back(Msg::Close(tx));
        }
        self.queue.ready.notify_one();
        if tokio::time::timeout(timeout, rx).await.is_err() {
            log_warn!(logger, "store writer did not close in time; its unanswered jobs are failed", {
                "timeoutMs": timeout.as_millis() as u64,
            });
            self.abandon();
        }
    }

    /// Fails every job not answered yet, the running one included.
    fn abandon(&self) {
        let (jobs, current) = {
            let mut st = self.queue.state.lock();
            st.abandoned = true;
            (std::mem::take(&mut st.jobs), st.current.take())
        };
        self.queue.ready.notify_all();
        if let Some(cancel) = current {
            cancel();
        }
        for msg in jobs {
            if let Msg::Job(job) = msg {
                (job.cancel)();
            }
        }
    }
}

/// The thread's loop.
fn run(queue: Arc<Queue>, mut w: WriterConn) {
    let logger = w.ctx.logger.clone();
    loop {
        let msg = {
            let mut st = queue.state.lock();
            loop {
                if let Some(msg) = st.jobs.pop_front() {
                    break Some(msg);
                }
                if st.abandoned {
                    break None;
                }
                queue.ready.wait(&mut st);
            }
        };
        match msg {
            None => return,
            Some(Msg::Close(done)) => {
                if let Err(e) = w.conn.execute_batch("PRAGMA optimize") {
                    log_warn!(logger, "PRAGMA optimize failed at close", { "err": e.to_string() });
                }
                drop(w);
                let _ = done.send(());
                return;
            }
            Some(Msg::Job(job)) => {
                let run = {
                    let mut st = queue.state.lock();
                    st.current = Some(job.cancel);
                    job.run
                };
                if catch_unwind(AssertUnwindSafe(|| run(&mut w))).is_err() {
                    log_error!(logger, "store writer job panicked; its transaction was rolled back");
                    w.reset();
                }
                queue.state.lock().current = None;
            }
        }
    }
}
