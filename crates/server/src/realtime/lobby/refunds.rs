//! Rating refund notices (the former `refund-notices.js`): a victim of a cheater whose rating
//! was given back gets `Notice{RatingRestored, arg: points}` once, outside a game.
//!
//! Every [`REFUND_POLL`] (and when the anti-cheat says refunds were granted) the refunds not
//! notified yet that are newer than the last poll are read; each waiting victim who is online,
//! neither playing nor starting a game, is notified: the notice is queued on the live connection
//! once its `Welcome` is out, then the refunds are marked notified. A victim who connects without
//! a game in progress is tried after [`RETRY_MS`] (after the `Welcome`), then up to [`RETRIES`]
//! times while the notice cannot be delivered; a victim whose game ends is tried at once.
//!
//! The store reads and writes run in tasks of the lobby; a victim is in flight from the read of
//! their refunds until they are marked, so that no notice is sent twice.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use super::actor::{Done, LobbyActor};
use crate::ids::UserId;
use crate::log::{self};
use crate::realtime::frames;
use crate::store::{PendingRefund, PendingRefunds, StoreError};
use crate::{log_error, log_info};
use scacelith_protocol::NoticeCode;

/// Interval of the polls for refunds not notified yet.
pub(crate) const REFUND_POLL: Duration = Duration::from_secs(5);
/// Delay of a retry.
const RETRY_MS: f64 = 250.0;
/// Retries of one victim.
const RETRIES: u32 = 20;
/// Refunds read per page.
const PAGE: i64 = 1000;

#[derive(Debug)]
struct Retry {
    left: u32,
    /// Monotonic ms of the scheduled retry.
    due: Option<i64>,
}

/// The refund notices' state.
#[derive(Debug, Default)]
pub(crate) struct RefundNotices {
    /// Victim to refunds seen for them (a change during a send means new ones).
    waiting: HashMap<UserId, u32>,
    last_id: i64,
    in_flight: HashSet<UserId>,
    retries: HashMap<UserId, Retry>,
    due: BTreeSet<(i64, UserId)>,
    polling: bool,
    poll_again: bool,
}

impl RefundNotices {
    #[cfg(test)]
    pub(crate) fn waiting(&self, user: UserId) -> bool {
        self.waiting.contains_key(&user)
    }
}

