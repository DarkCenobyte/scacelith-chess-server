//! Tests of the journal: ports of store.journal.test.js and the journal parts of
//! store.journal-compaction.test.js (the host parts belong to the game module), crash tests with
//! a SIGKILLed child process, the commit gate's accounting and the fault-injection hooks.

mod basic;
mod compaction;
mod crash;
mod durability;
mod failures;
mod handle;
mod long_games;
mod support;

/// The store tests' log capture (one capture at a time in the whole test binary).
pub use crate::store::tests::support::LogCapture;
