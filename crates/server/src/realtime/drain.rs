//! The drain at shutdown: every connection learns the phase from a watch channel, and the
//! connections still open are counted so that the shutdown can wait for their last frames and
//! their releases before it stops the game hosts.
//!
//! * [`DrainPhase::Running`]: connections are served.
//! * [`DrainPhase::Grace`]: players are warned (`Notice{ServerShutdown, arg = grace}`); a new
//!   connection, or a Hello that ends now, is refused with `ShuttingDown`.
//! * [`DrainPhase::Closing`]: every connection gets a fatal `Error{ShuttingDown}` and closes
//!   with 4008.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, watch};

/// Where the drain is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainPhase {
    Running,
    /// Players were warned; their connections close after `grace_ms`.
    Grace {
        grace_ms: u64,
    },
    Closing,
}

#[derive(Debug)]
struct Live {
    count: AtomicUsize,
    /// Notified when the count drops to zero.
    idle: Notify,
}

/// The drain's control (one per server).
#[derive(Debug, Clone)]
pub struct Drain {
    phase: Arc<watch::Sender<DrainPhase>>,
    live: Arc<Live>,
}

impl Default for Drain {
    fn default() -> Drain {
        Drain::new()
    }
}

impl Drain {
    pub fn new() -> Drain {
        Drain {
            phase: Arc::new(watch::Sender::new(DrainPhase::Running)),
            live: Arc::new(Live { count: AtomicUsize::new(0), idle: Notify::new() }),
        }
    }

    /// The current phase.
    pub fn phase(&self) -> DrainPhase {
        *self.phase.borrow()
    }

    /// Whether the drain started (new players are refused).
    pub fn is_draining(&self) -> bool {
        self.phase() != DrainPhase::Running
    }

    /// Moves to `phase` (the phases only move forward).
    pub fn set(&self, phase: DrainPhase) {
        self.phase.send_if_modified(|current| {
            let forward = rank(phase) > rank(*current);
            if forward {
                *current = phase;
            }
            forward
        });
    }

    /// A connection's view of the drain; it counts as live until dropped.
    pub(crate) fn enter(&self) -> DrainWatch {
        self.live.count.fetch_add(1, Ordering::AcqRel);
        DrainWatch { rx: self.phase.subscribe(), live: self.live.clone() }
    }

    /// Connections open.
    pub fn live(&self) -> usize {
        self.live.count.load(Ordering::Acquire)
    }

    /// Waits until every connection ended, `timeout` at most. Returns whether they all did.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let wait = async {
            loop {
                let idle = self.live.idle.notified();
                tokio::pin!(idle);
                idle.as_mut().enable();
                if self.live() == 0 {
                    return;
                }
                idle.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }
}

fn rank(phase: DrainPhase) -> u8 {
    match phase {
        DrainPhase::Running => 0,
        DrainPhase::Grace { .. } => 1,
        DrainPhase::Closing => 2,
    }
}

/// A connection's view of the drain (see [`Drain::enter`]).
#[derive(Debug)]
pub(crate) struct DrainWatch {
    rx: watch::Receiver<DrainPhase>,
    live: Arc<Live>,
}

impl DrainWatch {
    /// The current phase, marked as seen.
    pub(crate) fn current(&mut self) -> DrainPhase {
        *self.rx.borrow_and_update()
    }

    /// The next phase not seen yet (never resolves once the drain's control is gone).
    pub(crate) async fn changed(&mut self) -> DrainPhase {
        if self.rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        *self.rx.borrow_and_update()
    }

    /// Resolves once the drain reached [`DrainPhase::Closing`].
    pub(crate) async fn closing(&mut self) {
        while self.current() != DrainPhase::Closing {
            self.changed().await;
        }
    }
}

impl Drop for DrainWatch {
    fn drop(&mut self) {
        if self.live.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.live.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn phases_move_forward_and_live_connections_are_counted() {
        let drain = Drain::new();
        let mut a = drain.enter();
        let b = drain.enter();
        assert_eq!((drain.live(), a.current()), (2, DrainPhase::Running));
        drain.set(DrainPhase::Grace { grace_ms: 50 });
        assert_eq!(a.changed().await, DrainPhase::Grace { grace_ms: 50 });
        drain.set(DrainPhase::Running);
        assert!(drain.is_draining(), "no way back");
        drain.set(DrainPhase::Closing);
        a.closing().await;
        assert!(!drain.wait_idle(Duration::from_millis(10)).await);
        drop(b);
        let waiter = tokio::spawn({
            let drain = drain.clone();
            async move { drain.wait_idle(Duration::from_secs(5)).await }
        });
        tokio::task::yield_now().await;
        drop(a);
        assert!(waiter.await.unwrap());
        assert_eq!(drain.live(), 0);
    }
}
