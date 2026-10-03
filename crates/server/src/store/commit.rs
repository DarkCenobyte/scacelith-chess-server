//! The commit of finished games (`finish_batch`): game rows, both ratings of each rated game,
//! refunds owed at commit and analysis jobs, in one transaction (DESIGN 5.5, 6.5).

use rusqlite::params;
use serde_json::json;

use super::analysis::{self, Priority};
use super::db::Db;
use super::error::{ErrorKind, Result, StoreError};
use super::games::{GameRecord, sql_id, status};
use super::moderation::Source;
use super::ratings::{RatingRecord, SideOutcome};
use super::refunds::{GivenRefund, RefundGame};
use super::values::{pack_u16, pack_u32};
use crate::ids::{GameId, UserId, is_game_id};
use crate::log_warn;

/// A report flags the reported player's next games for 30 days.
const REPORT_SIGNAL_MS: i64 = 30 * 86_400_000;

/// A report flags the reported player's games only from a credible reporter (the stored weight
/// reaches the low-credibility threshold of the anti-cheat's report rules).
const REPORT_SIGNAL_MIN_WEIGHT: f64 = 0.5;

/// One side's rating after a committed game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatingChange {
    pub before: i64,
    pub after: i64,
    /// Games played in the category, this one included.
    pub games: i64,
    pub provisional: bool,
}

/// Both sides' ratings after a committed game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitRatings {
    pub white: RatingChange,
    pub black: RatingChange,
}

/// Why a finished rated game was not queued for engine analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// Not drawn by `ANALYSIS_SAMPLE_RATE`.
    Sample,
    /// `ANALYSIS_QUEUE_MAX` ordinary jobs already wait.
    Backlog,
    /// A player already has `SIGNAL_JOBS_PER_PLAYER` flagged games waiting.
    Player,
}

impl SkipReason {
    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Sample => "sample",
            SkipReason::Backlog => "backlog",
            SkipReason::Player => "player",
        }
    }
}

/// The outcome of one game of a batch, in record order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEntry {
    pub game_id: GameId,
    /// The game was already stored (a re-commit after a restart): nothing was written, `ratings`
    /// are the stored changes with the players' current game counts.
    pub duplicate: bool,
    /// `None` when the game was not rated. `after` excludes a refund given at the same commit.
    pub ratings: Option<CommitRatings>,
    pub analysis_skipped: Option<SkipReason>,
    /// Games whose waiting signal job this game took over.
    pub analysis_displaced: Vec<GameId>,
}

/// A refund given at the commit of a game whose opponent is a banned cheater.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRefund {
    pub refund: GivenRefund,
    pub cheater_id: UserId,
    pub sanction_id: i64,
}

/// What a batch did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitBatch {
    pub entries: Vec<CommitEntry>,
    /// Refunds given (to log once committed).
    pub refunds: Vec<CommitRefund>,
}

/// State shared by the games of one batch.
struct Batch {
    now: i64,
    /// Ordinary jobs waiting, counted once per batch (the write lock is held).
    ordinary: Option<i64>,
    refunds: Vec<CommitRefund>,
}

/// Commits `records` in one savepoint (the whole batch or nothing). An error caused by one record
/// carries its game id ([`StoreError::game_id`]): `invalid_record`, `foreign_key` (an unknown
/// player), `invalid`.
pub(crate) fn finish_batch(db: &Db<'_>, records: &[GameRecord], now: i64) -> Result<CommitBatch> {
    if records.is_empty() {
        return Ok(CommitBatch::default());
    }
    db.transaction(|db| {
        let mut batch = Batch { now, ordinary: None, refunds: Vec::new() };
        let mut entries = Vec::with_capacity(records.len());
        for r in records {
            let entry = commit_game(db, r, &mut batch).map_err(|e| match e.kind() {
                ErrorKind::InvalidRecord
                | ErrorKind::ForeignKey
                | ErrorKind::Invalid
                | ErrorKind::Duplicate
                    if e.game_id().is_none() =>
                {
                    e.with_game(r.id)
                }
                _ => e,
            })?;
            entries.push(entry);
        }
        Ok(CommitBatch { entries, refunds: batch.refunds })
    })
}

