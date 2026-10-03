//! WebSocket connections (RFC 6455, DESIGN 5.8): the server side of the frame codec and the
//! closing handshake, exposed as a reader/writer pair that the realtime connection task drives.
//!
//! Client frames must be masked binary frames (text closes with 1003), one message is at most
//! `WS_MAX_MESSAGE_BYTES` (checked from the frame header, before any payload is buffered) and at
//! most 64 frames. Pings are answered at most 2 per second (burst 5), close frames are echoed,
//! and every closing path ends the socket 2 s after the close started at the latest.
//!
//! The reader handles control frames itself; its [`WsReader::next`] is cancel-safe, so it can
//! sit in a `select!` beside the connection's outbound queue. The realtime module owns that
//! queue and the slow-consumer rule (close 4303 through [`WsInfo::close`]).

use std::collections::{HashSet, VecDeque};
use std::future::pending;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::clock::SharedClock;
use crate::ids::ConnId;
use crate::log::Logger;
use crate::log_debug;
use crate::metrics::{self, Counter, CounterVec};

/// Normal closure.
pub const CLOSE_NORMAL: u16 = 1000;
/// The server is going away (shutdown).
pub const CLOSE_GOING_AWAY: u16 = 1001;
/// Protocol error.
pub const CLOSE_PROTOCOL_ERROR: u16 = 1002;
/// Unsupported data (a text frame).
pub const CLOSE_UNSUPPORTED: u16 = 1003;
/// No status code in the peer's close frame.
pub const CLOSE_NO_STATUS: u16 = 1005;
/// The connection ended without a close frame.
pub const CLOSE_ABNORMAL: u16 = 1006;
/// A close reason that is not UTF-8.
pub const CLOSE_INVALID_DATA: u16 = 1007;
/// A message over the size or fragment limit.
pub const CLOSE_TOO_BIG: u16 = 1009;
/// An internal error.
pub const CLOSE_INTERNAL: u16 = 1011;
/// The client did not read fast enough.
pub const CLOSE_SLOW_CONSUMER: u16 = 4303;

/// Frames per message, the first included.
pub const MAX_FRAGMENTS: u32 = 64;
/// How long a closing connection may take before its socket is dropped.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Pongs per second.
pub const PING_RATE: f64 = 2.0;
/// Pongs in a burst.
pub const PING_BURST: f64 = 5.0;
/// Longest close reason sent (bytes).
pub const MAX_CLOSE_REASON: usize = 123;

const OP_CONT: u8 = 0;
const OP_TEXT: u8 = 1;
const OP_BINARY: u8 = 2;
const OP_CLOSE: u8 = 8;
const OP_PING: u8 = 9;
const OP_PONG: u8 = 10;

/// Whether a peer may send `code` in a close frame.
pub fn is_valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// The `code` label of `scacelith_ws_closes_total`.
fn close_label(code: u16) -> String {
    if matches!(code, 1000..=1015 | 4000..=4399) { code.to_string() } else { "other".to_string() }
}

struct WsMetrics {
    bytes_in: Counter,
    bytes_out: Counter,
    frames_in: Counter,
    too_big: Counter,
    protocol_errors: Counter,
    pings_dropped: Counter,
    closes: CounterVec,
}

fn ws_metrics() -> &'static WsMetrics {
    static M: LazyLock<WsMetrics> = LazyLock::new(|| WsMetrics {
        bytes_in: metrics::counter("scacelith_ws_bytes_in_total", "Bytes received on WebSocket connections"),
        bytes_out: metrics::counter("scacelith_ws_bytes_out_total", "Bytes queued on WebSocket connections"),
        frames_in: metrics::counter("scacelith_ws_frames_in_total", "WebSocket data messages received"),
        too_big: metrics::counter(
            "scacelith_ws_too_big_total",
            "Connections closed for an oversized message",
        ),
        protocol_errors: metrics::counter(
            "scacelith_ws_protocol_errors_total",
            "Connections failed for a WebSocket protocol violation",
        ),
        pings_dropped: metrics::counter(
            "scacelith_ws_pings_dropped_total",
            "WebSocket pings not answered (rate limit)",
        ),
        closes: metrics::counter_vec(
            "scacelith_ws_closes_total",
            "Closed WebSocket connections by close code",
            &["code"],
        ),
    });
    &M
}

