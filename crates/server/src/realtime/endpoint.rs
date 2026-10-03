//! Outbound side of a realtime connection, shared by the connection task (which writes the
//! frames to the socket), the host actors and the lobby (which queue frames for a player).
//!
//! [`Outbound`] is a byte-accounted queue of protocol messages. Senders never block: a frame is
//! queued, or refused when the connection is closing. When the bytes waiting for the socket would
//! exceed the configured limit (`WS_SEND_BUFFER_LIMIT`), the connection is closed with 4303 (slow
//! consumer) and the queue is dropped. Droppable frames (the opponent's gestures) are skipped as
//! soon as a quarter of the limit is waiting, so cosmetic traffic never closes a slow consumer.
//!
//! [`Endpoint`] is a player's handle on a connection: its ids, its outbound queue and its
//! smoothed round-trip time, which the host uses for lag compensation.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::ids::{ConnId, UserId};

/// WebSocket close code sent when the outbound queue overflows.
pub const CLOSE_SLOW_CONSUMER: u16 = 4303;

/// A close request: code and reason of the WebSocket close frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseRequest {
    pub code: u16,
    pub reason: String,
}

/// What the writer takes from the queue in one go.
#[derive(Debug, Default)]
pub struct Batch {
    /// Frames to write, in order.
    pub frames: Vec<Bytes>,
    /// Set once every frame queued before the close request has been taken.
    pub close: Option<CloseRequest>,
}

#[derive(Debug, Default)]
struct State {
    frames: VecDeque<Bytes>,
    queued: usize,
    in_flight: usize,
    close: Option<CloseRequest>,
    close_taken: bool,
}

/// Byte-accounted outbound queue of one connection (see the module documentation).
#[derive(Debug)]
pub struct Outbound {
    state: Mutex<State>,
    wake: Notify,
    limit: usize,
}

impl Outbound {
    /// A queue that closes the connection when more than `limit` bytes wait for the socket.
    pub fn new(limit: usize) -> Arc<Outbound> {
        Arc::new(Outbound { state: Mutex::new(State::default()), wake: Notify::new(), limit: limit.max(1) })
    }

    /// Queues a frame. Returns `false` when the frame was refused: the connection is closing, or
    /// this frame overflowed the queue (the connection is then closed with 4303).
    pub fn send(&self, frame: Bytes) -> bool {
        let mut st = self.state.lock();
        if st.close.is_some() {
            return false;
        }
        if st.queued + st.in_flight + frame.len() > self.limit {
            st.frames.clear();
            st.queued = 0;
            st.close = Some(CloseRequest { code: CLOSE_SLOW_CONSUMER, reason: "slow consumer".into() });
            drop(st);
            self.wake.notify_one();
            return false;
        }
        st.queued += frame.len();
        st.frames.push_back(frame);
        drop(st);
        self.wake.notify_one();
        true
    }

    /// Queues a frame unless a quarter of the limit is already waiting (the frame is then
    /// dropped, the connection stays open). Returns whether the frame was queued.
    pub fn send_droppable(&self, frame: Bytes) -> bool {
        {
            let st = self.state.lock();
            if st.close.is_some() || st.queued + st.in_flight > self.limit / 4 {
                return false;
            }
        }
        self.send(frame)
    }

    /// Asks the writer to close the connection after the frames already queued. Later frames are
    /// refused. The first close request wins.
    pub fn close(&self, code: u16, reason: &str) {
        let mut st = self.state.lock();
        if st.close.is_none() {
            st.close = Some(CloseRequest { code, reason: reason.to_string() });
            drop(st);
            self.wake.notify_one();
        }
    }

    /// Whether frames are still accepted.
    pub fn is_open(&self) -> bool {
        self.state.lock().close.is_none()
    }

    /// Bytes queued or being written.
    pub fn buffered(&self) -> usize {
        let st = self.state.lock();
        st.queued + st.in_flight
    }

    /// The queue's limit in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Writer side: resolves when frames or a close request may be waiting.
    pub async fn ready(&self) {
        self.wake.notified().await;
    }

    /// Writer side: takes every queued frame (and the close request once the queue is empty).
    /// The bytes taken count as in flight until [`Outbound::written`] is called.
    pub fn take(&self) -> Batch {
        let mut st = self.state.lock();
        let frames: Vec<Bytes> = st.frames.drain(..).collect();
        st.in_flight += st.queued;
        st.queued = 0;
        let close = if st.close_taken { None } else { st.close.clone() };
        if close.is_some() {
            st.close_taken = true;
        }
        Batch { frames, close }
    }

