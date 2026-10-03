//! The engine analysis queue (DESIGN 6.5): jobs claimed by priority then age, every fourth claim
//! reserved for the oldest ordinary job, stale claims re-queued, at most three attempts.

use rusqlite::{Row, params};
use serde_json::Value;

use super::db::Db;
use super::error::{Result, StoreError};
use super::games::{sql_id, status};
use super::metrics::SIGNAL_JOBS_PER_PLAYER;
use super::values::{json_text, json_value, text_enum, truncate_utf16};
use crate::ids::{GameId, UserId};

/// A job is claimed at most this many times.
pub const MAX_ATTEMPTS: i64 = 3;

/// A running job not renewed for this long is re-queued by the next claim (its worker vanished).
pub const STALE_MS: i64 = 10 * 60_000;

/// Every `ORDINARY_SHARE`-th claim takes the oldest ordinary job first (when one waits): the
/// ordinary games keep that share of the engine time however many prioritized games arrive.
pub const ORDINARY_SHARE: u64 = 4;

/// [`Analysis::backlog`] counts at most this many jobs per tier.
pub const BACKLOG_COUNT_MAX: i64 = 100_000;

/// Longest stored error text, in UTF-16 units.
const ERROR_MAX_UNITS: usize = 2000;

/// Priority of a job: the highest waiting priority is analysed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// The random sample feeding the population statistics.
    Ordinary = 0,
    /// A suspicion signal at the end of the game.
    Signal = 1,
    /// A player report.
    Report = 2,
    /// A moderator request.
    Manual = 3,
}

impl Priority {
    /// The priority of a stored value (unknown values read as ordinary).
    pub fn from_i64(v: i64) -> Priority {
        match v {
            1 => Priority::Signal,
            2 => Priority::Report,
            3 => Priority::Manual,
            _ => Priority::Ordinary,
        }
    }

    /// `ordinary`, `signal`, `report` or `manual`.
    pub fn parse(s: &str) -> Option<Priority> {
        match s {
            "ordinary" => Some(Priority::Ordinary),
            "signal" => Some(Priority::Signal),
            "report" => Some(Priority::Report),
            "manual" => Some(Priority::Manual),
            _ => None,
        }
    }
}

text_enum! {
    /// State of a job.
    pub enum JobStatus {
        Queued = "queued",
        Running = "running",
        Done = "done",
        Failed = "failed",
    }
}

/// A claimed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub game_id: GameId,
    pub priority: Priority,
    pub attempts: i64,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub worker: Option<String>,
}

/// The job of a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub status: JobStatus,
    pub priority: Priority,
    pub attempts: i64,
    pub queued_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

/// Jobs waiting, each tier counted up to [`BACKLOG_COUNT_MAX`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backlog {
    pub ordinary: i64,
    /// Every job above ordinary.
    pub priority: i64,
}

/// Jobs by status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub queued: i64,
    pub running: i64,
    pub done: i64,
    pub failed: i64,
}

/// A player's side in a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    White,
    Black,
}

impl Color {
    /// `white` or `black`.
    pub fn as_str(self) -> &'static str {
        match self {
            Color::White => "white",
            Color::Black => "black",
        }
    }
}

/// A job of a player's game ([`Analysis::for_user`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerJob {
    pub game_id: GameId,
    pub status: JobStatus,
    pub attempts: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub features: Option<Value>,
    pub category: String,
    pub color: Color,
    pub ended_at: i64,
    pub ply_count: i64,
}

/// A waiting signal job (the queue policy of a finished game).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SignalJob {
    pub game_id: GameId,
    pub white_id: Option<UserId>,
    pub black_id: Option<UserId>,
}

/// A player's analysis jobs, newest game first (`?1` the player, `?2` the limit). The done-only
/// filter is written `+a.status` (no index term): SQLite walks the player's games newest first
/// through `games_white` / `games_black` with a primary-key probe of `analysis_jobs` each, stopped
/// at the limit, instead of reading every done job of the server and sorting them (tested).
pub const ANALYSED_FOR_USER_SQL: &str =
    "SELECT a.game_id, a.status, a.attempts, a.finished_at, a.error, a.features, g.category,
    g.white_id, g.ended_at, g.ply_count FROM games g JOIN analysis_jobs a ON a.game_id = g.id WHERE g.id IN (
    SELECT id FROM games WHERE white_id = ?1 UNION SELECT id FROM games WHERE black_id = ?1)
    AND +a.status = 'done' ORDER BY g.id DESC LIMIT ?2";