/// The header of an unmasked server frame.
fn frame_header(op: u8, len: usize, out: &mut BytesMut) {
    out.extend_from_slice(&[0x80 | op]);
    if len < 126 {
        out.extend_from_slice(&[len as u8]);
    } else if len < 65536 {
        out.extend_from_slice(&[126]);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.extend_from_slice(&[127]);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

/// A whole server frame.
fn server_frame(op: u8, payload: &[u8]) -> BytesMut {
    let mut out = BytesMut::with_capacity(payload.len() + 10);
    frame_header(op, payload.len(), &mut out);
    out.extend_from_slice(payload);
    out
}

/// A close frame: the code and the reason cut to 123 bytes (no payload for 1005, 1006 and 0).
fn close_frame(code: u16, reason: &str) -> BytesMut {
    if code == 0 || code == CLOSE_NO_STATUS || code == CLOSE_ABNORMAL {
        return server_frame(OP_CLOSE, &[]);
    }
    let r = &reason.as_bytes()[..reason.len().min(MAX_CLOSE_REASON)];
    let mut payload = Vec::with_capacity(2 + r.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(r);
    server_frame(OP_CLOSE, &payload)
}

/// How a connection ended: the code and reason it reports (ours when we closed first, the
/// peer's when it did, 1006 when it vanished).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseInfo {
    /// The close code.
    pub code: u16,
    /// The close reason.
    pub reason: String,
}

/// What the frame parser found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A whole binary message.
    Message(Bytes),
    /// A ping to answer with this payload.
    Pong(Bytes),
    /// A ping over the rate, not answered.
    PingDropped,
    /// The peer's close frame (1005: no status).
    PeerClose(CloseInfo),
    /// A protocol violation: close with this code and reason.
    Fail(CloseInfo),
}

/// The client frame parser: a pure state machine fed with the bytes read.
#[derive(Debug)]
pub struct FrameParser {
    max_message: usize,
    buf: BytesMut,
    frag: Option<BytesMut>,
    frag_count: u32,
    ping_tokens: f64,
    ping_at: f64,
    close_sent: bool,
    done: bool,
}

impl FrameParser {
    /// A parser for messages of at most `max_message` bytes, opened at `now` (monotonic ms).
    pub fn new(max_message: usize, now: f64) -> FrameParser {
        FrameParser {
            max_message,
            buf: BytesMut::new(),
            frag: None,
            frag_count: 0,
            ping_tokens: PING_BURST,
            ping_at: now,
            close_sent: false,
            done: false,
        }
    }

    /// From now on data frames are dropped and pings unanswered (our close frame is out).
    pub fn set_close_sent(&mut self) {
        self.close_sent = true;
    }

    /// Whether parsing stopped (a close frame or a violation).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Parses `data` read at `now`, appending what it finds to `out`.
    pub fn feed(&mut self, data: &[u8], now: f64, out: &mut Vec<Action>) {
        if self.done {
            return;
        }
        self.buf.extend_from_slice(data);
        self.parse(now, out);
    }

    fn fail(&mut self, code: u16, why: &str, out: &mut Vec<Action>) {
        self.done = true;
        self.buf.clear();
        self.frag = None;
        out.push(Action::Fail(CloseInfo { code, reason: why.to_string() }));
    }

    fn parse(&mut self, now: f64, out: &mut Vec<Action>) {
        while !self.done {
            let avail = self.buf.len();
            if avail < 2 {
                return;
            }
            let (b0, b1) = (self.buf[0], self.buf[1]);
            let fin = b0 & 0x80 != 0;
            let op = b0 & 0x0f;
            if b0 & 0x70 != 0 {
                return self.fail(CLOSE_PROTOCOL_ERROR, "reserved bits", out);
            }
            if b1 & 0x80 == 0 {
                return self.fail(CLOSE_PROTOCOL_ERROR, "unmasked frame", out);
            }
            let len7 = b1 & 0x7f;
            if op >= 8 {
                if op > OP_PONG {
                    return self.fail(CLOSE_PROTOCOL_ERROR, "reserved opcode", out);
                }
                if !fin {
                    return self.fail(CLOSE_PROTOCOL_ERROR, "fragmented control frame", out);
                }
                if len7 > 125 {
                    return self.fail(CLOSE_PROTOCOL_ERROR, "control frame too long", out);
                }
            } else if op == OP_TEXT {
                return self.fail(CLOSE_UNSUPPORTED, "text frames are not accepted", out);
            } else if op != OP_BINARY && op != OP_CONT {
                return self.fail(CLOSE_PROTOCOL_ERROR, "reserved opcode", out);
            }
            let hl = match len7 {
                126 => 8,
                127 => 14,
                _ => 6,
            };
            if avail < hl {
                return;
            }
            let len: u64 = match len7 {
                126 => {
                    let l = u16::from_be_bytes([self.buf[2], self.buf[3]]) as u64;
                    if l < 126 {
                        return self.fail(CLOSE_PROTOCOL_ERROR, "non-minimal length", out);
                    }
                    l
                }
                127 => {
                    let hi = u32::from_be_bytes([self.buf[2], self.buf[3], self.buf[4], self.buf[5]]);
                    let lo = u32::from_be_bytes([self.buf[6], self.buf[7], self.buf[8], self.buf[9]]);
                    if hi > 0x1f_ffff {
                        return self.fail(CLOSE_PROTOCOL_ERROR, "length above 2^53", out);
                    }
                    if hi == 0 && lo < 65536 {
                        return self.fail(CLOSE_PROTOCOL_ERROR, "non-minimal length", out);
                    }
                    (hi as u64) << 32 | lo as u64
                }
                l => l as u64,
            };
            if op < 8 {
                let before = if op == OP_CONT { self.frag.as_ref().map_or(0, |f| f.len()) } else { 0 };
                if before as u64 + len > self.max_message as u64 {
                    ws_metrics().too_big.inc();
                    return self.fail(CLOSE_TOO_BIG, "message too big", out);
                }
            }
            // The buffer grows with what arrives, never reserved from the announced length.
            let total = hl + len as usize;
            if avail < total {
                return;
            }
            let mut frame = self.buf.split_to(total);
            let key = [frame[hl - 4], frame[hl - 3], frame[hl - 2], frame[hl - 1]];
            let mut payload = frame.split_off(hl);
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= key[i & 3];
            }
            self.frame(fin, op, payload, now, out);
        }
    }

    fn frame(&mut self, fin: bool, op: u8, payload: BytesMut, now: f64, out: &mut Vec<Action>) {
        match op {
            OP_BINARY => {
                if self.frag.is_some() {
                    return self.fail(CLOSE_PROTOCOL_ERROR, "expected a continuation frame", out);
                }
                if self.close_sent {
                    return;
                }
                if fin {
                    ws_metrics().frames_in.inc();
                    out.push(Action::Message(payload.freeze()));
                } else {
                    self.frag = Some(payload);
                    self.frag_count = 1;
                }
            }
            OP_CONT => {
                let Some(frag) = self.frag.as_mut() else {
                    return self.fail(CLOSE_PROTOCOL_ERROR, "unexpected continuation frame", out);
                };
                self.frag_count += 1;
                if self.frag_count > MAX_FRAGMENTS {
                    return self.fail(CLOSE_TOO_BIG, "too many fragments", out);
                }
                frag.extend_from_slice(&payload);
                if !fin {
                    return;
                }
                let msg = self.frag.take().map(BytesMut::freeze).unwrap_or_default();
                if self.close_sent {
                    return;
                }
                ws_metrics().frames_in.inc();
                out.push(Action::Message(msg));
            }
            OP_PING => {
                self.ping_tokens =
                    (self.ping_tokens + (now - self.ping_at) * PING_RATE / 1000.0).min(PING_BURST);
                self.ping_at = now;
                if self.ping_tokens >= 1.0 {
                    self.ping_tokens -= 1.0;
                    if !self.close_sent {
                        out.push(Action::Pong(payload.freeze()));
                    }
                } else {
                    ws_metrics().pings_dropped.inc();
                    out.push(Action::PingDropped);
                }
            }
            OP_PONG => {}
            _ => {
                // OP_CLOSE: the opcode checks above leave nothing else.
                let (code, reason) = match payload.len() {
                    0 => (CLOSE_NO_STATUS, String::new()),
                    1 => return self.fail(CLOSE_PROTOCOL_ERROR, "bad close payload", out),
                    _ => {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        if !is_valid_close_code(code) {
                            return self.fail(CLOSE_PROTOCOL_ERROR, "invalid close code", out);
                        }
                        match std::str::from_utf8(&payload[2..]) {
                            Ok(r) => (code, r.to_string()),
                            Err(_) => return self.fail(CLOSE_INVALID_DATA, "close reason not UTF-8", out),
                        }
                    }
                };
                self.done = true;
                self.buf.clear();
                self.frag = None;
                out.push(Action::PeerClose(CloseInfo { code, reason }));
            }
        }
    }
}

