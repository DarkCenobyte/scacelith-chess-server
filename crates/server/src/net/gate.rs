//! The TLS gate (DESIGN 5.7): admission of new TCP connections on the native TLS listeners,
//! before any TLS work.
//!
//! For each accepted socket, in order:
//! 1. stage 0, the protection per address ([`IpGuard::connection`]): a blocked address, too many
//!    new connections per second or too many open ones;
//! 2. the waiting room: at most `16 * MAX_PENDING_HANDSHAKES` sockets in total and
//!    `4 * MAX_PENDING_HANDSHAKES_PER_IP` per address group (IPv4 address, IPv6 /48) wait for the
//!    first record of their ClientHello, 3 s at most, without a handshake slot;
//! 3. the record must be a TLS handshake record (type 22, version 3.x, 1 to 2^14 bytes) starting
//!    a ClientHello (only the first record is awaited: a fragmented ClientHello is fine);
//! 4. a handshake slot: `MAX_PENDING_HANDSHAKES` in total, `MAX_PENDING_HANDSHAKES_PER_IP` per
//!    group, and while the server is full the listener carrying the upgrades lets through only
//!    `max(1, ceil(MAX_PENDING_HANDSHAKES / 2))` new connections per second;
//! 5. the TLS handshake, 10 s at most, reads the bytes the gate read first ([`Prefixed`]); its
//!    slot comes back when it completes or fails.
//!
//! A refusal closes the socket with an RST (zero linger: nothing sent, no TIME_WAIT). A client that
//! ends its side while waiting is closed quietly. The refusals that say something about one
//! address (`per_ip`, `waiting_per_ip`, `bad_hello`, `hello_timeout`, a failed handshake) count 1
//! toward a block of it; the server-wide ones (`handshakes`, `waiting`, `server_full`) do not.
//! Limits are those of the whole process.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use super::guard::{AddressKeys, ConnRefusal, IpGuard, OpenConnection};
use super::ip::AddrKey;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::metrics::{self, CounterVec};

/// Time a new connection has to send the first record of its ClientHello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
/// Time a TLS handshake may take once admitted.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest body of a TLS record (RFC 8446 section 5.1).
const MAX_RECORD_BODY: usize = 16384;
/// Bytes read from a waiting socket at once.
const READ_CHUNK: usize = 4096;

/// Tells whether the server is full (the realtime module's `MAX_CONNECTIONS` signal).
pub type FullSignal = Arc<dyn Fn() -> bool + Send + Sync>;

fn refused_metric() -> &'static CounterVec {
    static M: LazyLock<CounterVec> = LazyLock::new(|| {
        metrics::counter_vec(
            "scacelith_tls_refused_total",
            "New connections closed before the TLS handshake, by reason",
            &["reason"],
        )
    });
    &M
}

/// Default handshake slots per address group: `max(1, min(n - 1, max(2, floor(n / 32))))`.
pub fn default_pending_per_group(n: u32) -> u32 {
    n.saturating_sub(1).min((n / 32).max(2)).max(1)
}

/// The caps of the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct GateLimits {
    /// Handshakes in progress (`MAX_PENDING_HANDSHAKES`).
    pub max_pending: u32,
    /// Handshakes in progress per address group (`MAX_PENDING_HANDSHAKES_PER_IP`).
    pub max_pending_per_ip: u32,
    /// Sockets waiting for their ClientHello (`16 * max_pending`).
    pub max_waiting: u32,
    /// Of them per address group (`4 * max_pending_per_ip`).
    pub max_waiting_per_ip: u32,
    /// Time to send the first record of the ClientHello.
    pub hello_timeout: Duration,
    /// Time of the handshake once admitted.
    pub handshake_timeout: Duration,
    /// New connections let through per second while the server is full (burst: one second).
    pub full_rate: f64,
}

impl GateLimits {
    /// The caps derived from `max_pending` and `max_pending_per_ip` (0: the default share).
    pub fn new(max_pending: i64, max_pending_per_ip: i64) -> GateLimits {
        let max_pending = u32::try_from(max_pending.max(1)).unwrap_or(u32::MAX);
        let max_pending_per_ip = match u32::try_from(max_pending_per_ip) {
            Ok(n) if n >= 1 => n,
            _ => default_pending_per_group(max_pending),
        };
        GateLimits {
            max_pending,
            max_pending_per_ip,
            max_waiting: max_pending.saturating_mul(16),
            max_waiting_per_ip: max_pending_per_ip.saturating_mul(4),
            hello_timeout: HELLO_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            full_rate: f64::from(max_pending.div_ceil(2).max(1)),
        }
    }

    /// The caps of `config`.
    pub fn from_config(config: &Config) -> GateLimits {
        GateLimits::new(config.max_pending_handshakes, config.max_pending_handshakes_per_ip)
    }
}

/// Why the gate closed a new connection (the label of `scacelith_tls_refused_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateRefusal {
    /// Every handshake slot is taken.
    Handshakes,
    /// The address group holds its handshake slots.
    PerIp,
    /// The server is full and this second's connections went through.
    ServerFull,
    /// The waiting room is full.
    Waiting,
    /// The address group fills its part of the waiting room.
    WaitingPerIp,
    /// No ClientHello in time.
    HelloTimeout,
    /// The first record is not the start of a ClientHello.
    BadHello,
    /// The address is blocked.
    Blocked,
    /// Too many new connections per second from the address.
    ConnRate,
    /// Too many open connections from the address.
    ConnOpen,
}

