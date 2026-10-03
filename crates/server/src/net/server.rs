//! The network server: binds the ports of the configuration and serves them until the shutdown.
//!
//! Topology (DESIGN 5.7):
//!
//! * `TLS_MODE=native`: every new TCP connection goes through the [`TlsGate`] (per-address limits,
//!   ClientHello waiting room, handshake slots), then the rustls handshake. With
//!   `WS_PORT == API_PORT` one port carries the API and the WebSocket upgrade on `/ws`, and the gate
//!   sheds new connections there while the realtime layer is full; with distinct ports the API
//!   port is never shed and the dedicated WebSocket port is. The certificate is watched (10 s
//!   poll) and reloaded on [`ServerHandle::reload_certificates`] (SIGHUP).
//! * `TLS_MODE=proxy` / `off`: plain TCP, no gate; in proxy mode the client address comes from
//!   `X-Forwarded-For` when the peer is a trusted proxy.
//! * `METRICS_PORT` (unless 0): the plain-HTTP metrics and health endpoint on `METRICS_BIND`.
//!
//! [`ServerHandle::shutdown`] stops accepting: `/readyz` turns 503, new upgrades get
//! `503 shutting_down`, every answer from then on carries `Connection: close`, the listening
//! sockets close, idle kept-alive connections close at once and the requests in progress finish
//! (at most `SHUTDOWN_GRACE_MS`, then their connections are dropped). Upgraded WebSockets belong to
//! the realtime layer and are left alone. [`Server::run`] returns once that drain is over; the
//! metrics endpoint answers until then.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::gate::{FullSignal, GateLimits, TlsGate};
use super::guard::{IpGuard, OpenConnection};
use super::health::Readiness;
use super::http1::HttpListener;
use super::ip::{ClientAddress, IpListError};
use super::listener::{self, Acceptor, ListenError};
use super::metrics_server::{self, MetricsEndpoint};
use super::tls::{CertError, TlsContext};
use super::upgrade::WsEndpoint;
use crate::config::{Config, TlsMode};
use crate::http::Api;
use crate::log::Logger;
use crate::log_warn;

/// What the application gives the server.
pub struct ServerParts {
    /// The HTTP API: routes, authentication hook, rates.
    pub api: Arc<Api>,
    /// The WebSocket endpoint with its admission hook and connection handler; the server adds the
    /// protection per address and the client address of the configuration.
    pub ws: WsEndpoint,
    /// The protection per address, shared with the API.
    pub guard: Arc<IpGuard>,
    /// Readiness for `/readyz`: the application sets it once started; the shutdown clears it.
    pub readiness: Readiness,
    /// True while the realtime layer is full: the gate then sheds new connections on the port that
    /// carries the WebSocket upgrade (`None`: never full).
    pub full: Option<FullSignal>,
}

impl fmt::Debug for ServerParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerParts").field("ws", &self.ws).finish_non_exhaustive()
    }
}

/// What a listening socket carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    /// The API only (`WS_PORT != API_PORT`).
    Api,
    /// The API and the WebSocket upgrade (`WS_PORT == API_PORT`).
    ApiWs,
    /// The dedicated WebSocket port.
    Ws,
    /// The metrics and health endpoint.
    Metrics,
}

impl ListenerKind {
    /// The name of the Node listeners (`api`, `api+ws`, `ws`, `metrics`).
    pub fn as_str(self) -> &'static str {
        match self {
            ListenerKind::Api => "api",
            ListenerKind::ApiWs => "api+ws",
            ListenerKind::Ws => "ws",
            ListenerKind::Metrics => "metrics",
        }
    }

    /// Whether the gate sheds this listener's new connections while the server is full.
    fn sheds(self) -> bool {
        matches!(self, ListenerKind::ApiWs | ListenerKind::Ws)
    }
}

/// Why the server could not start.
#[derive(Debug)]
pub enum ServerError {
    /// A port could not be bound.
    Listen(ListenError),
    /// The certificate or the key could not be loaded (`TLS_MODE=native`).
    Certificate(CertError),
    /// `TRUSTED_PROXIES` holds an invalid entry.
    TrustedProxies(IpListError),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Listen(e) => e.fmt(f),
            ServerError::Certificate(e) => write!(f, "TLS certificate: {e}"),
            ServerError::TrustedProxies(e) => write!(f, "TRUSTED_PROXIES: {e}"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServerError::Listen(e) => Some(e),
            ServerError::Certificate(e) => Some(e),
            ServerError::TrustedProxies(e) => Some(e),
        }
    }
}

impl From<ListenError> for ServerError {
    fn from(e: ListenError) -> ServerError {
        ServerError::Listen(e)
    }
}

/// The state the handle and the connections share.
struct Shared {
    stop: watch::Sender<bool>,
    http: Arc<HttpListener>,
    ws: Arc<WsEndpoint>,
    readiness: Readiness,
    tls: Option<Arc<TlsContext>>,
    gate: Option<Arc<TlsGate>>,
}

/// Controls a running server from elsewhere (signal handlers, the application's shutdown).
#[derive(Clone)]
pub struct ServerHandle {
    shared: Arc<Shared>,
}

