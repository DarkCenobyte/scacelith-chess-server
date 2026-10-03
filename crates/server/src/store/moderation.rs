//! Conduct events and cooldowns, sanctions, anomalies and security events.

use rusqlite::{Row, params};
use serde_json::Value;

use super::db::Db;
use super::error::Result;
use super::values::{json_text, json_value, text_enum};
use crate::ids::{GameId, UserId};

text_enum! {
    /// A conduct incident.
    pub enum ConductKind {
        /// Left a game in progress (forfeit by disconnection).
        Abandon = "abandon",
        /// Aborted a game.
        Abort = "abort",
        /// Never made the first move.
        NoShow = "noshow",
    }
}

/// Incidents of a player since some time, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConductCounts {
    pub abandon: i64,
    pub abort: i64,
    pub noshow: i64,
}

impl ConductCounts {
    /// Every incident.
    pub fn total(&self) -> i64 {
        self.abandon + self.abort + self.noshow
    }
}

/// A conduct incident of a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConductEvent {
    pub kind: ConductKind,
    pub at: i64,
}

/// The matchmaking cooldown of a player (zeros without one).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cooldown {
    pub until: i64,
    pub level: i64,
    pub updated_at: i64,
}

/// Conduct events and cooldowns.
#[derive(Debug, Clone, Copy)]
pub struct Conduct<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Conduct<'_> {
    /// Records an incident.
    pub fn record(&self, user_id: UserId, kind: ConductKind, at: i64) -> Result<()> {
        self.db.exec(
            "INSERT INTO conduct_events (user_id, kind, at) VALUES (?1, ?2, ?3)",
            params![user_id, kind, at],
        )?;
        Ok(())
    }

    /// Incidents at `since` or later, by kind.
    pub fn count_since(&self, user_id: UserId, since: i64) -> Result<ConductCounts> {
        let rows: Vec<(ConductKind, i64)> = self.db.all(
            "SELECT kind, count(*) FROM conduct_events WHERE user_id = ?1 AND at >= ?2 GROUP BY kind",
            params![user_id, since],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut out = ConductCounts::default();
        for (kind, n) in rows {
            match kind {
                ConductKind::Abandon => out.abandon = n,
                ConductKind::Abort => out.abort = n,
                ConductKind::NoShow => out.noshow = n,
            }
        }
        Ok(out)
    }

    /// The player's incidents, newest first.
    pub fn for_user(&self, user_id: UserId, limit: i64) -> Result<Vec<ConductEvent>> {
        self.db.all(
            "SELECT kind, at FROM conduct_events WHERE user_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
            params![user_id, limit],
            |r| Ok(ConductEvent { kind: r.get(0)?, at: r.get(1)? }),
        )
    }

    /// The player's cooldown.
    pub fn cooldown(&self, user_id: UserId) -> Result<Cooldown> {
        let c = self.db.one(
            "SELECT cooldown_until, level, updated_at FROM conduct_state WHERE user_id = ?1",
            [user_id],
            |r| Ok(Cooldown { until: r.get(0)?, level: r.get(1)?, updated_at: r.get(2)? }),
        )?;
        Ok(c.unwrap_or_default())
    }

    /// Sets the player's cooldown.
    pub fn set_cooldown(&self, user_id: UserId, until: i64, level: i64, now: i64) -> Result<()> {
        self.db.exec(
            "INSERT INTO conduct_state (user_id, cooldown_until, level, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user_id) DO UPDATE SET cooldown_until = excluded.cooldown_until, level = excluded.level,
             updated_at = excluded.updated_at",
            params![user_id, until, level, now],
        )?;
        Ok(())
    }
}

text_enum! {
    /// Kind of a sanction.
    pub enum SanctionKind {
        Ban = "ban",
        /// Kept out of matchmaking.
        MmBlock = "mm_block",
        Warning = "warning",
    }
}

text_enum! {
    /// Who decided a sanction or a refund.
    pub enum Source {
        /// The anti-cheat.
        Auto = "auto",
        Moderator = "moderator",
    }
}

