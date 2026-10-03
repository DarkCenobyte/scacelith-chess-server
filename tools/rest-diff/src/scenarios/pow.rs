//! Work in progress.

use super::BoxFut;
use crate::duo::Duo;

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = d;
    })
}
