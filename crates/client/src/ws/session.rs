//! A WebSocket session carrying a Scacelith-style binary protocol: every client message starts
//! with a type byte and a `seq` (u32, little-endian) that numbers the messages of the
//! connection from 1 (true of the realtime protocol v1 and of the former protocol 3).
//!
//! One task per session owns the socket: it reads and parses the frames, answers WebSocket pings,
//! runs the optional [`AutoReply`] (the protocol's heartbeat answer), writes what the session's
//! owner queued, and runs the closing handshake. The owner sends with [`Session::send`] (which
//! numbers the message) and receives whole messages with [`Session::recv`], each with the
//! instant its last byte was read.
//!
//! Sequence numbers are taken in the order messages are queued, and the task writes them in that
//! order, automatic answers included: a message never overtakes one with a lower `seq`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::frame::{
    self, Frame, OP_BINARY, OP_CLOSE, OP_CONTINUATION, OP_PING, OP_PONG, OP_TEXT, Role, apply_mask,
    encode_header,
};
use super::handshake::{UpgradeRequest, client_handshake};
use crate::error::{ClientError, CloseInfo, Closer, Result, with_timeout};
use crate::net::{ConnectTimings, Endpoint};

/// An automatic answer to a received message: given the message, the answer to send at once
/// (its bytes 1 to 4 are replaced by its `seq`), or `None`.
pub type AutoReply = fn(&[u8]) -> Option<Vec<u8>>;

/// Settings of a session.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Request target of the upgrade.
    pub path: String,
    /// The subprotocol to ask for (and require in the answer).
    pub subprotocol: String,
    /// Largest message accepted from the server, all fragments together (close 1009 beyond).
    pub max_message: usize,
    /// Deadline of the WebSocket upgrade (after the TCP connect and TLS handshake).
    pub handshake_timeout: Duration,
    /// How long a closing handshake started by the client waits for the server's close frame.
    pub close_timeout: Duration,
    /// Automatic answer run on every received message, before it is delivered.
    pub auto_reply: Option<AutoReply>,
    /// More header lines in the upgrade request.
    pub extra_headers: Vec<(String, String)>,
}

impl SessionOptions {
    /// Defaults for `subprotocol`: path `/ws`, messages up to 64 KiB, 10 s upgrade deadline, 2 s
    /// closing handshake, no automatic answer.
    pub fn new(subprotocol: impl Into<String>) -> SessionOptions {
        SessionOptions {
            path: "/ws".into(),
            subprotocol: subprotocol.into(),
            max_message: 64 * 1024,
            handshake_timeout: Duration::from_secs(10),
            close_timeout: Duration::from_secs(2),
            auto_reply: None,
            extra_headers: Vec::new(),
        }
    }
}

/// A message received.
#[derive(Clone, Debug)]
pub struct Incoming {
    /// The message (all fragments joined).
    pub payload: Bytes,
    /// When the read that completed it returned.
    pub at: std::time::Instant,
}

/// Counters of a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Messages received.
    pub messages_in: u64,
    /// Messages queued by the owner.
    pub messages_out: u64,
    /// Bytes read from the socket (TLS removed).
    pub bytes_in: u64,
    /// Automatic answers sent.
    pub auto_replies: u64,
}

enum Outgoing {
    /// A complete, masked frame.
    Frame(Bytes),
    /// Start the closing handshake.
    Close { code: u16, reason: String },
}

/// Masking keys from the operating system's random source, fetched 64 at a time.
struct MaskSource {
    pool: [u8; 256],
    pos: usize,
}

impl MaskSource {
    fn new() -> MaskSource {
        MaskSource { pool: [0; 256], pos: 256 }
    }

    fn next_key(&mut self) -> [u8; 4] {
        if self.pos + 4 > self.pool.len() {
            getrandom::fill(&mut self.pool).expect("the operating system's random source is available");
            self.pos = 0;
        }
        let key = self.pool[self.pos..self.pos + 4].try_into().expect("4 bytes");
        self.pos += 4;
        key
    }
}