fn check_record(r: &GameRecord) -> Result<()> {
    let bad = |why: &str| {
        StoreError::new(ErrorKind::InvalidRecord, format!("game {}: invalid {why}", r.id)).with_game(r.id)
    };
    if !is_game_id(r.id) {
        return Err(bad("id"));
    }
    if !(status::WHITE_WINS..=status::ABORTED).contains(&r.status) {
        return Err(bad("status"));
    }
    if r.category.is_empty() {
        return Err(bad("category"));
    }
    Ok(())
}

/// The record stored after a game: the rating function's, with the unrated-phase sums cleared
/// once rated.
fn next_record(side: &SideOutcome) -> RatingRecord {
    let mut rec = side.record;
    if rec.rated {
        rec.unrated_games = 0;
        rec.unrated_opponents = 0;
        rec.unrated_half_points = 0;
    }
    rec
}

fn commit_game(db: &Db<'_>, r: &GameRecord, batch: &mut Batch) -> Result<CommitEntry> {
    check_record(r)?;
    let ctx = db.ctx();
    let now = batch.now;
    type Stored = (UserId, UserId, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>);
    let existing: Option<Stored> = db.one(
        "SELECT white_id, black_id, category, white_before, white_after, black_before, black_after FROM games WHERE id = ?1",
        [sql_id(r.id)],
        |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?, x.get(6)?)),
    )?;
    if let Some((white_id, black_id, category, wb, wa, bb, ba)) = existing {
        if white_id != r.white_id || black_id != r.black_id {
            log_warn!(db.logger(), "game id already stored for other players: this game is not stored", {
                "gameId": r.id,
            });
        }
        let ratings = match (wb, wa, bb, ba) {
            (Some(wb), Some(wa), Some(bb), Some(ba)) => {
                let w = db.ratings().get(white_id, &category)?;
                let b = db.ratings().get(black_id, &category)?;
                let pg = ctx.provisional_games;
                Some(CommitRatings {
                    white: RatingChange {
                        before: wb,
                        after: wa,
                        games: w.games,
                        provisional: w.is_provisional(pg),
                    },
                    black: RatingChange {
                        before: bb,
                        after: ba,
                        games: b.games,
                        provisional: b.is_provisional(pg),
                    },
                })
            }
            _ => None,
        };
        return Ok(CommitEntry {
            game_id: r.id,
            duplicate: true,
            ratings,
            analysis_skipped: None,
            analysis_displaced: Vec::new(),
        });
    }

    let rate = r.rated && r.status != status::ABORTED && r.category != "custom";
    let mut changes = None;
    let mut k = (None, None);
    if rate {
        let Some(rating_fn) = ctx.rating.as_ref() else {
            return Err(StoreError::new(
                ErrorKind::NoRatingFunction,
                "the store needs a rating function (StoreOptions::rating) to commit rated games",
            ));
        };
        let score = match r.status {
            status::WHITE_WINS => 1.0,
            status::BLACK_WINS => 0.0,
            _ => 0.5,
        };
        let w = db.ratings().get(r.white_id, &r.category)?;
        let b = db.ratings().get(r.black_id, &r.category)?;
        let res = rating_fn(&w, &b, score);
        let w_rec = next_record(&res.white);
        let b_rec = next_record(&res.black);
        // White first: a player on both sides keeps Black's record.
        db.ratings().put(r.white_id, &r.category, &w_rec, now)?;
        db.ratings().put(r.black_id, &r.category, &b_rec, now)?;
        let pg = ctx.provisional_games;
        changes = Some(CommitRatings {
            white: RatingChange {
                before: res.white.before,
                after: w_rec.rating,
                games: w_rec.games,
                provisional: w_rec.is_provisional(pg),
            },
            black: RatingChange {
                before: res.black.before,
                after: b_rec.rating,
                games: b_rec.games,
                provisional: b_rec.is_provisional(pg),
            },
        });
        k = (res.white.k, res.black.k);
    }

    let plies = r.moves.len() as i64;
    let started_at = r.started_at.or(r.ended_at).unwrap_or(now);
    db.exec(
        "INSERT INTO games (id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, black_name,
         white_rating, black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after,
         black_before, black_after, white_k, black_k, rematch_of, flags, moves, spent, clocks)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
         ?23, ?24, ?25, ?26, ?27)",
        params![
            sql_id(r.id),
            r.category,
            r.rated,
            r.base_ms,
            r.inc_ms,
            r.white_id,
            r.black_id,
            r.white_name,
            r.black_name,
            r.white_rating,
            r.black_rating,
            started_at,
            r.ended_at.unwrap_or(now),
            r.status,
            r.reason,
            plies,
            changes.map(|c| c.white.before),
            changes.map(|c| c.white.after),
            changes.map(|c| c.black.before),
            changes.map(|c| c.black.after),
            k.0,
            k.1,
            r.rematch_of.filter(|&g| g != 0).map(sql_id),
            r.flags,
            pack_u16(&r.moves),
            r.spent_ms.as_deref().map(pack_u32),
            r.clock_ms.as_deref().map(pack_u32),
        ],
    )?;
    let mut entry = CommitEntry {
        game_id: r.id,
        duplicate: false,
        ratings: changes,
        analysis_skipped: None,
        analysis_displaced: Vec::new(),
    };
    if let Some(changes) = changes {
        refund_at_commit(db, r, &changes, k, batch)?;
        if plies >= ctx.analysis_min_plies {
            queue_analysis(db, r, started_at, batch, &mut entry)?;
        }
    }
    Ok(entry)
}