impl fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerHandle").field("shutting_down", &self.is_shutting_down()).finish()
    }
}

impl ServerHandle {
    /// Starts the graceful shutdown (see the module docs). Idempotent.
    pub fn shutdown(&self) {
        let s = &self.shared;
        s.readiness.set(false);
        s.ws.set_accepting(false);
        s.http.set_draining();
        s.stop.send_replace(true);
    }

    /// Whether the shutdown started.
    pub fn is_shutting_down(&self) -> bool {
        *self.shared.stop.borrow()
    }

    /// Reloads the certificate and the key from their files (logging `SIGHUP: reloading
    /// certificates`, then the outcome). False when the reload failed (the current certificate is
    /// kept) or the server does not terminate TLS.
    pub async fn reload_certificates(&self) -> bool {
        match &self.shared.tls {
            Some(tls) => tls.reload_on_sighup().await,
            None => false,
        }
    }

    /// Reloads the certificate on every SIGHUP the process receives, until the server is dropped.
    /// `None` when the server does not terminate TLS.
    pub fn spawn_sighup_reload(&self) -> io::Result<Option<JoinHandle<()>>> {
        let Some(tls) = &self.shared.tls else { return Ok(None) };
        let weak: Weak<TlsContext> = Arc::downgrade(tls);
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        Ok(Some(tokio::spawn(async move {
            while hup.recv().await.is_some() {
                let Some(tls) = weak.upgrade() else { return };
                tls.reload_on_sighup().await;
            }
        })))
    }
}

/// One bound port.
struct Port {
    kind: ListenerKind,
    acceptor: Acceptor,
}

/// The bound ports of the configuration, ready to serve.
pub struct Server {
    shared: Arc<Shared>,
    ports: Vec<Port>,
    metrics: Option<(Arc<MetricsEndpoint>, TcpListener)>,
    guard: Arc<IpGuard>,
    grace: Duration,
    log: Logger,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server").field("addresses", &self.addresses()).finish_non_exhaustive()
    }
}

impl Server {
    /// Loads the certificate (`TLS_MODE=native`) and binds every port of `config`: the API port,
    /// the WebSocket port when it differs, the metrics port unless it is 0. The ports accept
    /// connections into their backlog from here on; [`Server::run`] serves them.
    pub async fn bind(config: &Config, parts: ServerParts, log: Logger) -> Result<Server, ServerError> {
        let native = config.tls_mode == TlsMode::Native;
        let client = ClientAddress::from_config(config).map_err(ServerError::TrustedProxies)?;
        let tls = if native {
            let (cfg, tls_log) = (config.clone(), log.child("tls"));
            let loaded = tokio::task::spawn_blocking(move || TlsContext::new(&cfg, tls_log)).await;
            Some(loaded.expect("certificate loading does not panic").map_err(ServerError::Certificate)?)
        } else {
            None
        };
        let shared_port = config.ws_port == config.api_port;
        let ServerParts { api, ws, guard, readiness, full } = parts;
        let ws = Arc::new(ws.guard(guard.clone()).client_address(client.clone()));
        let mut http = HttpListener::new(api, readiness.clone())
            .guard(guard.clone())
            .client_address(client)
            .hsts(native);
        if shared_port {
            http = http.upgrades(ws.clone());
        }
        let gate = native.then(|| {
            let mut gate = TlsGate::new(GateLimits::from_config(config), crate::clock::system())
                .with_guard(guard.clone());
            if let Some(full) = full {
                gate = gate.with_full_signal(full);
            }
            Arc::new(gate)
        });

        let ip = listener::bind_ip(config).map_err(|e| {
            ListenError::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.api_port), e)
        })?;
        let backlog = listener::backlog_of(config);
        let plan: &[(ListenerKind, u16)] = if shared_port {
            &[(ListenerKind::ApiWs, config.api_port)]
        } else {
            &[(ListenerKind::Api, config.api_port), (ListenerKind::Ws, config.ws_port)]
        };
        let mut ports = Vec::with_capacity(plan.len());
        for &(kind, port) in plan {
            let acceptor = Acceptor::new(listener::bind(ip, port, backlog)?, log.child(kind.as_str()));
            ports.push(Port { kind, acceptor });
        }
        let metrics = metrics_server::bind(config)?.map(|l| {
            let endpoint = MetricsEndpoint::new(config, readiness.clone(), log.child("metrics"));
            (Arc::new(endpoint), l)
        });

        let (stop, _) = watch::channel(false);
        let shared = Arc::new(Shared { stop, http: Arc::new(http), ws, readiness, tls, gate });
        let grace = Duration::from_millis(u64::try_from(config.shutdown_grace_ms).unwrap_or(0));
        Ok(Server { shared, ports, metrics, guard, grace, log })
    }

    /// The bound addresses (the actual ports when the configuration asked for port 0).
    pub fn addresses(&self) -> Vec<(ListenerKind, SocketAddr)> {
        let mut out: Vec<(ListenerKind, SocketAddr)> =
            self.ports.iter().filter_map(|p| Some((p.kind, p.acceptor.local_addr().ok()?))).collect();
        if let Some((_, l)) = &self.metrics
            && let Ok(a) = l.local_addr()
        {
            out.push((ListenerKind::Metrics, a));
        }
        out
    }

    /// The address of the listener of `kind`, if bound.
    pub fn address(&self, kind: ListenerKind) -> Option<SocketAddr> {
        self.addresses().into_iter().find(|(k, _)| *k == kind).map(|(_, a)| a)
    }

    /// A handle to shut the server down or reload its certificate.
    pub fn handle(&self) -> ServerHandle {
        ServerHandle { shared: self.shared.clone() }
    }

    /// The TLS context (`TLS_MODE=native`).
    pub fn tls(&self) -> Option<&Arc<TlsContext>> {
        self.shared.tls.as_ref()
    }

    /// The gate in front of the TLS handshakes (`TLS_MODE=native`).
    pub fn gate(&self) -> Option<&Arc<TlsGate>> {
        self.shared.gate.as_ref()
    }

    /// The HTTP side of the API port.
    pub fn http(&self) -> &Arc<HttpListener> {
        &self.shared.http
    }

    /// The WebSocket endpoint.
    pub fn ws(&self) -> &Arc<WsEndpoint> {
        &self.shared.ws
    }

    /// Serves every port until [`ServerHandle::shutdown`], then drains (see the module docs).
    pub async fn run(self) {
        let Server { shared, ports, metrics, guard, grace, log } = self;
        guard.register_gauges();
        let reporter = guard.spawn_reporter();
        if let Some(gate) = &shared.gate {
            gate.register_gauges();
        }
        let watcher = shared.tls.as_ref().map(|tls| tls.spawn_watcher());
        let (metrics_stop, metrics_rx) = watch::channel(false);
        let metrics_task = metrics.map(|(endpoint, l)| tokio::spawn(endpoint.serve(l, metrics_rx)));

        let loops: Vec<JoinHandle<JoinSet<()>>> =
            ports.into_iter().map(|port| tokio::spawn(accept_loop(shared.clone(), port))).collect();
        let mut sets = Vec::with_capacity(loops.len());
        for l in loops {
            if let Ok(set) = l.await {
                sets.push(set);
            }
        }
        let drain = async {
            for set in &mut sets {
                while set.join_next().await.is_some() {}
            }
        };
        if tokio::time::timeout(grace, drain).await.is_err() {
            let left: usize = sets.iter().map(JoinSet::len).sum();
            log_warn!(log, "shutdown grace elapsed, closing the remaining connections", {"connections": left});
        }
        drop(sets);

        metrics_stop.send_replace(true);
        if let Some(task) = metrics_task {
            let _ = task.await;
        }
        reporter.abort();
        if let Some(w) = watcher {
            w.abort();
        }
    }
}