/// A masked frame; with `seq`, bytes 1 to 4 of the payload are replaced by it first.
fn masked_frame(fin: bool, opcode: u8, payload: &[u8], seq: Option<u32>, key: [u8; 4]) -> BytesMut {
    let mut out = BytesMut::with_capacity(payload.len() + 14);
    encode_header(&mut out, fin, opcode, payload.len(), Some(key));
    let start = out.len();
    out.put_slice(payload);
    if let Some(seq) = seq {
        out[start + 1..start + 5].copy_from_slice(&seq.to_le_bytes());
    }
    apply_mask(&mut out[start..], key);
    out
}

struct SendState {
    next_seq: u32,
    masks: MaskSource,
}

#[derive(Default)]
struct Counters {
    messages_in: AtomicU64,
    messages_out: AtomicU64,
    bytes_in: AtomicU64,
    auto_replies: AtomicU64,
}

struct Shared {
    send: Mutex<SendState>,
    close: OnceLock<CloseInfo>,
    counters: Counters,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, SendState> {
        // A panic while holding the lock leaves a consistent state (a counter and a key pool).
        self.send.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// An open WebSocket session (see the module documentation).
pub struct Session {
    tx: mpsc::UnboundedSender<Outgoing>,
    rx: mpsc::UnboundedReceiver<Incoming>,
    shared: Arc<Shared>,
    done: watch::Receiver<bool>,
    headers: Vec<(String, String)>,
    timings: ConnectTimings,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("close", &self.shared.close.get())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Connects to `endpoint` (TCP, TLS when configured) and upgrades to a WebSocket.
    pub async fn connect(endpoint: &Endpoint, opts: &SessionOptions) -> Result<Session> {
        let (stream, mut timings) = endpoint.connect().await?;
        let started = std::time::Instant::now();
        let mut session = Session::handshake(stream, &endpoint.host_header(), opts).await?;
        timings.upgrade = started.elapsed();
        session.timings = timings;
        Ok(session)
    }

    /// Upgrades an open stream to a WebSocket; `host` is the Host header value.
    pub async fn handshake<S>(mut stream: S, host: &str, opts: &SessionOptions) -> Result<Session>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let req = UpgradeRequest {
            host,
            path: &opts.path,
            subprotocol: &opts.subprotocol,
            extra_headers: &opts.extra_headers,
        };
        let (headers, leftover) =
            with_timeout(opts.handshake_timeout, "WebSocket upgrade", client_handshake(&mut stream, &req))
                .await?;
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            send: Mutex::new(SendState { next_seq: 1, masks: MaskSource::new() }),
            close: OnceLock::new(),
            counters: Counters::default(),
        });
        let io = IoTask {
            stream,
            rbuf: leftover,
            wbuf: BytesMut::with_capacity(4096),
            out_rx,
            in_tx,
            shared: shared.clone(),
            masks: MaskSource::new(),
            max_message: opts.max_message,
            close_timeout: opts.close_timeout,
            auto_reply: opts.auto_reply,
            partial: None,
            sent_close: None,
            read_at: std::time::Instant::now(),
        };
        tokio::spawn(io.run(done_tx));
        Ok(Session {
            tx: out_tx,
            rx: in_rx,
            shared,
            done: done_rx,
            headers,
            timings: ConnectTimings::default(),
        })
    }

    /// Queues a client message, numbering it: its bytes 1 to 4 are replaced by the next `seq`,
    /// which is returned. The message must have at least 5 bytes.
    pub fn send(&self, payload: &[u8]) -> Result<u32> {
        if payload.len() < 5 {
            return Err(ClientError::Unexpected("a numbered message needs a type byte and a seq".into()));
        }
        let mut st = self.shared.lock();
        let seq = st.next_seq;
        let key = st.masks.next_key();
        let frame = masked_frame(true, OP_BINARY, payload, Some(seq), key);
        self.tx.send(Outgoing::Frame(frame.freeze())).map_err(|_| self.closed_error())?;
        st.next_seq = seq.wrapping_add(1);
        self.shared.counters.messages_out.fetch_add(1, Ordering::Relaxed);
        Ok(seq)
    }

    /// Queues a binary message exactly as given, without numbering it (tests of the server's
    /// refusals: a wrong seq, a message too large, an undecodable one).
    pub fn send_unsequenced(&self, payload: &[u8]) -> Result<()> {
        self.send_frame(&Frame::new(OP_BINARY, Bytes::copy_from_slice(payload)), true)
    }

    /// Queues any frame: a fragment, a text frame, a ping... `masked: false` sends it unmasked,
    /// which a server must refuse (tests only).
    pub fn send_frame(&self, frame: &Frame, masked: bool) -> Result<()> {
        let bytes = {
            let mut st = self.shared.lock();
            if masked {
                masked_frame(frame.fin, frame.opcode, &frame.payload, None, st.masks.next_key())
            } else {
                let mut out = BytesMut::new();
                frame::encode_frame(&mut out, frame.fin, frame.opcode, &frame.payload, None);
                out
            }
        };
        self.tx.send(Outgoing::Frame(bytes.freeze())).map_err(|_| self.closed_error())?;
        self.shared.counters.messages_out.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The `seq` the next numbered message will take.
    pub fn next_seq(&self) -> u32 {
        self.shared.lock().next_seq
    }

    /// The next message. Once the connection has ended and every message was taken, returns
    /// [`ClientError::Closed`] with how it ended. Cancel-safe.
    pub async fn recv(&mut self) -> Result<Incoming> {
        match self.rx.recv().await {
            Some(msg) => Ok(msg),
            None => Err(self.closed_error()),
        }
    }

    /// The next message if one is waiting.
    pub fn try_recv(&mut self) -> Option<Incoming> {
        self.rx.try_recv().ok()
    }

    /// Starts the closing handshake with `code` after the messages already queued.
    pub fn close(&self, code: u16, reason: &str) {
        let _ = self.tx.send(Outgoing::Close { code, reason: reason.to_string() });
    }

    /// Waits until the connection has ended (messages not taken stay available) and returns how.
    pub async fn wait_closed(&mut self) -> CloseInfo {
        let _ = self.done.wait_for(|done| *done).await;
        self.close_info().unwrap_or_else(|| CloseInfo::abnormal("session task ended"))
    }

    /// How the connection ended, once it has.
    pub fn close_info(&self) -> Option<CloseInfo> {
        self.shared.close.get().cloned()
    }

    /// Whether the connection has ended.
    pub fn is_closed(&self) -> bool {
        self.shared.close.get().is_some()
    }

    /// The headers of the `101` answer.
    pub fn upgrade_headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// A header of the `101` answer (`Scacelith-Server-Id`...).
    pub fn upgrade_header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Time spent opening the session (zero steps for [`Session::handshake`]).
    pub fn timings(&self) -> ConnectTimings {
        self.timings
    }

    pub(crate) fn timings_mut(&mut self) -> &mut ConnectTimings {
        &mut self.timings
    }

    /// The counters.
    pub fn stats(&self) -> SessionStats {
        let c = &self.shared.counters;
        SessionStats {
            messages_in: c.messages_in.load(Ordering::Relaxed),
            messages_out: c.messages_out.load(Ordering::Relaxed),
            bytes_in: c.bytes_in.load(Ordering::Relaxed),
            auto_replies: c.auto_replies.load(Ordering::Relaxed),
        }
    }

    fn closed_error(&self) -> ClientError {
        ClientError::Closed(self.close_info().unwrap_or_else(|| CloseInfo::abnormal("session task ended")))
    }
}

