//! Integrity levels and the memory rules of the automatic level (docs/ANTICHEAT.md): what a
//! player's stored record becomes after a new scoring ([`player_level`]). The rules are pure: the
//! store-side caller reads the record, applies them and writes the result back inside one write
//! job, so that a ban or a review committed meanwhile is never overwritten with an older view.

use std::borrow::Cow;
use std::fmt;

use serde_json::{Map, Value, json};

use super::num::{js_to_number, js_truthy as truthy, json_num};
use super::scoring::{PlayerScore, Population, SideRecord, model};

/// A player's integrity level, weakest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntegrityLevel {
    /// Nothing to report.
    #[default]
    None,
    /// One strong statistical signal: a moderator should look.
    Suspected,
    /// Independent signals agree.
    HighConfidence,
    /// Set by a moderator or by a certain protocol cheat, never by the model.
    Confirmed,
}

impl IntegrityLevel {
    /// Every level, weakest first.
    pub const ALL: [IntegrityLevel; 4] = [
        IntegrityLevel::None,
        IntegrityLevel::Suspected,
        IntegrityLevel::HighConfidence,
        IntegrityLevel::Confirmed,
    ];

    /// The stored name (`none`, `suspected`, `high_confidence`, `confirmed`).
    pub fn as_str(self) -> &'static str {
        match self {
            IntegrityLevel::None => "none",
            IntegrityLevel::Suspected => "suspected",
            IntegrityLevel::HighConfidence => "high_confidence",
            IntegrityLevel::Confirmed => "confirmed",
        }
    }

    /// The level of a stored name.
    pub fn parse(s: &str) -> Option<IntegrityLevel> {
        IntegrityLevel::ALL.into_iter().find(|l| l.as_str() == s)
    }

    /// The level of a stored value: unknown or missing values count as `none`.
    pub fn from_stored(s: Option<&str>) -> IntegrityLevel {
        s.and_then(IntegrityLevel::parse).unwrap_or_default()
    }

    /// Rank, 0 for `none` to 3 for `confirmed`.
    pub fn rank(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for IntegrityLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A structured value read back from the store: JSON values as they are, JSON text parsed (twice
/// when it was stored already encoded); `None` for `null` and unreadable text.
pub fn parse_maybe_json(v: &Value) -> Option<Cow<'_, Value>> {
    let mut x = Cow::Borrowed(v);
    for _ in 0..2 {
        let Value::String(s) = &*x else { break };
        x = Cow::Owned(serde_json::from_str(s).ok()?);
    }
    (!x.is_null()).then_some(x)
}

/// `+x || 0` of a JSON field.
fn number_or_zero(v: Option<&Value>) -> f64 {
    let x = js_to_number(v);
    if x.is_nan() { 0.0 } else { x }
}

/// A player's stored integrity record, with the defaults of a player never scored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IntegrityRecord {
    pub level: IntegrityLevel,
    pub score: f64,
    /// Free-form evidence object (`statistics`, `peak`, `review`, `certain`...).
    pub evidence: Map<String, Value>,
}