impl GateRefusal {
    const ALL: [GateRefusal; 10] = [
        GateRefusal::Handshakes,
        GateRefusal::PerIp,
        GateRefusal::ServerFull,
        GateRefusal::Waiting,
        GateRefusal::WaitingPerIp,
        GateRefusal::HelloTimeout,
        GateRefusal::BadHello,
        GateRefusal::Blocked,
        GateRefusal::ConnRate,
        GateRefusal::ConnOpen,
    ];

    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            GateRefusal::Handshakes => "handshakes",
            GateRefusal::PerIp => "per_ip",
            GateRefusal::ServerFull => "server_full",
            GateRefusal::Waiting => "waiting",
            GateRefusal::WaitingPerIp => "waiting_per_ip",
            GateRefusal::HelloTimeout => "hello_timeout",
            GateRefusal::BadHello => "bad_hello",
            GateRefusal::Blocked => "blocked",
            GateRefusal::ConnRate => "conn_rate",
            GateRefusal::ConnOpen => "conn_open",
        }
    }

    /// Whether the gate counts it toward a block (stage 0 refusals are counted by the guard).
    fn per_address(self) -> bool {
        matches!(
            self,
            GateRefusal::PerIp
                | GateRefusal::WaitingPerIp
                | GateRefusal::HelloTimeout
                | GateRefusal::BadHello
        )
    }

    fn index(self) -> usize {
        GateRefusal::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }
}

impl From<ConnRefusal> for GateRefusal {
    fn from(r: ConnRefusal) -> GateRefusal {
        match r {
            ConnRefusal::Blocked => GateRefusal::Blocked,
            ConnRefusal::ConnRate => GateRefusal::ConnRate,
            ConnRefusal::ConnOpen => GateRefusal::ConnOpen,
        }
    }
}

/// What the first bytes of a connection say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloCheck {
    /// The first record is not complete yet.
    NeedMore,
    /// The first record is complete and starts a ClientHello.
    Complete,
    /// Not a TLS handshake record starting a ClientHello.
    Bad,
}

/// Checks the bytes received so far: a 5-byte record header (handshake, version 3.x, a body of 1
/// to 2^14 bytes), then the whole record, whose first byte must be HandshakeType 1 (ClientHello).
pub fn check_hello(b: &[u8]) -> HelloCheck {
    if b.len() < 5 {
        return HelloCheck::NeedMore;
    }
    let body = usize::from(u16::from_be_bytes([b[3], b[4]]));
    if b[0] != 22 || b[1] != 3 || body == 0 || body > MAX_RECORD_BODY {
        return HelloCheck::Bad;
    }
    if b.len() < 5 + body {
        HelloCheck::NeedMore
    } else if b[5] == 1 {
        HelloCheck::Complete
    } else {
        HelloCheck::Bad
    }
}

#[derive(Debug)]
struct GateState {
    pending: u32,
    per_ip: HashMap<AddrKey, u32>,
    waiting: u32,
    waiting_per_ip: HashMap<AddrKey, u32>,
    tokens: f64,
    token_at: f64,
}

fn decrement(map: &mut HashMap<AddrKey, u32>, key: AddrKey) {
    if let Some(n) = map.get_mut(&key) {
        if *n <= 1 {
            map.remove(&key);
        } else {
            *n -= 1;
        }
    }
}

/// The gate of the native TLS listeners (one for the process, shared by the API and WebSocket
/// listeners).
pub struct TlsGate {
    limits: GateLimits,
    state: Mutex<GateState>,
    guard: Option<Arc<IpGuard>>,
    full: Option<FullSignal>,
    clock: SharedClock,
    refused: [AtomicU64; 10],
}

impl std::fmt::Debug for TlsGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.state.lock();
        f.debug_struct("TlsGate").field("pending", &st.pending).field("waiting", &st.waiting).finish()
    }
}

/// A place in the waiting room; given back when dropped.
#[derive(Debug)]
pub struct WaitingSlot {
    gate: Arc<TlsGate>,
    group: AddrKey,
}

impl Drop for WaitingSlot {
    fn drop(&mut self) {
        let mut st = self.gate.state.lock();
        st.waiting -= 1;
        decrement(&mut st.waiting_per_ip, self.group);
    }
}

/// A handshake slot; given back when dropped.
#[derive(Debug)]
pub struct HandshakeSlot {
    gate: Arc<TlsGate>,
    group: AddrKey,
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let mut st = self.gate.state.lock();
        st.pending -= 1;
        decrement(&mut st.per_ip, self.group);
    }
}

/// A connection the gate admitted: its first bytes, its handshake slot and its place among the
/// open connections of its address.
#[derive(Debug)]
pub struct Gated {
    /// The socket, replaying the bytes the gate read.
    pub stream: Prefixed<TcpStream>,
    /// The handshake slot.
    pub slot: HandshakeSlot,
    /// The open-connection count of the address (held for the connection's life).
    pub open: OpenConnection,
    /// The keys of the peer address.
    pub keys: AddressKeys,
}

/// A TLS connection through the gate and its handshake.
#[derive(Debug)]
pub struct Secured {
    /// The TLS stream.
    pub stream: TlsStream<Prefixed<TcpStream>>,
    /// The open-connection count of the address (held for the connection's life).
    pub open: OpenConnection,
    /// The keys of the peer address.
    pub keys: AddressKeys,
}

