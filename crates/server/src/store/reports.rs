//! Player reports.

use rusqlite::{Row, params};

use super::db::Db;
use super::error::{Result, StoreError};
use super::games::sql_id;
use super::values::text_enum;
use crate::ids::{GameId, UserId};

text_enum! {
    /// What a report is about.
    pub enum ReportCategory {
        Cheating = "cheating",
        Abuse = "abuse",
        Other = "other",
    }
}

text_enum! {
    /// State of a report.
    pub enum ReportStatus {
        Open = "open",
        Actioned = "actioned",
        Dismissed = "dismissed",
    }
}

/// A new report.
#[derive(Debug, Clone)]
pub struct NewReport {
    pub reporter_id: UserId,
    pub reported_id: UserId,
    /// `None`: not about a particular game.
    pub game_id: Option<GameId>,
    pub category: ReportCategory,
    pub comment: Option<String>,
    /// The reporter's credibility (1 by default).
    pub weight: f64,
    pub at: i64,
}

/// A report, with the players' names.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub id: i64,
    pub reporter_id: UserId,
    pub reported_id: UserId,
    pub game_id: Option<GameId>,
    pub category: ReportCategory,
    pub weight: f64,
    pub status: ReportStatus,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub comment: Option<String>,
    pub reporter_name: String,
    pub reported_name: String,
}

impl Report {
    /// The outcome shown to the reporter: `None` while open.
    pub fn outcome(&self) -> Option<ReportStatus> {
        (self.status != ReportStatus::Open).then_some(self.status)
    }
}

/// Summed weights of the reports against a player ([`Reports::weight_since`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReportWeights {
    pub total: f64,
    /// Of the reports below the low-credibility threshold.
    pub low: f64,
}

/// Reports against a player.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportCounts {
    pub total: i64,
    pub open: i64,
}

const REPORT_SELECT: &str = "SELECT r.id, r.reporter_id, r.reported_id, r.game_id, r.category, r.weight, r.status, \
     r.created_at, r.resolved_at, r.resolved_by, r.comment, a.username, b.username FROM reports r \
     JOIN users a ON a.id = r.reporter_id JOIN users b ON b.id = r.reported_id";

fn to_report(r: &Row<'_>) -> rusqlite::Result<Report> {
    let game: i64 = r.get(3)?;
    Ok(Report {
        id: r.get(0)?,
        reporter_id: r.get(1)?,
        reported_id: r.get(2)?,
        game_id: (game != 0).then_some(game as GameId),
        category: r.get(4)?,
        weight: r.get(5)?,
        status: r.get(6)?,
        created_at: r.get(7)?,
        resolved_at: r.get(8)?,
        resolved_by: r.get(9)?,
        comment: r.get(10)?,
        reporter_name: r.get(11)?,
        reported_name: r.get(12)?,
    })
}

fn check_outcome(outcome: ReportStatus) -> Result<()> {
    if outcome == ReportStatus::Open {
        return Err(StoreError::invalid("a report outcome is actioned or dismissed"));
    }
    Ok(())
}

