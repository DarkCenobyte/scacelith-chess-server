//! Conduct: abandoned, aborted and no-show games pause rated matchmaking (DESIGN 6.4).
//!
//! Rules:
//! * every incident is recorded;
//! * when the incidents of the last 24 h (abandon + abort + no-show) reach
//!   `CONDUCT_ABANDON_LIMIT`, rated matchmaking is paused: 15 min at level 0, then 1 h, then 6 h
//!   (and 6 h for every further offence). Each new incident while the 24 h count is at or over
//!   the limit is a repeated offence and starts the next cooldown from now; an active cooldown is
//!   never shortened;
//! * the level (number of cooldowns in the current series, at most 3) decays by one for every
//!   full 24 h without any incident; the decay is applied (and persisted) at the next incident,
//!   the only moment the level is used;
//! * direct challenges and casual games stay possible: the lobby checks the cooldown before a
//!   rated queue join and a rated rematch only.
//!
//! The incidents and the cooldown live in the store so that they survive restarts. The store
//! side is the [`ConductStore`] trait, which the store implements on one transaction: the lobby
//! runs [`record_incident`] inside a single write job, then caches the outcome with
//! [`Conduct::remember`]. The cooldown reads of rated queue joins are served by the [`Conduct`]
//! cache; on a miss the lobby reads [`ConductStore::cooldown`] and remembers the row.

use std::collections::HashMap;
use std::fmt;

use crate::ids::UserId;

/// Cooldown lengths by level.
pub const CONDUCT_COOLDOWNS_MS: [i64; 3] = [15 * 60_000, 60 * 60_000, 6 * 3_600_000];
/// Window in which incidents are counted, and decay period of the level.
pub const CONDUCT_WINDOW_MS: i64 = 24 * 3_600_000;
/// Highest level (number of cooldown lengths).
pub const MAX_LEVEL: u8 = CONDUCT_COOLDOWNS_MS.len() as u8;
/// Cached cooldown states before the cache is trimmed.
pub const CACHE_MAX: usize = 200_000;

/// Kind of a conduct incident.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IncidentKind {
    /// The player lost a running game by abandonment (grace expired, DESIGN 6.4).
    Abandon,
    /// The player aborted a game.
    Abort,
    /// The player let a game be aborted without making a first move (`NoShow`, DESIGN 6.1).
    NoShow,
}

impl IncidentKind {
    /// Every kind, in the store's order.
    pub const ALL: [IncidentKind; 3] = [IncidentKind::Abandon, IncidentKind::Abort, IncidentKind::NoShow];

    /// The stored name (`abandon`, `abort`, `noshow`).
    pub fn as_str(self) -> &'static str {
        match self {
            IncidentKind::Abandon => "abandon",
            IncidentKind::Abort => "abort",
            IncidentKind::NoShow => "noshow",
        }
    }

    /// The kind of a stored name.
    pub fn parse(s: &str) -> Option<IncidentKind> {
        IncidentKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl fmt::Display for IncidentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Incidents of one user since some time, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncidentCounts {
    pub abandon: u32,
    pub abort: u32,
    pub noshow: u32,
}

impl IncidentCounts {
    /// All kinds together.
    pub fn total(&self) -> i64 {
        i64::from(self.abandon) + i64::from(self.abort) + i64::from(self.noshow)
    }
}

/// Cooldown state of a user: end of the current pause (0 or past when none) and level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cooldown {
    pub until: i64,
    pub level: u8,
}

impl Cooldown {
    /// A state read from storage, with the level clamped to 0..=[`MAX_LEVEL`].
    pub fn normalized(until: i64, level: i64) -> Cooldown {
        Cooldown { until, level: level.clamp(0, i64::from(MAX_LEVEL)) as u8 }
    }

    /// End of the pause, or 0 when it is over at `now`.
    pub fn active_until(&self, now: i64) -> i64 {
        if self.until > now { self.until } else { 0 }
    }
}

/// The conduct side of the store, implemented on one transaction.
pub trait ConductStore {
    /// Storage failure.
    type Error;

    /// Records one incident.
    fn record(&mut self, user_id: UserId, kind: IncidentKind, at: i64) -> Result<(), Self::Error>;

    /// Incidents of a user with `at >= since`.
    fn count_since(&mut self, user_id: UserId, since: i64) -> Result<IncidentCounts, Self::Error>;

