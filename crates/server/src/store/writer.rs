//! The writer thread: the only writable connection of the process, running jobs one at a time in
//! the order they were submitted (RUST-PORT 6.1, 6.2).
//!
//! A job is queued when it is submitted, not when its future is first polled: two jobs submitted
//! one after the other by the same task run in that order, whatever happens to their futures.
//! The anti-cheat relies on it (anomalies written before the batch that queues the analysis).
//!
//! # Bounds
//!
//! The queue is one FIFO (the order above holds across every kind of job), with a budget per
//! [`Lane`]:
//!
//! * from [`WRITE_BACKLOG_BUSY`] jobs waiting, the writer counts as backlogged
//!   (`Store::writes_backlogged`): the HTTP layer refuses the requests that may write with 503
//!   `server_busy` before they run, no new game is created, and the session renewals are deferred
//!   (earlier still, from `SESSION_TOUCH_DEFER_BACKLOG`). This is the admission signal: it keeps
//!   new work out, it caps nothing by itself;
//! * an [`Lane::Ordinary`] job (every job but the critical ones: the API's writes, the session
//!   renewals, the anti-cheat's and the lobby's writes, the retention purge) is refused while
//!   [`WRITE_QUEUE_MAX`] jobs wait: its future fails at once with a `busy` error (the job never
//!   runs, nothing changed), as for a lock not obtained, which the callers already handle (503
//!   `server_busy`, a logged loss, a retry at the next interval).
//!   `scacelith_db_write_jobs_refused_total` counts these refusals, and each episode is logged
//!   once (a warning when the queue first refuses, the count when it is back under
//!   [`WRITE_BACKLOG_BUSY`]);
//! * a [`Lane::Critical`] job (a game commit, the anomaly batch the next commit's analysis policy
//!   reads, a migration) may also use the [`WRITE_CRITICAL_RESERVE`] jobs above
//!   [`WRITE_QUEUE_MAX`] that ordinary jobs never take, and is never refused for the queue's
//!   length: losing it would lose a finished game. Its producers keep it far below the reserve
//!   (one commit in flight per game host, at most 64 hosts; one anomaly batch queued at a time;
//!   the migrations at start). A critical job beyond the reserve (a bug of those producers) is
//!   still queued, and counted in `scacelith_db_write_reserve_exceeded_total` and logged as an
//!   error once per episode.
//!
//! The queue therefore holds at most [`WRITE_QUEUE_MAX`] + [`WRITE_CRITICAL_RESERVE`] jobs while
//! the producers keep their bounds. Its length is the gauge `scacelith_db_write_queue`, the
//! critical jobs among them `scacelith_db_write_queue_critical`.

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
use super::metrics::{WRITE_OVER_RESERVE, WRITE_QUEUE, WRITE_QUEUE_CRITICAL, WRITE_REFUSED};
use crate::log::Logger;
use crate::{log_error, log_info, log_warn};

/// How long [`Writer::close`] waits for the jobs already queued: longer than `busy_timeout` (5 s),
/// so that a last job that waits once for another process's write lock is still answered.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_millis(7000);

/// Jobs waiting from which the writer counts as backlogged: about a second of writes for a slow
/// disk, and hundreds of times what it holds in ordinary operation (module documentation).
pub const WRITE_BACKLOG_BUSY: usize = 10_000;

/// Jobs waiting from which an ordinary job is refused (module documentation): twice
/// [`WRITE_BACKLOG_BUSY`], room for the work admitted before the writer counted as backlogged.
pub const WRITE_QUEUE_MAX: usize = 2 * WRITE_BACKLOG_BUSY;

/// Jobs above [`WRITE_QUEUE_MAX`] that only critical jobs may take (module documentation): 16
/// times what the critical producers can queue at once (64 game hosts with one commit each, one
/// anomaly batch).
pub const WRITE_CRITICAL_RESERVE: usize = 1024;

/// The budget a job is admitted under (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    /// Refused while [`WRITE_QUEUE_MAX`] jobs wait.
    Ordinary,
    /// Game commits, the anomaly batches, the migrations: may take the reserve, never refused.
    Critical,
}

type Task = Box<dyn FnOnce(&mut WriterConn) + Send>;
type Cancel = Box<dyn FnOnce() + Send>;

/// A queued job: what it runs, how to answer it when the writer gives up on it, and its lane.
struct Job {
    run: Task,
    cancel: Cancel,
    lane: Lane,
}

enum Msg {
    Job(Job),
    Close(oneshot::Sender<()>),
}

#[derive(Default)]
struct State {
    jobs: VecDeque<Msg>,
    /// Critical jobs among `jobs`.
    critical: usize,
    /// Ordinary jobs refused since the queue last had room (the episode logged once).
    refused: u64,
    /// Critical jobs queued beyond the reserve in the same episode.
    over_reserve: u64,
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
    logger: Logger,
}

/// What [`State::admit`] decided; `first` marks the first event of an episode (logged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Queued,
    /// A critical job queued beyond the reserve.
    OverReserve {
        waiting: usize,
        first: bool,
    },
    Refused {
        waiting: usize,
        first: bool,
    },
}