/// Closes a socket with an RST: zero linger, nothing sent, no TIME_WAIT.
pub fn reset(tcp: TcpStream) {
    let _ = socket2::SockRef::from(&tcp).set_linger(Some(Duration::ZERO));
    drop(tcp);
}

impl TlsGate {
    /// A gate with `limits`; `clock` measures the shedding bucket (monotonic).
    pub fn new(limits: GateLimits, clock: SharedClock) -> TlsGate {
        TlsGate {
            state: Mutex::new(GateState {
                pending: 0,
                per_ip: HashMap::new(),
                waiting: 0,
                waiting_per_ip: HashMap::new(),
                tokens: limits.full_rate,
                token_at: f64::NEG_INFINITY,
            }),
            limits,
            guard: None,
            full: None,
            clock,
            refused: Default::default(),
        }
    }

    /// Puts the protection per address in front (stage 0) and counts refusals with it.
    pub fn with_guard(mut self, guard: Arc<IpGuard>) -> TlsGate {
        self.guard = Some(guard);
        self
    }

    /// Sheds new connections of the upgrade listener while `full` says the server is full.
    pub fn with_full_signal(mut self, full: FullSignal) -> TlsGate {
        self.full = Some(full);
        self
    }

    /// The caps.
    pub fn limits(&self) -> &GateLimits {
        &self.limits
    }

    /// Handshakes in progress.
    pub fn pending(&self) -> u32 {
        self.state.lock().pending
    }

    /// Sockets waiting for their ClientHello.
    pub fn waiting(&self) -> u32 {
        self.state.lock().waiting
    }

    /// Address groups holding a handshake slot or a waiting place (tests: nothing leaks).
    pub fn groups(&self) -> (usize, usize) {
        let st = self.state.lock();
        (st.per_ip.len(), st.waiting_per_ip.len())
    }

    /// Connections this gate refused for `reason`.
    pub fn refused(&self, reason: GateRefusal) -> u64 {
        self.refused[reason.index()].load(Ordering::Relaxed)
    }

    /// Registers the gauges `scacelith_tls_handshakes_pending` and `scacelith_tls_hello_waiting`
    /// (call it once, for the server's gate).
    pub fn register_gauges(self: &Arc<Self>) {
        let w = Arc::downgrade(self);
        metrics::gauge_fn("scacelith_tls_handshakes_pending", "TLS handshakes in progress", move || {
            w.upgrade().map_or(0.0, |g| f64::from(g.pending()))
        });
        let w = Arc::downgrade(self);
        metrics::gauge_fn(
            "scacelith_tls_hello_waiting",
            "New TLS connections waiting for their ClientHello (no handshake slot yet)",
            move || w.upgrade().map_or(0.0, |g| f64::from(g.waiting())),
        );
    }

    fn keys(&self, peer: IpAddr) -> AddressKeys {
        self.guard.as_ref().map_or_else(|| AddressKeys::of(peer), |g| g.keys(peer))
    }

    fn group(k: &AddressKeys) -> AddrKey {
        k.k48.unwrap_or(k.k64)
    }

    /// Counts a refusal (and toward a block of the address when it concerns it).
    fn refuse(&self, reason: GateRefusal, keys: &AddressKeys) -> GateRefusal {
        self.refused[reason.index()].fetch_add(1, Ordering::Relaxed);
        refused_metric().with(&[reason.as_str()]).inc();
        if reason.per_address()
            && let Some(g) = &self.guard
        {
            g.note_refusal(keys, 1.0);
        }
        reason
    }

    /// Stage 0: the protection per address (exempt addresses pass, uncounted).
    pub fn connection(self: &Arc<Self>, peer: IpAddr) -> Result<(OpenConnection, AddressKeys), GateRefusal> {
        let keys = self.keys(peer);
        match &self.guard {
            None => Ok((OpenConnection::detached(peer), keys)),
            Some(g) => match g.connection(peer) {
                Ok(open) => Ok((open, keys)),
                Err(r) => Err(self.refuse(r.into(), &keys)),
            },
        }
    }

    /// A place in the waiting room.
    pub fn enter_waiting(self: &Arc<Self>, keys: &AddressKeys) -> Result<WaitingSlot, GateRefusal> {
        let group = TlsGate::group(keys);
        let mut st = self.state.lock();
        if st.waiting >= self.limits.max_waiting {
            drop(st);
            return Err(self.refuse(GateRefusal::Waiting, keys));
        }
        let n = st.waiting_per_ip.get(&group).copied().unwrap_or(0);
        if n >= self.limits.max_waiting_per_ip {
            drop(st);
            return Err(self.refuse(GateRefusal::WaitingPerIp, keys));
        }
        st.waiting_per_ip.insert(group, n + 1);
        st.waiting += 1;
        Ok(WaitingSlot { gate: self.clone(), group })
    }