/// Player reports.
#[derive(Debug, Clone, Copy)]
pub struct Reports<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Reports<'_> {
    /// Files a report; returns its id. Error `duplicate` for a second report of the same game (or
    /// of no game) by the same reporter against the same player.
    pub fn create(&self, r: &NewReport) -> Result<i64> {
        self.db.insert(
            "INSERT INTO reports (reporter_id, reported_id, game_id, category, comment, weight, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                r.reporter_id,
                r.reported_id,
                r.game_id.map_or(0, sql_id),
                r.category,
                r.comment,
                r.weight,
                r.at
            ],
        )
    }

    /// Reports filed by a player at `since` or later.
    pub fn count_by_reporter_since(&self, reporter_id: UserId, since: i64) -> Result<i64> {
        self.db.count(
            "SELECT count(*) FROM reports WHERE reporter_id = ?1 AND created_at >= ?2",
            params![reporter_id, since],
        )
    }

    /// Whether the reporter already reported that player for that game (or for no game).
    pub fn exists(&self, reporter_id: UserId, reported_id: UserId, game_id: Option<GameId>) -> Result<bool> {
        Ok(self
            .db
            .one(
                "SELECT 1 FROM reports WHERE reporter_id = ?1 AND reported_id = ?2 AND game_id = ?3",
                params![reporter_id, reported_id, game_id.map_or(0, sql_id)],
                |_| Ok(()),
            )?
            .is_some())
    }

    /// Open reports, oldest first.
    pub fn list_open(&self, limit: i64) -> Result<Vec<Report>> {
        self.db.all(
            &format!("{REPORT_SELECT} WHERE r.status = 'open' ORDER BY r.created_at, r.id LIMIT ?1"),
            [limit],
            to_report,
        )
    }

    /// Reports against a player, newest first.
    pub fn for_reported(&self, user_id: UserId, limit: i64) -> Result<Vec<Report>> {
        self.db.all(
            &format!(
                "{REPORT_SELECT} WHERE r.reported_id = ?1 ORDER BY r.created_at DESC, r.id DESC LIMIT ?2"
            ),
            params![user_id, limit],
            to_report,
        )
    }

    /// Reports filed by a player, newest first (see [`Report::outcome`]).
    pub fn for_reporter(&self, user_id: UserId, limit: i64) -> Result<Vec<Report>> {
        self.db.all(
            &format!(
                "{REPORT_SELECT} WHERE r.reporter_id = ?1 ORDER BY r.created_at DESC, r.id DESC LIMIT ?2"
            ),
            params![user_id, limit],
            to_report,
        )
    }

    /// Resolves an open report; `true` when it was open. Error `invalid` for `Open`.
    pub fn resolve(&self, id: i64, outcome: ReportStatus, by: Option<&str>, now: i64) -> Result<bool> {
        check_outcome(outcome)?;
        let n = self.db.exec(
            "UPDATE reports SET status = ?1, resolved_at = ?2, resolved_by = ?3 WHERE id = ?4 AND status = 'open'",
            params![outcome, now, by, id],
        )?;
        Ok(n == 1)
    }

    /// Resolves every open report of `category` against the player in one statement; returns
    /// their ids (in no particular order). Error `invalid` for `Open`.
    pub fn resolve_open_for(
        &self,
        reported_id: UserId,
        category: ReportCategory,
        outcome: ReportStatus,
        by: Option<&str>,
        now: i64,
    ) -> Result<Vec<i64>> {
        check_outcome(outcome)?;
        self.db.all(
            "UPDATE reports SET status = ?1, resolved_at = ?2, resolved_by = ?3
             WHERE reported_id = ?4 AND category = ?5 AND status = 'open' RETURNING id",
            params![outcome, now, by, reported_id, category],
            |r| r.get(0),
        )
    }

    /// Summed weights of the reports against the player created at `since` or later: all, and
    /// those below `low_threshold`.
    pub fn weight_since(&self, reported_id: UserId, since: i64, low_threshold: f64) -> Result<ReportWeights> {
        let w = self.db.one(
            "SELECT coalesce(sum(weight), 0.0), coalesce(sum(CASE WHEN weight < ?3 THEN weight END), 0.0)
             FROM reports WHERE reported_id = ?1 AND created_at >= ?2",
            params![reported_id, since, low_threshold],
            |r| Ok(ReportWeights { total: r.get(0)?, low: r.get(1)? }),
        )?;
        Ok(w.unwrap_or_default())
    }

    /// Reports against the player, and those still open.
    pub fn count_for(&self, reported_id: UserId) -> Result<ReportCounts> {
        let c = self.db.one(
            "SELECT count(*), count(CASE WHEN status = 'open' THEN 1 END) FROM reports WHERE reported_id = ?1",
            [reported_id],
            |r| Ok(ReportCounts { total: r.get(0)?, open: r.get(1)? }),
        )?;
        Ok(c.unwrap_or_default())
    }
}