    /// The stored cooldown state of a user, `None` when there is none.
    fn cooldown(&mut self, user_id: UserId) -> Result<Option<Cooldown>, Self::Error>;

    /// Stores the cooldown state of a user.
    fn set_cooldown(&mut self, user_id: UserId, cooldown: Cooldown) -> Result<(), Self::Error>;
}

/// Result of [`record_incident`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncidentOutcome {
    /// End of the pause after the incident, 0 when there is none.
    pub until: i64,
    /// Level after the incident.
    pub level: u8,
    /// Incidents of the last 24 h, this one included.
    pub incidents: i64,
    /// Whether this incident started a cooldown (the lobby sends `Notice{MatchmakingCooldown}`
    /// and logs the `conduct_cooldown` security event).
    pub started: bool,
    /// The state to cache with [`Conduct::remember`].
    pub state: Cooldown,
}

/// Records an incident and starts a cooldown when the 24 h count reaches `limit`
/// (`CONDUCT_ABANDON_LIMIT`). Runs on one store transaction: the previous state is read from the
/// store itself, so a change made by an administrator is honoured even when the cache is stale.
pub fn record_incident<S: ConductStore + ?Sized>(
    store: &mut S,
    user_id: UserId,
    kind: IncidentKind,
    limit: i64,
    now: i64,
) -> Result<IncidentOutcome, S::Error> {
    let prev = store
        .cooldown(user_id)?
        .map_or(Cooldown::default(), |c| Cooldown::normalized(c.until, c.level.into()));
    let mut level = prev.level;
    // Decay: one level per full 24 h without incident before this one.
    let mut days = 1;
    while level > 0 && days <= i64::from(MAX_LEVEL) {
        if store.count_since(user_id, now - days * CONDUCT_WINDOW_MS)?.total() > 0 {
            break;
        }
        level -= 1;
        days += 1;
    }
    store.record(user_id, kind, now)?;
    let incidents = store.count_since(user_id, now - CONDUCT_WINDOW_MS)?.total();
    let mut until = prev.until;
    let mut started = false;
    if incidents >= limit {
        let end = now + CONDUCT_COOLDOWNS_MS[usize::from(level.min(MAX_LEVEL - 1))];
        until = until.max(end);
        level = (level + 1).min(MAX_LEVEL);
        started = true;
    }
    let state = Cooldown { until, level };
    if started || level != prev.level {
        store.set_cooldown(user_id, state)?;
    }
    Ok(IncidentOutcome { until: state.active_until(now), level, incidents, started, state })
}

/// Cache of the cooldown states read at every rated queue join.
#[derive(Debug)]
pub struct Conduct {
    cache: HashMap<UserId, Cooldown>,
    cap: usize,
}

impl Default for Conduct {
    fn default() -> Self {
        Conduct::new()
    }
}

impl Conduct {
    /// An empty cache holding up to [`CACHE_MAX`] states.
    pub fn new() -> Conduct {
        Conduct::with_cap(CACHE_MAX)
    }

    fn with_cap(cap: usize) -> Conduct {
        Conduct { cache: HashMap::new(), cap }
    }

    /// End of the rated matchmaking pause of a user (0 when there is none), or `None` when the
    /// user's state is not cached: the lobby then reads [`ConductStore::cooldown`] and calls
    /// [`Conduct::remember`] (with the default state when the store has none).
    pub fn cooldown_until(&self, user_id: UserId, now: i64) -> Option<i64> {
        self.cache.get(&user_id).map(|c| c.active_until(now))
    }

    /// The cached state of a user.
    pub fn state(&self, user_id: UserId) -> Option<Cooldown> {
        self.cache.get(&user_id).copied()
    }

    /// Caches the state of a user (read from the store, or [`IncidentOutcome::state`]). When the
    /// cache is full, the states without an active cooldown or a level are dropped first (cheap
    /// to read again), then everything.
    pub fn remember(&mut self, user_id: UserId, state: Cooldown, now: i64) {
        if self.cache.len() >= self.cap && !self.cache.contains_key(&user_id) {
            self.cache.retain(|_, s| s.until > now || s.level != 0);
            if self.cache.len() >= self.cap {
                self.cache.clear();
            }
        }
        self.cache.insert(user_id, state);
    }

    /// Forgets the cached state of a user (after a change made in the database).
    pub fn invalidate(&mut self, user_id: UserId) {
        self.cache.remove(&user_id);
    }

    /// Forgets every cached state.
    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    /// Cached states.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether no state is cached.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests;
