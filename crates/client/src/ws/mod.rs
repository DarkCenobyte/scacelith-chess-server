//! A WebSocket client (RFC 6455) written for the Scacelith protocols: the opening handshake with
//! one subprotocol, masked client frames, fragmented server messages, pings, pongs and the
//! closing handshake. No extension is negotiated.
//!
//! [`Session`] is the transport of the realtime [`Connection`](crate::Connection); it is public
//! so that other protocols with the same message numbering (the benchmark's protocol 3 adapter)
//! and raw-frame tests can use it.

pub mod frame;
mod handshake;
mod session;

pub use frame::{FrameError, Role};
pub use handshake::{UpgradeRequest, accept_key, client_handshake};
pub use session::{AutoReply, Incoming, Session, SessionOptions, SessionStats};
