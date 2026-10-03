//! Pure matchmaking logic owned by the lobby actor: FIDE Elo ([`elo`]), the matchmaker queues
//! ([`matchmaker`]), direct challenges and private codes ([`challenges`]) and conduct cooldowns
//! ([`conduct`]). DESIGN 5.4, 6.3, 6.4 and 6.6.
//!
//! Every type here is a plain state machine: no timer, no I/O, no lock. Time is passed in as
//! integer milliseconds (`now`), random draws come from injected sources, and the store
//! interactions of the conduct rules are expressed as a trait the caller implements on its
//! transaction. The lobby actor owns one instance of each and turns their results into frames.
//!
//! # What the lobby does around these modules
//!
//! Matchmaking (the former control plane, cluster notes 5.5):
//! * `QueueJoin`, checked in this order: the request comes from the user's live connection
//!   (else `QueueNotAllowed`), not banned (`Banned`), not busy (in a game or starting one:
//!   `AlreadyInGame`), official category (`InvalidCategory`), and for a rated queue no conduct
//!   cooldown ([`conduct::Conduct::cooldown_until`]: send `Notice{MatchmakingCooldown, arg:
//!   until}` then answer `MatchmakingCooldown`). A user already queued leaves first (no `Left`
//!   status), then [`matchmaker::Matchmaker::join`] with `joined_at = now`; on success answer the
//!   request and send the first `QueueStatus{Searching}` ([`matchmaker::Matchmaker::status_of`]).
//! * Every [`matchmaker::QUEUE_REFRESH_MS`] (3 s) every queued user gets a fresh
//!   `QueueStatus{Searching}` ([`matchmaker::Matchmaker::statuses`]).
//! * `QueueLeave`: [`matchmaker::Matchmaker::leave`]; when the user was queued, send
//!   `QueueStatus{Left, waitMs 0, window 0, queued 0}`; always answer `Ack`. A disconnection or a
//!   replaced connection leaves silently.
//! * Every `MATCH_TICK_MS` call [`matchmaker::Matchmaker::tick`]. For each pairing send both
//!   players `QueueStatus{Matched, waitMs: now - joinedAt, window 0, queued 0}` before creating
//!   the game. A rated game, once created (queue, challenge, private code or rematch), is counted
//!   with [`matchmaker::Matchmaker::record_pairing`]. When the game cannot be created, give the
//!   colours back with `record_colors(black, white)`, hold the pair for
//!   [`matchmaker::PAIR_RETRY_DELAY_MS`] (`hold_pair`), and put each player still connected,
//!   idle and not banned back with [`matchmaker::PairedPlayer::rejoin_request`] (the others get
//!   `QueueStatus{Left}`).
//!
//! Challenges (cluster notes 5.7): ban check, the `CHALLENGE_UNPLAYED_PER_MIN` limiter and the
//! repeat limit (`RatedRepeatLimit` for a rated official challenge to a target past
//! [`matchmaker::Matchmaker::repeat_limited`], checked before [`challenges::Challenges::create`],
//! and for a pending private code before [`challenges::Challenges::join_code`]) belong to the
//! lobby, as do the `ChallengeStatus` / `ChallengeReceived` frames, the expiry sweep every second
//! ([`challenges::Challenges::expire`]) and [`challenges::Challenges::drop_user`] when a user goes
//! offline or is banned.
//!
//! Conduct (DESIGN 6.4): when a game ends with an abandonment, an abort or a no-show, run
//! [`conduct::record_incident`] inside one store write job and cache the outcome with
//! [`conduct::Conduct::remember`]; when it started a cooldown, send `Notice{MatchmakingCooldown,
//! arg: until}` to the user.

pub mod challenges;
pub mod conduct;
pub mod elo;
pub mod matchmaker;

use std::fmt;

/// Refusals of the matching modules, with the protocol's `ErrorCode` numbers ([`MatchError::code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MatchError {
    /// Not an official category (matchmaking).
    InvalidCategory = 107,
    /// Already queued (leave first).
    QueueNotAllowed = 200,
    /// No such challenge, or not one the caller may act on (nothing leaks).
    ChallengeNotFound = 201,
    /// The target is unknown, offline or refuses challenges.
    UserUnavailable = 202,
    /// Too many pending challenges, or one already pending for this target.
    ChallengeLimit = 203,
    /// A challenge to oneself, or one's own code.
    CannotChallengeSelf = 204,
    /// No pending private game has this code.
    CodeInvalid = 205,
    /// A rated challenge needs an official time control.
    RatedRequiresOfficialTc = 206,
    /// Rated matchmaking is paused for this user (conduct).
    MatchmakingCooldown = 207,
    /// Time control out of bounds, or custom while custom time controls are disabled.
    InvalidTimeControl = 208,
    /// The rematch cannot be made.
    RematchUnavailable = 209,
    /// The two players played `MATCH_REPEAT_LIMIT` rated games together recently.
    RatedRepeatLimit = 210,
}

impl MatchError {
    /// The protocol's `ErrorCode` value.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The protocol's `ErrorCode` name.
    pub fn name(self) -> &'static str {
        match self {
            MatchError::InvalidCategory => "InvalidCategory",
            MatchError::QueueNotAllowed => "QueueNotAllowed",
            MatchError::ChallengeNotFound => "ChallengeNotFound",
            MatchError::UserUnavailable => "UserUnavailable",
            MatchError::ChallengeLimit => "ChallengeLimit",
            MatchError::CannotChallengeSelf => "CannotChallengeSelf",
            MatchError::CodeInvalid => "CodeInvalid",
            MatchError::RatedRequiresOfficialTc => "RatedRequiresOfficialTc",
            MatchError::MatchmakingCooldown => "MatchmakingCooldown",
            MatchError::InvalidTimeControl => "InvalidTimeControl",
            MatchError::RematchUnavailable => "RematchUnavailable",
            MatchError::RatedRepeatLimit => "RatedRepeatLimit",
        }
    }
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl std::error::Error for MatchError {}

/// Colour asked by a challenge's creator (the protocol's `ColorPref`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColorPref {
    #[default]
    Random = 0,
    White = 1,
    Black = 2,
}

impl ColorPref {
    /// The protocol value; any other value than White or Black reads as Random.
    pub fn from_u8(v: u8) -> ColorPref {
        match v {
            1 => ColorPref::White,
            2 => ColorPref::Black,
            _ => ColorPref::Random,
        }
    }

    /// The colour offered to the other player: White and Black swap, Random stays.
    pub fn opposite(self) -> ColorPref {
        match self {
            ColorPref::White => ColorPref::Black,
            ColorPref::Black => ColorPref::White,
            ColorPref::Random => ColorPref::Random,
        }
    }
}

/// State of a matchmaking search (the protocol's `QueueState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum QueueState {
    Left = 0,
    Searching = 1,
    Matched = 2,
}

/// State of a challenge (the protocol's `ChallengeState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChallengeState {
    Pending = 0,
    Accepted = 1,
    Declined = 2,
    Cancelled = 3,
    Expired = 4,
    Unavailable = 5,
}
