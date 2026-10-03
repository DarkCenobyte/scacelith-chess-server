//! The reader pool: `query_only` connections serving reads on tokio's blocking threads. A read
//! started after a write job answered sees that write (WAL).

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use tokio::sync::Semaphore;

use super::conn::{self, DbPath, Role, Tuning};
use super::db::{Db, SharedCtx};
use super::error::{Result, StoreError};
use crate::log_error;

/// Reader connections of a file database.
pub(crate) struct Readers {
    conns: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
    size: u32,
    ctx: SharedCtx,
}

impl Readers {
    /// Opens `n` connections (at least one). Blocking.
    pub(crate) fn open(
        path: &DbPath,
        role: Role,
        tuning: Tuning,
        n: usize,
        ctx: SharedCtx,
    ) -> Result<Readers> {
        let n = n.clamp(1, 64);
        let conns = (0..n).map(|_| conn::open(path, role, tuning)).collect::<Result<Vec<_>>>()?;
        Ok(Readers { conns: Mutex::new(conns), permits: Arc::new(Semaphore::new(n)), size: n as u32, ctx })
    }

    /// Runs `f` on a free connection, in a read transaction, on a blocking thread.
    pub(crate) fn read<R, E, F>(
        self: &Arc<Self>,
        f: F,
    ) -> impl Future<Output = Result<R, E>> + Send + 'static + use<R, E, F>
    where
        F: FnOnce(&Db<'_>) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let readers = self.clone();
        async move {
            let permit = readers.permits.clone().acquire_owned().await.map_err(|_| StoreError::closed())?;
            let joined = tokio::task::spawn_blocking(move || {
                let conn = readers.conns.lock().pop().expect("a permit guarantees a free reader connection");
                let out = catch_unwind(AssertUnwindSafe(|| read_in(&conn, &readers.ctx, f)));
                if out.is_err() {
                    log_error!(readers.ctx.logger, "store read job panicked");
                    if !conn.is_autocommit() {
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                }
                readers.conns.lock().push(conn);
                drop(permit);
                out
            })
            .await;
            match joined {
                Ok(Ok(out)) => out,
                Ok(Err(_)) | Err(_) => Err(StoreError::panicked().into()),
            }
        }
    }

    /// Waits for the reads in progress (at most `timeout`), then closes every connection. Later
    /// reads fail with `closed`. Blocking work (the last connection may checkpoint the WAL) runs on
    /// a blocking thread.
    pub(crate) async fn close(self: &Arc<Self>, timeout: Duration) {
        let all = tokio::time::timeout(timeout, self.permits.acquire_many(self.size)).await;
        self.permits.close();
        drop(all);
        let conns = std::mem::take(&mut *self.conns.lock());
        let _ = tokio::task::spawn_blocking(move || drop(conns)).await;
    }
}

fn read_in<R, E>(
    conn: &Connection,
    ctx: &super::db::Ctx,
    f: impl FnOnce(&Db<'_>) -> Result<R, E>,
) -> Result<R, E>
where
    E: From<StoreError>,
{
    conn.execute_batch("BEGIN").map_err(StoreError::from)?;
    let out = f(&Db::in_transaction(conn, ctx));
    let done = conn.execute_batch(if out.is_ok() { "COMMIT" } else { "ROLLBACK" });
    match (out, done) {
        (Ok(r), Ok(())) => Ok(r),
        (Ok(_), Err(e)) => Err(StoreError::from(e).into()),
        (Err(e), _) => Err(e),
    }
}
