//! HTTP/1.1 connections of the API port, on hyper (DESIGN 5.6 and 8; the Node listener's
//! hardening).
//!
//! Each connection runs hyper's HTTP/1 server over a [`GuardedIo`] that owns the timers and the
//! answers hyper cannot give the Node way:
//!
//! * a request head must arrive within 10 s (from the connection's start, or from the first byte
//!   of a request on a kept-alive connection), else the raw answer
//!   `HTTP/1.1 408 Request Timeout\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`;
//! * an idle kept-alive connection is closed quietly after 6 s (answers advertise
//!   `Keep-Alive: timeout=5, max=1000`);
//! * while an answer is being sent, 30 s without a byte in or out, or 60 s since the handler
//!   produced it, destroy the connection; a request whose handler is working has no timer (the
//!   handler's own timeouts answer it);
//! * hyper's own answers to unparsable requests are replaced by the raw `400` / `431` answers;
//!   requests llhttp would refuse but hyper accepts (unknown or lower-case methods, a head over
//!   8192 bytes counted as URL + header names + values, more than 64 header lines,
//!   `Content-Length` given twice or with `Transfer-Encoding`, a target with bytes outside ASCII)
//!   get the same raw answers. These
//!   client errors count in `scacelith_http_client_errors_total{reason}` and 1 toward a block of
//!   the peer (unless it is a trusted proxy);
//! * a connection the server closes after an answer keeps reading (and discarding) for 2 s, 1 s
//!   after a raw answer, so the client reads the answer before the socket goes.
//!
//! Then, for each request, in order: `CONNECT` closes the connection; an upgrade request goes to the
//! WebSocket endpoint when this listener carries upgrades; an HTTP/1.1 request without `Host` gets
//! `400` and an `Expect` other than `100-continue` gets `417` (both chunked and empty, as Node);
//! the protection per address takes a request token and an in-flight slot (`429 rate_limited`);
//! `GET`/`HEAD` of the health paths are answered here; everything else goes to [`Api::handle`] in
//! its own task (never cancelled by a disconnect; the in-flight slot is held until its answer is
//! sent). Every answer but the raw ones and the WebSocket refusals gets `Date`, then
//! `Connection: keep-alive` and `Keep-Alive`, or `Connection: close` (the 1000th request of a
//! connection, a request asking for it, a draining server).
//!
//! Known differences with Node's llhttp: header lines ending in a bare LF are accepted (httparse
//! is lenient there), and pipelined requests are answered one after the other (the
//! `Content-Length` lines of a request whose head came in with the previous request are not
//! counted).

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Version};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tokio::time::{Instant, Sleep};

use super::guard::{AddressKeys, InflightSlot, IpGuard, RequestRefusalReason};
use super::health::Readiness;
use super::ip::ClientAddress;
use super::upgrade::{HeadView, WsEndpoint};
use crate::http::Api;
use crate::http::answer::retry_after_secs;
use crate::http::api::{API_CSP, HSTS};
use crate::http::json::stringify;
use crate::metrics::{self, CounterVec};

/// Time to receive a request head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// An idle kept-alive connection is closed after this (Node: 5 s plus its 1 s buffer).
pub const KEEP_ALIVE_IDLE: Duration = Duration::from_secs(6);
/// The `Keep-Alive` header of kept-alive answers.
pub const KEEP_ALIVE_HEADER: &str = "timeout=5, max=1000";
/// No byte in or out for this long while an answer is being sent destroys the connection.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// An answer not sent this long after the handler produced it destroys the connection.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests per connection; the last one is answered with `Connection: close`.
pub const MAX_REQUESTS_PER_CONNECTION: u32 = 1000;
/// Largest request head, counted as Node does: URL + header names + header values.
pub const MAX_HEADER_SIZE: usize = 8192;
/// Header lines of a request head: Node's API sets `server.maxHeadersCount = 64`, and its
/// parser refuses a head with more (`431`). hyper itself refuses more than 100.
pub const MAX_HEADER_LINES: usize = 64;
/// hyper's read buffer: a raw head larger than this is refused 431 by hyper itself.
const MAX_BUF: usize = MAX_HEADER_SIZE + 1024;
/// A raw error answer and the connection behind it last this long at most.
const RAW_ANSWER_LINGER: Duration = Duration::from_secs(1);
/// A connection the server closes after an answer keeps reading for this long at most.
const CLOSE_DRAIN: Duration = Duration::from_secs(2);

