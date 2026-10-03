//! TCP listening sockets: binding with socket2 (`SO_REUSEADDR`, the configured backlog, dual-stack
//! when bound to `::`), the options of accepted sockets (`TCP_NODELAY`, TCP keep-alive), an accept
//! loop that survives transient failures, and the operator's way out of a refused privileged port.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::config::Config;
use crate::log::Logger;
use crate::log_error;

/// Backlog when `LISTEN_BACKLOG` is not a positive number (the kernel caps it at `somaxconn`).
pub const DEFAULT_BACKLOG: i32 = 2048;
/// Idle time before the kernel probes an accepted connection.
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
/// Time between two probes.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// Unanswered probes before the kernel drops the connection.
pub const KEEPALIVE_RETRIES: u32 = 4;
/// Pause of the accept loop after a failure that is not about one connection (`EMFILE`...).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// At most one log line per this long about failing accepts.
const ACCEPT_LOG_EVERY: Duration = Duration::from_secs(10);

/// The backlog of the configuration (`LISTEN_BACKLOG`), or [`DEFAULT_BACKLOG`].
pub fn backlog_of(config: &Config) -> i32 {
    match i32::try_from(config.listen_backlog) {
        Ok(n) if n > 0 => n,
        _ => DEFAULT_BACKLOG,
    }
}

/// The address to bind (`BIND_ADDRESS`, brackets around an IPv6 address allowed).
pub fn bind_ip(config: &Config) -> Result<IpAddr, io::Error> {
    let text = config.bind_address.trim();
    let bare = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
    bare.parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("BIND_ADDRESS is not an IP address: {text}"))
    })
}

/// The name of a few errno values of listen failures, as Node reports them.
fn errno_name(e: &io::Error) -> Option<&'static str> {
    match e.raw_os_error()? {
        libc::EACCES => Some("EACCES"),
        libc::EPERM => Some("EPERM"),
        libc::EADDRINUSE => Some("EADDRINUSE"),
        libc::EADDRNOTAVAIL => Some("EADDRNOTAVAIL"),
        libc::EINVAL => Some("EINVAL"),
        _ => None,
    }
}

/// The operator's way out of a listen failure, or `None` when the error has no specific advice:
/// an `EACCES` / `EPERM` on a port below 1024 (the process lacks `CAP_NET_BIND_SERVICE`).
/// `exe` is the server binary named in the `setcap` command.
pub fn listen_hint(code: Option<&str>, port: u16, exe: &str) -> Option<String> {
    let code = code?;
    if (code != "EACCES" && code != "EPERM") || !(1..1024).contains(&port) {
        return None;
    }
    Some(format!(
        "Cannot listen on port {port} ({code}): ports below 1024 need the CAP_NET_BIND_SERVICE capability. Either \
         run the server with systemd and give the unit AmbientCapabilities=CAP_NET_BIND_SERVICE and \
         CapabilityBoundingSet=CAP_NET_BIND_SERVICE (README, \"Running as a service\"); or give the server binary the \
         capability: sudo setcap cap_net_bind_service=+ep {exe} (again after each upgrade of the binary); or let \
         unprivileged processes bind it: sysctl -w net.ipv4.ip_unprivileged_port_start={port} (and in /etc/sysctl.d/ \
         to keep it); or choose a port of 1024 or above with API_PORT (and PUBLIC_API_PORT behind a port mapping)."
    ))
}

/// The running binary, for the `setcap` command of [`listen_hint`].
fn current_exe() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "scacelith-server".to_string())
}

/// A listen failure: the address, the errno name when it has one, the operator's hint for a
/// refused privileged port (its message then), and the cause.
#[derive(Debug)]
pub struct ListenError {
    /// The address that was asked for.
    pub addr: SocketAddr,
    /// `EACCES`, `EPERM`, `EADDRINUSE`... when the error has one of these errno values.
    pub code: Option<&'static str>,
    hint: Option<String>,
    source: io::Error,
}

impl ListenError {
    /// Wraps the failure of listening on `addr`.
    pub fn new(addr: SocketAddr, source: io::Error) -> ListenError {
        ListenError::with_exe(addr, source, &current_exe())
    }