    /// Writer side: `bytes` taken earlier have been handed to the socket.
    pub fn written(&self, bytes: usize) {
        let mut st = self.state.lock();
        st.in_flight = st.in_flight.saturating_sub(bytes);
    }
}

#[derive(Debug)]
struct EndpointInner {
    conn_id: ConnId,
    user_id: UserId,
    out: Arc<Outbound>,
    rtt_ms: AtomicU32,
}

/// A player's handle on a realtime connection. Cheap to clone; two clones are the same endpoint.
#[derive(Clone, Debug)]
pub struct Endpoint(Arc<EndpointInner>);

impl Endpoint {
    pub fn new(conn_id: ConnId, user_id: UserId, out: Arc<Outbound>) -> Endpoint {
        Endpoint(Arc::new(EndpointInner { conn_id, user_id, out, rtt_ms: AtomicU32::new(0) }))
    }

    /// An endpoint on a fresh queue, for tests: inspect the frames with `Outbound::take`.
    pub fn for_tests(conn_id: ConnId, user_id: UserId) -> (Endpoint, Arc<Outbound>) {
        let out = Outbound::new(1 << 20);
        (Endpoint::new(conn_id, user_id, out.clone()), out)
    }

    pub fn conn_id(&self) -> ConnId {
        self.0.conn_id
    }

    pub fn user_id(&self) -> UserId {
        self.0.user_id
    }

    /// Queues a frame; `false` when refused (see [`Outbound::send`]).
    pub fn send(&self, frame: Bytes) -> bool {
        self.0.out.send(frame)
    }

    /// Queues a frame that may be dropped under backlog (see [`Outbound::send_droppable`]).
    pub fn send_droppable(&self, frame: Bytes) -> bool {
        self.0.out.send_droppable(frame)
    }

    /// Closes the connection after the frames already queued.
    pub fn close(&self, code: u16, reason: &str) {
        self.0.out.close(code, reason);
    }

    pub fn is_open(&self) -> bool {
        self.0.out.is_open()
    }

    /// Smoothed round-trip time in milliseconds, 0 before the first measure.
    pub fn rtt_ms(&self) -> u32 {
        self.0.rtt_ms.load(Ordering::Relaxed)
    }

    pub fn set_rtt_ms(&self, ms: u32) {
        self.0.rtt_ms.store(ms, Ordering::Relaxed);
    }

    /// Whether both handles designate the same connection.
    pub fn same(&self, other: &Endpoint) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn outbound(&self) -> &Arc<Outbound> {
        &self.0.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queues_and_drains_in_order() {
        let (ep, out) = Endpoint::for_tests(7, 42);
        assert!(ep.send(Bytes::from_static(b"ab")));
        assert!(ep.send(Bytes::from_static(b"cde")));
        assert_eq!(out.buffered(), 5);
        let b = out.take();
        assert_eq!(b.frames, vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cde")]);
        assert!(b.close.is_none());
        assert_eq!(out.buffered(), 5);
        out.written(5);
        assert_eq!(out.buffered(), 0);
    }

    #[test]
    fn overflow_closes_with_slow_consumer() {
        let out = Outbound::new(10);
        assert!(out.send(Bytes::from(vec![0u8; 8])));
        assert!(!out.send(Bytes::from(vec![0u8; 3])));
        assert!(!out.is_open());
        let b = out.take();
        assert!(b.frames.is_empty());
        assert_eq!(b.close, Some(CloseRequest { code: CLOSE_SLOW_CONSUMER, reason: "slow consumer".into() }));
        assert!(out.take().close.is_none());
    }

    #[test]
    fn droppable_frames_are_skipped_under_backlog() {
        let out = Outbound::new(100);
        assert!(out.send(Bytes::from(vec![0u8; 26])));
        assert!(!out.send_droppable(Bytes::from_static(b"g")));
        assert!(out.is_open());
        out.take();
        out.written(26);
        assert!(out.send_droppable(Bytes::from_static(b"g")));
    }

    #[test]
    fn close_comes_after_queued_frames() {
        let (ep, out) = Endpoint::for_tests(1, 1);
        ep.send(Bytes::from_static(b"x"));
        ep.close(4007, "replaced");
        ep.close(1001, "ignored");
        assert!(!ep.send(Bytes::from_static(b"y")));
        let b = out.take();
        assert_eq!(b.frames.len(), 1);
        assert_eq!(b.close.unwrap().code, 4007);
        let (other, _) = Endpoint::for_tests(1, 1);
        assert!(ep.same(&ep.clone()));
        assert!(!ep.same(&other));
    }
}
