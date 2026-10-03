//! Direct challenges and private games joined with a code. A pure state machine owned by the
//! lobby actor: no timer, no I/O; time is passed in and random draws come from an injected source
//! (the operating system's CSPRNG by default: private codes must not be guessable). DESIGN 5.4.
//!
//! Rules:
//! * rated only with an official category (`RatedRequiresOfficialTc`); custom time controls only
//!   with `ALLOW_CUSTOM_TIME_CONTROLS` (`InvalidTimeControl` otherwise); base 15..=10800 s and
//!   increment 0..=180 s as in the protocol (`InvalidTimeControl`);
//! * at most [`MAX_PENDING_OUTGOING`] pending challenges and private games per creator
//!   (`ChallengeLimit`), and one pending direct challenge per (creator, target) pair
//!   (`ChallengeLimit` too);
//! * no self-challenge (`CannotChallengeSelf`), neither by name nor by joining one's own code;
//! * a direct challenge lives `CHALLENGE_TTL_MS`, a private code `PRIVATE_GAME_TTL_MS`;
//!   [`Challenges::expire`] returns (and forgets) the expired ones, and accept, decline, cancel and
//!   join_code already refuse them in between;
//! * only the target accepts or declines a direct challenge; a private game is joined only with
//!   its code (accepting it by id is refused); only the creator cancels. Every refusal of a
//!   challenge that exists but is not the caller's is `ChallengeNotFound` (nothing leaks).
//!
//! The lobby resolves the target username into a [`TargetUser`] (presence and account) before
//! [`Challenges::create`], and checks bans, the unplayed-challenge limiter and the rated repeat
//! limit itself (see the module documentation of [`crate::matching`]).

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use indexmap::IndexSet;

use super::elo::{CUSTOM_CATEGORY, Categories};
use super::{ChallengeState, ColorPref, MatchError};
use crate::config::Config;
use crate::ids::{ConnId, UserId};
use crate::metrics::{self, Counter, CounterVec};

/// Pending outgoing challenges (direct and private) per creator.
pub const MAX_PENDING_OUTGOING: usize = 3;
/// Private game codes: 6 characters without 0/O and 1/I/L.
pub const CODE_ALPHABET: &str = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
/// Length of a private game code.
pub const CODE_LENGTH: usize = 6;
/// Shortest base time of a challenge, in seconds.
pub const MIN_BASE_SEC: i64 = 15;
/// Longest base time of a challenge, in seconds.
pub const MAX_BASE_SEC: i64 = 10800;
/// Largest increment of a challenge, in seconds.
pub const MAX_INC_SEC: i64 = 180;

static CREATED: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec(
        "scacelith_challenges_created_total",
        "Challenges and private games created",
        &["kind"],
    )
});
static ACCEPTED: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter("scacelith_challenges_accepted_total", "Challenges and private games accepted")
});
static EXPIRED: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter("scacelith_challenges_expired_total", "Challenges and private codes that expired")
});

/// Whitespace as JavaScript's `\s` and `String.prototype.trim` see it (the former server's
/// normalisation of typed codes and names).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// Normalises a code typed by a player (case, spaces and dashes are ignored); `None` when it
/// cannot be a code.
pub fn normalize_code(code: &str) -> Option<String> {
    let c: String = code.to_uppercase().chars().filter(|&ch| ch != '-' && !is_js_space(ch)).collect();
    (c.len() == CODE_LENGTH && c.bytes().all(|b| CODE_ALPHABET.as_bytes().contains(&b))).then_some(c)
}

/// A source of uniform integers in `[0, max)` (codes and colour draws).
pub type RandomInt = Box<dyn FnMut(u32) -> u32 + Send>;

/// A uniform integer in `[0, max)` from the operating system's CSPRNG (rejection sampling, no
/// modulo bias). `max` must not be 0.
///
/// # Panics
/// When the operating system's random source fails, which a server cannot work around: a weaker
/// fallback would make private codes guessable.
pub fn crypto_random_int(max: u32) -> u32 {
    assert!(max > 0, "crypto_random_int: max must be positive");
    let zone = u32::MAX / max * max;
    loop {
        let v = getrandom::u32().expect("the operating system's random source failed");
        if v < zone {
            return v % max;
        }
    }
}

