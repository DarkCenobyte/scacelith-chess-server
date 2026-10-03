//! The database side of the automatic sanction of a certain cheat
//! ([`super::service::Anticheat::sanction`]): one store write job on the writer thread.

use serde_json::{Value, json};

use super::players::{read_integrity, write_integrity};
use super::refunds::{self, VictimTotal, ban_refunds, reason, refund_victims, refund_window_start};
use crate::anticheat::integrity::IntegrityLevel;
use crate::ids::{GameId, UserId};
use crate::log::Logger;
use crate::store::{
    CheaterRefunds, Db, GivenRefund, NewSanction, NewSecurityEvent, SanctionKind, Source, StoreError,
};
use crate::{log_error, log_warn};

/// Items of `evidence.certain` kept (the newest).
pub const MAX_EVIDENCE_ITEMS: usize = 50;
/// A ban without an end counts as ending this long after the anomaly.
const PERMANENT_MS: i64 = 100 * 365 * 24 * 3_600_000;
const HOUR_MS: i64 = 3_600_000;

/// A certain cheat to sanction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertainCheat {
    pub user: UserId,
    /// 0 when no game is concerned.
    pub game: GameId,
    /// The anomaly kind (`illegal_move`, `forged_type`...).
    pub kind: String,
    /// When the cheat was seen (wall clock, ms).
    pub at: i64,
}

/// The settings of the automatic sanction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SanctionSettings {
    /// `BAN_DURATION_HOURS`
    pub ban_hours: i64,
    /// `RATING_REFUND_DAYS`
    pub refund_days: i64,
}

/// What [`apply_certain_sanction`] did.
#[derive(Clone, Debug)]
pub struct AppliedSanction {
    /// End of the ban that stands for the cheat (the new one, or an existing longer one).
    pub until: i64,
    /// A new ban was created.
    pub created: bool,
    pub sanction_id: Option<i64>,
    /// The refunds given now.
    pub given: Vec<GivenRefund>,
    /// The points given back now, per victim.
    pub refunds: Vec<VictimTotal>,
    /// The refunds' parameters (for the log line written after the commit).
    pub refund_request: Option<CheaterRefunds>,
}

/// Bans a player for a certain cheat, inside the caller's write job: a ban of `ban_hours`
/// (source `auto`, reason `certain_cheat:<kind>`) unless an active ban for cheating that refunds
/// exists for the same game or lasts at least as long (another anomaly of the game, a moderator's
/// integrity confirm); integrity level `confirmed` with the cheat appended to `evidence.certain`
/// (the score and the rest of the evidence are kept); security event `sanction_auto` for a new
/// ban; the rating refunds of the player's victims (idempotent, so they also run when the ban
/// existed). A ban that cannot be stored is an error: the job is rolled back and nothing is
/// written, so that the next certain anomaly of the game tries again. The failure of a later step
/// is logged and does not undo the ban (each step runs in its own savepoint).
pub fn apply_certain_sanction(
    db: &Db<'_>,
    settings: SanctionSettings,
    cheat: &CertainCheat,
    logger: &Logger,
) -> Result<AppliedSanction, StoreError> {
    let CertainCheat { user, game, ref kind, at } = *cheat;
    let mut until = at + settings.ban_hours * HOUR_MS;
    let mut created = false;
    // Only a ban that refunds can stand for this one: under a ban for something else or an
    // integrity confirm with --no-refund, the games recorded later would not be refunded.
    let bans: Vec<_> =
        db.sanctions().active(user, at).unwrap_or_default().into_iter().filter(ban_refunds).collect();
    let end_of = |s: &crate::store::Sanction| s.ends_at.filter(|&e| e != 0).unwrap_or(at + PERMANENT_MS);
    let same_game = |s: &crate::store::Sanction| game != 0 && s.game_id == Some(game);
    let active = bans.iter().find(|s| same_game(s)).or_else(|| {
        bans.iter().fold(None, |best: Option<&crate::store::Sanction>, s| match best {
            Some(b) if end_of(b) >= end_of(s) => Some(b),
            _ => Some(s),
        })
    });
    let sanction_id = match active {
        Some(s) if same_game(s) || end_of(s) >= until => {
            // Already banned for cheating (an earlier anomaly of this game, a longer ban).
            until = end_of(s);
            Some(s.id)
        }
        _ => {
            let id = db.sanctions().create(&NewSanction {
                user_id: user,
                kind: SanctionKind::Ban,
                reason: Some(format!("{}{kind}", reason::CERTAIN)),
                source: Source::Auto,
                game_id: (game != 0).then_some(game),
                starts_at: at,
                ends_at: Some(until),
                created_by: None,
                created_at: at,
            })?;
            created = true;
            Some(id)
        }
    };

    let confirmed = db.transaction(|db| -> Result<(), StoreError> {
        let mut prev = read_integrity(db, user)?;
        // In place: the key keeps its position in the stored object.
        let slot = prev.evidence.entry("certain").or_insert(Value::Null);
        if !slot.is_array() {
            *slot = Value::Array(Vec::new());
        }
        if let Value::Array(certain) = slot {
            certain.push(json!({ "kind": kind, "gameId": game, "at": at, "banUntil": until }));
            let skip = certain.len().saturating_sub(MAX_EVIDENCE_ITEMS);
            certain.drain(..skip);
        }
        write_integrity(db, user, IntegrityLevel::Confirmed, prev.score, prev.evidence, at)
    });
    if let Err(e) = confirmed {
        log_error!(logger, "integrity not updated after a certain cheat", { "err": crate::log::error(&e), "userId": user });
    }

    if created {
        let event = NewSecurityEvent {
            kind: "sanction_auto".into(),
            user_id: Some(user),
            ip: None,
            at: Some(at),
            detail: Some(json!({ "kind": kind, "gameId": game, "until": until })),
        };
        if let Err(e) = db.security().insert_batch(&[event]) {
            log_warn!(logger, "security event not stored", { "err": crate::log::error(&e) });
        }
    }

    let mut given = Vec::new();
    let mut refund_request = None;
    if let Some(since) = refund_window_start(settings.refund_days, at) {
        let request =
            CheaterRefunds { cheater_id: user, since, now: at, sanction_id, source: Source::Auto, by: None };
        match refund_victims(db, &request, logger) {
            Ok(g) => given = g,
            Err(e) => {
                log_error!(logger, "rating refunds not applied", { "err": crate::log::error(&e), "userId": user });
            }
        }
        refund_request = Some(request);
    }
    Ok(AppliedSanction {
        until,
        created,
        sanction_id,
        refunds: refunds::victim_totals(&given),
        given,
        refund_request,
    })
}
