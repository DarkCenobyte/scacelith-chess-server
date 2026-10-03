//! Player integrity records and the population statistics of the analysis features.

use indexmap::IndexMap;
use rusqlite::{Row, params};
use serde_json::Value;

use super::db::Db;
use super::error::Result;
use super::values::{json_text, json_value, text_enum};
use crate::ids::UserId;

text_enum! {
    /// Integrity level of a player, from the anti-cheat's scoring or a moderator.
    pub enum IntegrityLevel {
        None = "none",
        Suspected = "suspected",
        HighConfidence = "high_confidence",
        Confirmed = "confirmed",
    }
}

impl IntegrityLevel {
    /// Rank of the level (none 0 .. confirmed 3).
    pub fn rank(self) -> i64 {
        match self {
            IntegrityLevel::None => 0,
            IntegrityLevel::Suspected => 1,
            IntegrityLevel::HighConfidence => 2,
            IntegrityLevel::Confirmed => 3,
        }
    }
}

/// A player's integrity record (level `none`, score 0 and `updated_at` 0 without one).
#[derive(Debug, Clone, PartialEq)]
pub struct Integrity {
    pub level: IntegrityLevel,
    pub score: f64,
    pub evidence: Option<Value>,
    pub updated_at: i64,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<i64>,
    pub note: Option<String>,
}

impl Default for Integrity {
    fn default() -> Integrity {
        Integrity {
            level: IntegrityLevel::None,
            score: 0.0,
            evidence: None,
            updated_at: 0,
            reviewed_by: None,
            reviewed_at: None,
            note: None,
        }
    }
}

/// Fields of an integrity record to change (`None`: kept). `updated_at` is not kept: `None` means
/// the store clock's time.
#[derive(Debug, Clone, Default)]
pub struct IntegrityUpdate {
    pub level: Option<IntegrityLevel>,
    pub score: Option<f64>,
    pub evidence: Option<Option<Value>>,
    pub reviewed_by: Option<Option<String>>,
    pub reviewed_at: Option<Option<i64>>,
    pub note: Option<Option<String>>,
    pub updated_at: Option<i64>,
}

/// A flagged player ([`IntegrityTable::list_flagged`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FlaggedPlayer {
    pub user_id: UserId,
    pub username: String,
    pub integrity: Integrity,
}

/// Running statistics of one metric.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopulationStat {
    pub n: i64,
    pub mean: f64,
    /// Sum of squared deviations.
    pub m2: f64,
    /// Sample variance (`m2 / (n - 1)`, 0 below two values).
    pub variance: f64,
    pub stdev: f64,
    pub updated_at: i64,
}

/// Observations merged into one statistic.
#[derive(Debug, Clone, PartialEq)]
pub enum Sample {
    /// Raw values (non-finite ones are ignored).
    Values(Vec<f64>),
    /// Statistics computed elsewhere.
    Stats { n: i64, mean: f64, m2: f64 },
}

/// One update of [`IntegrityTable::update_population`].
#[derive(Debug, Clone, PartialEq)]
pub struct PopulationUpdate {
    /// `<profile>|<category>|<ratingBucket>|<metric>`.
    pub key: String,
    pub sample: Sample,
}

const INTEGRITY_COLS: &str = "level, score, evidence, updated_at, reviewed_by, reviewed_at, note";

/// Reads an integrity record from the 7 columns of `INTEGRITY_COLS` starting at `at`.
fn to_integrity(r: &Row<'_>, at: usize) -> rusqlite::Result<Integrity> {
    Ok(Integrity {
        level: r.get(at)?,
        score: r.get(at + 1)?,
        evidence: json_value(r.get(at + 2)?),
        updated_at: r.get(at + 3)?,
        reviewed_by: r.get(at + 4)?,
        reviewed_at: r.get(at + 5)?,
        note: r.get(at + 6)?,
    })
}

const LEVEL_RANK: &str =
    "(CASE pi.level WHEN 'none' THEN 0 WHEN 'suspected' THEN 1 WHEN 'high_confidence' THEN 2 ELSE 3 END)";

/// `(n, mean, m2)` of a sample (Welford over the finite values).
fn stats_of(sample: &Sample) -> (i64, f64, f64) {
    match sample {
        Sample::Stats { n, mean, m2 } => (*n, *mean, *m2),
        Sample::Values(values) => {
            let (mut n, mut mean, mut m2) = (0i64, 0f64, 0f64);
            for &x in values.iter().filter(|x| x.is_finite()) {
                n += 1;
                let d = x - mean;
                mean += d / n as f64;
                m2 += d * (x - mean);
            }
            (n, mean, m2)
        }
    }
}