/// The default random source: [`crypto_random_int`].
pub fn os_random_int() -> RandomInt {
    Box::new(crypto_random_int)
}

/// A direct challenge (to one user) or a private game (joined with its code).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChallengeKind {
    Direct,
    Private,
}

impl ChallengeKind {
    /// The metric label and former JSON value (`direct`, `private`).
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeKind::Direct => "direct",
            ChallengeKind::Private => "private",
        }
    }
}

/// A player taking part in a challenge; `rating` is the player's rating in the challenge's
/// category (the caller computes the category with [`Categories::category_of`] first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChallengePlayer {
    pub user_id: UserId,
    pub username: String,
    pub rating: i64,
    pub provisional: bool,
    pub conn_id: ConnId,
}

/// The target of a direct challenge, resolved by the lobby from the username.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetUser {
    pub user_id: UserId,
    pub username: String,
    pub accept_challenges: bool,
    pub online: bool,
}

/// A request to create a challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateRequest {
    /// The creator (a real user: `user_id` is not 0).
    pub from: ChallengePlayer,
    /// Target username; empty (after trimming) for a private game.
    pub target: String,
    /// The account of `target`, `None` when there is no such account.
    pub target_user: Option<TargetUser>,
    pub base_sec: i64,
    pub inc_sec: i64,
    pub rated: bool,
    /// Colour asked by the creator.
    pub color: ColorPref,
}

/// A challenge, pending or (as returned by the transitions) in its final state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    pub id: u32,
    pub kind: ChallengeKind,
    pub from: ChallengePlayer,
    /// Target username ('' for a private game).
    pub target: String,
    /// Target user (0 for a private game).
    pub target_user_id: UserId,
    /// Private game code ('' for a direct challenge).
    pub code: String,
    pub base_sec: u32,
    pub inc_sec: u32,
    pub base_ms: i64,
    pub inc_ms: i64,
    /// Official category id, or `custom`.
    pub category: String,
    pub rated: bool,
    /// Colour asked by the creator.
    pub color: ColorPref,
    /// Colour offered to the receiver (`ChallengeReceived.yourColor`).
    pub receiver_color: ColorPref,
    pub created_at: i64,
    pub expires_at: i64,
    pub state: ChallengeState,
}

/// The game to create for an accepted challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameSpec {
    pub white: ChallengePlayer,
    pub black: ChallengePlayer,
    pub base_ms: i64,
    pub inc_ms: i64,
    pub category: String,
    pub rated: bool,
    pub challenge_id: u32,
    /// Always 0 here (rematches are made by the lobby).
    pub rematch_of: u64,
}

/// An accepted challenge (state `Accepted`) and its game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub challenge: Challenge,
    pub game: GameSpec,
}

/// Pending, unexpired challenges of one user, in creation order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserChallenges<'a> {
    pub outgoing: Vec<&'a Challenge>,
    pub incoming: Vec<&'a Challenge>,
}

/// The challenge settings of the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChallengeSettings {
    /// ALLOW_CUSTOM_TIME_CONTROLS
    pub allow_custom_time_controls: bool,
    /// CHALLENGE_TTL_MS
    pub challenge_ttl_ms: i64,
    /// PRIVATE_GAME_TTL_MS
    pub private_game_ttl_ms: i64,
}

impl ChallengeSettings {
    /// The settings of a loaded configuration.
    pub fn from_config(config: &Config) -> ChallengeSettings {
        ChallengeSettings {
            allow_custom_time_controls: config.allow_custom_time_controls,
            challenge_ttl_ms: config.challenge_ttl_ms,
            private_game_ttl_ms: config.private_game_ttl_ms,
        }
    }
}

// A pending challenge and its creation serial (expiry queue entries name the serial, so that a
// reused id never matches a stale entry).
#[derive(Debug)]
struct Slot {
    serial: u64,
    challenge: Challenge,
}

