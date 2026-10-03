//! The store reads of the realtime layer, done by the connection tasks before they post a request
//! and by the lobby's tasks, never by the lobby actor itself. A failed read is logged and counts
//! as "nothing stored" (no ban, the initial rating, no cooldown), as the former server did.

use crate::config::Config;
use crate::ids::UserId;
use crate::log::{self, Logger};
use crate::log_error;
use crate::matching::conduct::{ConductStore, Cooldown, IncidentCounts, IncidentKind};
use crate::matching::elo::CUSTOM_CATEGORY;
use crate::store::{ConductKind, Db, Sanction, Store, StoreError};

/// How long a permanent ban is held in the lobby's cache (it is read again afterwards).
pub(crate) const PERMANENT_BAN_HOLD_MS: i64 = 86_400_000;

/// The end of a ban for the lobby: a permanent ban counts for a day.
pub(crate) fn ban_end(ban: &Sanction, now: i64) -> i64 {
    ban.ends_at.unwrap_or(now + PERMANENT_BAN_HOLD_MS)
}

/// The end of the user's ban stored in the database, if any.
pub(crate) async fn stored_ban(store: &Store, user: UserId, now: i64, log: &Logger) -> Option<i64> {
    match store.sanctions().active_ban(user, now).await {
        Ok(ban) => ban.map(|b| ban_end(&b, now)),
        Err(e) => {
            log_error!(log, "ban lookup failed", { "err": log::error(&e) });
            None
        }
    }
}

/// The user's rating and provisional mark in `category` (`INITIAL_RATING`, provisional, for a
/// custom time control).
pub(crate) async fn rating_of(
    store: &Store,
    config: &Config,
    user: UserId,
    category: &str,
    log: &Logger,
) -> (i64, bool) {
    if category != CUSTOM_CATEGORY && !category.is_empty() {
        match store.ratings().get(user, category.to_string()).await {
            Ok(r) => return (r.rating, r.is_provisional(config.provisional_games)),
            Err(e) => log_error!(log, "rating read failed", { "err": log::error(&e) }),
        }
    }
    (config.initial_rating, true)
}

/// The user's conduct state stored in the database (the default one when there is none).
pub(crate) async fn stored_cooldown(store: &Store, user: UserId, log: &Logger) -> Option<Cooldown> {
    match store.conduct().cooldown(user).await {
        Ok(c) => Some(Cooldown::normalized(c.until, c.level)),
        Err(e) => {
            log_error!(log, "conduct lookup failed", { "err": log::error(&e) });
            None
        }
    }
}

/// The conduct rules' view of a write transaction ([`crate::matching::conduct::record_incident`]).
pub(crate) struct DbConduct<'a, 'b> {
    pub db: &'a Db<'b>,
    /// The time stamped on a stored cooldown.
    pub now: i64,
}

fn conduct_kind(kind: IncidentKind) -> ConductKind {
    match kind {
        IncidentKind::Abandon => ConductKind::Abandon,
        IncidentKind::Abort => ConductKind::Abort,
        IncidentKind::NoShow => ConductKind::NoShow,
    }
}

impl ConductStore for DbConduct<'_, '_> {
    type Error = StoreError;

    fn record(&mut self, user_id: UserId, kind: IncidentKind, at: i64) -> Result<(), StoreError> {
        self.db.conduct().record(user_id, conduct_kind(kind), at)
    }

    fn count_since(&mut self, user_id: UserId, since: i64) -> Result<IncidentCounts, StoreError> {
        let c = self.db.conduct().count_since(user_id, since)?;
        let n = |v: i64| u32::try_from(v.max(0)).unwrap_or(u32::MAX);
        Ok(IncidentCounts { abandon: n(c.abandon), abort: n(c.abort), noshow: n(c.noshow) })
    }

    fn cooldown(&mut self, user_id: UserId) -> Result<Option<Cooldown>, StoreError> {
        let c = self.db.conduct().cooldown(user_id)?;
        Ok(Some(Cooldown::normalized(c.until, c.level)))
    }

    fn set_cooldown(&mut self, user_id: UserId, cooldown: Cooldown) -> Result<(), StoreError> {
        self.db.conduct().set_cooldown(user_id, cooldown.until, i64::from(cooldown.level), self.now)
    }
}
