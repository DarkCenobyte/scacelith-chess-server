//! The synchronous face of the store: [`Db`], handed to the closures of [`Store::read`] and
//! [`Store::write`], gives the typed API of every table on one connection, inside the job's
//! transaction.
//!
//! [`Store::read`]: super::Store::read
//! [`Store::write`]: super::Store::write

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use rusqlite::{Connection, OptionalExtension, Params, Row};

use super::error::{Result, StoreError};
use super::{RandomFn, RatingFn};
use crate::clock::SharedClock;
use crate::log::Logger;

/// Settings and hooks shared by every connection of a store.
pub(crate) struct Ctx {
    pub logger: Logger,
    pub clock: SharedClock,
    pub rating: Option<RatingFn>,
    pub random: RandomFn,
    /// `PROVISIONAL_GAMES`: counted games below which a rating is provisional.
    pub provisional_games: i64,
    /// `INITIAL_RATING`: the rating of a player without a record.
    pub initial_rating: i64,
    /// `ANALYSIS_MIN_PLIES`: shorter games are never analysed.
    pub analysis_min_plies: i64,
    /// `ANALYSIS_QUEUE_MAX`: ordinary jobs waiting at most.
    pub analysis_queue_max: i64,
    /// `ANALYSIS_SAMPLE_RATE`: share of the ordinary games queued.
    pub analysis_sample_rate: f64,
    /// `RATING_REFUND_DAYS` in milliseconds (0: no refund at commit).
    pub refund_window_ms: i64,
    /// Analysis jobs claimed so far (every fourth claim takes an ordinary job first).
    pub claims: AtomicU64,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx").field("logger", &self.logger).finish_non_exhaustive()
    }
}

/// One `EXPLAIN QUERY PLAN` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub id: i64,
    pub parent: i64,
    pub detail: String,
}

/// A connection inside a transaction, with the typed API of every table.
///
/// In a write job the transaction is `BEGIN IMMEDIATE` on the writer connection: everything the
/// closure does commits together, or not at all when it returns an error or panics. In a read job
/// it is a read transaction on a reader connection: every query sees the same snapshot.
pub struct Db<'c> {
    conn: &'c Connection,
    ctx: &'c Ctx,
    /// Open transactions and savepoints (1 inside a job).
    depth: Cell<u32>,
}

impl std::fmt::Debug for Db<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("depth", &self.depth.get()).finish_non_exhaustive()
    }
}

impl<'c> Db<'c> {
    /// A `Db` over a connection whose job transaction is already open.
    pub(crate) fn in_transaction(conn: &'c Connection, ctx: &'c Ctx) -> Db<'c> {
        Db { conn, ctx, depth: Cell::new(1) }
    }

    pub(crate) fn ctx(&self) -> &'c Ctx {
        self.ctx
    }

    pub(crate) fn logger(&self) -> &'c Logger {
        &self.ctx.logger
    }

    /// The store clock's wall time in milliseconds (the default time of new rows).
    pub fn now(&self) -> i64 {
        self.ctx.clock.wall_ms()
    }

    /// The raw connection, for tools that need a statement the typed API does not have (a backup
    /// with `VACUUM INTO`, diagnostics). Prefer the typed API.
    pub fn connection(&self) -> &'c Connection {
        self.conn
    }

    /// Runs `f` in a savepoint: its changes are undone when it returns an error (or panics),
    /// without failing the enclosing job, which can go on.
    pub fn transaction<R, E>(&self, f: impl FnOnce(&Db<'c>) -> Result<R, E>) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        let depth = self.depth.get();
        let name = format!("sp{depth}");
        if depth == 0 {
            self.conn.execute_batch("BEGIN IMMEDIATE").map_err(StoreError::from)?;
        } else {
            self.conn.execute_batch(&format!("SAVEPOINT {name}")).map_err(StoreError::from)?;
        }
        self.depth.set(depth + 1);
        let guard = DepthGuard { db: self, depth };
        let out = f(self);
        let end = match (&out, depth) {
            (Ok(_), 0) => self.conn.execute_batch("COMMIT"),
            (Ok(_), _) => self.conn.execute_batch(&format!("RELEASE {name}")),
            (Err(_), 0) => self.conn.execute_batch("ROLLBACK"),
            (Err(_), _) => self.conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}")),
        };
        drop(guard);
        match (out, end) {
            (Ok(r), Ok(())) => Ok(r),
            (Ok(_), Err(e)) => {
                if depth == 0 {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                Err(StoreError::from(e).into())
            }
            (Err(e), _) => Err(e),
        }
    }

    /// The `EXPLAIN QUERY PLAN` rows of a statement (tests and diagnostics).
    pub fn explain_query_plan(&self, sql: &str, params: impl Params) -> Result<Vec<PlanRow>> {
        let mut stmt = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows =
            stmt.query_map(params, |r| Ok(PlanRow { id: r.get(0)?, parent: r.get(1)?, detail: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Runs a statement; returns the rows changed.
    pub(crate) fn exec(&self, sql: &str, params: impl Params) -> Result<usize> {
        Ok(self.conn.prepare_cached(sql)?.execute(params)?)
    }

    /// Runs an `INSERT`; returns the new rowid.
    pub(crate) fn insert(&self, sql: &str, params: impl Params) -> Result<i64> {
        self.conn.prepare_cached(sql)?.execute(params)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The first row of a query, if any.
    pub(crate) fn one<T>(
        &self,
        sql: &str,
        params: impl Params,
        f: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        Ok(self.conn.prepare_cached(sql)?.query_row(params, f).optional()?)
    }

    /// Every row of a query.
    pub(crate) fn all<T>(
        &self,
        sql: &str,
        params: impl Params,
        f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params, f)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A count (or any single integer) query.
    pub(crate) fn count(&self, sql: &str, params: impl Params) -> Result<i64> {
        Ok(self.conn.prepare_cached(sql)?.query_row(params, |r| r.get(0))?)
    }
}

/// Restores the transaction depth of a [`Db`], also when the closure panics.
struct DepthGuard<'a, 'c> {
    db: &'a Db<'c>,
    depth: u32,
}

impl Drop for DepthGuard<'_, '_> {
    fn drop(&mut self) {
        self.db.depth.set(self.depth);
    }
}

/// A shared `Ctx` (the writer and every reader hold one).
pub(crate) type SharedCtx = Arc<Ctx>;