/// The byte stream under a WebSocket connection.
pub trait WsIo: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> WsIo for T {}

/// Connection ids: u32 from 1, wrapping to 1, never one still open.
#[derive(Debug, Default)]
struct IdAllocator {
    next: ConnId,
    live: HashSet<ConnId>,
}

impl IdAllocator {
    fn allocate(&mut self) -> ConnId {
        loop {
            self.next = if self.next == ConnId::MAX { 1 } else { self.next + 1 };
            if self.live.insert(self.next) {
                return self.next;
            }
        }
    }
}

fn ids() -> &'static Mutex<IdAllocator> {
    static IDS: LazyLock<Mutex<IdAllocator>> = LazyLock::new(Mutex::default);
    &IDS
}

/// An allocated connection id (one allocator for the process), given back when dropped.
#[derive(Debug)]
pub struct ConnIdGuard(ConnId);

impl ConnIdGuard {
    /// Allocates the next free id.
    pub fn allocate() -> ConnIdGuard {
        ConnIdGuard(ids().lock().allocate())
    }

    /// The id.
    pub fn id(&self) -> ConnId {
        self.0
    }
}

impl Drop for ConnIdGuard {
    fn drop(&mut self) {
        ids().lock().live.remove(&self.0);
    }
}

/// What the admission hook gave a connection (released when the connection ends).
#[derive(Default)]
pub struct AdmissionPermit(Option<Box<dyn Send + Sync>>);

impl std::fmt::Debug for AdmissionPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "AdmissionPermit(held)" } else { "AdmissionPermit(none)" })
    }
}

impl AdmissionPermit {
    /// A permit holding `value` until the connection ends (its `Drop` releases the slot).
    pub fn new(value: impl Send + Sync + 'static) -> AdmissionPermit {
        AdmissionPermit(Some(Box::new(value)))
    }

    /// A permit that holds nothing.
    pub fn none() -> AdmissionPermit {
        AdmissionPermit(None)
    }
}

/// Settings of the WebSocket connections.
#[derive(Clone)]
pub struct WsSettings {
    /// Largest client message (bytes).
    pub max_message_bytes: usize,
    /// How long a closing connection may take.
    pub close_timeout: Duration,
    /// The monotonic clock of `last_recv_ms` and the ping bucket.
    pub clock: SharedClock,
    /// The logger.
    pub log: Logger,
}

impl std::fmt::Debug for WsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsSettings").field("max_message_bytes", &self.max_message_bytes).finish()
    }
}

struct WsShared {
    id: ConnIdGuard,
    ip: IpAddr,
    opened_at: f64,
    last_recv: AtomicU64,
    out: tokio::sync::Mutex<WriteHalf<Box<dyn WsIo>>>,
    close_sent: AtomicBool,
    close_written: AtomicBool,
    close: Mutex<Option<CloseInfo>>,
    destroy_at: watch::Sender<Option<Instant>>,
    close_timeout: Duration,
    _permit: AdmissionPermit,
}

impl Drop for WsShared {
    fn drop(&mut self) {
        let code = self.close.get_mut().as_ref().map_or(CLOSE_ABNORMAL, |c| c.code);
        ws_metrics().closes.with(&[&close_label(code)]).inc();
    }
}

impl WsShared {
    /// Completes when the socket must be dropped: `close_timeout` after the close started.
    async fn destroyed(&self) {
        let mut rx = self.destroy_at.subscribe();
        let at = loop {
            if let Some(at) = *rx.borrow_and_update() {
                break at;
            }
            if rx.changed().await.is_err() {
                pending::<()>().await;
            }
        };
        tokio::time::sleep_until(at).await;
    }

    fn reported(&self) -> CloseInfo {
        self.close.lock().clone().unwrap_or(CloseInfo { code: CLOSE_ABNORMAL, reason: String::new() })
    }