    fn with_exe(addr: SocketAddr, source: io::Error, exe: &str) -> ListenError {
        let code = errno_name(&source);
        let hint = listen_hint(code, addr.port(), exe);
        ListenError { addr, code, hint, source }
    }

    /// The port that was asked for.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The operator's way out, when there is a specific one.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The I/O error of the bind or the listen.
    pub fn cause(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.hint, self.code) {
            (Some(hint), _) => f.write_str(hint),
            (None, Some(code)) => write!(f, "listen {code}: {} {}", self.source, self.addr),
            (None, None) => write!(f, "listen: {} {}", self.source, self.addr),
        }
    }
}

impl std::error::Error for ListenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Opens a listening socket on `ip:port`: `SO_REUSEADDR`, dual-stack for an IPv6 address (IPv4
/// peers then appear IPv4-mapped; [`Acceptor`] gives them back as IPv4), non-blocking, registered
/// with the current tokio runtime.
pub fn bind(ip: IpAddr, port: u16, backlog: i32) -> Result<TcpListener, ListenError> {
    let addr = SocketAddr::new(ip, port);
    let fail = |e| ListenError::new(addr, e);
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP)).map_err(fail)?;
    socket.set_reuse_address(true).map_err(fail)?;
    if addr.is_ipv6() {
        socket.set_only_v6(false).map_err(fail)?;
    }
    socket.set_nonblocking(true).map_err(fail)?;
    socket.bind(&addr.into()).map_err(fail)?;
    socket.listen(backlog.max(1)).map_err(fail)?;
    TcpListener::from_std(socket.into()).map_err(fail)
}

/// Sets the options of an accepted connection: `TCP_NODELAY` (small frames go out at once) and TCP
/// keep-alive (a peer that vanished without a FIN is noticed after a few minutes). Failures are
/// ignored: the connection works without them.
pub fn tune_accepted(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_RETRIES);
    let _ = SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Whether an accept failure concerns only the connection being accepted.
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}

/// The accept loop of one listening socket.
#[derive(Debug)]
pub struct Acceptor {
    listener: TcpListener,
    log: Logger,
    last_log: Option<Instant>,
}

impl Acceptor {
    /// Accepts from `listener`; failures are logged on `log`.
    pub fn new(listener: TcpListener, log: Logger) -> Acceptor {
        Acceptor { listener, log, last_log: None }
    }