/// Pending direct challenges and private games of the lobby.
pub struct Challenges {
    settings: ChallengeSettings,
    categories: Categories,
    random_int: RandomInt,
    by_id: HashMap<u32, Slot>,
    by_code: HashMap<String, u32>,
    outgoing: HashMap<UserId, IndexSet<u32>>,
    incoming: HashMap<UserId, IndexSet<u32>>,
    last_id: u32,
    serial: u64,
    // Expiry queues, each in expiry order because each kind has one TTL (lazy deletion).
    exp_direct: VecDeque<(u32, u64)>,
    exp_private: VecDeque<(u32, u64)>,
}

impl std::fmt::Debug for Challenges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Challenges").field("pending", &self.by_id.len()).finish()
    }
}

impl Challenges {
    /// Challenges over the official `categories`, drawing codes and colours with `random_int`.
    pub fn new(settings: ChallengeSettings, categories: Categories, random_int: RandomInt) -> Challenges {
        Challenges {
            settings,
            categories,
            random_int,
            by_id: HashMap::new(),
            by_code: HashMap::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            last_id: 0,
            serial: 0,
            exp_direct: VecDeque::new(),
            exp_private: VecDeque::new(),
        }
    }

    /// Challenges configured from `config`, with the operating system's CSPRNG.
    pub fn from_config(config: &Config) -> Challenges {
        Challenges::new(
            ChallengeSettings::from_config(config),
            Categories::from_config(config),
            os_random_int(),
        )
    }

    /// Pending challenges (expired ones included until [`Challenges::expire`] reports them).
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether no challenge is pending.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    fn next_id(&mut self) -> u32 {
        loop {
            self.last_id = if self.last_id == u32::MAX { 1 } else { self.last_id + 1 };
            if !self.by_id.contains_key(&self.last_id) {
                return self.last_id;
            }
        }
    }

    fn new_code(&mut self) -> String {
        let alphabet = CODE_ALPHABET.as_bytes();
        loop {
            let code: String = (0..CODE_LENGTH)
                .map(|_| {
                    char::from(alphabet[(self.random_int)(alphabet.len() as u32) as usize % alphabet.len()])
                })
                .collect();
            if !self.by_code.contains_key(&code) {
                return code;
            }
        }
    }

