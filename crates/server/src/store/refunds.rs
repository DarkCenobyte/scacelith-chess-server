//! Rating refunds: the points the opponents of a banned cheater lost to them, given back.

use rusqlite::{Row, params};

use super::db::Db;
use super::error::Result;
use super::games::sql_id;
use super::moderation::Source;
use crate::ids::{GameId, UserId};

/// A refund just given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenRefund {
    pub id: i64,
    pub game_id: GameId,
    pub victim_id: UserId,
    pub category: String,
    pub points: i64,
    pub ended_at: i64,
}

/// A stored refund, with the players' names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refund {
    pub id: i64,
    pub game_id: GameId,
    pub victim_id: UserId,
    pub cheater_id: UserId,
    pub category: String,
    pub points: i64,
    pub created_at: i64,
    pub sanction_id: Option<i64>,
    pub source: Source,
    pub created_by: Option<String>,
    pub notified_at: Option<i64>,
    pub victim_name: String,
    pub cheater_name: String,
}

/// A refund not notified yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRefund {
    pub id: i64,
    pub victim_id: UserId,
    pub points: i64,
}

/// A victim's refunds not notified yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingRefunds {
    pub ids: Vec<i64>,
    pub points: i64,
}

/// Which refunds [`Refunds::list`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundScope {
    /// The refunds of a cheater's games.
    Cheater(UserId),
    /// The refunds a player received.
    Victim(UserId),
    All,
}

/// What [`Refunds::apply_for_cheater`] refunds.
#[derive(Debug, Clone)]
pub struct CheaterRefunds {
    pub cheater_id: UserId,
    /// Games ended at this time or later.
    pub since: i64,
    pub now: i64,
    /// The ban that triggered the refunds.
    pub sanction_id: Option<i64>,
    pub source: Source,
    /// The moderator.
    pub by: Option<String>,
}

/// One game a refund may be owed for.
pub(crate) struct RefundGame<'s> {
    pub id: GameId,
    pub victim: UserId,
    pub category: &'s str,
    pub points: i64,
    pub ended_at: i64,
}