/// The methods Node's parser (llhttp) knows; hyper accepts any token.
const METHODS: [&str; 34] = [
    "DELETE",
    "GET",
    "HEAD",
    "POST",
    "PUT",
    "CONNECT",
    "OPTIONS",
    "TRACE",
    "COPY",
    "LOCK",
    "MKCOL",
    "MOVE",
    "PROPFIND",
    "PROPPATCH",
    "SEARCH",
    "UNLOCK",
    "BIND",
    "REBIND",
    "UNBIND",
    "ACL",
    "REPORT",
    "MKACTIVITY",
    "CHECKOUT",
    "MERGE",
    "M-SEARCH",
    "NOTIFY",
    "SUBSCRIBE",
    "UNSUBSCRIBE",
    "PATCH",
    "PURGE",
    "MKCALENDAR",
    "LINK",
    "UNLINK",
    "SOURCE",
];

fn client_errors_metric() -> &'static CounterVec {
    static M: LazyLock<CounterVec> = LazyLock::new(|| {
        metrics::counter_vec(
            "scacelith_http_client_errors_total",
            "Malformed HTTP requests closed before the API handler, by reason",
            &["reason"],
        )
    });
    &M
}

/// The timers of the API connections (tests shorten them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTimeouts {
    /// Time to receive a request head ([`HEAD_TIMEOUT`]).
    pub head: Duration,
    /// Idle kept-alive connection ([`KEEP_ALIVE_IDLE`]).
    pub keep_alive: Duration,
    /// No progress while sending an answer ([`IDLE_TIMEOUT`]).
    pub idle: Duration,
    /// Time to send an answer ([`SEND_TIMEOUT`]).
    pub send: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> HttpTimeouts {
        HttpTimeouts {
            head: HEAD_TIMEOUT,
            keep_alive: KEEP_ALIVE_IDLE,
            idle: IDLE_TIMEOUT,
            send: SEND_TIMEOUT,
        }
    }
}

/// A request the listener refuses before the API, with a raw answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    /// Not parsable (400).
    Malformed,
    /// Head too large (431).
    TooLarge,
    /// Head not received in time (408).
    Timeout,
}

impl ClientError {
    const ALL: [ClientError; 3] = [ClientError::Malformed, ClientError::TooLarge, ClientError::Timeout];

    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientError::Malformed => "malformed",
            ClientError::TooLarge => "too_large",
            ClientError::Timeout => "timeout",
        }
    }

    /// The raw answer.
    pub fn raw_answer(self) -> &'static [u8] {
        match self {
            ClientError::Malformed => b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            ClientError::TooLarge => {
                b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
            ClientError::Timeout => b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            ClientError::Malformed => StatusCode::BAD_REQUEST,
            ClientError::TooLarge => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            ClientError::Timeout => StatusCode::REQUEST_TIMEOUT,
        }
    }

    fn index(self) -> usize {
        ClientError::ALL.iter().position(|e| *e == self).unwrap_or(0)
    }

    /// The error of hyper's own answer, from its status line.
    fn of_answer(head: &[u8]) -> ClientError {
        match head.get(9..12) {
            Some(b"431") | Some(b"414") => ClientError::TooLarge,
            Some(b"408") => ClientError::Timeout,
            _ => ClientError::Malformed,
        }
    }
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// An IMF-fixdate (`Sat, 03 Oct 2026 10:51:31 GMT`) of seconds since the epoch.
pub fn imf_fixdate(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = crate::log::civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    let month = MONTHS[(m as usize).clamp(1, 12) - 1];
    format!("{weekday}, {d:02} {month} {y} {:02}:{:02}:{:02} GMT", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The `Date` header value now, computed once per second.
pub fn http_date() -> HeaderValue {
    static CACHE: LazyLock<Mutex<(i64, HeaderValue)>> =
        LazyLock::new(|| Mutex::new((i64::MIN, HeaderValue::from_static(""))));
    let secs = crate::clock::wall_ms().div_euclid(1000);
    let mut c = CACHE.lock();
    if c.0 != secs {
        let text = imf_fixdate(secs);
        *c = (secs, HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("")));
    }
    c.1.clone()
}

/// Whether a comma-separated header holds `token` (case-insensitive), in any of its values.
fn has_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|v| {
        v.to_str().unwrap_or("").split(',').any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(token))
    })
}

/// The request target as the client sent it (origin-form) or as hyper rebuilt it.
fn target_of<B>(req: &Request<B>) -> String {
    match req.uri().path_and_query() {
        Some(pq) if req.uri().scheme().is_none() => pq.as_str().to_string(),
        _ => req.uri().to_string(),
    }
}

