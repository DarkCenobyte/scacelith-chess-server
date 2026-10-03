//! SQLite store: schema and migrations, the single writer thread (FIFO, `BEGIN IMMEDIATE`), the
//! reader pool, and the typed API of every table (users, sessions, tokens, signups, ratings,
//! games, conduct, sanctions, anomalies, analysis queue, integrity, reports, refunds), plus the
//! retention purge. Owner: store. See docs/RUST-PORT.md section 6 and DESIGN 5.5, 7.
//!
//! # Shape
//!
//! [`Store`] is a cheap handle (`Clone`). Every write of the process goes through one writer
//! thread that owns the only writable connection and runs jobs one at a time, in submission order,
//! each in one `BEGIN IMMEDIATE` transaction:
//!
//! ```ignore
//! let id = store.write(|db| {
//!     if db.users().by_email(&email)?.is_some() {
//!         return Err(StoreError::new(ErrorKind::EmailTaken, "e-mail address already used"));
//!     }
//!     db.users().create(&new_user)
//! }).await?;
//! ```
//!
//! Reads run on a pool of read-only connections (tokio blocking threads) in a read transaction;
//! a read started after a write answered sees it. The closure receives a [`Db`], the synchronous
//! typed API of every table on that connection (`db.users()`, `db.games()`...), so that a
//! read-modify-write flow stays atomic inside one job. The async table handles
//! (`store.users().by_id(id).await`) wrap one call each in its own job.
//!
//! # Ordering
//!
//! A write job is queued when [`Store::write`] (or an async table method that writes) is called,
//! not when its future is first polled: jobs submitted one after the other run in that order. The
//! anti-cheat relies on it: anomalies submitted before a game batch are visible to the analysis
//! queue policy of that batch.
//!
//! # Errors
//!
//! [`StoreError`] has a [`kind`](StoreError::kind) (`Busy` after `busy_timeout`, the constraint
//! kinds, `Closed`...) and, for a failed game commit, the [`game_id`](StoreError::game_id) of the
//! record at fault, so that the host can retry the batch without it.

mod analysis;
mod commit;
mod conn;
mod db;
mod error;
mod games;
mod integrity;
mod metrics;
pub mod migrate;
mod moderation;
mod ratings;
mod readers;
mod refunds;
mod reports;
pub mod retention;
mod sessions;
mod tokens;
mod users;
mod values;
mod writer;

#[cfg(test)]
mod tests;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusqlite::params;
use rusqlite::types::Value as SqlValue;

pub use analysis::{
    ANALYSED_FOR_USER_SQL, Analysis, BACKLOG_COUNT_MAX, Backlog, ClaimedJob, Color, Job, JobStatus,
    MAX_ATTEMPTS, ORDINARY_SHARE, PlayerJob, Priority, QueueStats, STALE_MS,
};
pub use commit::{CommitBatch, CommitEntry, CommitRatings, CommitRefund, RatingChange, SkipReason};
pub use db::{Db, PlanRow};
pub use error::{ErrorKind, Result, StoreError};
pub use games::{
    GAMES_COUNT_FOR_USER_SQL, GAMES_FOR_USER_SQL, Game, GameFilter, GameRecord, GameSummary, Games,
    RatingChanges, RatingDelta, ResultFilter, flags, status,
};
pub use integrity::{
    FlaggedPlayer, Integrity, IntegrityLevel, IntegrityTable, IntegrityUpdate, PopulationStat,
    PopulationUpdate, Sample,
};
pub use metrics::SIGNAL_JOBS_PER_PLAYER;
pub use migrate::{Migration, MigrationReport};
pub use moderation::{
    Anomalies, Anomaly, Conduct, ConductCounts, ConductEvent, ConductKind, Cooldown, NewAnomaly, NewSanction,
    NewSecurityEvent, Sanction, SanctionKind, Sanctions, Security, SecurityEvent, Severity, Source,
};
pub use ratings::{
    CategoryRating, GameOutcome, LeaderboardRow, RatingFn, RatingRecord, Ratings, SideOutcome,
};
pub use refunds::{CheaterRefunds, GivenRefund, PendingRefund, PendingRefunds, Refund, RefundScope, Refunds};
pub use reports::{NewReport, Report, ReportCategory, ReportCounts, ReportStatus, ReportWeights, Reports};
pub use retention::{
    AbortFlag, PurgeCounts, PurgeOptions, RetentionApi, RetentionError, RetentionPolicy, RetentionScheduler,
    SchedulerOptions, SecurityPurge,
};
pub use sessions::{NewSession, SessionAuth, SessionInfo, Sessions};
pub use tokens::{NewSignup, NewToken, Signup, Signups, Sso, SsoIdentity, SsoLink, Token, Tokens};
pub use users::{Anonymized, Mfa, NewUser, User, UserStatus, UserUpdate, Users};
pub use values::{NO_CURSOR, clean_email, js_trim, normalize_email};