impl LobbyActor {
    /// Reads the refunds not notified yet that are new since the last poll, then notifies every
    /// waiting victim who can be.
    pub(super) fn refunds_poll(&mut self) {
        if self.refunds.polling {
            self.refunds.poll_again = true;
            return;
        }
        self.refunds.polling = true;
        let store = self.store.clone();
        let mut after = self.refunds.last_id;
        self.spawn(async move {
            let mut rows = Vec::new();
            let result = loop {
                match store.refunds().pending_since(after, PAGE).await {
                    Ok(page) => {
                        let full = page.len() as i64 >= PAGE;
                        after = page.iter().map(|r| r.id).max().unwrap_or(after).max(after);
                        rows.extend(page);
                        if !full {
                            break Ok(rows);
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            Done::RefundsPolled { result }
        });
    }

    pub(super) fn refunds_polled(&mut self, result: Result<Vec<PendingRefund>, StoreError>) {
        self.refunds.polling = false;
        match result {
            Ok(rows) => {
                for r in rows {
                    self.refunds.last_id = self.refunds.last_id.max(r.id);
                    *self.refunds.waiting.entry(r.victim_id).or_insert(0) += 1;
                }
            }
            Err(e) => log_error!(self.log, "refunds to notify not read", { "err": log::error(&e) }),
        }
        let users: Vec<UserId> = self.refunds.waiting.keys().copied().collect();
        for user in users {
            self.refunds_notify(user);
        }
        if std::mem::take(&mut self.refunds.poll_again) {
            self.refunds_poll();
        }
    }

    /// A connection of the user was admitted: notified after its `Welcome`, unless it resumes a
    /// game.
    pub(super) fn refunds_connected(&mut self, user: UserId, active_game: u64) {
        if active_game != 0 || !self.refunds.waiting.contains_key(&user) {
            return;
        }
        self.refunds_forget_retry(user);
        self.refund_retry(user);
    }

    /// These players' game ended.
    pub(super) fn refunds_game_ended(&mut self, users: &[UserId]) {
        for &user in users {
            if user != 0 && self.refunds.waiting.contains_key(&user) {
                self.refunds_notify(user);
            }
        }
    }

    /// Stops the retries (the polls stop with the lobby's timers).
    pub(super) fn refunds_stop(&mut self) {
        self.refunds.retries.clear();
        self.refunds.due.clear();
    }

    fn can_notify(&self, user: UserId) -> bool {
        self.presence.get(user).is_some() && !self.busy(user)
    }

    /// Sends the victim's notice now if they can get it.
    fn refunds_notify(&mut self, user: UserId) {
        let Some(&seen) = self.refunds.waiting.get(&user) else { return };
        if self.refunds.in_flight.contains(&user) || !self.can_notify(user) {
            return;
        }
        self.refunds.in_flight.insert(user);
        let store = self.store.clone();
        self.spawn(async move {
            Done::RefundsRead { user, seen, result: store.refunds().pending_for(user).await }
        });
    }

    pub(super) fn refunds_read(
        &mut self,
        user: UserId,
        seen: u32,
        result: Result<PendingRefunds, StoreError>,
    ) {
        let pending = match result {
            Ok(p) => p,
            Err(e) => {
                self.refunds.in_flight.remove(&user);
                log_error!(self.log, "refunds to notify not read", { "err": log::error(&e), "userId": user });
                return;
            }
        };
        if pending.ids.is_empty() {
            self.refunds.in_flight.remove(&user);
            self.refunds_forget(user);
            return;
        }
        let delivered = self.presence.get(user).is_some_and(|link| {
            link.is_welcomed() && link.send(frames::notice(NoticeCode::RatingRestored, pending.points as f64))
        });
        if !delivered {
            self.refunds.in_flight.remove(&user);
            self.refund_retry(user);
            return;
        }
        let store = self.store.clone();
        let now = self.now();
        self.spawn(async move {
            let result = store.refunds().mark_notified(pending.ids.clone(), now).await;
            Done::RefundsMarked { user, seen, pending, result }
        });
    }

    pub(super) fn refunds_marked(
        &mut self,
        user: UserId,
        seen: u32,
        pending: PendingRefunds,
        result: Result<usize, StoreError>,
    ) {
        self.refunds.in_flight.remove(&user);
        if let Err(e) = result {
            log_error!(self.log, "refunds notified but not marked: the notice is sent again after a restart", {
                "err": log::error(&e), "userId": user,
            });
        }
        log_info!(self.log, "rating refund notified", {
            "userId": user, "points": pending.points, "refunds": pending.ids.len(),
        });
        if self.refunds.waiting.get(&user) == Some(&seen) {
            self.refunds_forget(user);
        } else {
            self.refunds_notify(user);
        }
    }

    fn refunds_forget(&mut self, user: UserId) {
        self.refunds.waiting.remove(&user);
        self.refunds_forget_retry(user);
    }

    fn refunds_forget_retry(&mut self, user: UserId) {
        if let Some(Retry { due: Some(due), .. }) = self.refunds.retries.remove(&user) {
            self.refunds.due.remove(&(due, user));
        }
    }

    /// Schedules a retry unless one is scheduled or the victim's budget is spent (the polls
    /// then try once each).
    fn refund_retry(&mut self, user: UserId) {
        let due = (self.clock.mono_ms() + RETRY_MS).ceil() as i64;
        let r = self.refunds.retries.entry(user).or_insert(Retry { left: RETRIES, due: None });
        if r.due.is_some() || r.left == 0 {
            return;
        }
        r.left -= 1;
        r.due = Some(due);
        self.refunds.due.insert((due, user));
    }

    /// Runs the retries that are due.
    pub(super) fn refund_retries(&mut self) {
        let now = self.clock.mono_ms();
        while let Some(&(due, user)) = self.refunds.due.first() {
            if due as f64 > now {
                break;
            }
            self.refunds.due.pop_first();
            let Some(r) = self.refunds.retries.get_mut(&user) else { continue };
            if r.due != Some(due) {
                continue;
            }
            r.due = None;
            if self.refunds.waiting.contains_key(&user) {
                self.refunds_notify(user);
            } else {
                self.refunds.retries.remove(&user);
            }
        }
    }

    /// When the next retry is due.
    pub(super) fn next_refund_retry(&self) -> Option<Instant> {
        let &(due, _) = self.refunds.due.first()?;
        let wait = (due as f64 - self.clock.mono_ms()).max(0.0);
        Some(Instant::now() + Duration::from_secs_f64(wait / 1000.0))
    }
}
