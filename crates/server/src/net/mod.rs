//! Network edge: TCP listener, native TLS (rustls, certificate reload), the pre-TLS gate, the
//! per-address guard, HTTP/1.1 serving on hyper with the hardening limits, the WebSocket upgrade
//! and frame codec, and the plain-HTTP metrics endpoint. Owner: net. See docs/RUST-PORT.md.

pub mod abuse;
pub mod gate;
pub mod guard;
pub mod health;
pub mod http1;
pub mod ip;
pub mod limits;
mod linked;
pub mod listener;
pub mod metrics_server;
pub mod server;
pub mod tls;
pub mod upgrade;
pub mod ws;

pub use linked::LinkedMap;