    /// Creates a direct challenge (non-empty target) or a private game (empty target).
    pub fn create(&mut self, req: CreateRequest, now: i64) -> Result<Challenge, MatchError> {
        debug_assert!(req.from.user_id != 0, "Challenges::create: from.user_id required");
        let (base_sec, inc_sec) = (req.base_sec, req.inc_sec);
        if !(MIN_BASE_SEC..=MAX_BASE_SEC).contains(&base_sec) || !(0..=MAX_INC_SEC).contains(&inc_sec) {
            return Err(MatchError::InvalidTimeControl);
        }
        let (base_ms, inc_ms) = (base_sec * 1000, inc_sec * 1000);
        let category = self.categories.category_of(base_ms, inc_ms).to_string();
        if category == CUSTOM_CATEGORY {
            if req.rated {
                return Err(MatchError::RatedRequiresOfficialTc);
            }
            if !self.settings.allow_custom_time_controls {
                return Err(MatchError::InvalidTimeControl);
            }
        }
        let target = req.target.trim_matches(is_js_space);
        let is_private = target.is_empty();
        let (mut target_user_id, mut target_name) = (0, String::new());
        if !is_private {
            let from_name = &req.from.username;
            if !from_name.is_empty() && target.to_lowercase() == from_name.to_lowercase() {
                return Err(MatchError::CannotChallengeSelf);
            }
            let tu = match &req.target_user {
                Some(tu) if tu.user_id != 0 => tu,
                _ => return Err(MatchError::UserUnavailable),
            };
            if tu.user_id == req.from.user_id {
                return Err(MatchError::CannotChallengeSelf);
            }
            if !tu.online || !tu.accept_challenges {
                return Err(MatchError::UserUnavailable);
            }
            target_user_id = tu.user_id;
            target_name = if tu.username.is_empty() { target.to_string() } else { tu.username.clone() };
        }
        if let Some(mine) = self.outgoing.get_mut(&req.from.user_id) {
            // Expired entries do not count; the next expire() reports them.
            let by_id = &self.by_id;
            mine.retain(|id| by_id.get(id).is_some_and(|s| s.challenge.expires_at > now));
            if mine.len() >= MAX_PENDING_OUTGOING {
                return Err(MatchError::ChallengeLimit);
            }
            if !is_private && mine.iter().any(|id| by_id[id].challenge.target_user_id == target_user_id) {
                return Err(MatchError::ChallengeLimit);
            }
        }
        let ttl = if is_private { self.settings.private_game_ttl_ms } else { self.settings.challenge_ttl_ms };
        let id = self.next_id();
        let code = if is_private { self.new_code() } else { String::new() };
        let kind = if is_private { ChallengeKind::Private } else { ChallengeKind::Direct };
        let challenge = Challenge {
            id,
            kind,
            from: req.from,
            target: target_name,
            target_user_id,
            code,
            base_sec: base_sec as u32,
            inc_sec: inc_sec as u32,
            base_ms,
            inc_ms,
            category,
            rated: req.rated,
            color: req.color,
            receiver_color: req.color.opposite(),
            created_at: now,
            expires_at: now + ttl,
            state: ChallengeState::Pending,
        };
        self.serial += 1;
        let serial = self.serial;
        self.outgoing.entry(challenge.from.user_id).or_default().insert(id);
        if is_private {
            self.by_code.insert(challenge.code.clone(), id);
            self.exp_private.push_back((id, serial));
        } else {
            self.incoming.entry(target_user_id).or_default().insert(id);
            self.exp_direct.push_back((id, serial));
        }
        CREATED.with(&[kind.as_str()]).inc();
        self.by_id.insert(id, Slot { serial, challenge: challenge.clone() });
        Ok(challenge)
    }

    /// The target accepts a direct challenge.
    pub fn accept(&mut self, id: u32, by: ChallengePlayer, now: i64) -> Result<Started, MatchError> {
        let c = self
            .get(id, now)
            .filter(|c| c.kind == ChallengeKind::Direct)
            .ok_or(MatchError::ChallengeNotFound)?;
        if by.user_id == c.from.user_id {
            return Err(MatchError::CannotChallengeSelf);
        }
        if by.user_id != c.target_user_id {
            return Err(MatchError::ChallengeNotFound);
        }
        Ok(self.start(id, by))
    }

    /// Joins a private game with its code (as typed: case, spaces and dashes are ignored).
    pub fn join_code(&mut self, code: &str, by: ChallengePlayer, now: i64) -> Result<Started, MatchError> {
        let c = self.get_code(code, now).ok_or(MatchError::CodeInvalid)?;
        if by.user_id == c.from.user_id {
            return Err(MatchError::CannotChallengeSelf);
        }
        let id = c.id;
        Ok(self.start(id, by))
    }

    /// The target declines a direct challenge.
    pub fn decline(&mut self, id: u32, user_id: UserId, now: i64) -> Result<Challenge, MatchError> {
        match self.get(id, now) {
            Some(c) if c.kind == ChallengeKind::Direct && c.target_user_id == user_id => {
                Ok(self.forget(id, ChallengeState::Declined))
            }
            _ => Err(MatchError::ChallengeNotFound),
        }
    }

    /// The creator withdraws a challenge or a private game.
    pub fn cancel(&mut self, id: u32, user_id: UserId, now: i64) -> Result<Challenge, MatchError> {
        match self.get(id, now) {
            Some(c) if c.from.user_id == user_id => Ok(self.forget(id, ChallengeState::Cancelled)),
            _ => Err(MatchError::ChallengeNotFound),
        }
    }