/// A body length llhttp refuses and hyper accepts: `Content-Length` given twice (even with the
/// same value), or together with `Transfer-Encoding`. hyper keeps one `Content-Length` of equal
/// ones and drops it next to `Transfer-Encoding`, so the lines are counted in `raw`, the bytes
/// read while the head came in. When `raw` holds no complete head (pipelined requests), the
/// request passes.
fn ambiguous_length(h: &HeaderMap, raw: &[u8]) -> bool {
    if !h.contains_key(header::CONTENT_LENGTH) && !h.contains_key(header::TRANSFER_ENCODING) {
        return false;
    }
    let (mut lengths, mut chunked) = (0, false);
    // The first line is the request line (or the end of one when the head started earlier).
    for line in raw.split_inclusive(|&b| b == b'\n').skip(1) {
        let Some(line) = line.strip_suffix(b"\n") else {
            return false;
        };
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return lengths > 1 || (lengths == 1 && chunked);
        }
        let name = line.split(|&b| b == b':').next().unwrap_or_default();
        if name.eq_ignore_ascii_case(b"content-length") {
            lengths += 1;
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            chunked = true;
        }
    }
    false
}

/// Node's head size: URL + header names + header values.
fn node_head_size<B>(req: &Request<B>) -> usize {
    let url = match req.uri().path_and_query() {
        Some(pq) if req.uri().scheme().is_none() => pq.as_str().len(),
        _ => req.uri().to_string().len(),
    };
    url + req.headers().iter().map(|(k, v)| k.as_str().len() + v.len()).sum::<usize>()
}

/// What a connection is doing (the timers follow it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for (the rest of) a request head.
    Head,
    /// The request is in the handler.
    Request,
    /// The answer is being sent.
    Responding,
    /// Idle between requests.
    KeepAlive,
    /// Handed to the WebSocket code.
    Upgraded,
    /// Ending: a raw answer, or the drain after the last answer.
    Closing,
}

#[derive(Debug)]
struct Track {
    phase: Phase,
    head_deadline: Instant,
    idle_deadline: Instant,
    send_deadline: Instant,
    response_started: Instant,
    closing_deadline: Instant,
    body_done: bool,
    close_after: Option<Duration>,
    answered: bool,
    requests: u32,
    /// The bytes read while waiting for a request head (at most `MAX_BUF`).
    head: Vec<u8>,
}

/// The state of one connection, shared by its [`GuardedIo`] and its service.
#[derive(Debug)]
struct ConnState {
    edge: Arc<HttpListener>,
    peer: IpAddr,
    track: Mutex<Track>,
}

impl ConnState {
    fn new(edge: Arc<HttpListener>, peer: IpAddr) -> ConnState {
        let now = Instant::now();
        let head = now + edge.timeouts.head;
        ConnState {
            edge,
            peer,
            track: Mutex::new(Track {
                phase: Phase::Head,
                head_deadline: head,
                idle_deadline: head,
                send_deadline: head,
                response_started: now,
                closing_deadline: head,
                body_done: false,
                close_after: None,
                answered: false,
                requests: 0,
                head: Vec::new(),
            }),
        }
    }

    /// A request head was parsed: its number on the connection and the bytes read for it.
    fn begin_request(&self) -> (u32, Vec<u8>) {
        let mut t = self.track.lock();
        if t.phase != Phase::Closing {
            t.phase = Phase::Request;
        }
        t.requests += 1;
        (t.requests, std::mem::take(&mut t.head))
    }

    /// The service produced an answer; `close_after`: the server closes the connection after it
    /// and keeps reading this long.
    fn begin_response(&self, close_after: Option<Duration>) {
        let now = Instant::now();
        let mut t = self.track.lock();
        if t.phase == Phase::Closing {
            return;
        }
        t.phase = Phase::Responding;
        t.response_started = now;
        t.send_deadline = now + self.edge.timeouts.send;
        t.body_done = false;
        t.close_after = close_after;
    }

    fn upgraded(&self) {
        self.track.lock().phase = Phase::Upgraded;
    }

    fn body_finished(&self) {
        self.track.lock().body_done = true;
    }

    /// Everything hyper had to write reached the stream; true when the answer is complete and
    /// the connection now waits for its next request.
    fn flushed(&self) -> bool {
        let mut t = self.track.lock();
        if t.phase == Phase::Responding && t.body_done {
            t.phase = Phase::KeepAlive;
            t.idle_deadline = Instant::now() + self.edge.timeouts.keep_alive;
            return true;
        }
        false
    }

    /// Bytes came in: a new request starts on an idle connection; the bytes of a head are kept
    /// for [`ambiguous_length`].
    fn bytes_in(&self, data: &[u8]) {
        let mut t = self.track.lock();
        if t.phase == Phase::KeepAlive {
            t.phase = Phase::Head;
            t.head_deadline = Instant::now() + self.edge.timeouts.head;
        }
        if t.phase == Phase::Head {
            let room = MAX_BUF.saturating_sub(t.head.len());
            t.head.extend_from_slice(&data[..data.len().min(room)]);
        }
    }

