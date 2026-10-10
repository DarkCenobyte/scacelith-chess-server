//! What waits for a host actor, and its bounds. The inbox is one channel, so that a connection's
//! frames and its detach stay ordered; what it holds is counted, and every message is admitted at
//! the door under the budget of its [`Class`], so that a host that falls behind (an overloaded
//! CPU, a stalled disk under its journal) holds a bounded amount of work:
//!
//! * from [`INBOX_BUSY`] messages waiting, or [`JOURNAL_PENDING_BUSY`] bytes or more of journal
//!   records not handed to its I/O thread, the host is busy: no new game is placed on it, and
//!   once every host of the range is busy, new games and the lobby requests that would create one
//!   are refused with `RateLimited` until the hosts catch up. The games in progress go on. This is
//!   the admission signal: it keeps new games out, it caps nothing by itself;
//! * a cosmetic message is dropped at the door (nothing answers it, nothing depends on it): a
//!   gesture beyond [`GESTURE_INBOX_MAX`] gestures waiting (the player's next one supersedes it,
//!   and the opponent's client copes with missing ones, as with those dropped for a slow link),
//!   and any gesture, stance, round-trip measurement or rematch decline (the window closes on its
//!   own timer) while [`INBOX_MAX`] messages wait;
//! * a game request (Move, DrawOffer, Resync, Rematch) is refused while [`INBOX_MAX`] messages
//!   wait: its connection gets `Error{RateLimited}` with the request's `seq` at once, so the
//!   client knows it was not played, and may send it again;
//! * a request that ends a game (Resign, Abort, DrawAnswer, DrawClaim) may also use the
//!   [`INBOX_RESERVE`] messages above [`INBOX_MAX`], and is refused the same way beyond them:
//!   a player can still end a game on a host far behind;
//! * a lifecycle message (a connection attached or detached, a game created or cancelled, a
//!   sanction's forfeit, the stats, the shutdown) is never refused: losing it would leave a
//!   player bound to a closed connection or lose a game. Its sources are bounded: the lobby
//!   creates games only on hosts that are not busy, with its own requests in flight bounded per
//!   connection; one stats or shutdown request per caller at a time; and the attaches and
//!   detaches are counted per player and game, not per connection: while [`LINK_PENDING_MAX`]
//!   of them wait for one player's game (an earlier connection's attach and detach), a newer
//!   attach is not posted but waits in its connection, which tries it again
//!   (`HostHandle::attach`). A player who reconnects over and over to a host that does not catch
//!   up therefore adds nothing to its inbox: what waits for a player's game is at most
//!   [`LINK_PENDING_MAX`] messages and the detach of each of the player's connections still
//!   attached to it, whatever the number of reconnections. One beyond [`INBOX_MAX`] +
//!   [`INBOX_RESERVE`] is still delivered, and counted (`scacelith_game_inbox_over_reserve_total`).
//!
//! The inbox therefore holds at most [`INBOX_MAX`] cosmetic messages and requests, and
//! [`INBOX_MAX`] + [`INBOX_RESERVE`] messages while the lifecycle sources keep their bounds. The
//! refusals are counted by kind (`scacelith_game_inbox_refused_total{kind}`, the gestures in
//! `scacelith_gestures_dropped_total{reason="overload"}`) and logged once per episode by the
//! host's beat. The counts are exported every beat (`scacelith_game_inbox_messages` and
//! `scacelith_journal_pending_bytes`, per shard).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::metrics;
use crate::ids::{GameId, UserId};

/// Gestures that may wait in a host's inbox: about 170 ms of the gestures of 6,000 games whose
/// players all move all the time (4 per second each). A host that far behind drops the stale ones
/// instead of relaying them late.
pub const GESTURE_INBOX_MAX: usize = 8192;

/// Messages waiting in a host's inbox from which it takes no new game (see the module
/// documentation).
pub const INBOX_BUSY: usize = 16_384;

/// Messages waiting in a host's inbox from which the cosmetic messages are dropped and the game
/// requests refused (see the module documentation): twice [`INBOX_BUSY`], room for the requests
/// of the games already running once the host took no new one.
pub const INBOX_MAX: usize = 2 * INBOX_BUSY;