impl State {
    /// Queues `job` within its lane's budget (module documentation); a refused job is given back.
    fn admit(&mut self, job: Job) -> (Admission, Option<Job>) {
        let waiting = self.jobs.len();
        match job.lane {
            Lane::Ordinary if waiting >= WRITE_QUEUE_MAX => {
                self.refused += 1;
                (Admission::Refused { waiting, first: self.refused == 1 }, Some(job))
            }
            Lane::Ordinary => {
                self.jobs.push_back(Msg::Job(job));
                (Admission::Queued, None)
            }
            Lane::Critical => {
                self.critical += 1;
                self.jobs.push_back(Msg::Job(job));
                if waiting < WRITE_QUEUE_MAX + WRITE_CRITICAL_RESERVE {
                    return (Admission::Queued, None);
                }
                self.over_reserve += 1;
                (Admission::OverReserve { waiting, first: self.over_reserve == 1 }, None)
            }
        }
    }

    /// The next message, with what the writer must log about the episode that just ended.
    fn pop(&mut self) -> (Option<Msg>, Option<(u64, u64)>) {
        let msg = self.jobs.pop_front();
        if let Some(Msg::Job(job)) = &msg
            && job.lane == Lane::Critical
        {
            self.critical -= 1;
        }
        let ended = (self.jobs.len() < WRITE_BACKLOG_BUSY && (self.refused > 0 || self.over_reserve > 0))
            .then(|| (std::mem::take(&mut self.refused), std::mem::take(&mut self.over_reserve)));
        (msg, ended)
    }
}

impl Writer {
    /// Opens the writer connection on a new thread and starts it. Blocks until the connection is
    /// open (call from a blocking context).
    pub(crate) fn start(path: DbPath, tuning: Tuning, ctx: SharedCtx) -> Result<Writer> {
        let queue = Arc::new(Queue { state: Mutex::new(State::default()), ready: Condvar::new() });
        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let q = queue.clone();
        let logger = ctx.logger.clone();
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
            Ok(Ok(())) => Ok(Writer { queue, logger }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(StoreError::new(super::ErrorKind::Sqlite, "store writer thread exited at start")),
        }
    }

    /// Queues `f` (now, before the future is polled) in `lane` and returns its outcome. Fails with
    /// [`ErrorKind::Closed`](super::ErrorKind::Closed) when the writer is closing, or when it closed
    /// before answering, and with [`ErrorKind::Busy`](super::ErrorKind::Busy) for an ordinary job
    /// refused because [`WRITE_QUEUE_MAX`] jobs wait (it never runs).
    pub(crate) fn submit<R, E, F>(
        &self,
        lane: Lane,
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
            lane,
        };
        let admitted = {
            let mut st = self.queue.state.lock();
            if st.closing { Err(job) } else { Ok(st.admit(job)) }
        };
        let accepted = match admitted {
            // Dropped outside the lock (its closure owns the caller's data).
            Err(job) => {
                drop(job);
                Err(StoreError::closed())
            }
            Ok((Admission::Refused { waiting, first }, job)) => {
                drop(job);
                WRITE_REFUSED.inc();
                if first {
                    log_warn!(self.logger, "store writer queue full: ordinary write jobs are refused until it drains", {
                        "waiting": waiting, "max": WRITE_QUEUE_MAX,
                    });
                }
                Err(StoreError::queue_full())
            }
            Ok((admission, _)) => {
                WRITE_QUEUE.inc();
                if lane == Lane::Critical {
                    WRITE_QUEUE_CRITICAL.inc();
                }
                if let Admission::OverReserve { waiting, first } = admission {
                    WRITE_OVER_RESERVE.inc();
                    if first {
                        log_error!(self.logger, "store writer: a critical job went beyond the reserve; queued anyway", {
                            "waiting": waiting, "max": WRITE_QUEUE_MAX, "reserve": WRITE_CRITICAL_RESERVE,
                        });
                    }
                }
                self.queue.ready.notify_one();
                Ok(())
            }
        };
        async move {
            accepted.map_err(E::from)?;
            match rx.await {
                Ok(out) => out,
                // The job panicked (logged by the thread) and was rolled back.
                Err(_) => Err(StoreError::panicked().into()),
            }
        }
    }

    /// Jobs waiting (the one running aside).
    pub(crate) fn backlog(&self) -> usize {
        self.queue.state.lock().jobs.len()
    }

    /// Critical jobs waiting (the one running aside).
    pub(crate) fn critical_backlog(&self) -> usize {
        self.queue.state.lock().critical
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
            st.critical = 0;
            (std::mem::take(&mut st.jobs), st.current.take())
        };
        self.queue.ready.notify_all();
        if let Some(cancel) = current {
            cancel();
        }
        for msg in jobs {
            if let Msg::Job(job) = msg {
                WRITE_QUEUE.dec();
                if job.lane == Lane::Critical {
                    WRITE_QUEUE_CRITICAL.dec();
                }
                (job.cancel)();
            }
        }
    }
}

/// The thread's loop.
fn run(queue: Arc<Queue>, mut w: WriterConn) {
    let logger = w.ctx.logger.clone();
    loop {
        let (msg, episode) = {
            let mut st = queue.state.lock();
            loop {
                if let (Some(msg), episode) = st.pop() {
                    break (Some(msg), episode);
                }
                if st.abandoned {
                    break (None, None);
                }
                queue.ready.wait(&mut st);
            }
        };
        if let Some((refused, over_reserve)) = episode {
            log_info!(logger, "store writer queue back under its busy threshold", {
                "refused": refused, "overReserve": over_reserve, "busy": WRITE_BACKLOG_BUSY,
            });
        }
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
                WRITE_QUEUE.dec();
                if job.lane == Lane::Critical {
                    WRITE_QUEUE_CRITICAL.dec();
                }
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