impl IntegrityRecord {
    /// A record from its stored columns: unknown levels count as `none`, a missing or
    /// non-numeric score as 0, and evidence that is not an object (or JSON text of one) as empty.
    pub fn from_stored(level: Option<&str>, score: Option<f64>, evidence: Option<&Value>) -> IntegrityRecord {
        let evidence = match evidence.and_then(parse_maybe_json).as_deref() {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        IntegrityRecord {
            level: IntegrityLevel::from_stored(level),
            score: score.filter(|s| !s.is_nan()).unwrap_or(0.0),
            evidence,
        }
    }
}

/// Below this score and without a level, only a compact summary is stored.
pub const NOTABLE_SCORE: f64 = 2.0;

/// What a player's integrity record becomes after a scoring ([`player_level`]).
#[derive(Clone, Debug, PartialEq)]
pub struct LevelUpdate {
    /// The new level.
    pub level: IntegrityLevel,
    /// The level before.
    pub previous: IntegrityLevel,
    /// The score to store (the model's score, never negative).
    pub score: f64,
    /// The evidence to store: the previous evidence with `statistics` (and `peak`) replaced.
    pub evidence: Map<String, Value>,
}

impl LevelUpdate {
    /// Whether the level changed (the caller logs a security event `integrity.level`).
    pub fn changed(&self) -> bool {
        self.level != self.previous
    }
}

/// Applies the memory rules on top of a fresh scoring to the stored record `prev`.
///
/// `games` are the records the player was scored on (all of their recent analysed games, any
/// profile), `result` the scoring of these games against `population`. Rules: `confirmed` is
/// never touched; a player whose recent games were analysed with another profile keeps their
/// level until they have [`model::suspected::MIN_GAMES`] games of the population's profile (the
/// statistics restarted); `high_confidence` never falls back below `suspected` without a
/// moderator; a suspected player stays suspected until the accuracy-type score drops
/// [`model::suspected::HYSTERESIS`] below the threshold; after a moderator cleared the player the
/// level only rises again on new evidence (high_confidence, or a score 1.0 above the cleared one
/// with at least 5 games analysed since).
///
/// The evidence keeps everything else of the previous record. Every analysed player gets a row,
/// so `statistics` stays compact unless there is something to explain.
pub fn player_level(
    prev: &IntegrityRecord,
    games: &[SideRecord],
    result: &PlayerScore,
    population: &Population,
    now: i64,
) -> LevelUpdate {
    use model::suspected as s;
    let restarted = games.iter().any(|g| !population.holds(g.profile.as_deref()));
    let mut level = result.level;
    let mut ev = prev.evidence.clone();
    if prev.level == IntegrityLevel::Confirmed {
        level = IntegrityLevel::Confirmed;
    } else {
        // Statistics restarted by a new profile: too few of its games to judge yet.
        if restarted && result.games < s::MIN_GAMES && level < prev.level {
            level = prev.level;
        }
        if prev.level == IntegrityLevel::HighConfidence && level < IntegrityLevel::Suspected {
            level = IntegrityLevel::Suspected;
        }
        // Hysteresis: no flapping of the moderators' queue around the threshold.
        if prev.level == IntegrityLevel::Suspected
            && level == IntegrityLevel::None
            && result.groups.accuracy_type.is_some_and(|a| a >= s::ACCURACY_TYPE - s::HYSTERESIS)
        {
            level = IntegrityLevel::Suspected;
        }
        let review = ev.get("review").filter(|r| truthy(Some(r)));
        let cleared_at = review.and_then(|r| r.get("clearedAt"));
        if truthy(cleared_at) && prev.level == IntegrityLevel::None && level == IntegrityLevel::Suspected {
            let cleared_at = number_or_zero(cleared_at);
            let since = games
                .iter()
                .filter(|g| {
                    let at = [g.analysed_at, g.ended_at].into_iter().find(|&t| t != 0.0).unwrap_or(0.0);
                    population.holds(g.profile.as_deref()) && at > cleared_at
                })
                .count();
            let cleared_score = number_or_zero(review.and_then(|r| r.get("clearedScore")));
            if !(result.score >= cleared_score + 1.0 && since >= 5) {
                level = IntegrityLevel::None;
            }
        }
    }

    let notable = level != IntegrityLevel::None || result.score >= NOTABLE_SCORE;
    let profile = population.profile().map_or(Value::Null, |p| json!(p));
    let mut stats = Map::new();
    stats.insert("model".into(), json!(model::VERSION));
    stats.insert("profile".into(), profile);
    stats.insert("computedAt".into(), json!(now));
    stats.insert("level".into(), json!(result.level.as_str()));
    stats.insert("score".into(), json_num(result.score));
    if notable {
        stats.insert("trigger".into(), result.trigger_json());
        stats.insert("groups".into(), result.groups.to_json());
        stats.insert("windows".into(), result.windows_json());
        stats.insert("jump".into(), result.jump_json());
        stats.insert("reasons".into(), result.reasons_json());
        stats.insert("highWindow".into(), result.high_window.map_or(Value::Null, |w| json!(w)));
    } else {
        stats.insert("groups".into(), result.groups.to_json());
    }
    stats.insert("games".into(), json!(result.games));
    stats.insert("moves".into(), json_num(result.moves));
    ev.insert("statistics".into(), Value::Object(stats));

    let peak = ev.get("peak");
    if notable && (!truthy(peak) || result.score > number_or_zero(peak.and_then(|p| p.get("score")))) {
        let peak = json!({ "at": now, "score": json_num(result.score), "level": result.level.as_str(),
            "trigger": result.trigger_json(), "groups": result.groups.to_json(), "reasons": result.reasons_json() });
        ev.insert("peak".into(), peak);
    }
    LevelUpdate { level, previous: prev.level, score: result.score.max(0.0), evidence: ev }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_names_and_ranks() {
        for (i, l) in IntegrityLevel::ALL.into_iter().enumerate() {
            assert_eq!(IntegrityLevel::parse(l.as_str()), Some(l));
            assert_eq!(usize::from(l.rank()), i);
        }
        assert_eq!(
            IntegrityLevel::from_stored(Some("weird")),
            IntegrityLevel::None,
            "unknown values count as none"
        );
        assert_eq!(IntegrityLevel::from_stored(None), IntegrityLevel::None);
        assert!(IntegrityLevel::HighConfidence > IntegrityLevel::Suspected);
        assert_eq!(IntegrityLevel::HighConfidence.to_string(), "high_confidence");
    }

    #[test]
    fn stored_values_are_parsed_once_or_twice() {
        let obj = json!({ "a": 1 });
        assert_eq!(parse_maybe_json(&obj).as_deref(), Some(&obj));
        assert_eq!(parse_maybe_json(&json!("{\"a\":1}")).as_deref(), Some(&obj));
        let twice = json!(serde_json::to_string(&json!("{\"a\":1}")).expect("a string encodes"));
        assert_eq!(parse_maybe_json(&twice).as_deref(), Some(&obj));
        assert_eq!(parse_maybe_json(&json!("{nope")), None);
        assert_eq!(parse_maybe_json(&Value::Null), None);
        assert_eq!(parse_maybe_json(&json!("null")), None);

        let r = IntegrityRecord::from_stored(
            Some("suspected"),
            Some(3.5),
            Some(&json!("{\"review\":{\"by\":\"m\"}}")),
        );
        assert_eq!(r.level, IntegrityLevel::Suspected);
        assert_eq!(r.evidence["review"]["by"], "m");
        let r = IntegrityRecord::from_stored(Some("nope"), None, Some(&json!([1, 2])));
        assert_eq!(r, IntegrityRecord::default());
    }
}