/// Accepts on one port until the shutdown; returns the connections still being served.
async fn accept_loop(shared: Arc<Shared>, port: Port) -> JoinSet<()> {
    let Port { kind, mut acceptor } = port;
    let mut stop = shared.stop.subscribe();
    let mut conns = JoinSet::new();
    loop {
        let (tcp, peer) = tokio::select! {
            biased;
            _ = stop.wait_for(|stop| *stop) => break,
            accepted = acceptor.accept() => accepted,
        };
        while conns.try_join_next().is_some() {}
        conns.spawn(serve_connection(shared.clone(), kind, tcp, peer.ip(), stop.clone()));
    }
    conns
}

/// Serves one accepted connection of a listener of `kind`. A connection still in the gate or its
/// TLS handshake when the shutdown starts is dropped.
async fn serve_connection(
    shared: Arc<Shared>,
    kind: ListenerKind,
    tcp: TcpStream,
    ip: IpAddr,
    stop: watch::Receiver<bool>,
) {
    match (&shared.gate, &shared.tls) {
        (Some(gate), Some(tls)) => {
            let secure = async {
                let gated = gate.accept(tcp, ip, kind.sheds()).await?;
                gate.handshake(gated, tls.acceptor()).await
            };
            let mut draining = stop.clone();
            let secured = tokio::select! {
                biased;
                _ = draining.wait_for(|stop| *stop) => None,
                secured = secure => secured,
            };
            let Some(secured) = secured else { return };
            let io = Held { inner: secured.stream, _open: secured.open };
            match kind {
                ListenerKind::Ws => shared.ws.serve_dedicated(Box::new(io), ip).await,
                _ => shared.http.serve(io, ip, stop).await,
            }
        }
        _ => match kind {
            ListenerKind::Ws => shared.ws.serve_dedicated(Box::new(tcp), ip).await,
            _ => shared.http.serve(tcp, ip, stop).await,
        },
    }
}

/// A stream that holds the open-connection count of its address for as long as it lives, upgraded
/// to a WebSocket or not.
struct Held<S> {
    inner: S,
    _open: OpenConnection,
}

impl<S: AsyncRead + Unpin> AsyncRead for Held<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Held<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, data)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