    /// Forgets every challenge whose time is over and returns them (state `Expired`), direct
    /// challenges first. O(expired) amortised.
    pub fn expire(&mut self, now: i64) -> Vec<Challenge> {
        let mut out = Vec::new();
        for private in [false, true] {
            loop {
                let queue = if private { &self.exp_private } else { &self.exp_direct };
                let Some(&(id, serial)) = queue.front() else { break };
                if let Some(slot) = self.by_id.get(&id).filter(|s| s.serial == serial) {
                    if slot.challenge.expires_at > now {
                        break;
                    }
                    out.push(self.forget(id, ChallengeState::Expired));
                }
                if private {
                    self.exp_private.pop_front();
                } else {
                    self.exp_direct.pop_front();
                }
            }
        }
        if !out.is_empty() {
            EXPIRED.add(out.len() as u64);
        }
        out
    }

    /// Pending, unexpired challenges of a user.
    pub fn for_user(&self, user_id: UserId, now: i64) -> UserChallenges<'_> {
        let pick = |ids: Option<&IndexSet<u32>>| -> Vec<&Challenge> {
            ids.into_iter()
                .flatten()
                .filter_map(|id| self.by_id.get(id).map(|s| &s.challenge))
                .filter(|c| c.expires_at > now)
                .collect()
        };
        UserChallenges {
            outgoing: pick(self.outgoing.get(&user_id)),
            incoming: pick(self.incoming.get(&user_id)),
        }
    }

    /// A pending, unexpired challenge by id.
    pub fn get(&self, id: u32, now: i64) -> Option<&Challenge> {
        self.by_id.get(&id).map(|s| &s.challenge).filter(|c| c.expires_at > now)
    }

    /// The pending, unexpired private game of a code (as typed); the code stays usable.
    pub fn get_code(&self, code: &str, now: i64) -> Option<&Challenge> {
        let id = self.by_code.get(&normalize_code(code)?)?;
        self.get(*id, now)
    }

    /// The user went offline or was banned: outgoing challenges are cancelled, incoming ones
    /// become `Unavailable`. Returns the affected challenges.
    pub fn drop_user(&mut self, user_id: UserId) -> Vec<Challenge> {
        let mut out = Vec::new();
        for (incoming, state) in [(false, ChallengeState::Cancelled), (true, ChallengeState::Unavailable)] {
            let map = if incoming { &self.incoming } else { &self.outgoing };
            let ids: Vec<u32> = map.get(&user_id).map(|s| s.iter().copied().collect()).unwrap_or_default();
            for id in ids {
                if self.by_id.contains_key(&id) {
                    out.push(self.forget(id, state));
                }
            }
        }
        out
    }

    fn start(&mut self, id: u32, by: ChallengePlayer) -> Started {
        let color = self.by_id[&id].challenge.color;
        let creator_white = match color {
            ColorPref::White => true,
            ColorPref::Black => false,
            ColorPref::Random => (self.random_int)(2) == 0,
        };
        let challenge = self.forget(id, ChallengeState::Accepted);
        ACCEPTED.inc();
        let creator = challenge.from.clone();
        let (white, black) = if creator_white { (creator, by) } else { (by, creator) };
        let game = GameSpec {
            white,
            black,
            base_ms: challenge.base_ms,
            inc_ms: challenge.inc_ms,
            category: challenge.category.clone(),
            rated: challenge.rated,
            challenge_id: challenge.id,
            rematch_of: 0,
        };
        Started { challenge, game }
    }

    // Removes a pending challenge from every index and returns it in its final state.
    fn forget(&mut self, id: u32, state: ChallengeState) -> Challenge {
        let mut c = self.by_id.remove(&id).expect("forgotten challenges are pending").challenge;
        c.state = state;
        if !c.code.is_empty() {
            self.by_code.remove(&c.code);
        }
        remove_from(&mut self.outgoing, c.from.user_id, id);
        if c.target_user_id != 0 {
            remove_from(&mut self.incoming, c.target_user_id, id);
        }
        c
    }
}

fn remove_from(map: &mut HashMap<UserId, IndexSet<u32>>, key: UserId, id: u32) {
    if let Some(set) = map.get_mut(&key) {
        set.shift_remove(&id);
        if set.is_empty() {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests;