    /// A handshake slot; `shed`: this listener carries the upgrades (server-full shedding).
    pub fn admit(self: &Arc<Self>, keys: &AddressKeys, shed: bool) -> Result<HandshakeSlot, GateRefusal> {
        let group = TlsGate::group(keys);
        let mut st = self.state.lock();
        if st.pending >= self.limits.max_pending {
            drop(st);
            return Err(self.refuse(GateRefusal::Handshakes, keys));
        }
        let n = st.per_ip.get(&group).copied().unwrap_or(0);
        if n >= self.limits.max_pending_per_ip {
            drop(st);
            return Err(self.refuse(GateRefusal::PerIp, keys));
        }
        if shed && self.full.as_ref().is_some_and(|full| full()) && !self.take_full_token(&mut st) {
            drop(st);
            return Err(self.refuse(GateRefusal::ServerFull, keys));
        }
        st.per_ip.insert(group, n + 1);
        st.pending += 1;
        Ok(HandshakeSlot { gate: self.clone(), group })
    }

    /// Token bucket of the connections let through while full. The clock is monotonic, and one
    /// that goes back anyway adds nothing rather than a debt.
    fn take_full_token(&self, st: &mut GateState) -> bool {
        let now = self.clock.mono_ms();
        let rate = self.limits.full_rate;
        let mut t = st.tokens + (now - st.token_at).max(0.0) * rate / 1000.0;
        if t.is_nan() || t > rate {
            t = rate;
        }
        st.token_at = now;
        if t < 1.0 {
            st.tokens = t;
            return false;
        }
        st.tokens = t - 1.0;
        true
    }

    /// Runs a new TCP connection through the gate: stage 0, the wait for the first record of its
    /// ClientHello, a handshake slot. Refused sockets are reset; `None` then.
    pub async fn accept(self: &Arc<Self>, mut tcp: TcpStream, peer: IpAddr, shed: bool) -> Option<Gated> {
        let (open, keys) = match self.connection(peer) {
            Ok(v) => v,
            Err(_) => {
                reset(tcp);
                return None;
            }
        };
        let waiting = match self.enter_waiting(&keys) {
            Ok(w) => w,
            Err(_) => {
                reset(tcp);
                return None;
            }
        };
        let deadline = tokio::time::Instant::now() + self.limits.hello_timeout;
        let mut first: Vec<u8> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            match tokio::time::timeout_at(deadline, tcp.read(&mut chunk)).await {
                Err(_) => {
                    drop(waiting);
                    self.refuse(GateRefusal::HelloTimeout, &keys);
                    reset(tcp);
                    return None;
                }
                // The client ended its side (quiet close) or reset the connection.
                Ok(Ok(0)) | Ok(Err(_)) => return None,
                Ok(Ok(n)) => first.extend_from_slice(&chunk[..n]),
            }
            match check_hello(&first) {
                HelloCheck::NeedMore => continue,
                HelloCheck::Bad => {
                    drop(waiting);
                    self.refuse(GateRefusal::BadHello, &keys);
                    reset(tcp);
                    return None;
                }
                HelloCheck::Complete => break,
            }
        }
        drop(waiting);
        match self.admit(&keys, shed) {
            Ok(slot) => Some(Gated { stream: Prefixed::new(Bytes::from(first), tcp), slot, open, keys }),
            Err(_) => {
                reset(tcp);
                None
            }
        }
    }

    /// The TLS handshake of an admitted connection, within the handshake timeout. Its slot comes
    /// back either way; a failure counts 1 toward a block of the address.
    pub async fn handshake(self: &Arc<Self>, gated: Gated, acceptor: &TlsAcceptor) -> Option<Secured> {
        let Gated { stream, slot, open, keys } = gated;
        let result = tokio::time::timeout(self.limits.handshake_timeout, acceptor.accept(stream)).await;
        drop(slot);
        match result {
            Ok(Ok(stream)) => Some(Secured { stream, open, keys }),
            Ok(Err(_)) | Err(_) => {
                if let Some(g) = &self.guard {
                    g.note_refusal(&keys, 1.0);
                }
                None
            }
        }
    }
}

/// A stream that first replays bytes already read from it.
#[derive(Debug)]
pub struct Prefixed<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> Prefixed<S> {
    /// `inner`, whose next reads return `prefix` first.
    pub fn new(prefix: Bytes, inner: S) -> Prefixed<S> {
        Prefixed { prefix, inner }
    }

