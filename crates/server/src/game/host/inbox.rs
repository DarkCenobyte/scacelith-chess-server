//! What waits for a host actor, and its bounds. The inbox is an unbounded channel: a message that
//! carries a move, a clock, a result or a connection's lifecycle (attach, detach, a game created
//! or cancelled, a forfeit, a rematch window, the shutdown) is never refused nor dropped, since
//! losing it would lose a game's data or leave a player stuck. What the inbox holds is counted
//! instead, and bounded where it can be:
//!
//! * a gesture is replaceable (the player's next one supersedes it, and the opponent's client
//!   copes with missing ones, as with those dropped for a slow link): beyond
//!   [`GESTURE_INBOX_MAX`] gestures waiting, a new one is dropped at the door
//!   (`scacelith_gestures_dropped_total{reason="overload"}`);
//! * a host with [`INBOX_BUSY`] messages or more waiting, or [`JOURNAL_PENDING_BUSY`] bytes or
//!   more of journal records not handed to its I/O thread, is busy: no new game is placed on it,
//!   and once every host of the range is busy, new games and the lobby requests that would create
//!   one are refused with `RateLimited` until the hosts catch up. The games in progress go on.
//!
//! Every other message comes from a source limited elsewhere (the message rate of each
//! connection, `MAX_CONNECTIONS`, the lobby's requests in flight per connection), for the games
//! the host already holds, which no longer grow once it is busy. The counts are exported every
//! beat (`scacelith_game_inbox_messages` and `scacelith_journal_pending_bytes`, per shard).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::metrics;

/// Gestures that may wait in a host's inbox: about 170 ms of the gestures of 6,000 games whose
/// players all move all the time (4 per second each). A host that far behind drops the stale ones
/// instead of relaying them late.
pub const GESTURE_INBOX_MAX: usize = 8192;

/// Messages waiting in a host's inbox from which it takes no new game (see the module
/// documentation).
pub const INBOX_BUSY: usize = 16_384;

/// Journal bytes not handed to the I/O thread from which a host takes no new game: minutes of
/// records of a busy shard whose disk does not answer.
pub const JOURNAL_PENDING_BUSY: usize = 16 << 20;

/// The counts of what waits for one host, shared by its handles and its actor.
#[derive(Debug, Default)]
pub(super) struct Backlog {
    /// Messages posted and not taken by the actor yet.
    messages: AtomicUsize,
    /// Gestures among them.
    gestures: AtomicUsize,
    /// Gestures dropped at the door since the host started.
    refused: AtomicU64,
    /// Journal bytes not handed to the I/O thread, as of the latest beat.
    journal_pending: AtomicUsize,
}

impl Backlog {
    /// A message is about to be posted.
    pub(super) fn posting(&self) {
        self.messages.fetch_add(1, Ordering::Relaxed);
    }

    /// A gesture is about to be posted: false when [`GESTURE_INBOX_MAX`] gestures already wait
    /// (it is dropped and counted).
    pub(super) fn posting_gesture(&self) -> bool {
        if self.gestures.fetch_add(1, Ordering::Relaxed) >= GESTURE_INBOX_MAX {
            self.gestures.fetch_sub(1, Ordering::Relaxed);
            self.refused.fetch_add(1, Ordering::Relaxed);
            metrics::gesture_overload();
            return false;
        }
        self.posting();
        true
    }

    /// A message left the inbox: taken by the actor, or never posted (the host is gone).
    pub(super) fn taken(&self, gesture: bool) {
        self.messages.fetch_sub(1, Ordering::Relaxed);
        if gesture {
            self.gestures.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Messages waiting.
    pub(super) fn messages(&self) -> usize {
        self.messages.load(Ordering::Relaxed)
    }

    /// Gestures waiting.
    pub(super) fn gestures(&self) -> usize {
        self.gestures.load(Ordering::Relaxed)
    }

    /// Gestures dropped at the door since the host started.
    pub(super) fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
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