const JOBS_FOR_USER_SQL: &str =
    "SELECT a.game_id, a.status, a.attempts, a.finished_at, a.error, a.features, g.category,
    g.white_id, g.ended_at, g.ply_count FROM games g JOIN analysis_jobs a ON a.game_id = g.id WHERE g.id IN (
    SELECT id FROM games WHERE white_id = ?1 UNION SELECT id FROM games WHERE black_id = ?1)
    ORDER BY g.id DESC LIMIT ?2";

const CLAIM_ANY_SQL: &str = "UPDATE analysis_jobs SET status = 'running', worker = ?1, started_at = ?2,
    attempts = attempts + 1 WHERE game_id IN (SELECT game_id FROM analysis_jobs WHERE status = 'queued'
    ORDER BY priority DESC, queued_at, game_id LIMIT ?3)
    RETURNING game_id, priority, attempts, queued_at, started_at, worker";

const CLAIM_ORDINARY_SQL: &str = "UPDATE analysis_jobs SET status = 'running', worker = ?1, started_at = ?2,
    attempts = attempts + 1 WHERE game_id IN (SELECT game_id FROM analysis_jobs WHERE status = 'queued' AND priority = 0
    ORDER BY queued_at, game_id LIMIT ?3)
    RETURNING game_id, priority, attempts, queued_at, started_at, worker";

fn to_claimed(r: &Row<'_>) -> rusqlite::Result<ClaimedJob> {
    Ok(ClaimedJob {
        game_id: r.get::<_, i64>(0)? as GameId,
        priority: Priority::from_i64(r.get(1)?),
        attempts: r.get(2)?,
        queued_at: r.get(3)?,
        started_at: r.get(4)?,
        worker: r.get(5)?,
    })
}

/// Whether the player already has `SIGNAL_JOBS_PER_PLAYER` signal jobs waiting (two range scans of
/// the partial indexes; the literals `'queued'` and `1` match their WHERE clause).
pub(crate) fn signal_cap_reached(db: &Db<'_>, user_id: UserId) -> Result<bool> {
    let n = db.count(
        "SELECT count(*) FROM (SELECT 1 FROM analysis_jobs WHERE white_id = ?1 AND status = 'queued' AND priority = 1
         UNION ALL SELECT 1 FROM analysis_jobs WHERE black_id = ?1 AND status = 'queued' AND priority = 1 LIMIT ?2)",
        params![user_id, SIGNAL_JOBS_PER_PLAYER],
    )?;
    Ok(n >= SIGNAL_JOBS_PER_PLAYER)
}

/// The oldest waiting signal job of the player whose game has no non-info anomaly of its own, if
/// any: the job a game with an anomaly of its own displaces when the player's cap is reached.
pub(crate) fn displaceable_signal_job(db: &Db<'_>, user_id: UserId) -> Result<Option<SignalJob>> {
    db.one(
        "SELECT j.game_id, j.white_id, j.black_id FROM analysis_jobs j JOIN games g ON g.id = j.game_id
         WHERE j.game_id IN (SELECT game_id FROM analysis_jobs WHERE white_id = ?1 AND status = 'queued' AND priority = 1
             UNION ALL SELECT game_id FROM analysis_jobs WHERE black_id = ?1 AND status = 'queued' AND priority = 1)
         AND NOT EXISTS (SELECT 1 FROM anomalies a WHERE a.user_id IN (j.white_id, j.black_id) AND a.at >= g.started_at
             AND a.game_id = j.game_id AND a.severity <> 'info')
         ORDER BY j.queued_at, j.game_id LIMIT 1",
        [user_id],
        |r| Ok(SignalJob { game_id: r.get::<_, i64>(0)? as GameId, white_id: r.get(1)?, black_id: r.get(2)? }),
    )
}