use crate::clock::SharedClock;
use crate::config::Config;
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::log_security;
use conn::{DbPath, Role, Tuning};
use db::Ctx;
use readers::Readers;
use writer::{CLOSE_TIMEOUT, Writer};

/// A uniform draw in `[0, 1)` (the analysis sampling; injectable for tests).
pub type RandomFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Options of [`Store::open`].
#[derive(Clone, Default)]
pub struct StoreOptions {
    /// Database file overriding `DB_PATH`; `""` or `:memory:` (also as a file name) opens a private
    /// in-memory database (tests), whose reads go through the writer.
    pub path: Option<String>,
    /// Opens the file read-only: no writer, every write fails with `readonly`.
    pub readonly: bool,
    /// The rating function, required to commit rated games.
    pub rating: Option<RatingFn>,
    /// The analysis sampling draw (default: the system's random source).
    pub random: Option<RandomFn>,
    /// The wall clock of default times (commit time, anomaly times; default: the system clock).
    pub clock: Option<SharedClock>,
    /// Reader connections (default 4).
    pub readers: Option<usize>,
    /// Logger (default `store`).
    pub logger: Option<Logger>,
}

impl std::fmt::Debug for StoreOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreOptions")
            .field("path", &self.path)
            .field("readonly", &self.readonly)
            .field("rating", &self.rating.is_some())
            .field("readers", &self.readers)
            .finish_non_exhaustive()
    }
}

/// A draw from the system's random source.
fn system_random() -> f64 {
    // 53 random bits as a fraction; a failing source draws 0 (the game is sampled in).
    getrandom::u64().map_or(0.0, |v| (v >> 11) as f64 / (1u64 << 53) as f64)
}

struct Inner {
    ctx: Arc<Ctx>,
    path: DbPath,
    readonly: bool,
    writer: Option<Writer>,
    readers: Option<Arc<Readers>>,
    closed: AtomicBool,
    close_lock: tokio::sync::Mutex<()>,
}

/// The store (module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.inner.path)
            .field("readonly", &self.inner.readonly)
            .finish()
    }
}