    /// Starts closing once: records the reported close, arms the destroy deadline, then writes
    /// `frame` (if any) after what is already being written and half-closes the socket.
    fn begin_close(self: &Arc<Self>, frame: Option<BytesMut>, reported: CloseInfo) -> bool {
        if self.close_sent.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.close.lock().get_or_insert(reported);
        self.destroy_at.send_replace(Some(Instant::now() + self.close_timeout));
        let me = self.clone();
        tokio::spawn(async move {
            let work = async {
                let mut out = me.out.lock().await;
                me.close_written.store(true, Ordering::Release);
                if let Some(frame) = frame {
                    ws_metrics().bytes_out.add(frame.len() as u64);
                    if out.write_all(&frame).await.is_err() {
                        return;
                    }
                }
                let _ = out.flush().await;
                let _ = out.shutdown().await;
            };
            tokio::select! {
                _ = work => {}
                _ = me.destroyed() => {}
            }
        });
        true
    }

    /// Answers a ping (skipped once our close started).
    fn pong(self: &Arc<Self>, payload: Bytes) {
        let me = self.clone();
        tokio::spawn(async move {
            let work = async {
                let mut out = me.out.lock().await;
                if me.close_sent.load(Ordering::Acquire) {
                    return;
                }
                let frame = server_frame(OP_PONG, &payload);
                ws_metrics().bytes_out.add(frame.len() as u64);
                if out.write_all(&frame).await.is_ok() {
                    let _ = out.flush().await;
                }
            };
            tokio::select! {
                _ = work => {}
                _ = me.destroyed() => {}
            }
        });
    }
}

/// A cheap, cloneable view of a connection: its identity, liveness and a way to close it from
/// any task.
#[derive(Clone)]
pub struct WsInfo(Arc<WsShared>);

impl std::fmt::Debug for WsInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsInfo").field("id", &self.id()).field("closing", &self.is_closing()).finish()
    }
}

impl WsInfo {
    /// The connection id.
    pub fn id(&self) -> ConnId {
        self.0.id.id()
    }

    /// The client address.
    pub fn ip(&self) -> IpAddr {
        self.0.ip
    }

    /// When the connection opened (monotonic ms).
    pub fn opened_at_ms(&self) -> f64 {
        self.0.opened_at
    }

    /// When bytes last arrived, any bytes (monotonic ms).
    pub fn last_recv_ms(&self) -> f64 {
        f64::from_bits(self.0.last_recv.load(Ordering::Relaxed))
    }

    /// Whether the connection is closing (no more messages are sent).
    pub fn is_closing(&self) -> bool {
        self.0.close_sent.load(Ordering::Acquire)
    }

    /// Starts the closing handshake: a close frame with `code` and `reason` (cut to 123 bytes)
    /// after what is being written, then a half-close; the socket is dropped 2 s later at the
    /// latest. Only the first close counts.
    pub fn close(&self, code: u16, reason: &str) {
        self.0.begin_close(Some(close_frame(code, reason)), CloseInfo { code, reason: reason.to_string() });
    }
}

/// The error of a send on a closing connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsClosed;

impl std::fmt::Display for WsClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the WebSocket connection is closing")
    }
}

impl std::error::Error for WsClosed {}

/// The sending half: binary messages, one frame each.
pub struct WsWriter {
    shared: Arc<WsShared>,
    buf: BytesMut,
}

impl std::fmt::Debug for WsWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsWriter").field("id", &self.shared.id.id()).finish()
    }
}

impl WsWriter {
    /// The connection's view.
    pub fn info(&self) -> WsInfo {
        WsInfo(self.shared.clone())
    }

    /// Sends one binary message and flushes. Fails once the connection is closing; gives up when
    /// the socket is dropped (2 s after a close started). Not cancel-safe: a cancelled send may
    /// leave half a frame, so await it to the end.
    pub async fn send(&mut self, message: &[u8]) -> Result<(), WsClosed> {
        self.send_batch(std::slice::from_ref(&message)).await
    }

    /// Sends several binary messages in one write. Same rules as [`WsWriter::send`].
    pub async fn send_batch(&mut self, messages: &[&[u8]]) -> Result<(), WsClosed> {
        if self.shared.close_sent.load(Ordering::Acquire) {
            return Err(WsClosed);
        }
        self.buf.clear();
        for m in messages {
            frame_header(OP_BINARY, m.len(), &mut self.buf);
            self.buf.extend_from_slice(m);
        }
        let shared = self.shared.clone();
        let buf = &self.buf;
        let work = async {
            let mut out = shared.out.lock().await;
            if shared.close_written.load(Ordering::Acquire) {
                return Err(WsClosed);
            }
            ws_metrics().bytes_out.add(buf.len() as u64);
            out.write_all(buf).await.map_err(|_| WsClosed)?;
            out.flush().await.map_err(|_| WsClosed)
        };
        tokio::select! {
            r = work => r,
            _ = shared.destroyed() => Err(WsClosed),
        }
    }

    /// Starts the closing handshake (see [`WsInfo::close`]).
    pub fn close(&self, code: u16, reason: &str) {
        self.info().close(code, reason);
    }
}

/// What the reader returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    /// A binary message.
    Message(Bytes),
    /// The connection ended (returned again by every later call).
    Closed(CloseInfo),
}

/// The receiving half: messages in order, control frames handled inside.
pub struct WsReader {
    shared: Arc<WsShared>,
    io: ReadHalf<Box<dyn WsIo>>,
    parser: FrameParser,
    clock: SharedClock,
    log: Logger,
    pending: Vec<u8>,
    queue: VecDeque<Bytes>,
    finished: Option<CloseInfo>,
    actions: Vec<Action>,
}

impl std::fmt::Debug for WsReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsReader").field("id", &self.shared.id.id()).finish()
    }
}

/// A new WebSocket connection: its reader, writer and view.
#[derive(Debug)]
pub struct WsConnection {
    /// The receiving half.
    pub reader: WsReader,
    /// The sending half.
    pub writer: WsWriter,
    /// The view (id, address, liveness, close).
    pub info: WsInfo,
}