/// Rating refunds.
#[derive(Debug, Clone, Copy)]
pub struct Refunds<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Refunds<'_> {
    /// Gives the victim back the points lost in a game against the cheater, on their current
    /// record of its category (the peak rises with it). Nothing when the game already has its
    /// refund for that victim or the victim has no record.
    pub(crate) fn give(
        &self,
        g: &RefundGame<'_>,
        cheater_id: UserId,
        now: i64,
        sanction_id: Option<i64>,
        source: Source,
        by: Option<&str>,
    ) -> Result<Option<GivenRefund>> {
        let Some((rating, peak)) = self.db.one(
            "SELECT rating, peak FROM ratings WHERE user_id = ?1 AND category = ?2",
            params![g.victim, g.category],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?
        else {
            return Ok(None);
        };
        let inserted = self.db.exec(
            "INSERT INTO rating_refunds (game_id, victim_id, cheater_id, category, points, created_at, sanction_id,
             source, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT (game_id, victim_id) DO NOTHING",
            params![
                sql_id(g.id),
                g.victim,
                cheater_id,
                g.category,
                g.points,
                now,
                sanction_id.filter(|&s| s != 0),
                source,
                by
            ],
        )?;
        if inserted != 1 {
            return Ok(None);
        }
        let id = self.db.connection().last_insert_rowid();
        let new_rating = rating + g.points;
        self.db.exec(
            "UPDATE ratings SET rating = ?1, peak = ?2, updated_at = ?3 WHERE user_id = ?4 AND category = ?5",
            params![new_rating, peak.max(new_rating), now, g.victim, g.category],
        )?;
        Ok(Some(GivenRefund {
            id,
            game_id: g.id,
            victim_id: g.victim,
            category: g.category.to_string(),
            points: g.points,
            ended_at: g.ended_at,
        }))
    }

    /// Refunds the victims of a banned cheater, in one transaction: the cheater's rated games
    /// ended at `since` or later in which the opponent's rating fell by the K formula (`k` not 0),
    /// at most one refund per game and victim (a second call skips those already given).
    pub fn apply_for_cheater(&self, c: &CheaterRefunds) -> Result<Vec<GivenRefund>> {
        self.db.transaction(|db| {
            type Row = (i64, String, i64, UserId, i64, Option<i64>);
            let games: Vec<Row> = db.all(
                "SELECT id, category, ended_at, black_id AS victim, black_before - black_after AS points, black_k AS k
                 FROM games WHERE white_id = ?1 AND ended_at >= ?2 AND rated = 1 AND black_before IS NOT NULL
                 UNION ALL SELECT id, category, ended_at, white_id, white_before - white_after, white_k
                 FROM games WHERE black_id = ?1 AND ended_at >= ?2 AND rated = 1 AND white_before IS NOT NULL
                 ORDER BY 1",
                params![c.cheater_id, c.since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
            let mut out = Vec::new();
            for (id, category, ended_at, victim, points, k) in games {
                if points <= 0 || k == Some(0) || victim == c.cheater_id {
                    continue;
                }
                let g = RefundGame { id: id as GameId, victim, category: &category, points, ended_at };
                if let Some(given) = db.refunds().give(&g, c.cheater_id, c.now, c.sanction_id, c.source, c.by.as_deref())? {
                    out.push(given);
                }
            }
            Ok(out)
        })
    }

    /// Refunds, newest first.
    pub fn list(&self, scope: RefundScope, limit: i64) -> Result<Vec<Refund>> {
        let (filter, id) = match scope {
            RefundScope::Cheater(u) => ("f.cheater_id = ?1", Some(u)),
            RefundScope::Victim(u) => ("f.victim_id = ?1", Some(u)),
            RefundScope::All => ("?1 IS NULL", None),
        };
        self.db.all(
            &format!(
                "SELECT f.id, f.game_id, f.victim_id, f.cheater_id, f.category, f.points, f.created_at, f.sanction_id,
                 f.source, f.created_by, f.notified_at, v.username, c.username FROM rating_refunds f
                 JOIN users v ON v.id = f.victim_id JOIN users c ON c.id = f.cheater_id WHERE {filter}
                 ORDER BY f.id DESC LIMIT ?2"
            ),
            params![id, limit],
            to_refund,
        )
    }

    /// Refunds not notified yet with an id above `after_id`, oldest first.
    pub fn pending_since(&self, after_id: i64, limit: i64) -> Result<Vec<PendingRefund>> {
        self.db.all(
            "SELECT id, victim_id, points FROM rating_refunds WHERE notified_at IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
            params![after_id, limit],
            |r| Ok(PendingRefund { id: r.get(0)?, victim_id: r.get(1)?, points: r.get(2)? }),
        )
    }

    /// A victim's refunds not notified yet.
    pub fn pending_for(&self, victim_id: UserId) -> Result<PendingRefunds> {
        let rows: Vec<(i64, i64)> = self.db.all(
            "SELECT id, points FROM rating_refunds WHERE victim_id = ?1 AND notified_at IS NULL",
            [victim_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(PendingRefunds {
            points: rows.iter().map(|r| r.1).sum(),
            ids: rows.into_iter().map(|r| r.0).collect(),
        })
    }

    /// Marks refunds notified (those not marked yet); returns how many changed.
    pub fn mark_notified(&self, ids: &[i64], now: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.db.transaction(|db| {
            let mut n = 0;
            for id in ids {
                n += db.exec(
                    "UPDATE rating_refunds SET notified_at = ?1 WHERE id = ?2 AND notified_at IS NULL",
                    params![now, id],
                )?;
            }
            Ok(n)
        })
    }
}

fn to_refund(r: &Row<'_>) -> rusqlite::Result<Refund> {
    Ok(Refund {
        id: r.get(0)?,
        game_id: r.get::<_, i64>(1)? as GameId,
        victim_id: r.get(2)?,
        cheater_id: r.get(3)?,
        category: r.get(4)?,
        points: r.get(5)?,
        created_at: r.get(6)?,
        sanction_id: r.get(7)?,
        source: r.get(8)?,
        created_by: r.get(9)?,
        notified_at: r.get(10)?,
        victim_name: r.get(11)?,
        cheater_name: r.get(12)?,
    })
}