impl Store {
    /// Opens the database (it is not migrated: call [`Store::migrate`] at start-up). The file and
    /// its directory are created when missing (not for a read-only store).
    pub async fn open(config: &Config, options: StoreOptions) -> Result<Store> {
        let path = DbPath::parse(options.path.as_deref().unwrap_or(&config.db_path));
        if options.readonly && path == DbPath::Memory {
            return Err(StoreError::invalid("an in-memory store cannot be read-only"));
        }
        let ctx = Arc::new(Ctx {
            logger: options.logger.clone().unwrap_or_else(|| Logger::root().child("store")),
            clock: options.clock.clone().unwrap_or_else(crate::clock::system),
            rating: options.rating.clone(),
            random: options.random.clone().unwrap_or_else(|| Arc::new(system_random)),
            provisional_games: config.provisional_games,
            initial_rating: config.initial_rating,
            analysis_min_plies: config.analysis_min_plies,
            analysis_queue_max: config.analysis_queue_max,
            analysis_sample_rate: config.analysis_sample_rate,
            refund_window_ms: config.rating_refund_days.max(0) * 86_400_000,
            claims: AtomicU64::new(0),
        });
        let tuning = Tuning { cache_mb: config.db_cache_mb, mmap_mb: config.db_mmap_mb };
        let readonly = options.readonly;
        let n_readers = options.readers.unwrap_or(4);
        let (c, p) = (ctx.clone(), path.clone());
        let (writer, readers) =
            tokio::task::spawn_blocking(move || -> Result<(Option<Writer>, Option<Arc<Readers>>)> {
                let writer = if readonly { None } else { Some(Writer::start(p.clone(), tuning, c.clone())?) };
                let readers = match &p {
                    DbPath::Memory => None,
                    DbPath::File(_) => {
                        let role = if readonly { Role::ReadOnly } else { Role::Reader };
                        Some(Arc::new(Readers::open(&p, role, tuning, n_readers, c)?))
                    }
                };
                Ok((writer, readers))
            })
            .await
            .map_err(|e| StoreError::new(ErrorKind::Internal, format!("store open failed: {e}")))??;
        Ok(Store {
            inner: Arc::new(Inner {
                ctx,
                path,
                readonly,
                writer,
                readers,
                closed: AtomicBool::new(false),
                close_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Whether the store was opened read-only.
    pub fn readonly(&self) -> bool {
        self.inner.readonly
    }

    /// The database path (`:memory:` for an in-memory database).
    pub fn path(&self) -> String {
        self.inner.path.display()
    }

    /// Whether [`Store::close`] was called.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn writer(&self) -> Option<&Writer> {
        self.inner.writer.as_ref()
    }

    /// Runs `f` in a read transaction on a reader connection (on a blocking thread): every query
    /// of `f` sees the same snapshot, which includes every write answered before the call.
    pub fn read<R, E, F>(&self, f: F) -> impl Future<Output = Result<R, E>> + Send + 'static + use<R, E, F>
    where
        F: FnOnce(&Db<'_>) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let inner = self.inner.clone();
        async move {
            if inner.closed.load(Ordering::SeqCst) {
                return Err(StoreError::closed().into());
            }
            match (&inner.readers, &inner.writer) {
                (Some(readers), _) => readers.read(f).await,
                (None, Some(writer)) => writer.submit(move |w| w.read(f)).await,
                (None, None) => Err(StoreError::closed().into()),
            }
        }
    }

    /// Runs `f` on the writer thread in one `BEGIN IMMEDIATE` transaction, committed when it
    /// returns `Ok`, rolled back when it returns an error or panics. The job is queued now (see
    /// the module documentation on ordering). Errors: `readonly`, `closed`, `busy` (the lock was
    /// not obtained within `busy_timeout`), and whatever `f` returns.
    pub fn write<R, E, F>(&self, f: F) -> impl Future<Output = Result<R, E>> + Send + 'static + use<R, E, F>
    where
        F: FnOnce(&Db<'_>) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let submitted = match (&self.inner.writer, self.is_closed()) {
            (Some(writer), false) => Ok(writer.submit(move |w| w.transact(f).0)),
            (Some(_), true) => Err(StoreError::closed()),
            (None, _) => Err(StoreError::new(ErrorKind::ReadOnly, "read-only store")),
        };
        async move {
            match submitted {
                Ok(fut) => fut.await,
                Err(e) => Err(e.into()),
            }
        }
    }

    /// Applies the migrations of this server ([`migrate::embedded`]) and creates the server id.
    pub async fn migrate(&self) -> Result<MigrationReport> {
        let now = self.inner.ctx.clock.wall_ms();
        self.migrate_with(migrate::embedded(), now).await
    }

    /// Applies `migrations` (tests and tools; [`Store::migrate`] at start-up).
    pub async fn migrate_with(&self, migrations: Vec<Migration>, now: i64) -> Result<MigrationReport> {
        let Some(writer) = self.writer() else {
            return Err(StoreError::new(ErrorKind::ReadOnly, "cannot migrate a read-only store"));
        };
        writer.submit(move |w| migrate::run(w.conn(), &migrations, now)).await
    }

    /// The `EXPLAIN QUERY PLAN` rows of a statement (tests and diagnostics).
    pub async fn explain_query_plan(&self, sql: &str, params: Vec<SqlValue>) -> Result<Vec<PlanRow>> {
        let sql = sql.to_string();
        self.read(move |db| db.explain_query_plan(&sql, rusqlite::params_from_iter(params))).await
    }

    /// The server id created by the first migration run.
    pub async fn server_id(&self) -> Result<Option<String>> {
        self.meta().get("server_id".into()).await
    }

    /// Closes the store: later calls fail with `closed`; the reads in progress and the write jobs
    /// already queued finish (at most 7 s, longer than `busy_timeout`; a job not answered by then
    /// fails with "outcome unknown"), then `PRAGMA optimize` and every connection is closed.
    /// Idempotent.
    pub async fn close(&self) {
        let _guard = self.inner.close_lock.lock().await;
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let logger = self.inner.ctx.logger.clone();
        let readers = async {
            if let Some(r) = &self.inner.readers {
                r.close(CLOSE_TIMEOUT).await;
            }
        };
        let writer = async {
            if let Some(w) = &self.inner.writer {
                w.close(CLOSE_TIMEOUT, &logger).await;
            }
        };
        tokio::join!(readers, writer);
    }

    /// Commits finished games in one transaction (DESIGN 5.5): game rows, both ratings of each
    /// rated game (read and written inside the transaction), refunds owed at commit and analysis
    /// jobs. Idempotent per game id (a stored id gives a `duplicate` entry). Entries are in record
    /// order. The whole batch is rolled back on any error; an error caused by one record carries
    /// its game id. The commit metrics are counted and the refunds logged after the commit.
    pub fn finish_batch(
        &self,
        records: Vec<GameRecord>,
    ) -> impl Future<Output = Result<Vec<CommitEntry>>> + Send + 'static + use<> {
        let logger = self.inner.ctx.logger.clone();
        let submitted = if records.is_empty() {
            None
        } else {
            Some(self.write_timed(move |db| commit::finish_batch(db, &records, db.now())))
        };
        async move {
            let Some(fut) = submitted else { return Ok(Vec::new()) };
            let (batch, ms) = fut.await?;
            for f in &batch.refunds {
                log_security!(logger, "rating.refund", {
                    "cheaterId": f.cheater_id,
                    "source": "auto",
                    "sanctionId": f.sanction_id,
                    "gameId": f.refund.game_id,
                    "refunds": 1,
                    "victims": 1,
                    "points": f.refund.points,
                });
            }
            metrics::count_commit(&batch.entries, ms);
            Ok(batch.entries)
        }
    }

    /// [`Store::write`] that also returns the duration of the transaction in milliseconds.
    fn write_timed<R, F>(&self, f: F) -> impl Future<Output = Result<(R, f64)>> + Send + 'static + use<R, F>
    where
        F: FnOnce(&Db<'_>) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let submitted = match (&self.inner.writer, self.is_closed()) {
            (Some(writer), false) => Ok(writer.submit(move |w| {
                let (out, ms) = w.transact(f);
                out.map(|r| (r, ms))
            })),
            (Some(_), true) => Err(StoreError::closed()),
            (None, _) => Err(StoreError::new(ErrorKind::ReadOnly, "read-only store")),
        };
        async move { submitted?.await }
    }

    /// The retention purge.
    pub fn retention(&self) -> RetentionApi {
        RetentionApi { store: self.clone() }
    }
}

/// Server-wide values.
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    db: &'a Db<'a>,
}