    /// Whether hyper writing now means its own answer to a parse error.
    fn writes_are_hypers(&self) -> bool {
        matches!(self.track.lock().phase, Phase::Head | Phase::KeepAlive)
    }

    /// Starts the end of the connection with a raw answer.
    fn begin_raw(&self) {
        let mut t = self.track.lock();
        t.phase = Phase::Closing;
        t.closing_deadline = Instant::now() + RAW_ANSWER_LINGER;
    }

    /// The stream is being shut down: whether to drain it (and until when).
    fn begin_drain(&self) -> bool {
        let mut t = self.track.lock();
        match t.phase {
            Phase::Closing => true,
            Phase::Upgraded => false,
            _ => match t.close_after {
                Some(d) => {
                    t.phase = Phase::Closing;
                    t.closing_deadline = Instant::now() + d;
                    true
                }
                None => false,
            },
        }
    }

    fn phase(&self) -> Phase {
        self.track.lock().phase
    }

    /// Counts a client error once per connection: the metric, and 1 toward a block of the peer
    /// unless it is a trusted proxy. False when one was already counted.
    fn client_error(&self, e: ClientError) -> bool {
        {
            let mut t = self.track.lock();
            if t.answered {
                return false;
            }
            t.answered = true;
        }
        self.edge.client_errors[e.index()].fetch_add(1, Ordering::Relaxed);
        client_errors_metric().with(&[e.as_str()]).inc();
        if let Some(g) = &self.edge.guard
            && !self.edge.client.is_trusted_peer(self.peer)
        {
            g.note_refusal(&g.keys(self.peer), 1.0);
        }
        true
    }

    fn answered(&self) -> bool {
        self.track.lock().answered
    }
}

/// What a timer says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expired {
    HeadTimeout,
    IdleClose,
    Destroy,
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connection timer expired")
}

/// The stream under hyper: timers, raw answers, drain on close (see the module docs).
struct GuardedIo<IO> {
    io: IO,
    st: Arc<ConnState>,
    timer: Pin<Box<Sleep>>,
    timer_at: Option<Instant>,
    last_progress: Instant,
    raw: Option<(&'static [u8], usize)>,
    swallow: bool,
    eof: bool,
    shut: bool,
    draining: bool,
}

impl<IO: AsyncRead + AsyncWrite + Unpin> GuardedIo<IO> {
    fn new(io: IO, st: Arc<ConnState>) -> GuardedIo<IO> {
        let now = Instant::now();
        GuardedIo {
            io,
            st,
            timer: Box::pin(tokio::time::sleep_until(now)),
            timer_at: None,
            last_progress: now,
            raw: None,
            swallow: false,
            eof: false,
            shut: false,
            draining: false,
        }
    }

    fn deadline(&self) -> Option<(Instant, Expired)> {
        let t = self.st.track.lock();
        match t.phase {
            Phase::Head => Some((t.head_deadline, Expired::HeadTimeout)),
            Phase::KeepAlive => Some((t.idle_deadline, Expired::IdleClose)),
            Phase::Responding => {
                let idle = self.last_progress.max(t.response_started) + self.st.edge.timeouts.idle;
                Some((t.send_deadline.min(idle), Expired::Destroy))
            }
            Phase::Closing => Some((t.closing_deadline, Expired::Destroy)),
            Phase::Request | Phase::Upgraded => None,
        }
    }

    /// Polls the timer of the current phase (registering the waker); `Some` when it expired.
    fn poll_deadline(&mut self, cx: &mut Context<'_>) -> Option<Expired> {
        let (at, what) = self.deadline()?;
        if self.timer_at != Some(at) {
            self.timer.as_mut().reset(at);
            self.timer_at = Some(at);
        }
        match self.timer.as_mut().poll(cx) {
            Poll::Ready(()) => Some(what),
            Poll::Pending => None,
        }
    }

    /// Writes the pending raw answer and flushes it.
    fn poll_raw(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some((bytes, pos)) = self.raw {
            if pos == bytes.len() {
                break;
            }
            match Pin::new(&mut self.io).poll_write(cx, &bytes[pos..]) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(n)) => self.raw = Some((bytes, pos + n)),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    return match self.poll_deadline(cx) {
                        Some(_) => Poll::Ready(Err(timed_out())),
                        None => Poll::Pending,
                    };
                }
            }
        }
        match Pin::new(&mut self.io).poll_flush(cx) {
            Poll::Ready(r) => {
                self.raw = None;
                Poll::Ready(r)
            }
            Poll::Pending => match self.poll_deadline(cx) {
                Some(_) => Poll::Ready(Err(timed_out())),
                None => Poll::Pending,
            },
        }
    }

    /// Answers a client error with its raw answer (once per connection).
    fn start_raw(&mut self, e: ClientError) {
        self.swallow = true;
        if self.st.client_error(e) {
            self.raw = Some((e.raw_answer(), 0));
        }
        self.st.begin_raw();
    }
}

