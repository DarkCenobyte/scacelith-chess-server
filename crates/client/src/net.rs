//! Where a server is and how to reach it: [`Endpoint`] (address, TLS server name, trust) and the
//! [`Stream`] it opens (plain TCP or TLS over TCP).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpSocket, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::error::{ClientError, Result};
use crate::tls::TlsConfig;

/// Default deadline of a TCP connect plus TLS handshake.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A server address with its TLS settings.
///
/// ```no_run
/// # async fn demo() -> scacelith_client::Result<()> {
/// use scacelith_client::{Endpoint, TlsConfig};
/// // A server started in-process by a test, without TLS:
/// let plain = Endpoint::plain("127.0.0.1:8080".parse().unwrap());
/// // The same with TLS and its throw-away certificate:
/// let tls = Endpoint::tls("127.0.0.1:8443".parse().unwrap(), "localhost",
///                         TlsConfig::with_root_file("cert.pem")?);
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct Endpoint {
    addr: SocketAddr,
    host: String,
    tls: Option<TlsConfig>,
    connect_timeout: Duration,
    local: Option<IpAddr>,
}

/// Time spent opening a connection, step by step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectTimings {
    /// TCP connect.
    pub tcp: Duration,
    /// TLS handshake (zero without TLS).
    pub tls: Duration,
    /// WebSocket upgrade: request sent to `101` read (zero for plain HTTP).
    pub upgrade: Duration,
    /// Realtime `Hello` sent to `Welcome` read (zero below the realtime layer).
    pub hello: Duration,
}

impl ConnectTimings {
    /// Sum of the steps.
    pub fn total(&self) -> Duration {
        self.tcp + self.tls + self.upgrade + self.hello
    }
}

impl Endpoint {
    /// A server without TLS (`TLS_MODE=off`, local tests). The host is the IP address.
    pub fn plain(addr: SocketAddr) -> Endpoint {
        Endpoint {
            addr,
            host: addr.ip().to_string(),
            tls: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            local: None,
        }
    }

    /// A server with TLS. `host` is the name the certificate must be valid for (also sent as SNI
    /// and in the Host header): a DNS name or an IP address, without brackets or port.
    pub fn tls(addr: SocketAddr, host: impl Into<String>, tls: TlsConfig) -> Endpoint {
        Endpoint {
            addr,
            host: host.into(),
            tls: Some(tls),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            local: None,
        }
    }

    /// Resolves `host` and builds an endpoint on its first address (TLS when `tls` is given).
    pub async fn lookup(host: &str, port: u16, tls: Option<TlsConfig>) -> Result<Endpoint> {
        let addr = tokio::net::lookup_host((host, port))
            .await?
            .next()
            .ok_or_else(|| ClientError::Io(io::Error::new(io::ErrorKind::NotFound, "no address")))?;
        Ok(match tls {
            Some(tls) => Endpoint::tls(addr, host, tls),
            None => Endpoint { host: host.to_string(), ..Endpoint::plain(addr) },
        })
    }

    /// The same endpoint with another deadline for the TCP connect plus TLS handshake.
    pub fn with_connect_timeout(mut self, limit: Duration) -> Endpoint {
        self.connect_timeout = limit;
        self
    }

    /// The same endpoint, its connections opened from the local address `ip` (port of the
    /// system's choice): tests of a server's protection per address use the loopback aliases
    /// (`127.0.0.2`...), which Linux routes like `127.0.0.1`.
    pub fn with_local_addr(mut self, ip: IpAddr) -> Endpoint {
        self.local = Some(ip);
        self
    }

    /// The local address connections are opened from, when one was chosen.
    pub fn local_addr(&self) -> Option<IpAddr> {
        self.local
    }

    /// The socket address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The host name (TLS server name and Host header).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The TLS settings, `None` for plain TCP.
    pub fn tls_config(&self) -> Option<&TlsConfig> {
        self.tls.as_ref()
    }

    /// Whether the endpoint uses TLS.
    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Value of the Host header: the host (an IPv6 address in brackets), with the port unless it
    /// is the scheme's default.
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default_port = if self.is_tls() { 443 } else { 80 };
        match self.addr.port() {
            port if port == default_port => host,
            port => format!("{host}:{port}"),
        }
    }

    /// Opens a connection: TCP (no Nagle delay, from the local address when one was chosen), then
    /// the TLS handshake when configured.
    pub async fn connect(&self) -> Result<(Stream, ConnectTimings)> {
        let fut = async {
            let mut timings = ConnectTimings::default();
            let started = Instant::now();
            let tcp = match self.local {
                None => TcpStream::connect(self.addr).await?,
                Some(ip) => {
                    let socket = if ip.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
                    socket.bind(SocketAddr::new(ip, 0))?;
                    socket.connect(self.addr).await?
                }
            };
            tcp.set_nodelay(true)?;
            timings.tcp = started.elapsed();
            let Some(tls) = &self.tls else {
                return Ok((Stream::Plain(tcp), timings));
            };
            let name = ServerName::try_from(self.host.clone())
                .map_err(|e| ClientError::Tls(format!("invalid server name {:?}: {e}", self.host)))?;
            let tls_started = Instant::now();
            let stream = TlsConnector::from(tls.rustls().clone()).connect(name, tcp).await?;
            timings.tls = tls_started.elapsed();
            Ok((Stream::Tls(Box::new(stream)), timings))
        };
        crate::error::with_timeout(self.connect_timeout, "connect", fut).await
    }
}

/// A connection to a server: plain TCP or TLS.
#[derive(Debug)]
pub enum Stream {
    /// Plain TCP.
    Plain(TcpStream),
    /// TLS over TCP.
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    /// The TCP socket under the stream.
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref().0,
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Stream::Plain(s) => s.is_write_vectored(),
            Stream::Tls(s) => s.is_write_vectored(),
        }
    }
}