impl Meta<'_> {
    /// The value of a key.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        self.db.one("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
    }

    /// Sets a key.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db.exec(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

impl<'c> Db<'c> {
    /// Server-wide values.
    pub fn meta(&self) -> Meta<'_> {
        Meta { db: self }
    }

    /// Accounts.
    pub fn users(&self) -> Users<'_> {
        Users { db: self }
    }

    /// MFA recovery codes.
    pub fn mfa(&self) -> Mfa<'_> {
        Mfa { db: self }
    }

    /// Login sessions.
    pub fn sessions(&self) -> Sessions<'_> {
        Sessions { db: self }
    }

    /// Single-use tokens.
    pub fn tokens(&self) -> Tokens<'_> {
        Tokens { db: self }
    }

    /// Pending signups.
    pub fn signups(&self) -> Signups<'_> {
        Signups { db: self }
    }

    /// SSO identities.
    pub fn sso(&self) -> Sso<'_> {
        Sso { db: self }
    }

    /// Rating records.
    pub fn ratings(&self) -> Ratings<'_> {
        Ratings { db: self }
    }

    /// Finished games.
    pub fn games(&self) -> Games<'_> {
        Games { db: self }
    }

    /// Conduct events and cooldowns.
    pub fn conduct(&self) -> Conduct<'_> {
        Conduct { db: self }
    }

    /// Sanctions.
    pub fn sanctions(&self) -> Sanctions<'_> {
        Sanctions { db: self }
    }

    /// Anomalies.
    pub fn anomalies(&self) -> Anomalies<'_> {
        Anomalies { db: self }
    }

    /// Security events.
    pub fn security(&self) -> Security<'_> {
        Security { db: self }
    }

    /// The analysis queue.
    pub fn analysis(&self) -> Analysis<'_> {
        Analysis { db: self }
    }

    /// Integrity records and population statistics.
    pub fn integrity(&self) -> IntegrityTable<'_> {
        IntegrityTable { db: self }
    }

    /// Player reports.
    pub fn reports(&self) -> Reports<'_> {
        Reports { db: self }
    }

    /// Rating refunds.
    pub fn refunds(&self) -> Refunds<'_> {
        Refunds { db: self }
    }

    /// Commits finished games inside this job (see [`Store::finish_batch`], which also counts the
    /// commit metrics and logs the refunds once committed: prefer it).
    pub fn finish_batch(&self, records: &[GameRecord]) -> Result<CommitBatch> {
        commit::finish_batch(self, records, self.now())
    }
}

