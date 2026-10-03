//! Realtime connections: the per-connection task (Hello, rate buckets, heartbeats, dispatch),
//! outbound queues with slow-consumer handling, the lobby actor (presence, matchmaking,
//! challenges, conduct, rematches, sanctions and notices) and the drain at shutdown. Owner:
//! realtime (wave 2). See docs/RUST-PORT.md.

pub mod endpoint;

pub use endpoint::{Endpoint, Outbound};