impl WsConnection {
    /// Wraps an upgraded stream. `leftover` holds the bytes read after the request head.
    pub fn new(
        io: Box<dyn WsIo>,
        leftover: Bytes,
        ip: IpAddr,
        permit: AdmissionPermit,
        settings: &WsSettings,
    ) -> WsConnection {
        let now = settings.clock.mono_ms();
        let (rd, wr) = tokio::io::split(io);
        let shared = Arc::new(WsShared {
            id: ConnIdGuard::allocate(),
            ip,
            opened_at: now,
            last_recv: AtomicU64::new(now.to_bits()),
            out: tokio::sync::Mutex::new(wr),
            close_sent: AtomicBool::new(false),
            close_written: AtomicBool::new(false),
            close: Mutex::new(None),
            destroy_at: watch::Sender::new(None),
            close_timeout: settings.close_timeout,
            _permit: permit,
        });
        let reader = WsReader {
            shared: shared.clone(),
            io: rd,
            parser: FrameParser::new(settings.max_message_bytes, now),
            clock: settings.clock.clone(),
            log: settings.log.clone(),
            pending: leftover.to_vec(),
            queue: VecDeque::new(),
            finished: None,
            actions: Vec::new(),
        };
        let writer = WsWriter { shared: shared.clone(), buf: BytesMut::new() };
        WsConnection { reader, writer, info: WsInfo(shared) }
    }
}

impl WsReader {
    /// The connection's view.
    pub fn info(&self) -> WsInfo {
        WsInfo(self.shared.clone())
    }

    /// The next message, or how the connection ended. Cancel-safe.
    pub async fn next(&mut self) -> WsEvent {
        loop {
            if let Some(m) = self.queue.pop_front() {
                return WsEvent::Message(m);
            }
            if let Some(c) = &self.finished {
                return WsEvent::Closed(c.clone());
            }
            if !self.pending.is_empty() {
                let data = std::mem::take(&mut self.pending);
                self.received(&data);
                continue;
            }
            let mut chunk = [0u8; 4096];
            let read = tokio::select! {
                r = self.io.read(&mut chunk) => Some(r),
                _ = self.shared.destroyed() => None,
            };
            match read {
                None => self.finish(),
                Some(Ok(0)) => {
                    // The peer half-closed: end our side too.
                    self.shared.begin_close(None, CloseInfo { code: CLOSE_ABNORMAL, reason: String::new() });
                    self.finish();
                }
                Some(Ok(n)) => self.received(&chunk[..n]),
                Some(Err(_)) => {
                    self.shared
                        .close
                        .lock()
                        .get_or_insert(CloseInfo { code: CLOSE_ABNORMAL, reason: String::new() });
                    self.finish();
                }
            }
        }
    }

    fn finish(&mut self) {
        self.finished = Some(self.shared.reported());
    }