    /// The wrapped stream.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, ManualClock};
    use crate::config::Config;
    use crate::log::Logger;
    use crate::net::abuse::BlockOrder;
    use crate::net::tls::tests::{TempDir, context, test_cert, test_client};
    use std::sync::atomic::AtomicBool;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("an address")
    }

    fn gate_with(limits: GateLimits) -> (Arc<TlsGate>, Arc<ManualClock>) {
        let clock = ManualClock::new(10_000.0, 0);
        (Arc::new(TlsGate::new(limits, clock.clone() as Arc<dyn Clock>)), clock)
    }

    fn keys(s: &str) -> AddressKeys {
        AddressKeys::of(ip(s))
    }

    /// A TLS record holding a handshake message of `size` bytes.
    fn hello_record(size: usize, rec_type: u8, hs_type: u8, rec_len: usize) -> Vec<u8> {
        let mut b = vec![0xab; (5 + rec_len).max(9)];
        b[0] = rec_type;
        b[1] = 3;
        b[2] = 1;
        b[3..5].copy_from_slice(&(rec_len as u16).to_be_bytes());
        b[5] = hs_type;
        b[6..9].copy_from_slice(&(size as u32).to_be_bytes()[1..]);
        b.truncate(5 + rec_len);
        b
    }

    fn hello(size: usize) -> Vec<u8> {
        hello_record(size, 22, 1, size + 4)
    }

    #[test]
    fn admits_up_to_the_cap_and_slots_come_back_once() {
        let (gate, _) = gate_with(GateLimits::new(3, 0));
        let s: Vec<HandshakeSlot> =
            (1..=3).map(|i| gate.admit(&keys(&format!("192.0.2.{i}")), false).expect("slot")).collect();
        assert_eq!(gate.pending(), 3);
        assert_eq!(gate.admit(&keys("192.0.2.4"), false).unwrap_err(), GateRefusal::Handshakes);
        assert_eq!(gate.refused(GateRefusal::Handshakes), 1);
        drop(s);
        assert_eq!((gate.pending(), gate.groups()), (0, (0, 0)));
    }

    #[test]
    fn limits_the_handshakes_of_one_group() {
        let (gate, _) = gate_with(GateLimits::new(100, 2));
        let a = gate.admit(&keys("2001:db8:1:2::1"), false).expect("a");
        let _b = gate.admit(&keys("2001:db8:1:3:ffff::9"), false).expect("b");
        assert_eq!(
            gate.admit(&keys("2001:db8:1:ffff:aaaa::5"), false).unwrap_err(),
            GateRefusal::PerIp,
            "same /48"
        );
        let _c = gate.admit(&keys("2001:db8:2::1"), false).expect("another /48");
        let _d = gate.admit(&keys("::ffff:198.51.100.7"), false).expect("mapped");
        let _e = gate.admit(&keys("198.51.100.7"), false).expect("same address");
        assert_eq!(
            gate.admit(&keys("198.51.100.7"), false).unwrap_err(),
            GateRefusal::PerIp,
            "mapped is the same"
        );
        assert_eq!(gate.refused(GateRefusal::PerIp), 2);
        drop(a);
        assert!(gate.admit(&keys("2001:db8:1:77::77"), false).is_ok());
    }

    #[test]
    fn default_caps() {
        let l = GateLimits::new(128, 0);
        assert_eq!(
            (l.max_pending_per_ip, l.max_waiting, l.max_waiting_per_ip, l.full_rate),
            (4, 2048, 16, 64.0)
        );
        let shares: Vec<u32> = [2, 3, 32, 64, 100, 1000, 100_000].map(default_pending_per_group).to_vec();
        assert_eq!(shares, [1, 2, 2, 2, 3, 31, 3125]);
        for n in [2, 3, 5, 64, 128, 4096] {
            assert!(default_pending_per_group(n) < n);
        }
        let (gate, _) = gate_with(GateLimits::new(128, 0));
        let mut held = Vec::new();
        for i in 0..8 {
            for _ in 0..16 {
                if let Ok(s) = gate.admit(&keys(&format!("127.0.1.{}", i + 1)), false) {
                    held.push(s);
                }
            }
        }
        assert_eq!(held.len(), 32, "32 groups are needed to hold every slot");
        let c = Config::for_tests();
        assert_eq!(GateLimits::from_config(&c).max_pending, c.max_pending_handshakes as u32);
    }

    #[test]
    fn sheds_only_the_upgrade_listener_while_full_at_a_limited_rate() {
        let full = Arc::new(AtomicBool::new(true));
        let mut l = GateLimits::new(100, 50);
        l.full_rate = 2.0;
        let clock = ManualClock::new(10_000.0, 0);
        let f = full.clone();
        let gate = Arc::new(
            TlsGate::new(l, clock.clone() as Arc<dyn Clock>)
                .with_full_signal(Arc::new(move || f.load(Ordering::Relaxed))),
        );
        let k = keys("192.0.2.1");
        let mut held = vec![gate.admit(&k, true).expect("1"), gate.admit(&k, true).expect("2")];
        assert_eq!(gate.admit(&k, true).unwrap_err(), GateRefusal::ServerFull);
        held.push(gate.admit(&k, false).expect("the API-only listener is never shed"));
        clock.advance(500.0);
        held.push(gate.admit(&k, true).expect("one more token"));
        assert_eq!(gate.admit(&k, true).unwrap_err(), GateRefusal::ServerFull);
        full.store(false, Ordering::Relaxed);
        for i in 0..10 {
            held.push(gate.admit(&keys(&format!("203.0.113.{i}")), true).expect("not full"));
        }
        assert_eq!(gate.refused(GateRefusal::ServerFull), 2);
    }

    #[test]
    fn a_clock_going_back_leaves_no_token_debt() {
        let mut l = GateLimits::new(100, 50);
        l.full_rate = 10.0;
        let clock = ManualClock::new(1e6, 0);
        let gate =
            Arc::new(TlsGate::new(l, clock.clone() as Arc<dyn Clock>).with_full_signal(Arc::new(|| true)));
        let k = keys("192.0.2.1");
        let mut held = Vec::new();
        for _ in 0..10 {
            held.push(gate.admit(&k, true).expect("burst"));
        }
        assert!(gate.admit(&k, true).is_err());
        clock.advance(-60_000.0);
        assert!(gate.admit(&k, true).is_err());
        clock.advance(100.0);
        assert!(gate.admit(&k, true).is_ok(), "one token, not minus 599");
    }

    #[test]
    fn the_first_record_must_start_a_client_hello() {
        assert_eq!(check_hello(&hello(700)[..4]), HelloCheck::NeedMore);
        assert_eq!(check_hello(&hello(700)[..704]), HelloCheck::NeedMore);
        assert_eq!(check_hello(&hello(700)), HelloCheck::Complete);
        for size in [300, 9000, 16380] {
            let rec = hello(size);
            for i in 1..rec.len() {
                assert_ne!(check_hello(&rec[..i]), HelloCheck::Complete);
            }
            assert_eq!(check_hello(&rec), HelloCheck::Complete);
        }
        let bad: [Vec<u8>; 6] = [
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
            hello_record(300, 23, 1, 304),
            hello(16381),
            hello_record(0, 22, 1, 0),
            hello_record(300, 22, 2, 304),
            vec![0x80, 0x2e, 0x01, 0x03, 0x01],
        ];
        for b in bad {
            assert_eq!(check_hello(&b), HelloCheck::Bad, "{:?}", &b[..5]);
        }
        // A ClientHello fragmented over records: the first record (even 1 byte) is enough.
        let msg = &hello(1000)[5..];
        let mut rec = vec![22, 3, 1, 0, 50];
        rec.extend_from_slice(&msg[..50]);
        assert_eq!(check_hello(&rec[..54]), HelloCheck::NeedMore);
        assert_eq!(check_hello(&rec), HelloCheck::Complete);
        assert_eq!(check_hello(&[22, 3, 1, 0, 1, 1]), HelloCheck::Complete);
    }

    #[test]
    fn bounds_the_waiting_sockets_in_total_and_per_group() {
        let mut l = GateLimits::new(8, 4);
        l.max_waiting = 3;
        l.max_waiting_per_ip = 2;
        let (gate, _) = gate_with(l);
        let a = gate.enter_waiting(&keys("2001:db8:5:1::1")).expect("a");
        let _b = gate.enter_waiting(&keys("2001:db8:5:2::1")).expect("b");
        assert_eq!(gate.enter_waiting(&keys("2001:db8:5:3::1")).unwrap_err(), GateRefusal::WaitingPerIp);
        let _c = gate.enter_waiting(&keys("192.0.2.9")).expect("c");
        assert_eq!(gate.enter_waiting(&keys("192.0.2.10")).unwrap_err(), GateRefusal::Waiting);
        assert_eq!(gate.waiting(), 3);
        drop(a);
        assert!(gate.enter_waiting(&keys("2001:db8:5:4::1")).is_ok());
        assert_eq!(gate.refused(GateRefusal::WaitingPerIp) + gate.refused(GateRefusal::Waiting), 2);
    }

    fn guarded(
        env: impl FnOnce(&mut Config),
        limits: GateLimits,
    ) -> (Arc<TlsGate>, Arc<IpGuard>, Arc<ManualClock>) {
        let mut c = Config::for_tests();
        env(&mut c);
        let clock = ManualClock::new(1e6, 0);
        let guard =
            Arc::new(IpGuard::new(&c, clock.clone() as Arc<dyn Clock>, Logger::root()).expect("a guard"));
        let gate = Arc::new(TlsGate::new(limits, clock.clone() as Arc<dyn Clock>).with_guard(guard.clone()));
        (gate, guard, clock)
    }

    #[test]
    fn stage_0_blocks_and_connection_limits() {
        let (gate, guard, clock) = guarded(
            |c| {
                c.ip_conn_rate = 1;
                c.ip_max_connections = 2;
            },
            GateLimits::new(8, 4),
        );
        let a = gate.connection(ip("192.0.2.1")).expect("a");
        let _b = gate.connection(ip("192.0.2.1")).expect("b");
        assert_eq!(gate.connection(ip("192.0.2.1")).unwrap_err(), GateRefusal::ConnOpen);
        assert_eq!(guard.open_total(), 2);
        drop(a);
        assert_eq!(guard.open_total(), 1, "a closed connection gives its place back");
        let _c = gate.connection(ip("192.0.2.1")).expect("the 4th token");
        assert_eq!(gate.connection(ip("192.0.2.1")).unwrap_err(), GateRefusal::ConnRate);
        clock.advance(1000.0);
        drop(_c);
        assert!(gate.connection(ip("192.0.2.1")).is_ok());
        guard.apply_blocks(&[
            BlockOrder::new(AddrKey::of(ip("192.0.2.66")), 60_000.0, 1),
            BlockOrder::new(AddrKey::site_of(ip("2001:db8:5::1")), 60_000.0, 1),
        ]);
        guard.flush_reports();
        assert_eq!(gate.connection(ip("192.0.2.66")).unwrap_err(), GateRefusal::Blocked);
        assert_eq!(gate.connection(ip("2001:db8:5:1::1")).unwrap_err(), GateRefusal::Blocked, "a /48 block");
        assert_eq!(gate.refused(GateRefusal::Blocked), 2);
        assert!(guard.flush_reports().is_empty(), "refusals of a blocked address are not counted again");
    }

    #[test]
    fn per_address_refusals_count_toward_a_block_server_wide_ones_do_not() {
        let mut l = GateLimits::new(2, 1);
        l.max_waiting_per_ip = 2;
        let (gate, guard, _) = guarded(|_| {}, l);
        let _p1 = gate.admit(&keys("198.51.100.1"), false).expect("p1");
        assert_eq!(gate.admit(&keys("198.51.100.1"), false).unwrap_err(), GateRefusal::PerIp);
        let _q1 = gate.admit(&keys("198.51.100.2"), false).expect("q1");
        assert_eq!(gate.admit(&keys("198.51.100.3"), false).unwrap_err(), GateRefusal::Handshakes);
        let k4 = keys("198.51.100.4");
        let _w = [gate.enter_waiting(&k4).expect("w1"), gate.enter_waiting(&k4).expect("w2")];
        assert_eq!(gate.enter_waiting(&k4).unwrap_err(), GateRefusal::WaitingPerIp);
        gate.refuse(GateRefusal::BadHello, &k4);
        gate.refuse(GateRefusal::HelloTimeout, &k4);
        let mut entries: Vec<(AddrKey, f64)> =
            guard.flush_reports().into_iter().map(|e| (e.k64, e.weight)).collect();
        entries.sort_by_key(|e| e.0);
        assert_eq!(entries, [(AddrKey::of(ip("198.51.100.1")), 1.0), (AddrKey::of(ip("198.51.100.4")), 3.0)]);
    }

    // ---- Real sockets ----

    async fn listener() -> (TcpListener, u16) {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = l.local_addr().expect("addr").port();
        (l, port)
    }

    /// Accepts connections through `gate` and the TLS handshake; answers "pong" to each secured one.
    fn serve(
        gate: Arc<TlsGate>,
        l: TcpListener,
        acceptor: TlsAcceptor,
        shed: bool,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = l.accept().await else { return };
                let gate = gate.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Some(gated) = gate.accept(tcp, peer.ip(), shed).await else { return };
                    let Some(mut s) = gate.handshake(gated, &acceptor).await else { return };
                    let mut buf = [0u8; 16];
                    if let Ok(n) = s.stream.read(&mut buf).await
                        && n > 0
                    {
                        let _ = s.stream.write_all(b"pong").await;
                        let _ = s.stream.flush().await;
                    }
                    let mut rest = [0u8; 16];
                    while matches!(s.stream.read(&mut rest).await, Ok(n) if n > 0) {}
                });
            }
        })
    }

    async fn tls_ping(port: u16) -> io::Result<String> {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
        let connector = tokio_rustls::TlsConnector::from(test_client());
        let name = rustls::pki_types::ServerName::try_from("gate.test").expect("a name");
        let mut s = connector.connect(name, tcp).await?;
        s.write_all(b"ping").await?;
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    async fn until(mut f: impl FnMut() -> bool) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not reached");
    }

    /// Whether the server reset the connection (an RST, not a FIN).
    async fn was_reset(mut s: TcpStream) -> bool {
        let mut buf = [0u8; 64];
        loop {
            match tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf)).await {
                Ok(Err(e)) => return e.kind() == io::ErrorKind::ConnectionReset,
                Ok(Ok(0)) | Err(_) => return false,
                Ok(Ok(_)) => continue,
            }
        }
    }

    #[tokio::test]
    async fn silent_connections_hold_no_slot_and_are_reset_at_the_deadline() {
        let dir = TempDir::new("gate-silent");
        let ctx = context(&dir, &test_cert("gate.test"));
        let mut l = GateLimits::new(2, 2);
        l.max_waiting_per_ip = 50;
        l.hello_timeout = Duration::from_millis(300);
        let (gate, _) = gate_with(l);
        let (lst, port) = listener().await;
        let task = serve(gate.clone(), lst, ctx.acceptor().clone(), false);
        let mut idle = Vec::new();
        for _ in 0..10 {
            idle.push(TcpStream::connect(("127.0.0.1", port)).await.expect("connect"));
        }
        until(|| gate.waiting() == 10).await;
        assert_eq!(gate.pending(), 0);
        assert_eq!(tls_ping(port).await.expect("a TLS client gets through"), "pong");
        for s in idle {
            assert!(was_reset(s).await, "reset at the deadline");
        }
        assert_eq!(gate.refused(GateRefusal::HelloTimeout), 10);
        until(|| gate.waiting() == 0 && gate.pending() == 0).await;
        task.abort();
    }

    #[tokio::test]
    async fn refusals_reset_and_slots_come_back() {
        let dir = TempDir::new("gate-slots");
        let ctx = context(&dir, &test_cert("gate.test"));
        let mut l = GateLimits::new(2, 2);
        l.handshake_timeout = Duration::from_millis(400);
        let (gate, _) = gate_with(l);
        let (lst, port) = listener().await;
        let task = serve(gate.clone(), lst, ctx.acceptor().clone(), false);
        // Two clients that sent a ClientHello and stopped hold both slots.
        let hello = capture_client_hello().await;
        let mut stalled1 = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        stalled1.write_all(&hello).await.expect("write");
        let mut stalled2 = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        stalled2.write_all(&hello).await.expect("write");
        until(|| gate.pending() == 2).await;
        assert!(tls_ping(port).await.is_err(), "no slot left");
        assert_eq!(gate.refused(GateRefusal::Handshakes), 1);
        drop(stalled1);
        until(|| gate.pending() == 1).await;
        for _ in 0..4 {
            assert_eq!(tls_ping(port).await.expect("completed handshakes give their slot back"), "pong");
        }
        // Plain HTTP on the TLS port never reaches TLS.
        let mut http = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
        assert!(was_reset(http).await);
        assert_eq!(gate.refused(GateRefusal::BadHello), 1);
        // The stalled handshake times out and is closed by the server.
        let mut buf = vec![0u8; 8192];
        let mut answered = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(3), stalled2.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => answered += n,
                Err(_) => panic!("the server did not close the stalled handshake"),
            }
        }
        assert!(answered > 0, "the server did its part of the handshake");
        until(|| gate.pending() == 0 && gate.groups() == (0, 0)).await;
        task.abort();
    }

    #[tokio::test]
    async fn a_client_ending_while_waiting_is_closed_quietly() {
        let mut l = GateLimits::new(8, 4);
        l.hello_timeout = Duration::from_secs(5);
        let (gate, _) = gate_with(l);
        let (lst, port) = listener().await;
        let dir = TempDir::new("gate-fin");
        let ctx = context(&dir, &test_cert("gate.test"));
        let task = serve(gate.clone(), lst, ctx.acceptor().clone(), false);
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        s.write_all(&hello(300)[..10]).await.expect("write");
        s.shutdown().await.expect("FIN");
        assert!(!was_reset(s).await, "a FIN, not an RST");
        until(|| gate.waiting() == 0).await;
        assert_eq!(GateRefusal::ALL.iter().map(|r| gate.refused(*r)).sum::<u64>(), 0, "not counted");
        task.abort();
    }

    #[tokio::test]
    async fn a_blocked_address_is_reset_before_a_single_byte() {
        let dir = TempDir::new("gate-block");
        let ctx = context(&dir, &test_cert("gate.test"));
        let (gate, guard, _) = guarded(|_| {}, GateLimits::new(8, 4));
        guard.apply_blocks(&[BlockOrder::new(AddrKey::of(ip("127.0.0.1")), 60_000.0, 1)]);
        let (lst, port) = listener().await;
        let task = serve(gate.clone(), lst, ctx.acceptor().clone(), false);
        let s = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        assert!(was_reset(s).await);
        assert!(tls_ping(port).await.is_err());
        assert_eq!(gate.refused(GateRefusal::Blocked), 2);
        task.abort();
    }

    #[tokio::test]
    async fn hands_tls_the_bytes_after_the_first_record() {
        let dir = TempDir::new("gate-extra");
        let ctx = context(&dir, &test_cert("gate.test"));
        let mut l = GateLimits::new(4, 4);
        l.handshake_timeout = Duration::from_secs(5);
        let (gate, _) = gate_with(l);
        let (lst, port) = listener().await;
        let task = serve(gate.clone(), lst, ctx.acceptor().clone(), false);
        let hello = capture_client_hello().await;
        let mut stalled = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        stalled.write_all(&hello).await.expect("write");
        let mut extra = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let mut both = hello.clone();
        both.extend_from_slice(&[23, 3, 3, 0, 4, 1, 2, 3, 4]);
        extra.write_all(&both).await.expect("write");
        // TLS refuses the application data record at once; the ClientHello alone waits.
        let mut buf = vec![0u8; 8192];
        loop {
            match tokio::time::timeout(Duration::from_secs(3), extra.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(_)) => continue,
                Err(_) => panic!("the bad record was not seen"),
            }
        }
        until(|| gate.pending() == 1).await;
        drop(stalled);
        task.abort();
    }

    #[tokio::test]
    async fn a_client_hello_in_pieces_completes() {
        let dir = TempDir::new("gate-pieces");
        let ctx = context(&dir, &test_cert("gate.test"));
        let (gate, _) = gate_with(GateLimits::new(4, 4));
        let (lst, port) = listener().await;
        let task = serve(gate.clone(), lst, ctx.acceptor().clone(), false);
        for piece in [100usize, 7, 1] {
            // A real client whose first flight is written `piece` bytes at a time.
            let (client_io, relay) = tokio::io::duplex(1 << 16);
            let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            let relay_task = tokio::spawn(relay_in_pieces(relay, tcp, piece));
            let connector = tokio_rustls::TlsConnector::from(test_client());
            let name = rustls::pki_types::ServerName::try_from("gate.test").expect("a name");
            let mut s = connector.connect(name, client_io).await.expect("handshake");
            s.write_all(b"ping").await.expect("write");
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.expect("read");
            assert_eq!(&buf, b"pong");
            drop(s);
            relay_task.abort();
        }
        assert_eq!(gate.pending(), 0);
        task.abort();
    }

    /// Relays between a client and the server, writing the client's first flight `piece` bytes
    /// at a time.
    async fn relay_in_pieces(client: tokio::io::DuplexStream, tcp: TcpStream, piece: usize) {
        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut tr, mut tw) = tcp.into_split();
        let up = async move {
            let mut buf = vec![0u8; 16384];
            let mut first = true;
            while let Ok(n) = cr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if first {
                    first = false;
                    for part in buf[..n].chunks(piece) {
                        if tw.write_all(part).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                } else if tw.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        };
        let down = async move {
            let _ = tokio::io::copy(&mut tr, &mut cw).await;
        };
        tokio::join!(up, down);
    }

    /// The ClientHello of a real TLS client, captured without any server.
    async fn capture_client_hello() -> Vec<u8> {
        let (client_io, mut server_io) = tokio::io::duplex(1 << 16);
        let connector = tokio_rustls::TlsConnector::from(test_client());
        let name = rustls::pki_types::ServerName::try_from("gate.test").expect("a name");
        let task = tokio::spawn(async move { connector.connect(name, client_io).await.map(|_| ()) });
        let mut buf = vec![0u8; 16384];
        let n = server_io.read(&mut buf).await.expect("the ClientHello");
        task.abort();
        buf.truncate(n);
        assert_eq!(check_hello(&buf), HelloCheck::Complete);
        buf
    }
}