/// Messages above [`INBOX_MAX`] that only the requests ending a game and the lifecycle messages
/// may take (see the module documentation): the attach and detach of 8,192 reconnecting players.
pub const INBOX_RESERVE: usize = INBOX_BUSY;

/// Attach and detach messages of one player's game that may wait in a host's inbox before a newer
/// attach of that player's game waits in its connection instead (see the module documentation):
/// those of one earlier connection, its attach and its detach. A player who comes back while the
/// host lags behind is attached at once; one who comes back again before the host handled the
/// first return waits for it.
pub const LINK_PENDING_MAX: usize = 2;

/// Journal bytes not handed to the I/O thread from which a host takes no new game: minutes of
/// records of a busy shard whose disk does not answer.
pub const JOURNAL_PENDING_BUSY: usize = 16 << 20;

/// The budget a message is admitted under (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    /// Dropped while [`INBOX_MAX`] messages wait (a gesture also beyond [`GESTURE_INBOX_MAX`]
    /// gestures).
    Cosmetic(Refusal),
    /// A game request: refused while [`INBOX_MAX`] messages wait.
    Request,
    /// A request that ends a game: refused while [`INBOX_MAX`] + [`INBOX_RESERVE`] wait.
    Ending,
    /// Never refused.
    Lifecycle,
}

/// What was refused at the door of an inbox (label of `scacelith_game_inbox_refused_total`; the
/// gestures are counted as `scacelith_gestures_dropped_total{reason="overload"}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// A gesture (dropped).
    Gesture,
    /// A stance (dropped).
    Stance,
    /// A round-trip measurement (dropped).
    Rtt,
    /// A rematch decline (dropped: the window closes on its timer).
    RematchDecline,
    /// A game request (answered `Error{RateLimited}`).
    Request,
    /// A request that ends a game (answered `Error{RateLimited}`).
    Ending,
}

impl Refusal {
    /// Every kind, in label order.
    pub const ALL: [Refusal; 6] = [
        Refusal::Gesture,
        Refusal::Stance,
        Refusal::Rtt,
        Refusal::RematchDecline,
        Refusal::Request,
        Refusal::Ending,
    ];

    /// The metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Gesture => "gesture",
            Refusal::Stance => "stance",
            Refusal::Rtt => "rtt",
            Refusal::RematchDecline => "rematch_decline",
            Refusal::Request => "request",
            Refusal::Ending => "ending",
        }
    }

    fn index(self) -> usize {
        Refusal::ALL.iter().position(|&r| r == self).unwrap_or(0)
    }
}

/// The counts of what waits for one host, shared by its handles and its actor.
#[derive(Debug, Default)]
pub(super) struct Backlog {
    /// Messages posted and not taken by the actor yet.
    messages: AtomicUsize,
    /// Gestures among them.
    gestures: AtomicUsize,
    /// Messages refused at the door since the host started, by [`Refusal`] kind.
    refused: [AtomicU64; 6],
    /// Lifecycle messages admitted beyond [`INBOX_MAX`] + [`INBOX_RESERVE`] since the host
    /// started.
    over_reserve: AtomicU64,
    /// Attach and detach messages waiting, by player's game (an entry only while one waits).
    links: Mutex<HashMap<(GameId, UserId), usize>>,
    /// Attaches not posted since the host started: [`LINK_PENDING_MAX`] messages of the same
    /// player's game were waiting.
    attaches_deferred: AtomicU64,
    /// Journal bytes not handed to the I/O thread, as of the latest beat.
    journal_pending: AtomicUsize,
}

impl Backlog {
    /// A message of `class` is about to be posted: false when it is refused (counted).
    pub(super) fn admit(&self, class: Class) -> bool {
        let (limit, refusal) = match class {
            Class::Cosmetic(r) => (INBOX_MAX, Some(r)),
            Class::Request => (INBOX_MAX, Some(Refusal::Request)),
            Class::Ending => (INBOX_MAX + INBOX_RESERVE, Some(Refusal::Ending)),
            Class::Lifecycle => (usize::MAX, None),
        };
        let gesture = class == Class::Cosmetic(Refusal::Gesture);
        if gesture && self.gestures.fetch_add(1, Ordering::Relaxed) >= GESTURE_INBOX_MAX {
            self.gestures.fetch_sub(1, Ordering::Relaxed);
            self.refuse(Refusal::Gesture);
            return false;
        }
        let waiting = self.messages.fetch_add(1, Ordering::Relaxed);
        if waiting >= limit {
            self.messages.fetch_sub(1, Ordering::Relaxed);
            if gesture {
                self.gestures.fetch_sub(1, Ordering::Relaxed);
            }
            self.refuse(refusal.unwrap_or(Refusal::Request));
            return false;
        }
        if class == Class::Lifecycle && waiting >= INBOX_MAX + INBOX_RESERVE {
            self.over_reserve.fetch_add(1, Ordering::Relaxed);
            metrics::inbox_over_reserve();
        }
        true
    }

