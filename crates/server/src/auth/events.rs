//! Security events (failed logins, lockouts, proof of work, password resets, MFA changes, session
//! revocations...): logged at once at the `security` level and saved in batches, one second after
//! the first event of a batch (DESIGN.md section 7: batched 1 s, retained
//! `RETENTION_SECURITY_DAYS`).
//!
//! The queue is bounded: under a flood the oldest unsaved events are dropped (and counted); the
//! log line of each event has already been written. The detail of an event is stored as a JSON
//! object (the former server stored it as a JSON string literal; readers accept both).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::clock::SharedClock;
use crate::ids::UserId;
use crate::log::{self, Level, Logger};
use crate::log_error;
use crate::metrics::{self, Counter, CounterVec};
use crate::store::{NewSecurityEvent, Store};

static EVENTS: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec("scacelith_auth_security_events_total", "Security events recorded", &["kind"])
});
static DROPPED: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter(
        "scacelith_auth_security_events_dropped_total",
        "Security events dropped before being saved",
    )
});

/// Unsaved events kept at most.
pub const MAX_QUEUE: usize = 10_000;

/// Delay between the first event of a batch and its save.
pub const FLUSH_DELAY: Duration = Duration::from_secs(1);

struct State {
    queue: VecDeque<NewSecurityEvent>,
    scheduled: bool,
}

struct Inner {
    store: Store,
    log: Logger,
    clock: SharedClock,
    max_queue: usize,
    flush_delay: Duration,
    state: Mutex<State>,
}

/// The security events of the auth service (module documentation). Cheap to clone.
#[derive(Clone)]
pub struct SecurityEvents {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SecurityEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityEvents").field("pending", &self.pending()).finish()
    }
}

impl SecurityEvents {
    /// Events saved into `store`, logged by `log`, timed by `clock` (wall time).
    pub fn new(store: Store, log: Logger, clock: SharedClock) -> SecurityEvents {
        SecurityEvents::with_limits(store, log, clock, MAX_QUEUE, FLUSH_DELAY)
    }

    /// [`SecurityEvents::new`] with another queue bound and flush delay (tests).
    pub fn with_limits(
        store: Store,
        log: Logger,
        clock: SharedClock,
        max_queue: usize,
        flush_delay: Duration,
    ) -> SecurityEvents {
        SecurityEvents {
            inner: Arc::new(Inner {
                store,
                log,
                clock,
                max_queue: max_queue.max(1),
                flush_delay,
                state: Mutex::new(State { queue: VecDeque::new(), scheduled: false }),
            }),
        }
    }

    /// Records one event: counted, logged now, saved with the next batch. `detail` must not hold
    /// credentials (it is stored).
    pub fn record(&self, kind: &str, user_id: Option<UserId>, ip: Option<&str>, detail: Option<Value>) {
        let inner = &self.inner;
        EVENTS.with(&[kind]).inc();
        if inner.log.enabled(Level::Security) {
            let mut fields = Map::new();
            if let Some(id) = user_id {
                fields.insert("userId".into(), id.into());
            }
            if let Some(ip) = ip.and_then(log::ip) {
                fields.insert("ip".into(), ip.into());
            }
            if let Some(Value::Object(d)) = &detail {
                fields.extend(d.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            inner.log.emit(Level::Security, kind, Some(Value::Object(fields)));
        }
        let event = NewSecurityEvent {
            kind: kind.to_owned(),
            user_id,
            ip: ip.filter(|s| !s.is_empty()).map(str::to_owned),
            at: Some(inner.clock.wall_ms()),
            detail,
        };
        let schedule = {
            let mut st = inner.state.lock();
            if st.queue.len() >= inner.max_queue {
                st.queue.pop_front();
                DROPPED.inc();
            }
            st.queue.push_back(event);
            !std::mem::replace(&mut st.scheduled, true)
        };
        if schedule {
            self.schedule_flush();
        }
    }

    /// Saves the batch in one second, on the runtime (outside a runtime the batch waits for the
    /// next [`SecurityEvents::flush`]).
    fn schedule_flush(&self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.inner.state.lock().scheduled = false;
            return;
        };
        let this = self.clone();
        rt.spawn(async move {
            tokio::time::sleep(this.inner.flush_delay).await;
            this.inner.state.lock().scheduled = false;
            this.flush().await;
        });
    }

    /// Saves the events waiting now. The store write is queued when this is called (before any
    /// later write of the caller, such as an anonymisation that must erase their addresses); the
    /// future reports a failure in the log.
    pub fn flush(&self) -> impl Future<Output = ()> + Send + 'static {
        let batch: Vec<NewSecurityEvent> = self.inner.state.lock().queue.drain(..).collect();
        let count = batch.len();
        let write = (count > 0).then(|| self.inner.store.security().insert_batch(batch));
        let log = self.inner.log.clone();
        async move {
            if let Some(write) = write
                && let Err(e) = write.await
            {
                log_error!(log, "security events not saved", { "count": count, "err": log::error(&e) });
            }
        }
    }

    /// Events waiting to be saved.
    pub fn pending(&self) -> usize {
        self.inner.state.lock().queue.len()
    }
}