    /// The bound address (the actual port when the configuration asked for port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The next connection, tuned (see [`tune_accepted`]), with its peer address in canonical form
    /// (an IPv4-mapped IPv6 address becomes IPv4). A failure about one connection is skipped; any
    /// other (out of file descriptors...) pauses the loop for 100 ms and is logged now and then.
    /// Cancel-safe: dropping the future loses no connection.
    pub async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer)) => {
                    tune_accepted(&stream);
                    return (stream, SocketAddr::new(peer.ip().to_canonical(), peer.port()));
                }
                Err(e) if is_connection_error(&e) => continue,
                Err(e) => {
                    let now = Instant::now();
                    if self.last_log.is_none_or(|t| now.duration_since(t) >= ACCEPT_LOG_EVERY) {
                        self.last_log = Some(now);
                        log_error!(self.log, "accept failed", {"err": {"message": e.to_string()}});
                    }
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
    }

    #[test]
    fn names_the_three_fixes_for_eacces_and_eperm_below_1024() {
        for code in ["EACCES", "EPERM"] {
            let hint = listen_hint(Some(code), 443, "/usr/bin/scacelith-server").expect("a hint");
            assert!(
                hint.starts_with(&format!(
                    "Cannot listen on port 443 ({code}): ports below 1024 need the CAP_NET_BIND_SERVICE capability."
                )),
                "{hint}"
            );
            assert!(hint.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"), "{hint}");
            assert!(hint.contains("CapabilityBoundingSet=CAP_NET_BIND_SERVICE"), "{hint}");
            assert!(
                hint.contains("sudo setcap cap_net_bind_service=+ep /usr/bin/scacelith-server"),
                "{hint}"
            );
            assert!(hint.contains("sysctl -w net.ipv4.ip_unprivileged_port_start=443"), "{hint}");
            assert!(hint.contains("API_PORT"), "{hint}");
        }
        assert!(
            listen_hint(Some("EACCES"), 80, "x").expect("a hint").contains("ip_unprivileged_port_start=80")
        );
    }

    #[test]
    fn has_no_advice_for_other_errors_or_unprivileged_ports() {
        assert_eq!(listen_hint(Some("EADDRINUSE"), 443, "x"), None);
        assert_eq!(listen_hint(Some("EACCES"), 1024, "x"), None);
        assert_eq!(listen_hint(Some("EACCES"), 8443, "x"), None);
        assert_eq!(listen_hint(Some("EACCES"), 0, "x"), None);
        assert_eq!(listen_hint(None, 443, "x"), None);
    }

    #[test]
    fn a_listen_error_carries_the_hint_the_code_the_port_and_the_cause() {
        let err = ListenError::with_exe(addr(443), io::Error::from_raw_os_error(libc::EACCES), "/srv/bin");
        assert_eq!(err.code, Some("EACCES"));
        assert_eq!(err.port(), 443);
        assert_eq!(err.cause().raw_os_error(), Some(libc::EACCES));
        assert!(err.to_string().contains("CAP_NET_BIND_SERVICE"));
        assert!(err.to_string().contains("setcap cap_net_bind_service=+ep /srv/bin"));
        assert_eq!(err.hint(), Some(err.to_string().as_str()));
        let src = std::error::Error::source(&err).expect("a cause");
        assert!(src.to_string().contains("ermission denied"), "{src}");
    }

    #[tokio::test]
    async fn other_errors_keep_their_own_message() {
        let first = bind(LOCAL, 0, 16).expect("bound");
        let port = first.local_addr().expect("address").port();
        let err = bind(LOCAL, port, 16).expect_err("the port is taken");
        assert_eq!(err.code, Some("EADDRINUSE"));
        assert_eq!(err.hint(), None);
        assert_eq!(err.cause().kind(), io::ErrorKind::AddrInUse);
        assert!(err.to_string().starts_with("listen EADDRINUSE: "), "{err}");
        assert!(err.to_string().ends_with(&format!(" 127.0.0.1:{port}")), "{err}");
    }

    #[test]
    fn the_backlog_and_the_address_come_from_the_configuration() {
        let mut c = Config::for_tests();
        assert_eq!(backlog_of(&c), 2048);
        c.listen_backlog = 128;
        assert_eq!(backlog_of(&c), 128);
        c.listen_backlog = 0;
        assert_eq!(backlog_of(&c), DEFAULT_BACKLOG);
        c.listen_backlog = i64::MAX;
        assert_eq!(backlog_of(&c), DEFAULT_BACKLOG);
        assert_eq!(bind_ip(&c).expect("an address"), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        c.bind_address = "[::1]".to_string();
        assert_eq!(bind_ip(&c).expect("an address"), "::1".parse::<IpAddr>().expect("ip"));
        c.bind_address = "localhost".to_string();
        assert_eq!(bind_ip(&c).expect_err("a name").kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn accepted_connections_have_nodelay_and_keepalive() {
        let mut acceptor = Acceptor::new(bind(LOCAL, 0, 16).expect("bound"), Logger::root());
        let at = acceptor.local_addr().expect("address");
        let _client = TcpStream::connect(at).await.expect("connected");
        let (stream, peer) = acceptor.accept().await;
        assert_eq!(peer.ip(), LOCAL);
        assert!(stream.nodelay().expect("nodelay"));
        let sock = SockRef::from(&stream);
        assert!(sock.keepalive().expect("keepalive"));
        assert_eq!(sock.tcp_keepalive_time().expect("idle"), KEEPALIVE_IDLE);
        assert_eq!(sock.tcp_keepalive_interval().expect("interval"), KEEPALIVE_INTERVAL);
        assert_eq!(sock.tcp_keepalive_retries().expect("retries"), KEEPALIVE_RETRIES);
    }

    #[tokio::test]
    async fn a_dual_stack_listener_reports_ipv4_peers_as_ipv4() {
        let Ok(listener) = bind(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0, 16) else {
            return; // no IPv6 on this machine
        };
        let mut acceptor = Acceptor::new(listener, Logger::root());
        let port = acceptor.local_addr().expect("address").port();
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.expect("connected");
        let (_stream, peer) = acceptor.accept().await;
        assert_eq!(peer.ip(), LOCAL);
    }
}