/// A new sanction.
#[derive(Debug, Clone)]
pub struct NewSanction {
    pub user_id: UserId,
    pub kind: SanctionKind,
    /// `certain_cheat:<kind>` (automatic ban), `confirmed: <reason>` (refunding moderator ban)...
    pub reason: Option<String>,
    pub source: Source,
    pub game_id: Option<GameId>,
    pub starts_at: i64,
    /// `None`: permanent.
    pub ends_at: Option<i64>,
    pub created_by: Option<String>,
    pub created_at: i64,
}

/// A sanction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanction {
    pub id: i64,
    pub user_id: UserId,
    pub kind: SanctionKind,
    pub reason: Option<String>,
    pub source: Source,
    pub game_id: Option<GameId>,
    pub starts_at: i64,
    pub ends_at: Option<i64>,
    pub created_at: i64,
    pub created_by: Option<String>,
    pub lifted_at: Option<i64>,
    pub lifted_by: Option<String>,
}

const SANCTION_COLS: &str = "id, user_id, kind, reason, source, game_id, starts_at, ends_at, created_at, created_by, lifted_at, lifted_by";

fn to_sanction(r: &Row<'_>) -> rusqlite::Result<Sanction> {
    Ok(Sanction {
        id: r.get(0)?,
        user_id: r.get(1)?,
        kind: r.get(2)?,
        reason: r.get(3)?,
        source: r.get(4)?,
        game_id: r.get::<_, Option<i64>>(5)?.map(|g| g as GameId),
        starts_at: r.get(6)?,
        ends_at: r.get(7)?,
        created_at: r.get(8)?,
        created_by: r.get(9)?,
        lifted_at: r.get(10)?,
        lifted_by: r.get(11)?,
    })
}

/// Sanctions.
#[derive(Debug, Clone, Copy)]
pub struct Sanctions<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Sanctions<'_> {
    /// Records a sanction; returns its id.
    pub fn create(&self, s: &NewSanction) -> Result<i64> {
        self.db.insert(
            "INSERT INTO sanctions (user_id, kind, reason, source, game_id, starts_at, ends_at, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                s.user_id,
                s.kind,
                s.reason,
                s.source,
                s.game_id.filter(|&g| g != 0).map(|g| g as i64),
                s.starts_at,
                s.ends_at,
                s.created_at,
                s.created_by,
            ],
        )
    }

    /// The active ban lasting longest (a permanent one first).
    pub fn active_ban(&self, user_id: UserId, now: i64) -> Result<Option<Sanction>> {
        self.db.one(
            &format!(
                "SELECT {SANCTION_COLS} FROM sanctions WHERE user_id = ?1 AND kind = 'ban' AND lifted_at IS NULL
                 AND starts_at <= ?2 AND (ends_at IS NULL OR ends_at > ?2) ORDER BY ends_at IS NULL DESC, ends_at DESC LIMIT 1"
            ),
            params![user_id, now],
            to_sanction,
        )
    }

    /// Every active sanction of the player, oldest start first.
    pub fn active(&self, user_id: UserId, now: i64) -> Result<Vec<Sanction>> {
        self.db.all(
            &format!(
                "SELECT {SANCTION_COLS} FROM sanctions WHERE user_id = ?1 AND lifted_at IS NULL AND starts_at <= ?2
                 AND (ends_at IS NULL OR ends_at > ?2) ORDER BY starts_at"
            ),
            params![user_id, now],
            to_sanction,
        )
    }

    /// Every sanction of the player, lifted ones included, newest first (`created_by` and
    /// `lifted_by` name moderators: leave them out when showing the list to the player).
    pub fn list(&self, user_id: UserId) -> Result<Vec<Sanction>> {
        self.db.all(
            &format!(
                "SELECT {SANCTION_COLS} FROM sanctions WHERE user_id = ?1 ORDER BY created_at DESC, id DESC"
            ),
            [user_id],
            to_sanction,
        )
    }

    /// Lifts a sanction; `true` when it was not lifted yet.
    pub fn lift(&self, id: i64, by: Option<&str>, now: i64) -> Result<bool> {
        let n = self.db.exec(
            "UPDATE sanctions SET lifted_at = ?1, lifted_by = ?2 WHERE id = ?3 AND lifted_at IS NULL",
            params![now, by, id],
        )?;
        Ok(n == 1)
    }
}

text_enum! {
    /// Severity of an anomaly.
    pub enum Severity {
        Info = "info",
        Suspicious = "suspicious",
        /// Proof of cheating (kept by the retention purge).
        Certain = "certain",
    }
}

