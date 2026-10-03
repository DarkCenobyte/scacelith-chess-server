//! Rating refunds of the victims of a player banned for cheating (docs/ANTICHEAT.md, rating
//! refunds).
//!
//! When a player is banned as a cheater (a certain cheat, [`super::sanction`]; a moderator's
//! integrity confirm, `admin integrity confirm`), every opponent who lost rating points to them in
//! a rated game that ended within `RATING_REFUND_DAYS` before the ban gets exactly those points
//! back, added to their current rating in that category: the games are not recomputed, a win
//! against the cheater is left alone, and a draw that cost points is refunded like a loss. Only a
//! change of the K formula is refunded (the game that gave a player their first rating moved them
//! from a working rating, which was no rating to lose). The store gives one refund per game and
//! victim at most, so a second ban, a moderator's later `refunds apply` or a retry gives nothing
//! twice. The games recorded while the cheater is `confirmed` and under a ban that refunds
//! ([`ban_refunds`]) are refunded by the store as they are committed.
//!
//! Which bans are for cheating is told by their source and reason prefix ([`reason`]): a
//! `user ban` is not about cheating and refunds nothing, and a moderator who confirms a cheater
//! with `--no-refund` gives no refund at all. The store repeats the test of [`ban_refunds`] in SQL.
//!
//! Audit trail: the `rating_refunds` rows and one security event `rating_refund` per refund. The
//! victims learn of it through `Notice{RatingRestored}` ([`super::notices`]).

use serde_json::json;

use crate::ids::UserId;
use crate::log::Logger;
use crate::log_security;
use crate::log_warn;
use crate::store::{
    CheaterRefunds, Db, GivenRefund, NewSecurityEvent, Sanction, SanctionKind, Source, StoreError,
};

/// Reason prefixes of the bans given for cheating (`sanctions.reason`). They are stored with the
/// bans: changing one would change what the bans already given are.
pub mod reason {
    /// The automatic ban of a certain cheat (source `auto`), followed by the anomaly kind.
    pub const CERTAIN: &str = "certain_cheat:";
    /// A moderator's integrity confirm with refunds, followed by the moderator's reason.
    pub const CONFIRMED: &str = "confirmed: ";
    /// A moderator's integrity confirm without refunds (`--no-refund`).
    pub const CONFIRMED_NO_REFUND: &str = "confirmed, no refund: ";
}

/// Milliseconds in a day.
pub const DAY_MS: i64 = 86_400_000;

fn has_prefix(s: &Sanction, prefix: &str) -> bool {
    s.reason.as_deref().is_some_and(|r| r.starts_with(prefix))
}

/// Whether a sanction is a ban for cheating that refunds its player's victims (at the ban, and at
/// the commit of the games recorded while it is active): an automatic ban of a certain cheat, or
/// an integrity confirm without `--no-refund`.
pub fn ban_refunds(s: &Sanction) -> bool {
    if s.kind != SanctionKind::Ban {
        return false;
    }
    match s.source {
        Source::Auto => has_prefix(s, reason::CERTAIN),
        Source::Moderator => has_prefix(s, reason::CONFIRMED),
    }
}

/// Whether a sanction is a ban for cheating, with or without refunds.
pub fn is_cheating_ban(s: &Sanction) -> bool {
    ban_refunds(s)
        || (s.kind == SanctionKind::Ban
            && s.source == Source::Moderator
            && has_prefix(s, reason::CONFIRMED_NO_REFUND))
}

/// Start of the automatic refund window of a ban at `ban_at`: `refund_days` (`RATING_REFUND_DAYS`)
/// before it, or `None` when it is 0 (no automatic refunds).
pub fn refund_window_start(refund_days: i64, ban_at: i64) -> Option<i64> {
    let days = refund_days.max(0);
    (days > 0).then(|| ban_at - days * DAY_MS)
}

/// Points given back to one victim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VictimTotal {
    pub victim_id: UserId,
    pub points: i64,
}

/// Points given back per victim, in the order of their first refund.
pub fn victim_totals(given: &[GivenRefund]) -> Vec<VictimTotal> {
    let mut out: Vec<VictimTotal> = Vec::new();
    for r in given {
        match out.iter_mut().find(|t| t.victim_id == r.victim_id) {
            Some(t) => t.points += r.points,
            None => out.push(VictimTotal { victim_id: r.victim_id, points: r.points }),
        }
    }
    out
}