/// The analysis queue.
#[derive(Debug, Clone, Copy)]
pub struct Analysis<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Analysis<'_> {
    /// Claims up to `limit` queued jobs for `worker`: the highest priority, then the oldest
    /// first, except that every [`ORDINARY_SHARE`]-th claim of the store takes the oldest ordinary
    /// job first when one waits. Running jobs claimed more than [`STALE_MS`] ago are re-queued (or
    /// failed at [`MAX_ATTEMPTS`]) first. Sorted by priority (highest first), age, game id.
    pub fn next(&self, limit: i64, worker: Option<&str>, now: i64) -> Result<Vec<ClaimedJob>> {
        let n = limit.max(0);
        let claims = &self.db.ctx().claims;
        let mut rows = self.db.transaction(|db| {
            db.exec(
                "UPDATE analysis_jobs SET status = CASE WHEN attempts >= ?1 THEN 'failed' ELSE 'queued' END, worker = NULL,
                 error = 'stale: worker vanished', finished_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE NULL END
                 WHERE status = 'running' AND started_at < ?3",
                params![MAX_ATTEMPTS, now, now - STALE_MS],
            )?;
            // Claims are numbered claims .. claims + n - 1; those numbered ORDINARY_SHARE - 1
            // modulo ORDINARY_SHARE are the reserved ones.
            let c = claims.load(std::sync::atomic::Ordering::Relaxed);
            let reserved = (c + n as u64) / ORDINARY_SHARE - c / ORDINARY_SHARE;
            let mut out = if reserved > 0 {
                db.all(CLAIM_ORDINARY_SQL, params![worker, now, reserved as i64], to_claimed)?
            } else {
                Vec::new()
            };
            let rest = n - out.len() as i64;
            if rest > 0 {
                out.extend(db.all(CLAIM_ANY_SQL, params![worker, now, rest], to_claimed)?);
            }
            Ok::<_, StoreError>(out)
        })?;
        claims.fetch_add(rows.len() as u64, std::sync::atomic::Ordering::Relaxed);
        rows.sort_by(|a, b| {
            b.priority.cmp(&a.priority).then(a.queued_at.cmp(&b.queued_at)).then(a.game_id.cmp(&b.game_id))
        });
        Ok(rows)
    }

    /// Stores the features of an analysed game (any state); `true` when the job exists.
    pub fn complete(&self, game_id: GameId, features: Option<&Value>, now: i64) -> Result<bool> {
        let n = self.db.exec(
            "UPDATE analysis_jobs SET status = 'done', features = ?1, finished_at = ?2, error = NULL, worker = NULL
             WHERE game_id = ?3",
            params![json_text(features), now, sql_id(game_id)],
        )?;
        Ok(n == 1)
    }

    /// Heartbeat of a job being analysed: its claim time moves to `now`, so that only the jobs of a
    /// worker that stopped renewing them are re-queued. `true` while it runs for that worker.
    pub fn touch(&self, game_id: GameId, worker: Option<&str>, now: i64) -> Result<bool> {
        let n = self.db.exec(
            "UPDATE analysis_jobs SET started_at = ?1 WHERE game_id = ?2 AND status = 'running' AND worker IS ?3",
            params![now, sql_id(game_id), worker],
        )?;
        Ok(n == 1)
    }

    /// Re-queues the job, or fails it once tried [`MAX_ATTEMPTS`] times; returns the new status,
    /// `None` without a job. The error text is kept to 2000 UTF-16 units.
    pub fn fail(&self, game_id: GameId, error: Option<&str>, now: i64) -> Result<Option<JobStatus>> {
        self.db.transaction(|db| {
            let Some(attempts) =
                db.one("SELECT attempts FROM analysis_jobs WHERE game_id = ?1", [sql_id(game_id)], |r| r.get::<_, i64>(0))?
            else {
                return Ok(None);
            };
            let status = if attempts >= MAX_ATTEMPTS { JobStatus::Failed } else { JobStatus::Queued };
            db.exec(
                "UPDATE analysis_jobs SET status = ?1, error = ?2, worker = NULL, finished_at = ?3 WHERE game_id = ?4",
                params![
                    status,
                    error.map(|e| truncate_utf16(e, ERROR_MAX_UNITS)),
                    (status == JobStatus::Failed).then_some(now),
                    sql_id(game_id),
                ],
            )?;
            Ok(Some(status))
        })
    }

    /// The job of a game.
    pub fn job(&self, game_id: GameId) -> Result<Option<Job>> {
        self.db.one(
            "SELECT status, priority, attempts, queued_at, finished_at, error FROM analysis_jobs WHERE game_id = ?1",
            [sql_id(game_id)],
            |r| {
                Ok(Job {
                    status: r.get(0)?,
                    priority: Priority::from_i64(r.get(1)?),
                    attempts: r.get(2)?,
                    queued_at: r.get(3)?,
                    finished_at: r.get(4)?,
                    error: r.get(5)?,
                })
            },
        )
    }

    /// Moderator request: (re-)analyses any stored game before every other job (priority
    /// `manual`), whatever its job's state. Error `foreign_key` for an unknown game.
    pub fn enqueue(&self, game_id: GameId, now: i64) -> Result<()> {
        self.db.exec(
            "INSERT INTO analysis_jobs (game_id, queued_at, priority, white_id, black_id)
             VALUES (?1, ?2, ?3, (SELECT white_id FROM games WHERE id = ?1), (SELECT black_id FROM games WHERE id = ?1))
             ON CONFLICT (game_id) DO UPDATE SET status = 'queued', attempts = 0, worker = NULL, started_at = NULL,
             finished_at = NULL, error = NULL, queued_at = excluded.queued_at, priority = excluded.priority,
             white_id = excluded.white_id, black_id = excluded.black_id",
            params![sql_id(game_id), now, Priority::Manual as i64],
        )?;
        Ok(())
    }

    /// Makes sure a game the automatic policy analyses (rated, official category, played out, at
    /// least `ANALYSIS_MIN_PLIES` plies) gets analysed with at least `priority`: queued when it
    /// has no job, its priority raised while it waits, a failed job queued again; a running or
    /// done job is left alone. `Signal` is refused while either player is at the signal cap.
    /// `true` when a job was queued or changed. Error `invalid` for [`Priority::Ordinary`] (it
    /// would bypass `ANALYSIS_QUEUE_MAX`).
    pub fn request(&self, game_id: GameId, priority: Priority, now: i64) -> Result<bool> {
        if priority == Priority::Ordinary {
            return Err(StoreError::invalid("analysis request: ordinary priority"));
        }
        let min_plies = self.db.ctx().analysis_min_plies;
        self.db.transaction(|db| {
            if priority == Priority::Signal {
                let players: Option<(UserId, UserId)> = db.one(
                    "SELECT white_id, black_id FROM games WHERE id = ?1",
                    [sql_id(game_id)],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let Some((w, b)) = players else { return Ok(false) };
                if signal_cap_reached(db, w)? || signal_cap_reached(db, b)? {
                    return Ok(false);
                }
            }
            let n = db.exec(
                "INSERT INTO analysis_jobs (game_id, queued_at, priority, white_id, black_id)
                 SELECT id, ?2, ?3, white_id, black_id FROM games WHERE id = ?1 AND rated = 1 AND category <> 'custom'
                     AND status IN (?5, ?6, ?7) AND ply_count >= ?4
                 ON CONFLICT (game_id) DO UPDATE SET priority = max(priority, excluded.priority), status = 'queued',
                     attempts = CASE WHEN status = 'failed' THEN 0 ELSE attempts END,
                     error = CASE WHEN status = 'failed' THEN NULL ELSE error END, finished_at = NULL,
                     white_id = excluded.white_id, black_id = excluded.black_id
                 WHERE status = 'failed' OR (status = 'queued' AND priority < excluded.priority)",
                params![
                    sql_id(game_id),
                    now,
                    priority as i64,
                    min_plies,
                    status::WHITE_WINS,
                    status::BLACK_WINS,
                    status::DRAW
                ],
            )?;
            Ok(n == 1)
        })
    }

    /// Jobs waiting.
    pub fn backlog(&self) -> Result<Backlog> {
        let b = self.db.one(
            "SELECT (SELECT count(*) FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority = 0 LIMIT ?1)),
             (SELECT count(*) FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority > 0 LIMIT ?1))",
            [BACKLOG_COUNT_MAX],
            |r| Ok(Backlog { ordinary: r.get(0)?, priority: r.get(1)? }),
        )?;
        Ok(b.unwrap_or_default())
    }

    /// The player's jobs, newest game first; with `done_only`, the completed analyses only (the
    /// scoring's window of their latest analysed games).
    pub fn for_user(&self, user_id: UserId, limit: i64, done_only: bool) -> Result<Vec<PlayerJob>> {
        let sql = if done_only { ANALYSED_FOR_USER_SQL } else { JOBS_FOR_USER_SQL };
        self.db.all(sql, params![user_id, limit], |r| {
            let white: UserId = r.get(7)?;
            Ok(PlayerJob {
                game_id: r.get::<_, i64>(0)? as GameId,
                status: r.get(1)?,
                attempts: r.get(2)?,
                finished_at: r.get(3)?,
                error: r.get(4)?,
                features: json_value(r.get(5)?),
                category: r.get(6)?,
                color: if white == user_id { Color::White } else { Color::Black },
                ended_at: r.get(8)?,
                ply_count: r.get(9)?,
            })
        })
    }

    /// Jobs by status.
    pub fn stats(&self) -> Result<QueueStats> {
        let rows: Vec<(JobStatus, i64)> =
            self.db.all("SELECT status, count(*) FROM analysis_jobs GROUP BY status", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let mut out = QueueStats::default();
        for (s, n) in rows {
            match s {
                JobStatus::Queued => out.queued = n,
                JobStatus::Running => out.running = n,
                JobStatus::Done => out.done = n,
                JobStatus::Failed => out.failed = n,
            }
        }
        Ok(out)
    }
}