/// Defines an async table handle whose methods run one call of the synchronous table API in its
/// own read or write job.
macro_rules! async_api {
    (
        $(#[$m:meta])*
        $api:ident {
            $( $(#[$fm:meta])* $kind:ident fn $name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty => |$db:ident| $body:expr; )*
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone)]
        pub struct $api {
            store: Store,
        }

        impl $api {
            $(
                $(#[$fm])*
                pub fn $name(&self, $($arg: $ty),*) -> impl Future<Output = Result<$ret>> + Send + 'static + use<> {
                    self.store.$kind(move |$db: &Db<'_>| $body)
                }
            )*
        }
    };
}

async_api! {
    /// Async face of [`Meta`].
    MetaApi {
        /// See [`Meta::get`].
        read fn get(key: String) -> Option<String> => |db| db.meta().get(&key);
        /// See [`Meta::set`].
        write fn set(key: String, value: String) -> () => |db| db.meta().set(&key, &value);
    }
}

async_api! {
    /// Async face of [`Users`].
    UsersApi {
        /// See [`Users::create`].
        write fn create(user: NewUser) -> UserId => |db| db.users().create(&user);
        /// See [`Users::by_id`].
        read fn by_id(id: UserId) -> Option<User> => |db| db.users().by_id(id);
        /// See [`Users::by_username`].
        read fn by_username(name: String) -> Option<User> => |db| db.users().by_username(&name);
        /// See [`Users::by_email`].
        read fn by_email(email: String) -> Option<User> => |db| db.users().by_email(&email);
        /// See [`Users::by_login`].
        read fn by_login(login: String) -> Option<User> => |db| db.users().by_login(&login);
        /// See [`Users::update`].
        write fn update(id: UserId, fields: UserUpdate) -> bool => |db| db.users().update(id, &fields);
        /// See [`Users::anonymize`].
        write fn anonymize(id: UserId, now: i64) -> Anonymized => |db| db.users().anonymize(id, now);
        /// See [`Users::advance_mfa_step`].
        write fn advance_mfa_step(id: UserId, step: i64) -> bool => |db| db.users().advance_mfa_step(id, step);
    }
}

async_api! {
    /// Async face of [`Mfa`].
    MfaApi {
        /// See [`Mfa::replace_recovery_codes`].
        write fn replace_recovery_codes(user_id: UserId, hashes: Vec<String>, now: i64) -> () =>
            |db| db.mfa().replace_recovery_codes(user_id, &hashes, now);
        /// See [`Mfa::consume_recovery_code`].
        write fn consume_recovery_code(user_id: UserId, hash: String) -> bool =>
            |db| db.mfa().consume_recovery_code(user_id, &hash);
        /// See [`Mfa::count_recovery_codes`].
        read fn count_recovery_codes(user_id: UserId) -> i64 => |db| db.mfa().count_recovery_codes(user_id);
    }
}

async_api! {
    /// Async face of [`Sessions`].
    SessionsApi {
        /// See [`Sessions::create`].
        write fn create(session: NewSession) -> i64 => |db| db.sessions().create(&session);
        /// See [`Sessions::by_token_hash`].
        read fn by_token_hash(hash: String) -> Option<SessionAuth> => |db| db.sessions().by_token_hash(&hash);
        /// See [`Sessions::touch`].
        write fn touch(id: i64, now: i64, idle_expires_at: i64) -> () => |db| db.sessions().touch(id, now, idle_expires_at);
        /// See [`Sessions::revoke`].
        write fn revoke(id: i64, user_id: Option<UserId>, now: i64) -> Option<String> =>
            |db| db.sessions().revoke(id, user_id, now);
        /// See [`Sessions::revoke_all_for_user`].
        write fn revoke_all_for_user(user_id: UserId, except: Option<i64>, now: i64) -> Vec<String> =>
            |db| db.sessions().revoke_all_for_user(user_id, except, now);
        /// See [`Sessions::list_for_user`].
        read fn list_for_user(user_id: UserId) -> Vec<SessionInfo> => |db| db.sessions().list_for_user(user_id);
        /// See [`Sessions::all_for_user`].
        read fn all_for_user(user_id: UserId) -> Vec<SessionInfo> => |db| db.sessions().all_for_user(user_id);
        /// See [`Sessions::enforce_limit`].
        write fn enforce_limit(user_id: UserId, max: i64, now: i64) -> Vec<String> =>
            |db| db.sessions().enforce_limit(user_id, max, now);
    }
}

async_api! {
    /// Async face of [`Tokens`].
    TokensApi {
        /// See [`Tokens::create`].
        write fn create(token: NewToken) -> i64 => |db| db.tokens().create(&token);
        /// See [`Tokens::consume`].
        write fn consume(kind: String, hash: String, now: i64) -> Option<Token> => |db| db.tokens().consume(&kind, &hash, now);
        /// See [`Tokens::get`].
        read fn get(kind: String, hash: String) -> Option<Token> => |db| db.tokens().get(&kind, &hash);
        /// See [`Tokens::reserve_try`].
        write fn reserve_try(kind: String, hash: String, max: i64, now: i64) -> Option<Token> =>
            |db| db.tokens().reserve_try(&kind, &hash, max, now);
        /// See [`Tokens::update`].
        write fn update(kind: String, hash: String, data: Option<serde_json::Value>) -> bool =>
            |db| db.tokens().update(&kind, &hash, data.as_ref());
        /// See [`Tokens::delete_for_user`].
        write fn delete_for_user(user_id: UserId, kind: String) -> usize => |db| db.tokens().delete_for_user(user_id, &kind);
        /// See [`Tokens::live_for_user`].
        read fn live_for_user(user_id: UserId, kind: String, now: i64) -> Option<Token> =>
            |db| db.tokens().live_for_user(user_id, &kind, now);
    }
}

async_api! {
    /// Async face of [`Signups`].
    SignupsApi {
        /// See [`Signups::create`].
        write fn create(signup: NewSignup) -> i64 => |db| db.signups().create(&signup);
        /// See [`Signups::by_username`].
        read fn by_username(name: String) -> Option<Signup> => |db| db.signups().by_username(&name);
        /// See [`Signups::by_email`].
        read fn by_email(email: String) -> Option<Signup> => |db| db.signups().by_email(&email);
        /// See [`Signups::by_token_hash`].
        read fn by_token_hash(hash: String) -> Option<Signup> => |db| db.signups().by_token_hash(&hash);
        /// See [`Signups::renew`].
        write fn renew(id: i64, token_hash: Option<String>, expires_at: i64) -> bool =>
            |db| db.signups().renew(id, token_hash.as_deref(), expires_at);
        /// See [`Signups::delete`].
        write fn delete(id: i64) -> bool => |db| db.signups().delete(id);
    }
}

async_api! {
    /// Async face of [`Sso`].
    SsoApi {
        /// See [`Sso::find`].
        read fn find(provider: String, subject: String) -> Option<SsoLink> => |db| db.sso().find(&provider, &subject);
        /// See [`Sso::link`].
        write fn link(user_id: UserId, provider: String, subject: String, email: Option<String>, now: i64) -> () =>
            |db| db.sso().link(user_id, &provider, &subject, email.as_deref(), now);
        /// See [`Sso::for_user`].
        read fn for_user(user_id: UserId) -> Vec<SsoIdentity> => |db| db.sso().for_user(user_id);
    }
}

async_api! {
    /// Async face of [`Ratings`].
    RatingsApi {
        /// See [`Ratings::get`].
        read fn get(user_id: UserId, category: String) -> RatingRecord => |db| db.ratings().get(user_id, &category);
        /// See [`Ratings::for_user`].
        read fn for_user(user_id: UserId) -> Vec<CategoryRating> => |db| db.ratings().for_user(user_id);
        /// See [`Ratings::leaderboard`].
        read fn leaderboard(category: String, limit: i64, min_games: Option<i64>) -> Vec<LeaderboardRow> =>
            |db| db.ratings().leaderboard(&category, limit, min_games);
    }
}

async_api! {
    /// Async face of [`Games`] (the commit is [`Store::finish_batch`]).
    GamesApi {
        /// See [`Games::by_id`].
        read fn by_id(id: GameId) -> Option<Game> => |db| db.games().by_id(id);
        /// See [`Games::last_id`].
        read fn last_id() -> GameId => |db| db.games().last_id();
        /// See [`Games::recent_for_user`].
        read fn recent_for_user(user_id: UserId, limit: i64, before: Option<GameId>) -> Vec<GameSummary> =>
            |db| db.games().recent_for_user(user_id, limit, before);
        /// See [`Games::count_between`].
        read fn count_between(a: UserId, b: UserId, since: i64, rated_only: bool) -> i64 =>
            |db| db.games().count_between(a, b, since, rated_only);
        /// See [`Games::list_for_user`].
        read fn list_for_user(user_id: UserId, before: Option<GameId>, limit: i64, filter: GameFilter) -> Vec<GameSummary> =>
            |db| db.games().list_for_user(user_id, before, limit, &filter);
        /// See [`Games::count_for_user`].
        read fn count_for_user(user_id: UserId, filter: Option<GameFilter>) -> i64 =>
            |db| db.games().count_for_user(user_id, filter.as_ref());
    }
}

async_api! {
    /// Async face of [`Conduct`].
    ConductApi {
        /// See [`Conduct::record`].
        write fn record(user_id: UserId, kind: ConductKind, at: i64) -> () => |db| db.conduct().record(user_id, kind, at);
        /// See [`Conduct::count_since`].
        read fn count_since(user_id: UserId, since: i64) -> ConductCounts => |db| db.conduct().count_since(user_id, since);
        /// See [`Conduct::for_user`].
        read fn for_user(user_id: UserId, limit: i64) -> Vec<ConductEvent> => |db| db.conduct().for_user(user_id, limit);
        /// See [`Conduct::cooldown`].
        read fn cooldown(user_id: UserId) -> Cooldown => |db| db.conduct().cooldown(user_id);
        /// See [`Conduct::set_cooldown`].
        write fn set_cooldown(user_id: UserId, until: i64, level: i64, now: i64) -> () =>
            |db| db.conduct().set_cooldown(user_id, until, level, now);
    }
}

async_api! {
    /// Async face of [`Sanctions`].
    SanctionsApi {
        /// See [`Sanctions::create`].
        write fn create(sanction: NewSanction) -> i64 => |db| db.sanctions().create(&sanction);
        /// See [`Sanctions::active_ban`].
        read fn active_ban(user_id: UserId, now: i64) -> Option<Sanction> => |db| db.sanctions().active_ban(user_id, now);
        /// See [`Sanctions::active`].
        read fn active(user_id: UserId, now: i64) -> Vec<Sanction> => |db| db.sanctions().active(user_id, now);
        /// See [`Sanctions::list`].
        read fn list(user_id: UserId) -> Vec<Sanction> => |db| db.sanctions().list(user_id);
        /// See [`Sanctions::lift`].
        write fn lift(id: i64, by: Option<String>, now: i64) -> bool => |db| db.sanctions().lift(id, by.as_deref(), now);
    }
}

async_api! {
    /// Async face of [`Anomalies`].
    AnomaliesApi {
        /// See [`Anomalies::insert_batch`].
        write fn insert_batch(list: Vec<NewAnomaly>) -> usize => |db| db.anomalies().insert_batch(&list);
        /// See [`Anomalies::for_user`].
        read fn for_user(user_id: UserId, limit: i64) -> Vec<Anomaly> => |db| db.anomalies().for_user(user_id, limit);
    }
}

async_api! {
    /// Async face of [`Security`] (the purge is [`RetentionApi::purge_security`]).
    SecurityApi {
        /// See [`Security::insert_batch`].
        write fn insert_batch(list: Vec<NewSecurityEvent>) -> usize => |db| db.security().insert_batch(&list);
        /// See [`Security::for_user`].
        read fn for_user(user_id: UserId, limit: i64) -> Vec<SecurityEvent> => |db| db.security().for_user(user_id, limit);
    }
}

async_api! {
    /// Async face of [`Analysis`].
    AnalysisApi {
        /// See [`Analysis::next`].
        write fn next(limit: i64, worker: Option<String>, now: i64) -> Vec<ClaimedJob> =>
            |db| db.analysis().next(limit, worker.as_deref(), now);
        /// See [`Analysis::complete`].
        write fn complete(game_id: GameId, features: Option<serde_json::Value>, now: i64) -> bool =>
            |db| db.analysis().complete(game_id, features.as_ref(), now);
        /// See [`Analysis::touch`].
        write fn touch(game_id: GameId, worker: Option<String>, now: i64) -> bool =>
            |db| db.analysis().touch(game_id, worker.as_deref(), now);
        /// See [`Analysis::fail`].
        write fn fail(game_id: GameId, error: Option<String>, now: i64) -> Option<JobStatus> =>
            |db| db.analysis().fail(game_id, error.as_deref(), now);
        /// See [`Analysis::job`].
        read fn job(game_id: GameId) -> Option<Job> => |db| db.analysis().job(game_id);
        /// See [`Analysis::enqueue`].
        write fn enqueue(game_id: GameId, now: i64) -> () => |db| db.analysis().enqueue(game_id, now);
        /// See [`Analysis::request`].
        write fn request(game_id: GameId, priority: Priority, now: i64) -> bool =>
            |db| db.analysis().request(game_id, priority, now);
        /// See [`Analysis::backlog`].
        read fn backlog() -> Backlog => |db| db.analysis().backlog();
        /// See [`Analysis::for_user`].
        read fn for_user(user_id: UserId, limit: i64, done_only: bool) -> Vec<PlayerJob> =>
            |db| db.analysis().for_user(user_id, limit, done_only);
        /// See [`Analysis::stats`].
        read fn stats() -> QueueStats => |db| db.analysis().stats();
    }
}

async_api! {
    /// Async face of [`IntegrityTable`].
    IntegrityApi {
        /// See [`IntegrityTable::get`].
        read fn get(user_id: UserId) -> Integrity => |db| db.integrity().get(user_id);
        /// See [`IntegrityTable::set`].
        write fn set(user_id: UserId, fields: IntegrityUpdate) -> () => |db| db.integrity().set(user_id, &fields);
        /// See [`IntegrityTable::list_flagged`].
        read fn list_flagged(min_level: IntegrityLevel, limit: i64) -> Vec<FlaggedPlayer> =>
            |db| db.integrity().list_flagged(min_level, limit);
        /// See [`IntegrityTable::population_stats`].
        read fn population_stats(prefix: String) -> indexmap::IndexMap<String, PopulationStat> =>
            |db| db.integrity().population_stats(&prefix);
        /// See [`IntegrityTable::update_population`].
        write fn update_population(updates: Vec<PopulationUpdate>, now: i64) -> () =>
            |db| db.integrity().update_population(&updates, now);
    }
}

async_api! {
    /// Async face of [`Reports`].
    ReportsApi {
        /// See [`Reports::create`].
        write fn create(report: NewReport) -> i64 => |db| db.reports().create(&report);
        /// See [`Reports::count_by_reporter_since`].
        read fn count_by_reporter_since(reporter_id: UserId, since: i64) -> i64 =>
            |db| db.reports().count_by_reporter_since(reporter_id, since);
        /// See [`Reports::exists`].
        read fn exists(reporter_id: UserId, reported_id: UserId, game_id: Option<GameId>) -> bool =>
            |db| db.reports().exists(reporter_id, reported_id, game_id);
        /// See [`Reports::list_open`].
        read fn list_open(limit: i64) -> Vec<Report> => |db| db.reports().list_open(limit);
        /// See [`Reports::for_reported`].
        read fn for_reported(user_id: UserId, limit: i64) -> Vec<Report> => |db| db.reports().for_reported(user_id, limit);
        /// See [`Reports::for_reporter`].
        read fn for_reporter(user_id: UserId, limit: i64) -> Vec<Report> => |db| db.reports().for_reporter(user_id, limit);
        /// See [`Reports::resolve`].
        write fn resolve(id: i64, outcome: ReportStatus, by: Option<String>, now: i64) -> bool =>
            |db| db.reports().resolve(id, outcome, by.as_deref(), now);
        /// See [`Reports::resolve_open_for`].
        write fn resolve_open_for(reported_id: UserId, category: ReportCategory, outcome: ReportStatus, by: Option<String>, now: i64) -> Vec<i64> =>
            |db| db.reports().resolve_open_for(reported_id, category, outcome, by.as_deref(), now);
        /// See [`Reports::weight_since`].
        read fn weight_since(reported_id: UserId, since: i64, low_threshold: f64) -> ReportWeights =>
            |db| db.reports().weight_since(reported_id, since, low_threshold);
        /// See [`Reports::count_for`].
        read fn count_for(reported_id: UserId) -> ReportCounts => |db| db.reports().count_for(reported_id);
    }
}

async_api! {
    /// Async face of [`Refunds`].
    RefundsApi {
        /// See [`Refunds::apply_for_cheater`].
        write fn apply_for_cheater(refunds: CheaterRefunds) -> Vec<GivenRefund> => |db| db.refunds().apply_for_cheater(&refunds);
        /// See [`Refunds::list`].
        read fn list(scope: RefundScope, limit: i64) -> Vec<Refund> => |db| db.refunds().list(scope, limit);
        /// See [`Refunds::pending_since`].
        read fn pending_since(after_id: i64, limit: i64) -> Vec<PendingRefund> => |db| db.refunds().pending_since(after_id, limit);
        /// See [`Refunds::pending_for`].
        read fn pending_for(victim_id: UserId) -> PendingRefunds => |db| db.refunds().pending_for(victim_id);
        /// See [`Refunds::mark_notified`].
        write fn mark_notified(ids: Vec<i64>, now: i64) -> usize => |db| db.refunds().mark_notified(&ids, now);
    }
}

impl Store {
    /// Server-wide values.
    pub fn meta(&self) -> MetaApi {
        MetaApi { store: self.clone() }
    }

    /// Accounts.
    pub fn users(&self) -> UsersApi {
        UsersApi { store: self.clone() }
    }

    /// MFA recovery codes.
    pub fn mfa(&self) -> MfaApi {
        MfaApi { store: self.clone() }
    }

    /// Login sessions.
    pub fn sessions(&self) -> SessionsApi {
        SessionsApi { store: self.clone() }
    }

    /// Single-use tokens.
    pub fn tokens(&self) -> TokensApi {
        TokensApi { store: self.clone() }
    }

    /// Pending signups.
    pub fn signups(&self) -> SignupsApi {
        SignupsApi { store: self.clone() }
    }

    /// SSO identities.
    pub fn sso(&self) -> SsoApi {
        SsoApi { store: self.clone() }
    }

    /// Rating records.
    pub fn ratings(&self) -> RatingsApi {
        RatingsApi { store: self.clone() }
    }

    /// Finished games (history queries; the commit is [`Store::finish_batch`]).
    pub fn games(&self) -> GamesApi {
        GamesApi { store: self.clone() }
    }

    /// Conduct events and cooldowns.
    pub fn conduct(&self) -> ConductApi {
        ConductApi { store: self.clone() }
    }

    /// Sanctions.
    pub fn sanctions(&self) -> SanctionsApi {
        SanctionsApi { store: self.clone() }
    }

    /// Anomalies.
    pub fn anomalies(&self) -> AnomaliesApi {
        AnomaliesApi { store: self.clone() }
    }

    /// Security events.
    pub fn security(&self) -> SecurityApi {
        SecurityApi { store: self.clone() }
    }

    /// The analysis queue.
    pub fn analysis(&self) -> AnalysisApi {
        AnalysisApi { store: self.clone() }
    }

    /// Integrity records and population statistics.
    pub fn integrity(&self) -> IntegrityApi {
        IntegrityApi { store: self.clone() }
    }

    /// Player reports.
    pub fn reports(&self) -> ReportsApi {
        ReportsApi { store: self.clone() }
    }

    /// Rating refunds.
    pub fn refunds(&self) -> RefundsApi {
        RefundsApi { store: self.clone() }
    }
}