impl<IO: AsyncRead + AsyncWrite + Unpin> AsyncRead for GuardedIo<IO> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.raw.is_some() {
            ready!(this.poll_raw(cx))?;
            this.eof = true;
        }
        if this.eof {
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.io).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    this.last_progress = Instant::now();
                    this.st.bytes_in(&buf.filled()[before..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match this.poll_deadline(cx) {
                None => Poll::Pending,
                Some(Expired::HeadTimeout) => {
                    this.start_raw(ClientError::Timeout);
                    if this.raw.is_none() {
                        this.eof = true;
                        return Poll::Ready(Ok(()));
                    }
                    Pin::new(this).poll_read(cx, buf)
                }
                Some(Expired::IdleClose) => {
                    this.eof = true;
                    Poll::Ready(Ok(()))
                }
                Some(Expired::Destroy) => Poll::Ready(Err(timed_out())),
            },
        }
    }
}

impl<IO: AsyncRead + AsyncWrite + Unpin> AsyncWrite for GuardedIo<IO> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.swallow && this.st.writes_are_hypers() {
            // hyper answers a request it could not parse: the raw answer instead.
            this.start_raw(ClientError::of_answer(data));
        }
        if this.swallow {
            if this.raw.is_some() {
                ready!(this.poll_raw(cx))?;
            }
            return Poll::Ready(Ok(data.len()));
        }
        match Pin::new(&mut this.io).poll_write(cx, data) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    this.last_progress = Instant::now();
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match this.poll_deadline(cx) {
                Some(Expired::Destroy) => Poll::Ready(Err(timed_out())),
                _ => Poll::Pending,
            },
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.raw.is_some() {
            return this.poll_raw(cx);
        }
        match Pin::new(&mut this.io).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                // hyper's pending read registered the timer of the response: arm the idle
                // timer in its place, or it would only be noticed at the next wake-up.
                if this.st.flushed() && this.poll_deadline(cx).is_some() {
                    cx.waker().wake_by_ref();
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match this.poll_deadline(cx) {
                Some(Expired::Destroy) => Poll::Ready(Err(timed_out())),
                _ => Poll::Pending,
            },
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.st.phase() == Phase::Upgraded {
            return Pin::new(&mut this.io).poll_shutdown(cx);
        }
        if this.raw.is_some() {
            ready!(this.poll_raw(cx))?;
        }
        if !this.shut {
            if let Err(e) = ready!(Pin::new(&mut this.io).poll_shutdown(cx)) {
                this.shut = true;
                return Poll::Ready(Err(e));
            }
            this.shut = true;
            this.draining = this.st.begin_drain();
        }
        while this.draining {
            let mut scratch = [0u8; 2048];
            let mut rb = ReadBuf::new(&mut scratch);
            match Pin::new(&mut this.io).poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) if !rb.filled().is_empty() => continue,
                Poll::Ready(_) => this.draining = false,
                Poll::Pending => {
                    if this.poll_deadline(cx).is_none() {
                        return Poll::Pending;
                    }
                    this.draining = false;
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

/// The body of every answer: its bytes (or an empty chunked body), the in-flight slot of the
/// request, and the end of the answer signalled to the connection's timers.
pub struct ResponseBody {
    data: Bytes,
    chunked: bool,
    done: bool,
    st: Option<Arc<ConnState>>,
    _slot: Option<InflightSlot>,
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseBody").field("len", &self.data.len()).field("chunked", &self.chunked).finish()
    }
}

impl ResponseBody {
    /// A body of known length.
    pub fn full(data: Bytes) -> ResponseBody {
        ResponseBody { data, chunked: false, done: false, st: None, _slot: None }
    }

    /// An empty body sent chunked (`Transfer-Encoding: chunked`, then `0\r\n\r\n`).
    pub fn chunked_empty() -> ResponseBody {
        ResponseBody { data: Bytes::new(), chunked: true, done: false, st: None, _slot: None }
    }

    fn tracked(mut self, st: &Arc<ConnState>, slot: Option<InflightSlot>) -> ResponseBody {
        self.st = Some(st.clone());
        self._slot = slot;
        self
    }

    fn finish(&mut self) {
        if !self.done {
            self.done = true;
            if let Some(st) = &self.st {
                st.body_finished();
            }
        }
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if !self.data.is_empty() {
            let data = std::mem::take(&mut self.data);
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        self.finish();
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        !self.chunked && self.data.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        if self.chunked { SizeHint::default() } else { SizeHint::with_exact(self.data.len() as u64) }
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The connection is closed without an answer (`CONNECT`).
#[derive(Debug, Clone, Copy)]
pub struct ClosedWithoutAnswer;

impl std::fmt::Display for ClosedWithoutAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection closed without an answer")
    }
}

impl std::error::Error for ClosedWithoutAnswer {}

/// The API port: what its connections need, shared by all of them.
pub struct HttpListener {
    api: Arc<Api>,
    ws: Option<Arc<WsEndpoint>>,
    guard: Option<Arc<IpGuard>>,
    client: ClientAddress,
    readiness: Readiness,
    hsts: bool,
    close_on_block: bool,
    timeouts: HttpTimeouts,
    draining: AtomicBool,
    client_errors: [AtomicU64; 3],
}

impl std::fmt::Debug for HttpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpListener")
            .field("upgrades", &self.ws.is_some())
            .field("guard", &self.guard.is_some())
            .field("draining", &self.draining.load(Ordering::Relaxed))
            .finish()
    }
}

