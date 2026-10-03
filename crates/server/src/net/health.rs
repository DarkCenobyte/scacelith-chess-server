//! Readiness: the flag behind `/readyz` on the API port and on the metrics endpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the server takes traffic: false until start-up finished and again while draining.
/// Cheap to clone; every clone shares the flag.
#[derive(Debug, Clone, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    /// A flag that starts not ready.
    pub fn new() -> Readiness {
        Readiness::default()
    }

    /// A flag that starts ready (tests, a handler used alone).
    pub fn ready() -> Readiness {
        let r = Readiness::new();
        r.set(true);
        r
    }

    /// Sets the flag.
    pub fn set(&self, ready: bool) {
        self.0.store(ready, Ordering::Release);
    }

    /// Reads the flag.
    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