/// Refunds a rated game recorded while the opponent of the player who lost points is a confirmed
/// cheater under an active ban that refunds (an automatic ban of a certain cheat,
/// `certain_cheat:<kind>`, or a moderator's confirm, `confirmed: <reason>`), whose refund window
/// (`RATING_REFUND_DAYS` before its start) covers the game's end: the games in progress at the ban
/// or still on their way to the database, which the ban's own refunds could not see.
fn refund_at_commit(
    db: &Db<'_>,
    r: &GameRecord,
    changes: &CommitRatings,
    k: (Option<i64>, Option<i64>),
    batch: &mut Batch,
) -> Result<()> {
    let window = db.ctx().refund_window_ms;
    if window <= 0 {
        return Ok(());
    }
    let now = batch.now;
    let ended_at = r.ended_at.unwrap_or(now);
    let sides = [(r.white_id, r.black_id, changes.white, k.0), (r.black_id, r.white_id, changes.black, k.1)];
    for (victim, cheater_id, change, kf) in sides {
        let points = change.before - change.after;
        if points <= 0 || kf == Some(0) {
            continue;
        }
        let ban: Option<i64> = db.one(
            "SELECT s.id FROM player_integrity pi JOIN sanctions s ON s.user_id = pi.user_id
             WHERE pi.user_id = ?1 AND pi.level = 'confirmed' AND s.kind = 'ban' AND s.lifted_at IS NULL
             AND s.starts_at <= ?2 AND (s.ends_at IS NULL OR s.ends_at > ?2) AND s.starts_at <= ?3
             AND ((s.source = 'auto' AND instr(s.reason, 'certain_cheat:') = 1)
                  OR (s.source = 'moderator' AND instr(s.reason, 'confirmed: ') = 1))
             ORDER BY s.starts_at, s.id LIMIT 1",
            params![cheater_id, now, ended_at + window],
            |x| x.get(0),
        )?;
        let Some(ban) = ban else { continue };
        let g = RefundGame { id: r.id, victim, category: &r.category, points, ended_at };
        let Some(given) = db.refunds().give(&g, cheater_id, now, Some(ban), Source::Auto, None)? else {
            continue;
        };
        let detail = json!({
            "refundId": given.id,
            "gameId": r.id,
            "cheaterId": cheater_id,
            "category": r.category,
            "points": points,
            "source": "auto",
            "sanctionId": ban,
            "by": null,
        });
        db.exec(
            "INSERT INTO security_events (kind, user_id, ip, at, detail) VALUES ('rating_refund', ?1, NULL, ?2, ?3)",
            params![victim, now, detail.to_string()],
        )?;
        batch.refunds.push(CommitRefund { refund: given, cheater_id, sanction_id: ban });
    }
    Ok(())
}

