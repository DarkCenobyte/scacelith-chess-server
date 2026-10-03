//! Work in progress.

use super::BoxFut;
use crate::duo::Duo;

/// The scenario.
pub fn open(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = d;
    })
}

/// The scenario.
pub fn closed(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = d;
    })
}

/// The scenario.
pub fn proxy(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = d;
    })
}