impl HttpListener {
    /// The API port serving `api`; `/readyz` follows `readiness`.
    pub fn new(api: Arc<Api>, readiness: Readiness) -> HttpListener {
        HttpListener {
            api,
            ws: None,
            guard: None,
            client: ClientAddress::direct(),
            readiness,
            hsts: false,
            close_on_block: true,
            timeouts: HttpTimeouts::default(),
            draining: AtomicBool::new(false),
            client_errors: Default::default(),
        }
    }

    /// Carries the WebSocket upgrades (`WS_PORT == API_PORT`).
    pub fn upgrades(mut self, ws: Arc<WsEndpoint>) -> HttpListener {
        self.ws = Some(ws);
        self
    }

    /// Applies the protection per address to every request.
    pub fn guard(mut self, guard: Arc<IpGuard>) -> HttpListener {
        self.guard = Some(guard);
        self
    }

    /// How client addresses are found; behind a proxy a block never closes the connection.
    pub fn client_address(mut self, client: ClientAddress) -> HttpListener {
        self.close_on_block = !client.is_proxy();
        self.client = client;
        self
    }

    /// Adds `Strict-Transport-Security` to the listener's own answers (native TLS).
    pub fn hsts(mut self, on: bool) -> HttpListener {
        self.hsts = on;
        self
    }

    /// Other timers (tests).
    pub fn timeouts(mut self, timeouts: HttpTimeouts) -> HttpListener {
        self.timeouts = timeouts;
        self
    }

    /// The server is stopping: answers close their connection.
    pub fn set_draining(&self) {
        self.draining.store(true, Ordering::Release);
    }

    /// Client errors this listener answered.
    pub fn client_errors(&self, e: ClientError) -> u64 {
        self.client_errors[e.index()].load(Ordering::Relaxed)
    }