    fn received(&mut self, data: &[u8]) {
        let now = self.clock.mono_ms();
        self.shared.last_recv.store(now.to_bits(), Ordering::Relaxed);
        ws_metrics().bytes_in.add(data.len() as u64);
        if self.shared.close_sent.load(Ordering::Acquire) {
            self.parser.set_close_sent();
        }
        let mut actions = std::mem::take(&mut self.actions);
        self.parser.feed(data, now, &mut actions);
        for action in actions.drain(..) {
            match action {
                Action::Message(m) => self.queue.push_back(m),
                Action::Pong(p) => self.shared.pong(p),
                Action::PingDropped => {}
                Action::PeerClose(info) => {
                    let echo = if info.code == CLOSE_NO_STATUS { CLOSE_NORMAL } else { info.code };
                    if !self.shared.begin_close(Some(close_frame(echo, "")), info) {
                        // Our close was out first: the handshake is complete.
                        self.finish();
                    }
                }
                Action::Fail(info) => {
                    ws_metrics().protocol_errors.inc();
                    log_debug!(self.log, "ws protocol error", {
                        "connId": self.shared.id.id(), "why": info.reason, "code": info.code,
                    });
                    self.shared.begin_close(Some(close_frame(info.code, &info.reason)), info);
                }
            }
        }
        self.actions = actions;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Options of a forged client frame.
    #[derive(Clone, Copy)]
    pub(crate) struct FrameOpts {
        pub fin: bool,
        pub mask: bool,
        pub rsv: u8,
        pub fake_length: Option<u64>,
    }

    impl Default for FrameOpts {
        fn default() -> FrameOpts {
            FrameOpts { fin: true, mask: true, rsv: 0, fake_length: None }
        }
    }

    /// A client frame (masked unless told otherwise).
    pub(crate) fn client_frame(op: u8, payload: &[u8], o: FrameOpts) -> Vec<u8> {
        let len = o.fake_length.unwrap_or(payload.len() as u64);
        let mut out = vec![(if o.fin { 0x80 } else { 0 }) | (o.rsv << 4) | op];
        let m = if o.mask { 0x80 } else { 0 };
        if len < 126 {
            out.push(m | len as u8);
        } else if len < 65536 {
            out.push(m | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(m | 127);
            out.extend_from_slice(&len.to_be_bytes());
        }
        if o.mask {
            let key = [0x12, 0x34, 0x56, 0x78];
            out.extend_from_slice(&key);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
        } else {
            out.extend_from_slice(payload);
        }
        out
    }

    pub(crate) fn frame(op: u8, payload: &[u8]) -> Vec<u8> {
        client_frame(op, payload, FrameOpts::default())
    }

    fn feed(max: usize, pieces: &[&[u8]]) -> Vec<Action> {
        let mut p = FrameParser::new(max, 0.0);
        let mut out = Vec::new();
        for piece in pieces {
            p.feed(piece, 0.0, &mut out);
        }
        out
    }

    fn fail_code(actions: &[Action]) -> Option<u16> {
        actions.iter().find_map(|a| match a {
            Action::Fail(c) => Some(c.code),
            _ => None,
        })
    }

    #[test]
    fn close_codes() {
        assert!(is_valid_close_code(1000) && is_valid_close_code(4999) && is_valid_close_code(1011));
        for bad in [1005, 1006, 999, 2000, 5000, 1004, 1015, 2999] {
            assert!(!is_valid_close_code(bad), "{bad}");
        }
        assert_eq!(
            (close_label(1000), close_label(4303), close_label(4400), close_label(3000)),
            ("1000".into(), "4303".into(), "other".into(), "other".into())
        );
    }

    #[test]
    fn server_frames() {
        assert_eq!(&server_frame(OP_BINARY, &[1, 2])[..], &[0x82, 2, 1, 2]);
        assert_eq!(&server_frame(OP_BINARY, &[0; 300])[..4], &[0x82, 126, 1, 44]);
        assert_eq!(&server_frame(OP_BINARY, &[0; 70000])[..10], &[0x82, 127, 0, 0, 0, 0, 0, 1, 0x11, 0x70]);
        assert_eq!(&close_frame(4000, "bye")[..], &[0x88, 5, 0x0f, 0xa0, b'b', b'y', b'e']);
        assert_eq!(&close_frame(1005, "x")[..], &[0x88, 0]);
        assert_eq!(close_frame(1000, &"é".repeat(100)).len(), 2 + 2 + 123, "the reason is cut to 123 bytes");
    }

    #[test]
    fn messages_and_violations() {
        let msgs = feed(512, &[&frame(2, &[0xee, 1, 2])]);
        assert_eq!(msgs, [Action::Message(Bytes::from_static(&[0xee, 1, 2]))]);
        let cases: Vec<(Vec<u8>, u16, &str)> = vec![
            (client_frame(2, &[1], FrameOpts { mask: false, ..Default::default() }), 1002, "unmasked frame"),
            (client_frame(2, &[1], FrameOpts { rsv: 4, ..Default::default() }), 1002, "reserved bits"),
            (frame(1, b"hello"), 1003, "text frames are not accepted"),
            (frame(3, &[1]), 1002, "reserved opcode"),
            (frame(11, &[1]), 1002, "reserved opcode"),
            (vec![0x82, 0x80 | 127, 0, 0, 0, 0, 0, 0x10, 0, 0, 1, 2, 3, 4], 1009, "message too big"),
            (vec![0x82, 0x80 | 126, 0x02, 0x01, 1, 2, 3, 4], 1009, "message too big"),
            (vec![0x82, 0x80 | 126, 0, 5, 1, 2, 3, 4, 0, 0, 0, 0, 0], 1002, "non-minimal length"),
            (vec![0x82, 0x80 | 127, 0xff, 0, 0, 0, 0, 0, 0, 1, 1, 2, 3, 4], 1002, "length above 2^53"),
            (vec![0x82, 0x80 | 127, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4], 1002, "non-minimal length"),
            (frame(0, &[1]), 1002, "unexpected continuation frame"),
            (frame(9, &[0; 126]), 1002, "control frame too long"),
            (
                client_frame(9, b"x", FrameOpts { fin: false, ..Default::default() }),
                1002,
                "fragmented control frame",
            ),
            (frame(8, &[3]), 1002, "bad close payload"),
            (frame(8, &1005u16.to_be_bytes()), 1002, "invalid close code"),
            (frame(8, &999u16.to_be_bytes()), 1002, "invalid close code"),
            (frame(8, &2500u16.to_be_bytes()), 1002, "invalid close code"),
            (frame(8, &[0x03, 0xe8, 0xff, 0xfe]), 1007, "close reason not UTF-8"),
        ];
        for (bytes, code, why) in cases {
            let out = feed(512, &[&bytes]);
            assert_eq!(out, [Action::Fail(CloseInfo { code, reason: why.into() })], "{why}");
        }
        assert_eq!(feed(512, &[&frame(2, &[1; 512])]).len(), 1, "exactly the limit");
        let close = feed(512, &[&frame(8, &[])]);
        assert_eq!(close, [Action::PeerClose(CloseInfo { code: 1005, reason: String::new() })]);
        let mut p = 4321u16.to_be_bytes().to_vec();
        p.extend_from_slice(b"bye");
        assert_eq!(
            feed(512, &[&frame(8, &p)]),
            [Action::PeerClose(CloseInfo { code: 4321, reason: "bye".into() })]
        );
    }

    #[test]
    fn fragments_and_interleaved_control_frames() {
        let nf = FrameOpts { fin: false, ..Default::default() };
        let all = [
            client_frame(2, &[1, 2], nf),
            frame(9, b"p"),
            client_frame(0, &[3, 4], nf),
            frame(10, b"ignored"),
            frame(0, &[5]),
        ]
        .concat();
        assert_eq!(
            feed(512, &[&all]),
            [Action::Pong(Bytes::from_static(b"p")), Action::Message(Bytes::from_static(&[1, 2, 3, 4, 5]))]
        );
        let too_big = [client_frame(2, &[0; 300], nf), frame(0, &[0; 300])].concat();
        assert_eq!(fail_code(&feed(512, &[&too_big])), Some(1009));
        let mut many = client_frame(2, &[], nf);
        for _ in 0..70 {
            many.extend(client_frame(0, &[], nf));
        }
        let out = feed(512, &[&many]);
        assert_eq!(out, [Action::Fail(CloseInfo { code: 1009, reason: "too many fragments".into() })]);
        let mut exactly = client_frame(2, &[], nf);
        for _ in 0..62 {
            exactly.extend(client_frame(0, &[], nf));
        }
        exactly.extend(frame(0, &[9]));
        assert_eq!(feed(512, &[&exactly]), [Action::Message(Bytes::from_static(&[9]))], "64 frames pass");
        let nested = [client_frame(2, &[1], nf), frame(2, &[2])].concat();
        assert_eq!(fail_code(&feed(512, &[&nested])), Some(1002));
    }

    #[test]
    fn pings_are_rate_limited() {
        let mut p = FrameParser::new(512, 0.0);
        let mut out = Vec::new();
        for i in 0..20u8 {
            p.feed(&frame(9, &[i]), 10.0, &mut out);
        }
        let pongs = out.iter().filter(|a| matches!(a, Action::Pong(_))).count();
        assert_eq!(pongs, 5, "the burst");
        out.clear();
        p.feed(&frame(9, &[0]), 510.0, &mut out);
        assert_eq!(out, [Action::Pong(Bytes::from_static(&[0]))], "one more after half a second");
        out.clear();
        p.feed(&frame(9, &[0]), 600.0, &mut out);
        assert_eq!(out, [Action::PingDropped]);
        p.set_close_sent();
        out.clear();
        p.feed(&[frame(9, &[1]), frame(2, &[1])].concat(), 100_000.0, &mut out);
        assert!(out.is_empty(), "no pong and no data after our close: {out:?}");
    }

    #[test]
    fn parsing_stops_after_a_close_or_a_failure() {
        let all = [frame(2, &[1]), frame(8, &[]), frame(2, &[2])].concat();
        let out = feed(512, &[&all]);
        assert_eq!(out.len(), 2);
        let all = [frame(1, b"x"), frame(2, &[2])].concat();
        assert_eq!(feed(512, &[&all]).len(), 1);
    }

    /// A deterministic pseudo-random source (the Node test's LCG).
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = (self.0.wrapping_mul(1_103_515_245).wrapping_add(12345)) & 0x7fff_ffff;
            (self.0 % n as u64) as usize
        }

        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next(256) as u8).collect()
        }
    }

    #[test]
    fn a_64k_frame_read_in_small_pieces() {
        let mut r = Lcg(7);
        let payload = r.bytes(65000);
        let f = frame(2, &payload);
        for size in [1, 7, 1448] {
            let pieces: Vec<&[u8]> = f.chunks(size).collect();
            let out = feed(65536, &pieces);
            assert_eq!(out, [Action::Message(Bytes::from(payload.clone()))], "pieces of {size}");
        }
    }

    #[test]
    fn the_same_actions_wherever_the_reads_are_cut() {
        let mut r = Lcg(4242);
        for t in 0..400 {
            let max = [125, 512, 65536][r.next(3)];
            let mut frames: Vec<u8> = Vec::new();
            for _ in 0..1 + r.next(5) {
                let k = r.next(10);
                let n = r.next(if k < 7 { max.min(70000) + 10 } else { 130 });
                let p = r.bytes(n);
                match k {
                    0..=4 => frames.extend(frame(2, &p)),
                    5 | 6 => {
                        let (a, b) = p.split_at(p.len() / 2);
                        frames.extend(client_frame(2, a, FrameOpts { fin: false, ..Default::default() }));
                        frames.extend(frame(0, b));
                    }
                    7 => frames.extend(frame(9, &p[..p.len().min(125)])),
                    8 => frames.extend(client_frame(
                        2,
                        &p[..p.len().min(20)],
                        FrameOpts { mask: false, ..Default::default() },
                    )),
                    _ => frames.extend(client_frame(
                        2,
                        &[],
                        FrameOpts { fake_length: Some(max as u64 + 1), ..Default::default() },
                    )),
                }
            }
            let whole = feed(max, &[&frames]);
            let mut pieces = Vec::new();
            let mut o = 0;
            while o < frames.len() {
                let bound = if r.next(2) == 1 { 3 } else { 2000 };
                let n = 1 + r.next(bound);
                let end = (o + n).min(frames.len());
                pieces.push(&frames[o..end]);
                o = end;
            }
            assert_eq!(feed(max, &pieces), whole, "sequence {t}");
        }
    }

    #[test]
    fn ids_wrap_and_skip_live_connections() {
        let mut ids = IdAllocator::default();
        assert_eq!((ids.allocate(), ids.allocate(), ids.allocate()), (1, 2, 3));
        ids.live.remove(&1);
        ids.next = ConnId::MAX - 1;
        assert_eq!(ids.allocate(), ConnId::MAX);
        assert_eq!(ids.allocate(), 1, "wraps to 1, free again");
        assert_eq!(ids.allocate(), 4, "2 and 3 are still open");
        let (a, b) = (ConnIdGuard::allocate(), ConnIdGuard::allocate());
        assert_ne!(a.id(), b.id());
    }

    /// Reads one server frame: `(opcode, payload)`, `None` at the end of the stream.
    pub(crate) async fn read_server_frame<R: AsyncRead + Unpin>(r: &mut R) -> Option<(u8, Vec<u8>)> {
        let mut h = [0u8; 2];
        r.read_exact(&mut h).await.ok()?;
        assert_eq!(h[1] & 0x80, 0, "server frames are not masked");
        let len = match h[1] & 0x7f {
            126 => {
                let mut l = [0u8; 2];
                r.read_exact(&mut l).await.ok()?;
                u16::from_be_bytes(l) as usize
            }
            127 => {
                let mut l = [0u8; 8];
                r.read_exact(&mut l).await.ok()?;
                u64::from_be_bytes(l) as usize
            }
            l => l as usize,
        };
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await.ok()?;
        Some((h[0] & 0x0f, payload))
    }

    fn settings() -> WsSettings {
        WsSettings {
            max_message_bytes: 512,
            close_timeout: CLOSE_TIMEOUT,
            clock: crate::clock::system(),
            log: Logger::root().child("ws-test"),
        }
    }

    fn open(buffer: usize) -> (WsConnection, tokio::io::DuplexStream) {
        let (server, client) = tokio::io::duplex(buffer);
        let conn = WsConnection::new(
            Box::new(server),
            Bytes::new(),
            IpAddr::from([192, 0, 2, 1]),
            AdmissionPermit::none(),
            &settings(),
        );
        (conn, client)
    }

    fn closed(code: u16, reason: &str) -> WsEvent {
        WsEvent::Closed(CloseInfo { code, reason: reason.to_string() })
    }

    #[tokio::test]
    async fn messages_both_ways_and_pongs() {
        let (mut c, mut client) = open(1 << 16);
        client.write_all(&frame(2, &[0xee, 1, 2])).await.expect("write");
        assert_eq!(c.reader.next().await, WsEvent::Message(Bytes::from_static(&[0xee, 1, 2])));
        c.writer.send(&[0xee, 1, 2]).await.expect("send");
        assert_eq!(read_server_frame(&mut client).await, Some((2, vec![0xee, 1, 2])));
        c.writer.send_batch(&[&[1], &[2, 3]]).await.expect("batch");
        assert_eq!(read_server_frame(&mut client).await, Some((2, vec![1])));
        assert_eq!(read_server_frame(&mut client).await, Some((2, vec![2, 3])));
        client.write_all(&[frame(9, b"abc"), frame(2, &[7])].concat()).await.expect("write");
        assert_eq!(c.reader.next().await, WsEvent::Message(Bytes::from_static(&[7])));
        assert_eq!(read_server_frame(&mut client).await, Some((10, b"abc".to_vec())));
        assert!(c.info.last_recv_ms() >= c.info.opened_at_ms());
    }

    /// Drives the reader on its own task (as the connection task would) until it ends.
    fn drive(mut reader: WsReader) -> tokio::task::JoinHandle<(WsEvent, WsEvent)> {
        tokio::spawn(async move {
            loop {
                if let ev @ WsEvent::Closed(_) = reader.next().await {
                    return (ev, reader.next().await);
                }
            }
        })
    }

    #[tokio::test]
    async fn a_client_close_is_echoed_and_reported() {
        let (c, mut client) = open(1 << 16);
        let WsConnection { reader, mut writer, .. } = c;
        let reading = drive(reader);
        let mut p = 4321u16.to_be_bytes().to_vec();
        p.extend_from_slice(b"bye");
        client.write_all(&frame(8, &p)).await.expect("write");
        assert_eq!(
            read_server_frame(&mut client).await,
            Some((8, 4321u16.to_be_bytes().to_vec())),
            "code, no reason"
        );
        assert_eq!(read_server_frame(&mut client).await, None, "then the server half-closes");
        drop(client);
        let (first, again) = reading.await.expect("the reader ends");
        assert_eq!((first, again), (closed(4321, "bye"), closed(4321, "bye")));
        assert_eq!(writer.send(&[1]).await, Err(WsClosed));
    }

    #[tokio::test]
    async fn an_empty_close_is_answered_with_1000() {
        let (c, mut client) = open(1 << 16);
        let reading = drive(c.reader);
        client.write_all(&frame(8, &[])).await.expect("write");
        assert_eq!(read_server_frame(&mut client).await, Some((8, 1000u16.to_be_bytes().to_vec())));
        drop(client);
        assert_eq!(reading.await.expect("ends").0, closed(1005, ""));
    }

    #[tokio::test]
    async fn a_server_close_carries_code_and_reason() {
        let (mut c, mut client) = open(1 << 16);
        c.writer.close(4000, "bye");
        assert!(c.info.is_closing());
        assert_eq!(c.writer.send(&[1]).await, Err(WsClosed));
        let mut want = 4000u16.to_be_bytes().to_vec();
        want.extend_from_slice(b"bye");
        assert_eq!(read_server_frame(&mut client).await, Some((8, want)));
        assert_eq!(read_server_frame(&mut client).await, None);
        client.write_all(&frame(8, &4000u16.to_be_bytes())).await.expect("echo");
        assert_eq!(c.reader.next().await, closed(4000, "bye"));
    }

    #[tokio::test]
    async fn violations_close_with_their_code() {
        let (c, mut client) = open(1 << 16);
        let reading = drive(c.reader);
        client.write_all(&frame(1, b"hello")).await.expect("write");
        let (op, payload) = read_server_frame(&mut client).await.expect("a close frame");
        assert_eq!((op, &payload[..2]), (8, &1003u16.to_be_bytes()[..]));
        assert_eq!(&payload[2..], b"text frames are not accepted");
        drop(client);
        assert_eq!(reading.await.expect("ends").0, closed(1003, "text frames are not accepted"));
    }

    #[tokio::test]
    async fn a_vanished_peer_reports_1006() {
        let (mut c, client) = open(1 << 16);
        drop(client);
        assert_eq!(c.reader.next().await, closed(1006, ""));
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_does_not_read_is_dropped_two_seconds_after_the_close() {
        let (c, mut client) = open(4096);
        let WsConnection { mut reader, mut writer, info } = c;
        let sender = tokio::spawn(async move {
            let chunk = vec![1u8; 60000];
            let mut sent = 0;
            while writer.send(&chunk).await.is_ok() {
                sent += 1;
            }
            sent
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        info.close(CLOSE_SLOW_CONSUMER, "slow consumer");
        let started = Instant::now();
        assert_eq!(sender.await.expect("the sender ends"), 0, "the first frame never fit");
        assert_eq!(reader.next().await, closed(4303, "slow consumer"));
        assert!(started.elapsed() >= CLOSE_TIMEOUT);
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut sink)).await;
    }

    #[tokio::test]
    async fn leftover_head_bytes_are_parsed_first() {
        let (server, _client) = tokio::io::duplex(1024);
        let mut c = WsConnection::new(
            Box::new(server),
            Bytes::from(frame(2, &[5, 6])),
            IpAddr::from([192, 0, 2, 1]),
            AdmissionPermit::none(),
            &settings(),
        );
        assert_eq!(c.reader.next().await, WsEvent::Message(Bytes::from_static(&[5, 6])));
    }
}