/// The analysis queue policy of a finished rated game (DESIGN 6.5): a suspicion signal (a non-info
/// anomaly of this game, either player's integrity level above `none`, a credible open report
/// against either player in the last 30 days) queues it as `signal`, under the cap of
/// `SIGNAL_JOBS_PER_PLAYER` waiting signal jobs per player: past it, a game with an anomaly of its
/// own takes over the oldest waiting job without one of each capped player, any other flagged game
/// is skipped. An ordinary game is drawn with `ANALYSIS_SAMPLE_RATE` and queued only while fewer
/// than `ANALYSIS_QUEUE_MAX` ordinary jobs wait.
fn queue_analysis(
    db: &Db<'_>,
    r: &GameRecord,
    started_at: i64,
    batch: &mut Batch,
    entry: &mut CommitEntry,
) -> Result<()> {
    let ctx = db.ctx();
    let now = batch.now;
    let (own, player): (bool, bool) = db
        .one(
            "SELECT EXISTS (SELECT 1 FROM anomalies WHERE user_id IN (?1, ?2) AND at >= ?4 AND game_id = ?5
             AND severity <> 'info') AS own,
             EXISTS (SELECT 1 FROM player_integrity WHERE user_id IN (?1, ?2) AND level <> 'none')
             OR EXISTS (SELECT 1 FROM reports WHERE reported_id IN (?1, ?2) AND created_at >= ?3 AND status = 'open'
                 AND category <> 'abuse' AND weight >= ?6) AS player",
            params![r.white_id, r.black_id, now - REPORT_SIGNAL_MS, started_at, sql_id(r.id), REPORT_SIGNAL_MIN_WEIGHT],
            |x| Ok((x.get(0)?, x.get(1)?)),
        )?
        .unwrap_or_default();
    let insert = |priority: Priority| {
        db.exec(
            "INSERT OR IGNORE INTO analysis_jobs (game_id, queued_at, priority, white_id, black_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![sql_id(r.id), now, priority as i64, r.white_id, r.black_id],
        )
    };
    if own || player {
        // Never sampled out nor capped by ANALYSIS_QUEUE_MAX: ordinary jobs are the random sample
        // of the population statistics.
        let mut capped = Vec::new();
        for p in [r.white_id, r.black_id] {
            if analysis::signal_cap_reached(db, p)? {
                capped.push(p);
            }
        }
        if !capped.is_empty() {
            if !own {
                entry.analysis_skipped = Some(SkipReason::Player);
                return Ok(());
            }
            // Both players capped: the job taken from the first may be one of the second's too.
            // Nothing is removed before a job is found for each.
            let mut out: Vec<analysis::SignalJob> = Vec::new();
            for p in capped {
                if out.iter().any(|v| v.white_id == Some(p) || v.black_id == Some(p)) {
                    continue;
                }
                match analysis::displaceable_signal_job(db, p)? {
                    Some(v) => out.push(v),
                    None => {
                        entry.analysis_skipped = Some(SkipReason::Player);
                        return Ok(());
                    }
                }
            }
            for v in &out {
                db.exec("DELETE FROM analysis_jobs WHERE game_id = ?1", [sql_id(v.game_id)])?;
            }
            entry.analysis_displaced = out.iter().map(|v| v.game_id).collect();
        }
        insert(Priority::Signal)?;
        return Ok(());
    }
    if ctx.analysis_sample_rate < 1.0 && (ctx.random)() >= ctx.analysis_sample_rate {
        entry.analysis_skipped = Some(SkipReason::Sample);
        return Ok(());
    }
    let ordinary = match batch.ordinary {
        Some(n) => n,
        None => db.count(
            "SELECT count(*) FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority = 0 LIMIT ?1)",
            [ctx.analysis_queue_max],
        )?,
    };
    if ordinary >= ctx.analysis_queue_max {
        batch.ordinary = Some(ordinary);
        entry.analysis_skipped = Some(SkipReason::Backlog);
        return Ok(());
    }
    insert(Priority::Ordinary)?;
    batch.ordinary = Some(ordinary + 1);
    Ok(())
}