    /// Serves one connection until it ends. When `shutdown` turns true, the connection finishes
    /// its request in progress, if any, and closes.
    pub async fn serve<IO>(self: &Arc<Self>, io: IO, peer: IpAddr, mut shutdown: watch::Receiver<bool>)
    where
        IO: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let st = Arc::new(ConnState::new(self.clone(), peer));
        let io = TokioIo::new(GuardedIo::new(io, st.clone()));
        let service = ConnService { edge: self.clone(), st: st.clone() };
        let mut builder = hyper::server::conn::http1::Builder::new();
        builder
            .title_case_headers(true)
            .auto_date_header(false)
            .header_read_timeout(None)
            .max_buf_size(MAX_BUF)
            .keep_alive(true);
        let conn = builder.serve_connection(io, service).with_upgrades();
        let mut conn = std::pin::pin!(conn);
        let stopped = async {
            if shutdown.wait_for(|s| *s).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let early = tokio::select! {
            r = conn.as_mut() => Some(r),
            () = stopped => None,
        };
        let result = match early {
            Some(r) => r,
            None => {
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        };
        if let Err(e) = result
            && (e.is_parse() || e.is_parse_too_large())
            && !st.answered()
        {
            st.client_error(if e.is_parse_too_large() {
                ClientError::TooLarge
            } else {
                ClientError::Malformed
            });
        }
    }

    /// The listener's own JSON answers (health, per-address refusals): most of the API's
    /// security headers, no `Cross-Origin-Resource-Policy`, no charset.
    fn listener_json(&self, status: u16, body: &Value, extra: &[(HeaderName, String)]) -> Response<Bytes> {
        let text = stringify(body);
        let len = text.len();
        let mut res = Response::new(Bytes::from(text));
        *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(API_CSP));
        if self.hsts {
            h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static(HSTS));
        }
        for (k, v) in extra {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(k.clone(), v);
            }
        }
        res
    }

    fn rate_limited(&self, retry_after_ms: f64, close: bool) -> Response<Bytes> {
        let s = retry_after_secs(retry_after_ms);
        let mut extra = vec![(header::RETRY_AFTER, s.to_string())];
        if close {
            extra.push((header::CONNECTION, "close".to_string()));
        }
        let body = json!({"error": "rate_limited", "message": "Too many requests; try again later.", "retryAfter": s});
        self.listener_json(429, &body, &extra)
    }

    /// Adds `Date` and the connection headers, and tells the connection the answer starts.
    fn finish(
        &self,
        st: &Arc<ConnState>,
        res: Response<Bytes>,
        keep_alive: bool,
        head: bool,
        slot: Option<InflightSlot>,
    ) -> Response<ResponseBody> {
        self.decorate(st, res, keep_alive, head).map(|b| ResponseBody::full(b).tracked(st, slot))
    }

    /// [`HttpListener::finish`] without the body.
    fn decorate(
        &self,
        st: &Arc<ConnState>,
        mut res: Response<Bytes>,
        keep_alive: bool,
        head: bool,
    ) -> Response<Bytes> {
        if head {
            *res.body_mut() = Bytes::new();
        }
        let h = res.headers_mut();
        h.insert(header::DATE, http_date());
        let keep_alive = keep_alive && !self.draining.load(Ordering::Acquire);
        let close = match h.get(header::CONNECTION) {
            Some(v) => v.as_bytes().eq_ignore_ascii_case(b"close"),
            None if keep_alive => {
                h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
                h.insert(HeaderName::from_static("keep-alive"), HeaderValue::from_static(KEEP_ALIVE_HEADER));
                false
            }
            None => {
                h.insert(header::CONNECTION, HeaderValue::from_static("close"));
                true
            }
        };
        st.begin_response(close.then_some(CLOSE_DRAIN));
        res
    }

    /// A raw error answer given through hyper (the request parsed, but Node would refuse it).
    fn raw_error(&self, st: &Arc<ConnState>, e: ClientError) -> Response<ResponseBody> {
        st.client_error(e);
        st.begin_response(Some(RAW_ANSWER_LINGER));
        let mut res = Response::new(ResponseBody::full(Bytes::new()).tracked(st, None));
        *res.status_mut() = e.status();
        res.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
        res.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(0));
        res
    }

    /// An empty chunked answer, as Node's own `400` (no Host) and `417`.
    fn chunked_empty(
        &self,
        st: &Arc<ConnState>,
        status: StatusCode,
        keep_alive: bool,
        close: bool,
    ) -> Response<ResponseBody> {
        let mut res = Response::new(Bytes::new());
        *res.status_mut() = status;
        if close {
            res.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
        }
        self.decorate(st, res, keep_alive, false).map(|_| ResponseBody::chunked_empty().tracked(st, None))
    }

    /// An upgrade request on a listener that carries them.
    fn upgrade(
        &self,
        ws: &Arc<WsEndpoint>,
        mut req: Request<Incoming>,
        st: &Arc<ConnState>,
    ) -> Response<ResponseBody> {
        let view =
            HeadView::from_request(req.method().as_str(), req.version(), &target_of(&req), req.headers());
        match ws.check(&view, st.peer) {
            Err(refusal) => {
                st.begin_response(Some(RAW_ANSWER_LINGER));
                refusal.to_response().map(|b| ResponseBody::full(b).tracked(st, None))
            }
            Ok(accepted) => {
                let res = ws.switching_response(&accepted.accept);
                let on_upgrade = hyper::upgrade::on(&mut req);
                let ws = ws.clone();
                tokio::spawn(async move {
                    if let Ok(upgraded) = on_upgrade.await {
                        ws.open(Box::new(TokioIo::new(upgraded)), Bytes::new(), accepted);
                    }
                });
                st.upgraded();
                res.map(ResponseBody::full)
            }
        }
    }

    /// One request, in the listener's order (module docs).
    async fn respond(
        self: Arc<Self>,
        req: Request<Incoming>,
        st: Arc<ConnState>,
    ) -> Result<Response<ResponseBody>, ClosedWithoutAnswer> {
        let (n, raw_head) = st.begin_request();
        let method = req.method().clone();
        if !METHODS.contains(&method.as_str()) {
            return Ok(self.raw_error(&st, ClientError::Malformed));
        }
        if method == Method::CONNECT {
            return Err(ClosedWithoutAnswer);
        }
        if node_head_size(&req) >= MAX_HEADER_SIZE || req.headers().len() > MAX_HEADER_LINES {
            return Ok(self.raw_error(&st, ClientError::TooLarge));
        }
        if ambiguous_length(req.headers(), &raw_head) || !target_of(&req).is_ascii() {
            return Ok(self.raw_error(&st, ClientError::Malformed));
        }
        let is_upgrade = has_token(req.headers(), header::CONNECTION, "upgrade")
            && req.headers().contains_key(header::UPGRADE);
        if let Some(ws) = &self.ws
            && is_upgrade
        {
            return Ok(self.upgrade(ws, req, &st));
        }
        let h = req.headers();
        let keep_alive = n < MAX_REQUESTS_PER_CONNECTION
            && match req.version() {
                Version::HTTP_11 => !has_token(h, header::CONNECTION, "close"),
                Version::HTTP_10 => has_token(h, header::CONNECTION, "keep-alive"),
                _ => false,
            };
        let head = method == Method::HEAD;
        if req.version() == Version::HTTP_11 {
            if !h.contains_key(header::HOST) {
                return Ok(self.chunked_empty(&st, StatusCode::BAD_REQUEST, false, true));
            }
            if let Some(expect) = h.get(header::EXPECT)
                && !expects_continue(expect)
            {
                return Ok(self.chunked_empty(&st, StatusCode::EXPECTATION_FAILED, keep_alive, false));
            }
        }
        let xff = join_values(h, "x-forwarded-for");
        let ip = self.client.resolve(st.peer, xff.as_deref());
        let keys = self.guard.as_ref().map_or_else(|| AddressKeys::of(ip), |g| g.keys(ip));
        let mut slot = None;
        if let Some(g) = &self.guard {
            if let Err(r) = g.request(&keys) {
                let close = r.reason == RequestRefusalReason::Blocked && self.close_on_block;
                return Ok(self.finish(
                    &st,
                    self.rate_limited(r.retry_after_ms, close),
                    keep_alive,
                    head,
                    None,
                ));
            }
            match g.enter_slot(&keys) {
                Some(s) => slot = Some(s),
                None => {
                    return Ok(self.finish(&st, self.rate_limited(1000.0, false), keep_alive, head, None));
                }
            }
        }
        if method == Method::GET || head {
            let target = target_of(&req);
            let path = target.split_once('?').map_or(target.as_str(), |(p, _)| p);
            let health = match path {
                "/healthz" | "/api/v1/healthz" => Some((200, json!({"status": "ok"}))),
                "/readyz" | "/api/v1/readyz" if self.readiness.is_ready() => {
                    Some((200, json!({"status": "ready"})))
                }
                "/readyz" | "/api/v1/readyz" => Some((503, json!({"status": "not_ready"}))),
                _ => None,
            };
            if let Some((status, body)) = health {
                return Ok(self.finish(&st, self.listener_json(status, &body, &[]), keep_alive, head, slot));
            }
        }
        let api = self.api.clone();
        let handled = tokio::spawn(async move {
            let res = api.handle(req, keys).await;
            (res, slot)
        })
        .await;
        Ok(match handled {
            Ok((res, slot)) => self.finish(&st, res, keep_alive, head, slot),
            Err(_) => {
                let body = json!({"error": "internal_error", "message": "Internal server error."});
                self.finish(&st, self.listener_json(500, &body, &[]), keep_alive, head, None)
            }
        })
    }
}