    fn refuse(&self, kind: Refusal) {
        self.refused[kind.index()].fetch_add(1, Ordering::Relaxed);
        if kind == Refusal::Gesture {
            metrics::gesture_overload();
        } else {
            metrics::inbox_refused(kind);
        }
    }

    /// An attach of `user` to `game` is about to be posted: false (nothing counted) while
    /// [`LINK_PENDING_MAX`] attach and detach messages of that player's game wait (counted as
    /// deferred: the connection tries again).
    pub(super) fn admit_attach(&self, game: GameId, user: UserId) -> bool {
        let mut links = self.links.lock();
        let waiting = links.entry((game, user)).or_insert(0);
        if *waiting >= LINK_PENDING_MAX {
            drop(links);
            self.attaches_deferred.fetch_add(1, Ordering::Relaxed);
            metrics::attach_deferred();
            return false;
        }
        *waiting += 1;
        true
    }

    /// A detach of `user` from `game` is about to be posted (never refused).
    pub(super) fn admit_detach(&self, game: GameId, user: UserId) {
        *self.links.lock().entry((game, user)).or_insert(0) += 1;
    }

    /// A message left the inbox: taken by the actor, or never posted (the host is gone).
    /// `link`: the player's game of an attach or a detach.
    pub(super) fn taken(&self, gesture: bool, link: Option<(GameId, UserId)>) {
        self.messages.fetch_sub(1, Ordering::Relaxed);
        if gesture {
            self.gestures.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(key) = link {
            let mut links = self.links.lock();
            if let Some(waiting) = links.get_mut(&key) {
                *waiting = waiting.saturating_sub(1);
                if *waiting == 0 {
                    links.remove(&key);
                }
            }
        }
    }

    /// Attach and detach messages of `user`'s `game` waiting.
    #[cfg(test)]
    pub(super) fn links(&self, game: GameId, user: UserId) -> usize {
        self.links.lock().get(&(game, user)).copied().unwrap_or(0)
    }

    /// Player's games with an attach or a detach waiting.
    #[cfg(test)]
    pub(super) fn linked_games(&self) -> usize {
        self.links.lock().len()
    }

    /// Attaches not posted since the host started (their connections tried them again).
    pub(super) fn attaches_deferred(&self) -> u64 {
        self.attaches_deferred.load(Ordering::Relaxed)
    }

    /// Messages waiting.
    pub(super) fn messages(&self) -> usize {
        self.messages.load(Ordering::Relaxed)
    }

    /// Gestures waiting.
    pub(super) fn gestures(&self) -> usize {
        self.gestures.load(Ordering::Relaxed)
    }

    /// Messages of `kind` refused at the door since the host started.
    pub(super) fn refused(&self, kind: Refusal) -> u64 {
        self.refused[kind.index()].load(Ordering::Relaxed)
    }

    /// Messages refused at the door since the host started, every kind together.
    pub(super) fn refused_total(&self) -> u64 {
        Refusal::ALL.iter().map(|&k| self.refused(k)).sum()
    }

    /// Lifecycle messages admitted beyond the reserve since the host started.
    pub(super) fn over_reserve(&self) -> u64 {
        self.over_reserve.load(Ordering::Relaxed)
    }

    /// The journal bytes not handed to the I/O thread (every beat).
    pub(super) fn set_journal_pending(&self, bytes: usize) {
        self.journal_pending.store(bytes, Ordering::Relaxed);
    }

    /// Whether the host takes no new game (see the module documentation).
    pub(super) fn busy(&self) -> bool {
        self.messages() >= INBOX_BUSY || self.journal_pending.load(Ordering::Relaxed) >= JOURNAL_PENDING_BUSY
    }
}
