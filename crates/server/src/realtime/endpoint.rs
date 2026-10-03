//! Outbound side of a realtime connection, shared by the connection task (which writes the
//! frames to the socket), the host actors and the lobby (which queue frames for a player).
//!
//! [`Outbound`] is a byte-accounted queue of protocol messages. Senders never block: a frame is
//! queued, or refused when the connection is closing. When the bytes waiting for the socket would
//! exceed the configured limit (`WS_SEND_BUFFER_LIMIT`), the connection is closed with 4303 (slow
//! consumer) and the queue is dropped. Droppable frames (the opponent's gestures) are skipped as
//! soon as a quarter of the limit is waiting, so cosmetic traffic never closes a slow consumer.
//!
//! A kick ([`Outbound::kick`]) queues its own frames (the fatal `Error` and its `Notice`) with the
//! close request, apart from the frames queued before: a connection kicked before its `Welcome`
//! writes the kick frames only ([`Outbound::take_kick`]), so that no lobby frame ever precedes the
//! `Welcome`.
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
    /// Frames of a kick, written after `frames`.
    kick: Vec<Bytes>,
    close: Option<CloseRequest>,
    close_taken: bool,
}

impl State {
    /// Records the close request; false when one was already there.
    fn request_close(&mut self, code: u16, reason: &str) -> bool {
        if self.close.is_some() {
            return false;
        }
        self.close = Some(CloseRequest { code, reason: reason.to_string() });
        true
    }
}

/// Byte-accounted outbound queue of one connection (see the module documentation).
#[derive(Debug)]
pub struct Outbound {
    state: Mutex<State>,
    /// Wakes the writer: frames or a close request may be waiting.
    wake: Notify,
    /// Wakes every task waiting in [`Outbound::closed`].
    closing: Notify,
    limit: usize,
}

impl Outbound {
    /// A queue that closes the connection when more than `limit` bytes wait for the socket.
    pub fn new(limit: usize) -> Arc<Outbound> {
        Arc::new(Outbound {
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            closing: Notify::new(),
            limit: limit.max(1),
        })
    }

    /// Wakes the writer and, after a close request, the tasks waiting for it.
    fn notify(&self, closed: bool) {
        self.wake.notify_one();
        if closed {
            self.closing.notify_waiters();
        }
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
            st.request_close(CLOSE_SLOW_CONSUMER, "slow consumer");
            drop(st);
            self.notify(true);
            return false;
        }
        st.queued += frame.len();
        st.frames.push_back(frame);
        drop(st);
        self.notify(false);
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
        if st.request_close(code, reason) {
            drop(st);
            self.notify(true);
        }
    }

    /// Kicks the connection: `frames` (a fatal `Error` and its `Notice`) are written after the
    /// frames already queued, then the connection closes with `code`. They are not counted
    /// against the limit. Returns `false` (nothing queued) when the connection was already
    /// closing.
    pub fn kick(&self, frames: &[Bytes], code: u16, reason: &str) -> bool {
        let mut st = self.state.lock();
        if !st.request_close(code, reason) {
            return false;
        }
        st.kick = frames.to_vec();
        drop(st);
        self.notify(true);
        true
    }

    /// The close request, if any (taken or not).
    pub fn close_request(&self) -> Option<CloseRequest> {
        self.state.lock().close.clone()
    }

    /// Resolves once a close has been requested (at once if it already was).
    pub async fn closed(&self) {
        loop {
            let notified = self.closing.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_open() {
                return;
            }
            notified.await;
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
        let mut frames: Vec<Bytes> = st.frames.drain(..).collect();
        let close = Self::take_close(&mut st);
        if close.is_some() {
            frames.append(&mut st.kick);
        }
        st.in_flight += frames.iter().map(Bytes::len).sum::<usize>();
        st.queued = 0;
        Batch { frames, close }
    }

    /// Writer side, before the `Welcome`: drops the frames queued so far and takes the frames of
    /// a kick with the close request (nothing when no close was requested).
    pub fn take_kick(&self) -> Batch {
        let mut st = self.state.lock();
        let close = Self::take_close(&mut st);
        if close.is_none() {
            return Batch::default();
        }
        st.frames.clear();
        st.queued = 0;
        let frames = std::mem::take(&mut st.kick);
        st.in_flight += frames.iter().map(Bytes::len).sum::<usize>();
        Batch { frames, close }
    }

    fn take_close(st: &mut State) -> Option<CloseRequest> {
        let close = if st.close_taken { None } else { st.close.clone() };
        if close.is_some() {
            st.close_taken = true;
        }
        close
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

    /// Kicks the connection: `frames`, then a close with `code` (see [`Outbound::kick`]).
    pub fn kick(&self, frames: &[Bytes], code: u16, reason: &str) -> bool {
        self.0.out.kick(frames, code, reason)
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

    #[test]
    fn a_kick_writes_its_frames_after_the_queue() {
        let (ep, out) = Endpoint::for_tests(1, 1);
        ep.send(Bytes::from_static(b"q"));
        assert!(ep.kick(&[Bytes::from_static(b"err"), Bytes::from_static(b"note")], 4007, ""));
        assert!(!ep.kick(&[Bytes::from_static(b"again")], 4004, ""), "the first close wins");
        let b = out.take();
        assert_eq!(
            b.frames,
            vec![Bytes::from_static(b"q"), Bytes::from_static(b"err"), Bytes::from_static(b"note")]
        );
        assert_eq!(b.close.unwrap().code, 4007);
        assert_eq!(out.buffered(), 8);
    }

    #[test]
    fn before_the_welcome_a_kick_drops_the_queue() {
        let (ep, out) = Endpoint::for_tests(1, 1);
        assert!(out.take_kick().close.is_none(), "nothing to take without a kick");
        ep.send(Bytes::from_static(b"queue status"));
        ep.kick(&[Bytes::from_static(b"err")], 4003, "");
        let b = out.take_kick();
        assert_eq!(b.frames, vec![Bytes::from_static(b"err")]);
        assert_eq!(b.close.unwrap().code, 4003);
        assert!(out.take().frames.is_empty());
    }

    #[tokio::test]
    async fn closed_resolves_on_any_close_request() {
        let out = Outbound::new(4);
        let waiter = {
            let out = out.clone();
            tokio::spawn(async move { out.closed().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(!out.send(Bytes::from_static(b"12345")), "overflow");
        waiter.await.expect("woken by the slow-consumer close");
        assert_eq!(out.close_request().unwrap().code, CLOSE_SLOW_CONSUMER);
        out.closed().await;
    }
}