/// Integrity records and population statistics.
#[derive(Debug, Clone, Copy)]
pub struct IntegrityTable<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl IntegrityTable<'_> {
    /// A player's record.
    pub fn get(&self, user_id: UserId) -> Result<Integrity> {
        let rec = self.db.one(
            &format!("SELECT {INTEGRITY_COLS} FROM player_integrity WHERE user_id = ?1"),
            [user_id],
            |r| to_integrity(r, 0),
        )?;
        Ok(rec.unwrap_or_default())
    }

    /// Changes the given fields of a player's record (created when missing), in one transaction.
    pub fn set(&self, user_id: UserId, f: &IntegrityUpdate) -> Result<()> {
        let now = self.db.now();
        self.db.transaction(|db| {
            let cur = db.integrity().get(user_id)?;
            let score = f.score.unwrap_or(cur.score);
            let evidence = f.evidence.clone().unwrap_or(cur.evidence);
            db.exec(
                "INSERT INTO player_integrity (user_id, level, score, evidence, updated_at, reviewed_by, reviewed_at, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT (user_id) DO UPDATE SET level = excluded.level,
                 score = excluded.score, evidence = excluded.evidence, updated_at = excluded.updated_at,
                 reviewed_by = excluded.reviewed_by, reviewed_at = excluded.reviewed_at, note = excluded.note",
                params![
                    user_id,
                    f.level.unwrap_or(cur.level),
                    if score.is_finite() { score } else { 0.0 },
                    json_text(evidence.as_ref()),
                    f.updated_at.unwrap_or(now),
                    f.reviewed_by.clone().unwrap_or(cur.reviewed_by),
                    f.reviewed_at.unwrap_or(cur.reviewed_at),
                    f.note.clone().unwrap_or(cur.note),
                ],
            )?;
            Ok(())
        })
    }

    /// Players at `min_level` or above (at least `suspected`), highest level then score first.
    pub fn list_flagged(&self, min_level: IntegrityLevel, limit: i64) -> Result<Vec<FlaggedPlayer>> {
        self.db.all(
            &format!(
                "SELECT pi.user_id, u.username, pi.level, pi.score, pi.evidence, pi.updated_at, pi.reviewed_by,
                 pi.reviewed_at, pi.note FROM player_integrity pi JOIN users u ON u.id = pi.user_id
                 WHERE pi.level <> 'none' AND {LEVEL_RANK} >= ?1 ORDER BY {LEVEL_RANK} DESC, pi.score DESC LIMIT ?2"
            ),
            params![min_level.rank().max(1), limit],
            |r| Ok(FlaggedPlayer { user_id: r.get(0)?, username: r.get(1)?, integrity: to_integrity(r, 2)? }),
        )
    }

    /// The statistics whose key starts with `prefix|`, keyed by the rest of the key (`prefix`:
    /// `<profile>|<category>|<ratingBucket>` for the metrics of one rating bucket).
    pub fn population_stats(&self, prefix: &str) -> Result<IndexMap<String, PopulationStat>> {
        // '|' is 0x7C and '}' 0x7D: the range holds exactly the keys starting with prefix + '|'.
        let rows: Vec<(String, PopulationStat)> = self.db.all(
            "SELECT key, n, mean, m2, updated_at FROM population_stats WHERE key > ?1 AND key < ?2",
            params![format!("{prefix}|"), format!("{prefix}}}")],
            |r| {
                let n: i64 = r.get(1)?;
                let m2: f64 = r.get(3)?;
                let variance = if n > 1 { m2 / (n - 1) as f64 } else { 0.0 };
                Ok((
                    r.get(0)?,
                    PopulationStat {
                        n,
                        mean: r.get(2)?,
                        m2,
                        variance,
                        stdev: variance.sqrt(),
                        updated_at: r.get(4)?,
                    },
                ))
            },
        )?;
        Ok(rows.into_iter().map(|(k, v)| (k[prefix.len() + 1..].to_string(), v)).collect())
    }

    /// Merges observations into the running statistics (Chan's parallel Welford formula), in one
    /// transaction. Updates without a value are skipped.
    pub fn update_population(&self, updates: &[PopulationUpdate], now: i64) -> Result<()> {
        self.db.transaction(|db| {
            for u in updates {
                let (add_n, add_mean, add_m2) = stats_of(&u.sample);
                if add_n == 0 {
                    continue;
                }
                let (cur_n, cur_mean, cur_m2) = db
                    .one("SELECT n, mean, m2 FROM population_stats WHERE key = ?1", [&u.key], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
                    })?
                    .unwrap_or((0, 0.0, 0.0));
                // The floating-point operations in this exact order (identical results).
                let n = cur_n + add_n;
                let d = add_mean - cur_mean;
                let mean = cur_mean + (d * add_n as f64) / n as f64;
                let m2 = cur_m2 + add_m2 + (d * d * cur_n as f64 * add_n as f64) / n as f64;
                db.exec(
                    "INSERT INTO population_stats (key, n, mean, m2, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (key) DO UPDATE SET n = excluded.n, mean = excluded.mean, m2 = excluded.m2,
                     updated_at = excluded.updated_at",
                    params![u.key, n, mean, m2, now],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welford_of_values() {
        let (n, mean, m2) = stats_of(&Sample::Values(vec![1.0, 2.0, f64::NAN, 3.0, 4.0]));
        assert_eq!(n, 4);
        assert_eq!(mean, 2.5);
        assert_eq!(m2, 5.0);
        assert_eq!(stats_of(&Sample::Values(vec![])), (0, 0.0, 0.0));
    }
}