/// `Expect: 100-continue` (Node: `/(?:^|\W)100-continue(?:$|\W)/i`).
fn expects_continue(v: &HeaderValue) -> bool {
    let s = v.to_str().unwrap_or("").to_ascii_lowercase();
    s.match_indices("100-continue").any(|(i, m)| {
        let before = s[..i].chars().next_back();
        let after = s[i + m.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        boundary(before) && boundary(after)
    })
}

/// Every value of a header joined with `, ` (Node's rule for most headers).
fn join_values(h: &HeaderMap, name: &str) -> Option<String> {
    let mut values = h.get_all(name).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let first = values.next()?;
    Some(values.fold(first, |mut acc, v| {
        acc.push_str(", ");
        acc.push_str(&v);
        acc
    }))
}

struct ConnService {
    edge: Arc<HttpListener>,
    st: Arc<ConnState>,
}

type ServiceFuture =
    Pin<Box<dyn Future<Output = Result<Response<ResponseBody>, ClosedWithoutAnswer>> + Send>>;

impl hyper::service::Service<Request<Incoming>> for ConnService {
    type Response = Response<ResponseBody>;
    type Error = ClosedWithoutAnswer;
    type Future = ServiceFuture;

    fn call(&self, req: Request<Incoming>) -> ServiceFuture {
        Box::pin(self.edge.clone().respond(req, self.st.clone()))
    }
}

#[cfg(test)]
#[path = "http1_tests.rs"]
mod tests;
