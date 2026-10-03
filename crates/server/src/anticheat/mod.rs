//! Anti-cheat: anomalies, automatic sanctions, rating refunds and their notices, reports, the
//! statistical model (priors, scoring, integrity levels), the Stockfish analysis pool and the
//! administration commands. Owners: matching (pure parts), anticheat (wave 2). See
//! docs/RUST-PORT.md.
//!
//! The pure parts (DESIGN 6.6, docs/ANTICHEAT.md) hold no store and no socket: [`analysis`]
//! drives a UCI engine and turns a finished game into per-player features, [`priors`] and
//! [`scoring`] turn a player's features into a proposed integrity level with its evidence, and
//! [`integrity`] applies the memory rules to the stored record. The store-side services call
//! them inside their store jobs: the statistics source is a [`scoring::PopulationSource`] they
//! implement on their connection, and every result they must persist is returned as data
//! ([`scoring::Observation`], [`integrity::LevelUpdate`]).

pub mod analysis;
pub mod integrity;
pub mod notices;
pub mod num;
pub mod players;
pub mod priors;
pub mod refunds;
pub mod sanction;
pub mod scoring;
pub mod service;

pub use service::{Anticheat, Classification, SanctionResult, classify};

#[cfg(test)]
mod synthetic;
#[cfg(test)]
pub(crate) mod testing;