/// The task that owns the socket.
struct IoTask<S> {
    stream: S,
    rbuf: BytesMut,
    wbuf: BytesMut,
    out_rx: mpsc::UnboundedReceiver<Outgoing>,
    in_tx: mpsc::UnboundedSender<Incoming>,
    shared: Arc<Shared>,
    masks: MaskSource,
    max_message: usize,
    close_timeout: Duration,
    auto_reply: Option<AutoReply>,
    /// Opcode and data of a fragmented message being received.
    partial: Option<(u8, BytesMut)>,
    /// The close frame the client sent, once it did.
    sent_close: Option<(u16, String)>,
    read_at: std::time::Instant,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> IoTask<S> {
    async fn run(mut self, done: watch::Sender<bool>) {
        let info = self.serve().await;
        let _ = self.shared.close.set(info);
        // Best effort: a TLS close_notify and a FIN.
        let _ = tokio::time::timeout(Duration::from_secs(1), self.stream.shutdown()).await;
        let _ = done.send(true);
        // Dropping `in_tx` (with `self`) ends the owner's `recv` once it took every message.
    }

    async fn serve(&mut self) -> CloseInfo {
        let mut close_deadline: Option<Instant> = None;
        loop {
            loop {
                match frame::parse_frame(&mut self.rbuf, Role::Client, self.max_message) {
                    Ok(Some(frame)) => {
                        if let Some(info) = self.on_frame(frame) {
                            let _ = self.flush().await;
                            return info;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let info = self.fail(e.code, e.reason);
                        let _ = self.flush().await;
                        return info;
                    }
                }
            }
            if let Err(e) = self.flush().await {
                return self.ended(format!("write failed: {e}"));
            }
            if self.sent_close.is_some() && close_deadline.is_none() {
                close_deadline = Some(Instant::now() + self.close_timeout);
            }
            tokio::select! {
                out = self.out_rx.recv(), if self.sent_close.is_none() => match out {
                    Some(out) => {
                        self.queue(out);
                        while self.sent_close.is_none() {
                            match self.out_rx.try_recv() {
                                Ok(out) => self.queue(out),
                                Err(_) => break,
                            }
                        }
                    }
                    // The owner dropped the session.
                    None => self.queue(Outgoing::Close { code: 1000, reason: String::new() }),
                },
                read = self.stream.read_buf(&mut self.rbuf) => match read {
                    Ok(0) => return self.ended("connection closed without a close frame".into()),
                    Ok(n) => {
                        self.read_at = std::time::Instant::now();
                        self.shared.counters.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Err(e) => return self.ended(format!("read failed: {e}")),
                },
                () = sleep_until_opt(close_deadline) => {
                    return self.ended("no close frame from the server in time".into());
                }
            }
        }
    }

    /// How the connection ended when the transport did: the client's close when it had sent
    /// one, an abnormal end otherwise.
    fn ended(&self, why: String) -> CloseInfo {
        match &self.sent_close {
            Some((code, reason)) => CloseInfo { code: *code, reason: reason.clone(), closer: Closer::Client },
            None => CloseInfo::abnormal(why),
        }
    }

    /// Writes what is queued.
    async fn flush(&mut self) -> std::io::Result<()> {
        if self.wbuf.is_empty() {
            return Ok(());
        }
        self.stream.write_all(&self.wbuf).await?;
        self.stream.flush().await?;
        self.wbuf.clear();
        Ok(())
    }

    fn queue(&mut self, out: Outgoing) {
        if self.sent_close.is_some() {
            return;
        }
        match out {
            Outgoing::Frame(bytes) => self.wbuf.extend_from_slice(&bytes),
            Outgoing::Close { code, reason } => self.queue_close(code, reason),
        }
    }

    fn queue_close(&mut self, code: u16, reason: String) {
        if self.sent_close.is_none() {
            let payload = frame::close_payload(code, &reason);
            let key = self.masks.next_key();
            self.wbuf.extend_from_slice(&masked_frame(true, OP_CLOSE, &payload, None, key));
            self.sent_close = Some((code, reason));
        }
    }

    /// A protocol error found by the client: close with `code`.
    fn fail(&mut self, code: u16, reason: &str) -> CloseInfo {
        self.queue_close(code, reason.to_string());
        CloseInfo { code, reason: reason.to_string(), closer: Closer::Client }
    }

    /// Handles one frame; `Some` ends the session.
    fn on_frame(&mut self, frame: Frame) -> Option<CloseInfo> {
        match frame.opcode {
            OP_PING => {
                if self.sent_close.is_none() {
                    let key = self.masks.next_key();
                    self.wbuf.extend_from_slice(&masked_frame(true, OP_PONG, &frame.payload, None, key));
                }
                None
            }
            OP_PONG => None,
            OP_CLOSE => {
                if let Some((code, reason)) = &self.sent_close {
                    return Some(CloseInfo { code: *code, reason: reason.clone(), closer: Closer::Client });
                }
                match frame::parse_close(&frame.payload) {
                    Ok((code, reason)) => {
                        // Echo the code (an empty close answers a close without one).
                        let payload = if code == CloseInfo::NO_STATUS {
                            Vec::new()
                        } else {
                            code.to_be_bytes().to_vec()
                        };
                        let key = self.masks.next_key();
                        self.wbuf.extend_from_slice(&masked_frame(true, OP_CLOSE, &payload, None, key));
                        Some(CloseInfo { code, reason, closer: Closer::Server })
                    }
                    Err(e) => Some(self.fail(e.code, e.reason)),
                }
            }
            OP_TEXT | OP_BINARY => {
                if self.partial.is_some() {
                    return Some(self.fail(1002, "new message inside a fragmented one"));
                }
                if frame.fin {
                    self.deliver(frame.opcode, frame.payload)
                } else {
                    self.partial = Some((frame.opcode, BytesMut::from(&frame.payload[..])));
                    None
                }
            }
            OP_CONTINUATION => {
                let Some((opcode, data)) = &mut self.partial else {
                    return Some(self.fail(1002, "continuation frame without a message"));
                };
                if data.len() + frame.payload.len() > self.max_message {
                    return Some(self.fail(1009, "message too big"));
                }
                data.extend_from_slice(&frame.payload);
                if !frame.fin {
                    return None;
                }
                let opcode = *opcode;
                let (_, data) = self.partial.take().expect("a fragmented message is in progress");
                self.deliver(opcode, data.freeze())
            }
            _ => Some(self.fail(1002, "reserved opcode")),
        }
    }

    /// A complete message: the automatic answer, then the owner.
    fn deliver(&mut self, opcode: u8, payload: Bytes) -> Option<CloseInfo> {
        if opcode == OP_TEXT {
            return Some(self.fail(1003, "text message"));
        }
        self.shared.counters.messages_in.fetch_add(1, Ordering::Relaxed);
        if let Some(reply) = self.auto_reply.and_then(|f| f(&payload))
            && reply.len() >= 5
        {
            self.queue_numbered(&reply);
        }
        let _ = self.in_tx.send(Incoming { payload, at: self.read_at });
        None
    }

    /// Queues an automatic answer with the next `seq`, after every message that took a lower one.
    fn queue_numbered(&mut self, payload: &[u8]) {
        let shared = self.shared.clone();
        let mut st = shared.lock();
        // A message numbered before this point is already in the channel: write it first.
        while let Ok(out) = self.out_rx.try_recv() {
            self.queue(out);
        }
        if self.sent_close.is_some() {
            return;
        }
        let seq = st.next_seq;
        st.next_seq = seq.wrapping_add(1);
        let key = self.masks.next_key();
        self.wbuf.extend_from_slice(&masked_frame(true, OP_BINARY, payload, Some(seq), key));
        shared.counters.auto_replies.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sleeps until `deadline`, forever without one.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