/// Gives the refunds of a cheater's games that ended at `c.since` or later and writes their
/// security events, inside the caller's store job. Returns the refunds given now (none for a game
/// already refunded). A failure of the refunds is returned (nothing of them is written); a failure
/// of the security events only is logged. Log the refunds with [`log_refunds`] once the job has
/// committed.
pub fn refund_victims(
    db: &Db<'_>,
    c: &CheaterRefunds,
    logger: &Logger,
) -> Result<Vec<GivenRefund>, StoreError> {
    let given = db.transaction(|db| db.refunds().apply_for_cheater(c))?;
    if given.is_empty() {
        return Ok(given);
    }
    let events: Vec<NewSecurityEvent> = given
        .iter()
        .map(|r| NewSecurityEvent {
            kind: "rating_refund".into(),
            user_id: Some(r.victim_id),
            ip: None,
            at: Some(c.now),
            detail: Some(json!({
                "refundId": r.id,
                "gameId": r.game_id,
                "cheaterId": c.cheater_id,
                "category": r.category,
                "points": r.points,
                "source": c.source.as_str(),
                "sanctionId": c.sanction_id,
                "by": c.by,
            })),
        })
        .collect();
    if let Err(e) = db.security().insert_batch(&events) {
        log_warn!(logger, "refund security events not stored", { "err": crate::log::error(&e), "cheaterId": c.cheater_id });
    }
    Ok(given)
}

/// The security log line of refunds given by [`refund_victims`] (`rating.refund`), written after
/// the commit. Nothing is logged when no refund was given.
pub fn log_refunds(logger: &Logger, c: &CheaterRefunds, given: &[GivenRefund]) {
    if given.is_empty() {
        return;
    }
    log_security!(logger, "rating.refund", {
        "cheaterId": c.cheater_id,
        "source": c.source.as_str(),
        "sanctionId": c.sanction_id,
        "by": c.by,
        "refunds": given.len(),
        "victims": victim_totals(given).len(),
        "points": given.iter().map(|r| r.points).sum::<i64>(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanction(kind: SanctionKind, source: Source, reason: Option<&str>) -> Sanction {
        Sanction {
            id: 1,
            user_id: 1,
            kind,
            reason: reason.map(Into::into),
            source,
            game_id: None,
            starts_at: 0,
            ends_at: None,
            created_at: 0,
            created_by: None,
            lifted_at: None,
            lifted_by: None,
        }
    }

    #[test]
    fn bans_for_cheating_are_told_by_source_and_reason() {
        use SanctionKind::*;
        use Source::*;
        let cases = [
            (Ban, Auto, Some("certain_cheat:illegal_move"), true, true),
            (Ban, Auto, Some("confirmed: engine"), false, false),
            (Ban, Moderator, Some("confirmed: engine"), true, true),
            (Ban, Moderator, Some("confirmed, no refund: engine"), false, true),
            (Ban, Moderator, Some("certain_cheat:x"), false, false),
            (Ban, Moderator, Some("abusive chat"), false, false),
            (Ban, Moderator, None, false, false),
            (MmBlock, Auto, Some("certain_cheat:x"), false, false),
            (Warning, Moderator, Some("confirmed: x"), false, false),
        ];
        for (kind, source, reason, refunds, cheating) in cases {
            let s = sanction(kind, source, reason);
            assert_eq!(ban_refunds(&s), refunds, "{kind:?} {source:?} {reason:?}");
            assert_eq!(is_cheating_ban(&s), cheating, "{kind:?} {source:?} {reason:?}");
        }
    }

    #[test]
    fn the_window_counts_back_from_the_ban() {
        let now = 1_788_264_000_000;
        assert_eq!(refund_window_start(0, now), None);
        assert_eq!(refund_window_start(-3, now), None);
        assert_eq!(refund_window_start(60, now), Some(now - 60 * DAY_MS));
        assert_eq!(refund_window_start(90, now), Some(now - 90 * DAY_MS));
    }

    #[test]
    fn totals_are_per_victim_in_first_refund_order() {
        let r = |victim_id, points| GivenRefund {
            id: 0,
            game_id: 1,
            victim_id,
            category: "3+2".into(),
            points,
            ended_at: 0,
        };
        assert_eq!(
            victim_totals(&[r(5, 10), r(3, 4), r(5, 2)]),
            vec![VictimTotal { victim_id: 5, points: 12 }, VictimTotal { victim_id: 3, points: 4 }]
        );
        assert!(victim_totals(&[]).is_empty());
    }
}
