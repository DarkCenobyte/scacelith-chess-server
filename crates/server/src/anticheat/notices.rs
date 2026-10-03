//! `Notice{RatingRestored, arg: points}` of the rating refunds ([`super::refunds`]), sent by the
//! lobby to each refunded victim, out of a game only (the notice must not interrupt one):
//!
//! * when the refund is given, if the victim is connected and neither playing nor starting a game
//!   ([`SanctionEvents::refunds_pending`](crate::events::SanctionEvents::refunds_pending) makes
//!   the lobby [`poll`](RefundNotices::poll) at once; the refunds of an admin command, another
//!   process, are found by the poll every [`REFUND_POLL_MS`]);
//! * otherwise right after the victim's current game ends ([`RefundNotices::game_ended`]);
//! * otherwise at the victim's next connection, right after Welcome, unless that connection
//!   resumes a game: then after that game ends ([`RefundNotices::connected`]).
//!
//! One notice carries every refund of the victim not notified yet (the total of their points). A
//! refund is marked notified only once the notice was queued on a connection past its Welcome
//! ([`RefundHost::send_notice`] answered `true`); otherwise the notice is tried again
//! [`RETRY_MS`] later, [`RETRIES`] times, then at each poll while the victim can get it.
//!
//! # Driving it
//!
//! [`RefundNotices`] is a state machine owned by the lobby actor: its methods are synchronous and
//! called from the actor only. Store reads and writes and the timers run in tasks it spawns, which
//! post a [`RefundEvent`] back through the function given to [`RefundNotices::new`] (a send to
//! the lobby's inbox); the lobby hands each event to [`RefundNotices::handle`] with itself as the
//! [`RefundHost`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::AbortHandle;

use crate::clock::SharedClock;
use crate::ids::UserId;
use crate::log::Logger;
use crate::store::{PendingRefund, PendingRefunds, Store, StoreError};
use crate::{log_error, log_info};

/// Interval at which the refunds not notified yet are looked for.
pub const REFUND_POLL_MS: u64 = 5000;
/// Delay before a notice that could not be queued is tried again.
pub const RETRY_MS: u64 = 250;
/// Tries after the first one before a victim waits for the polls.
pub const RETRIES: u32 = 20;
/// Refunds read per query.
const PAGE: i64 = 1000;

/// What the notices need from the lobby.
pub trait RefundHost {
    /// The user is connected (past Welcome), and neither playing nor starting a game.
    fn can_notify(&self, user: UserId) -> bool;

    /// Queues `Notice{RatingRestored, arg: points}` on the user's connection. `true` only when it
    /// was queued on a connection past its Welcome; `false` when there is none (closed, not
    /// welcomed yet, slow consumer).
    fn send_notice(&mut self, user: UserId, points: i64) -> bool;
}

/// A result of the notices' background work, for [`RefundNotices::handle`]. Opaque: the lobby only
/// forwards it.
#[derive(Debug)]
pub struct RefundEvent(Event);

#[derive(Debug)]
enum Event {
    /// The refunds not notified yet, read after `last_id`, and the error that stopped the read.
    Polled { rows: Vec<PendingRefund>, error: Option<StoreError> },
    /// The poll interval elapsed.
    Tick,
    /// A victim's refunds not notified yet, read for a notice (`seen`: refunds seen then).
    Pending { user: UserId, seen: u64, result: Result<PendingRefunds, StoreError> },
    /// The refunds of a notice were marked notified (or failed to be).
    Marked { user: UserId, seen: u64 },
    /// A retry delay elapsed.
    Retry { user: UserId },
}

/// Where the background tasks post their events (the lobby's inbox).
pub type RefundPost = Arc<dyn Fn(RefundEvent) + Send + Sync>;

struct Retry {
    left: u32,
    timer: Option<AbortHandle>,
}

/// The refund notices (module documentation).
pub struct RefundNotices {
    store: Store,
    clock: SharedClock,
    logger: Logger,
    post: RefundPost,
    poll_every: Duration,
    retry_after: Duration,
    /// Victim -> refunds seen for them (a change during a notice means new ones).
    waiting: HashMap<UserId, u64>,
    last_id: i64,
    /// Victims whose notice is being read, sent or marked.
    in_flight: HashSet<UserId>,
    retries: HashMap<UserId, Retry>,
    ticker: Option<AbortHandle>,
    polling: bool,
    repoll: bool,
    stopped: bool,
}

