//! Realtime connections: the per-connection task (Hello, rate buckets, heartbeats, dispatch),
//! outbound queues with slow-consumer handling, the lobby actor (presence, matchmaking,
//! challenges, conduct, rematches, sanctions and notices) and the drain at shutdown. Owner:
//! realtime (wave 2). See docs/RUST-PORT.md.

pub mod admission;
pub mod deps;
pub mod endpoint;
pub(crate) mod frames;
pub(crate) mod link;
pub mod lobby;
pub(crate) mod metrics;
pub(crate) mod presence;
pub(crate) mod reads;

pub use admission::Admissions;
pub use endpoint::{Endpoint, Outbound};
