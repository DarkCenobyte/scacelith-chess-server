//! Games (DESIGN 5.3, 6.1 to 6.5): the game clock, the room state machine (pure and
//! synchronous), the host shard actors (rooms, timers, stall credit, journal, commit gate,
//! recovery) and the test doubles. See docs/RUST-PORT.md section 8.1.

pub mod clock;
pub mod room;
pub mod rules;
pub mod testing;

pub use self::room::{GameRoom, Outcome, RoomSettings, RoomSpec, Timing};
pub use self::rules::{Rules, Side};