impl std::fmt::Debug for RefundNotices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefundNotices")
            .field("waiting", &self.waiting.len())
            .field("last_id", &self.last_id)
            .field("in_flight", &self.in_flight.len())
            .finish_non_exhaustive()
    }
}

impl RefundNotices {
    /// Notices of the refunds stored in `store`; `post` sends an event to the lobby's inbox. Must
    /// be used inside a tokio runtime.
    pub fn new(store: Store, clock: SharedClock, post: RefundPost) -> RefundNotices {
        RefundNotices {
            store,
            clock,
            logger: Logger::root().child("anticheat"),
            post,
            poll_every: Duration::from_millis(REFUND_POLL_MS),
            retry_after: Duration::from_millis(RETRY_MS),
            waiting: HashMap::new(),
            last_id: 0,
            in_flight: HashSet::new(),
            retries: HashMap::new(),
            ticker: None,
            polling: false,
            repoll: false,
            stopped: false,
        }
    }

    /// Changes the poll interval and the retry delay (tests).
    pub fn with_timing(mut self, poll_every: Duration, retry_after: Duration) -> RefundNotices {
        self.poll_every = poll_every;
        self.retry_after = retry_after;
        self
    }

    /// Reads the refunds already waiting, then polls every [`REFUND_POLL_MS`].
    pub fn start(&mut self) {
        self.stopped = false;
        self.poll();
        if self.ticker.is_none() {
            let (post, every) = (self.post.clone(), self.poll_every);
            let task = tokio::spawn(async move {
                let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    post(RefundEvent(Event::Tick));
                }
            });
            self.ticker = Some(task.abort_handle());
        }
    }

    /// Stops the polls and the retries (a notice already being sent completes).
    pub fn stop(&mut self) {
        self.stopped = true;
        if let Some(t) = self.ticker.take() {
            t.abort();
        }
        for r in self.retries.values_mut() {
            if let Some(t) = r.timer.take() {
                t.abort();
            }
        }
        self.retries.clear();
    }

    /// Victims waiting for a notice (diagnostics, tests).
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Looks for the refunds not notified yet that are new since the last poll, then notifies
    /// every waiting victim who can be. A poll asked while one runs is run after it.
    pub fn poll(&mut self) {
        if self.polling {
            self.repoll = true;
            return;
        }
        self.polling = true;
        let after = self.last_id;
        let read = self.store.read(move |db| {
            let mut rows = Vec::new();
            let mut cursor = after;
            loop {
                let page = match db.refunds().pending_since(cursor, PAGE) {
                    Ok(page) => page,
                    Err(e) => return Ok::<_, StoreError>((rows, Some(e))),
                };
                let full = page.len() as i64 == PAGE;
                cursor = page.last().map_or(cursor, |r| r.id);
                rows.extend(page);
                if !full {
                    return Ok((rows, None));
                }
            }
        });
        let post = self.post.clone();
        tokio::spawn(async move {
            let (rows, error) = match read.await {
                Ok(r) => r,
                Err(e) => (Vec::new(), Some(e)),
            };
            post(RefundEvent(Event::Polled { rows, error }));
        });
    }

    /// A connection of `user` was admitted: the notice follows Welcome, unless the connection
    /// resumes a game (`resumes_game`), whose end triggers it.
    pub fn connected(&mut self, user: UserId, resumes_game: bool) {
        if resumes_game || !self.waiting.contains_key(&user) {
            return;
        }
        // A new connection gets a new series of tries.
        if let Some(r) = self.retries.remove(&user)
            && let Some(t) = r.timer
        {
            t.abort();
        }
        self.retry(user);
    }

    /// These players' game ended (its result is committed).
    pub fn game_ended(&mut self, users: &[UserId], host: &mut impl RefundHost) {
        for &u in users {
            if u != 0 && self.waiting.contains_key(&u) {
                self.notify(u, &*host);
            }
        }
    }

    /// Handles the result of background work (module documentation).
    pub fn handle(&mut self, event: RefundEvent, host: &mut impl RefundHost) {
        match event.0 {
            Event::Polled { rows, error } => {
                self.polling = false;
                for r in rows {
                    self.last_id = self.last_id.max(r.id);
                    *self.waiting.entry(r.victim_id).or_insert(0) += 1;
                }
                if let Some(e) = error {
                    log_error!(self.logger, "refunds to notify not read", { "err": crate::log::error(&e) });
                }
                let users: Vec<UserId> = self.waiting.keys().copied().collect();
                for u in users {
                    self.notify(u, &*host);
                }
                if std::mem::take(&mut self.repoll) {
                    self.poll();
                }
            }
            Event::Tick => {
                if !self.stopped {
                    self.poll();
                }
            }
            Event::Pending { user, seen, result } => self.pending_read(user, seen, result, host),
            Event::Marked { user, seen } => {
                self.in_flight.remove(&user);
                if self.waiting.get(&user) == Some(&seen) {
                    self.forget(user);
                } else {
                    self.notify(user, &*host);
                }
            }
            Event::Retry { user } => {
                if let Some(r) = self.retries.get_mut(&user) {
                    r.timer = None;
                }
                if self.stopped {
                    return;
                }
                if self.waiting.contains_key(&user) {
                    self.notify(user, &*host);
                } else {
                    self.retries.remove(&user);
                }
            }
        }
    }

    /// Sends the victim's notice now if they can get it: reads their refunds, then
    /// [`RefundNotices::pending_read`].
    fn notify(&mut self, user: UserId, host: &impl RefundHost) {
        let Some(&seen) = self.waiting.get(&user) else { return };
        if self.in_flight.contains(&user) || !host.can_notify(user) {
            return;
        }
        self.in_flight.insert(user);
        let read = self.store.refunds().pending_for(user);
        let post = self.post.clone();
        tokio::spawn(async move {
            let result = read.await;
            post(RefundEvent(Event::Pending { user, seen, result }));
        });
    }

    fn pending_read(
        &mut self,
        user: UserId,
        seen: u64,
        result: Result<PendingRefunds, StoreError>,
        host: &mut impl RefundHost,
    ) {
        let pending = match result {
            Ok(p) => p,
            Err(e) => {
                self.in_flight.remove(&user);
                log_error!(self.logger, "refunds to notify not read", { "err": crate::log::error(&e), "userId": user });
                return;
            }
        };
        if pending.ids.is_empty() {
            self.in_flight.remove(&user);
            self.forget(user);
            return;
        }
        // The victim may have started a game or left while the refunds were read: the next game
        // end, connection or poll tries again.
        if !host.can_notify(user) {
            self.in_flight.remove(&user);
            return;
        }
        if !host.send_notice(user, pending.points) {
            self.in_flight.remove(&user);
            self.retry(user);
            return;
        }
        log_info!(self.logger, "rating refund notified", { "userId": user, "points": pending.points,
            "refunds": pending.ids.len() });
        // Still in flight until marked: a new notice must not read these refunds again.
        let (logger, post) = (self.logger.clone(), self.post.clone());
        let mark = self.store.refunds().mark_notified(pending.ids, self.clock.wall_ms());
        tokio::spawn(async move {
            if let Err(e) = mark.await {
                log_error!(logger, "refunds notified but not marked: the notice is sent again after a restart", {
                    "err": crate::log::error(&e), "userId": user });
            }
            post(RefundEvent(Event::Marked { user, seen }));
        });
    }

    fn forget(&mut self, user: UserId) {
        self.waiting.remove(&user);
        if let Some(r) = self.retries.remove(&user)
            && let Some(t) = r.timer
        {
            t.abort();
        }
    }

    fn retry(&mut self, user: UserId) {
        if self.stopped {
            return;
        }
        let r = self.retries.entry(user).or_insert(Retry { left: RETRIES, timer: None });
        // An exhausted budget is kept (until the notice is queued or the victim connects again):
        // the polls then try once each, they do not start a new series.
        if r.timer.is_some() || r.left == 0 {
            return;
        }
        r.left -= 1;
        let (post, after) = (self.post.clone(), self.retry_after);
        let task = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            post(RefundEvent(Event::Retry { user }));
        });
        r.timer = Some(task.abort_handle());
    }
}

impl Drop for RefundNotices {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