/// A new anomaly.
#[derive(Debug, Clone)]
pub struct NewAnomaly {
    pub user_id: Option<UserId>,
    pub game_id: Option<GameId>,
    pub kind: String,
    pub severity: Severity,
    /// `None`: the store clock's time.
    pub at: Option<i64>,
    pub detail: Option<Value>,
}

/// A stored anomaly.
#[derive(Debug, Clone, PartialEq)]
pub struct Anomaly {
    pub id: i64,
    pub user_id: Option<UserId>,
    pub game_id: Option<GameId>,
    pub kind: String,
    pub severity: Severity,
    pub at: i64,
    pub detail: Option<Value>,
}

/// Anomalies (protocol and analysis evidence; no foreign keys).
#[derive(Debug, Clone, Copy)]
pub struct Anomalies<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Anomalies<'_> {
    /// Writes a batch (all or nothing); returns the rows written.
    pub fn insert_batch(&self, list: &[NewAnomaly]) -> Result<usize> {
        if list.is_empty() {
            return Ok(0);
        }
        let now = self.db.now();
        self.db.transaction(|db| {
            for a in list {
                db.exec(
                    "INSERT INTO anomalies (user_id, game_id, kind, severity, at, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        a.user_id.filter(|&u| u != 0),
                        a.game_id.filter(|&g| g != 0).map(|g| g as i64),
                        a.kind,
                        a.severity,
                        a.at.unwrap_or(now),
                        json_text(a.detail.as_ref()),
                    ],
                )?;
            }
            Ok(list.len())
        })
    }

    /// The player's anomalies, newest first.
    pub fn for_user(&self, user_id: UserId, limit: i64) -> Result<Vec<Anomaly>> {
        self.db.all(
            "SELECT id, user_id, game_id, kind, severity, at, detail FROM anomalies WHERE user_id = ?1
             ORDER BY at DESC, id DESC LIMIT ?2",
            params![user_id, limit],
            |r| {
                Ok(Anomaly {
                    id: r.get(0)?,
                    user_id: r.get(1)?,
                    game_id: r.get::<_, Option<i64>>(2)?.map(|g| g as GameId),
                    kind: r.get(3)?,
                    severity: r.get(4)?,
                    at: r.get(5)?,
                    detail: json_value(r.get(6)?),
                })
            },
        )
    }
}

/// A new security event.
#[derive(Debug, Clone, Default)]
pub struct NewSecurityEvent {
    pub kind: String,
    pub user_id: Option<UserId>,
    /// The client address (erased after `RETENTION_IP_DAYS`).
    pub ip: Option<String>,
    /// `None`: the store clock's time.
    pub at: Option<i64>,
    pub detail: Option<Value>,
}

/// A stored security event.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityEvent {
    pub id: i64,
    pub kind: String,
    pub user_id: Option<UserId>,
    pub ip: Option<String>,
    pub at: i64,
    pub detail: Option<Value>,
}

/// Security events.
#[derive(Debug, Clone, Copy)]
pub struct Security<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Security<'_> {
    /// Writes a batch (all or nothing); returns the rows written.
    pub fn insert_batch(&self, list: &[NewSecurityEvent]) -> Result<usize> {
        if list.is_empty() {
            return Ok(0);
        }
        let now = self.db.now();
        self.db.transaction(|db| {
            for e in list {
                db.exec(
                    "INSERT INTO security_events (kind, user_id, ip, at, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        e.kind,
                        e.user_id.filter(|&u| u != 0),
                        e.ip,
                        e.at.unwrap_or(now),
                        json_text(e.detail.as_ref())
                    ],
                )?;
            }
            Ok(list.len())
        })
    }

    /// The player's events, newest first.
    pub fn for_user(&self, user_id: UserId, limit: i64) -> Result<Vec<SecurityEvent>> {
        self.db.all(
            "SELECT id, kind, user_id, ip, at, detail FROM security_events WHERE user_id = ?1
             ORDER BY at DESC, id DESC LIMIT ?2",
            params![user_id, limit],
            |r| {
                Ok(SecurityEvent {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    user_id: r.get(2)?,
                    ip: r.get(3)?,
                    at: r.get(4)?,
                    detail: json_value(r.get(5)?),
                })
            },
        )
    }
}
