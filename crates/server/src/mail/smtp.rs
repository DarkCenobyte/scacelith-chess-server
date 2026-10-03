//! Minimal SMTP client (RFC 5321) for the server's notification e-mails: one connection per
//! message, EHLO, STARTTLS (required, never opportunistic, when the security is `starttls`),
//! implicit TLS (`tls`, port 465), AUTH PLAIN or LOGIN, MAIL FROM / RCPT TO / DATA with dot
//! stuffing, QUIT. Connecting, the TLS handshake and every read or write have a timeout. Messages
//! are 7bit or quoted-printable (see `message`), so neither 8BITMIME nor SMTPUTF8 is needed.
//!
//! TLS is rustls (ring, TLS 1.2 and 1.3) with SNI and host name verification against the Mozilla
//! roots of `webpki-roots`, or against the roots of an injected client configuration (tests).
//!
//! STARTTLS response injection: bytes received after the 220 answer to STARTTLS and before the
//! TLS handshake are an attack (or a broken server); the client aborts.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use zeroize::Zeroizing;

use crate::config::{Config, SmtpSecurity};
use crate::security::encoding::{js_is_space, js_trim};

/// The default timeout of each step (connection, handshake, inactivity).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Most bytes buffered while waiting for the end of a line.
const MAX_BUFFERED: usize = 65_536;

/// Most lines buffered while waiting for the last line of a reply: a server that never sends it
/// (each chunk resets the inactivity timeout) would be read forever.
const MAX_LINES: usize = 512;

/// The longest error message built from a reply, in characters.
const MAX_ERROR_CHARS: usize = 300;

/// What went wrong while sending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpErrorKind {
    /// The connection could not be opened, failed or was closed by the server.
    Connection,
    /// A step took longer than the timeout.
    Timeout,
    /// The TLS handshake failed (untrusted certificate, wrong host name, protocol error).
    Tls,
    /// The server broke the protocol (malformed or endless reply, STARTTLS injection).
    Protocol,
    /// The server answered with an unexpected code.
    Rejected,
    /// STARTTLS is required but the server does not offer it.
    StarttlsUnavailable,
    /// Credentials are configured but the server offers neither AUTH PLAIN nor AUTH LOGIN.
    AuthUnavailable,
}

impl SmtpErrorKind {
    /// The error code of the logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SmtpErrorKind::Connection => "connection",
            SmtpErrorKind::Timeout => "timeout",
            SmtpErrorKind::Tls => "tls",
            SmtpErrorKind::Protocol => "protocol",
            SmtpErrorKind::Rejected => "rejected",
            SmtpErrorKind::StarttlsUnavailable => "starttls_unavailable",
            SmtpErrorKind::AuthUnavailable => "auth_unavailable",
        }
    }
}

/// A server reply: its code and the text of each line (without the code and separator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpReply {
    /// The three-digit code.
    pub code: u16,
    /// The text of each line.
    pub lines: Vec<String>,
}

/// A failed send.
#[derive(Clone, Debug)]
pub struct SmtpError {
    /// What went wrong.
    pub kind: SmtpErrorKind,
    /// A description (never holds credentials).
    pub message: String,
    /// The reply that was refused, for [`SmtpErrorKind::Rejected`].
    pub reply: Option<SmtpReply>,
}

impl SmtpError {
    fn new(kind: SmtpErrorKind, message: impl Into<String>) -> SmtpError {
        SmtpError { kind, message: message.into(), reply: None }
    }

    fn rejected(message: String, reply: SmtpReply) -> SmtpError {
        let message = message.chars().take(MAX_ERROR_CHARS).collect();
        SmtpError { kind: SmtpErrorKind::Rejected, message, reply: Some(reply) }
    }

    fn io(e: &io::Error) -> SmtpError {
        SmtpError::new(SmtpErrorKind::Connection, e.to_string())
    }

    /// The error code of the logs (`connection`, `timeout`, `tls`, ...).
    pub fn code(&self) -> &'static str {
        self.kind.as_str()
    }
}

impl fmt::Display for SmtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SmtpError {}

/// A TLS client configuration (ring, TLS 1.2 and 1.3) trusting `roots`.
pub fn tls_client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

/// The TLS client configuration trusting the Mozilla roots of `webpki-roots` (built once).
pub fn default_tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(|| tls_client_config(webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect())).clone()
}

/// The machine's host name for EHLO (`/proc/sys/kernel/hostname`), `localhost` when unknown.
pub fn system_helo_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let name = name.trim();
        if name.is_empty() { "localhost".to_string() } else { name.to_string() }
    })
    .clone()
}

/// The security of a port: an explicit `SMTP_SECURITY` is kept; otherwise port 465 means
/// implicit TLS and any other port the default, STARTTLS.
pub fn resolve_security(port: u16, security: SmtpSecurity, explicit: bool) -> SmtpSecurity {
    match (explicit, port) {
        (true, _) => security,
        (false, 465) => SmtpSecurity::Tls,
        (false, _) => SmtpSecurity::Starttls,
    }
}

/// Where and how to send.
#[derive(Clone)]
pub struct SmtpConfig {
    /// The relay host (name or IP address).
    pub host: String,
    /// The relay port.
    pub port: u16,
    /// STARTTLS (required), implicit TLS or none.
    pub security: SmtpSecurity,
    /// The user name; empty means no authentication.
    pub user: String,
    /// The password.
    pub password: Zeroizing<String>,
    /// The EHLO name (characters outside `A-Za-z0-9.-` are dropped).
    pub helo_name: String,
    /// The timeout of each step.
    pub timeout: Duration,
    /// The TLS client configuration (trusted roots).
    pub tls: Arc<ClientConfig>,
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("helo_name", &self.helo_name)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl SmtpConfig {
    /// The `SMTP_*` settings. Port 465 with the default security (`starttls`) means implicit TLS
    /// (the configuration does not say whether `SMTP_SECURITY` was set explicitly).
    pub fn from_config(config: &Config) -> SmtpConfig {
        let explicit = config.smtp_security != SmtpSecurity::Starttls;
        SmtpConfig {
            host: config.smtp_host.clone(),
            port: config.smtp_port,
            security: resolve_security(config.smtp_port, config.smtp_security, explicit),
            user: config.smtp_user.clone(),
            password: Zeroizing::new(
                config.smtp_password.as_ref().map(|p| p.as_str().to_string()).unwrap_or_default(),
            ),
            helo_name: system_helo_name(),
            timeout: DEFAULT_TIMEOUT,
            tls: default_tls_config(),
        }
    }
}

/// Dot stuffing (RFC 5321 4.5.2) and CRLF normalisation of a message for DATA, terminator
/// included: a `.` starting a line (after any line terminator) is doubled.
pub fn dot_stuff(raw: &str) -> String {
    let s = super::message::normalize_line_breaks(raw);
    let mut out = String::with_capacity(s.len() + 8);
    let mut at_line_start = true;
    for c in s.chars() {
        if at_line_start && c == '.' {
            out.push('.');
        }
        out.push(c);
        at_line_start = matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    }
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// The bytes of `s` on the wire: each UTF-16 unit as its low byte (Latin-1 for every text this
/// client sends, which is ASCII).
fn latin1(s: &str) -> Vec<u8> {
    s.encode_utf16().map(|u| u as u8).collect()
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf).await,
            Stream::Tls(s) => s.read(buf).await,
        }
    }

    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.write_all(bytes).await,
            Stream::Tls(s) => {
                s.write_all(bytes).await?;
                s.flush().await
            }
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.shutdown().await,
            Stream::Tls(s) => s.shutdown().await,
        }
    }
}

/// An SMTP connection: the stream, the bytes of an unfinished line and the complete lines not
/// yet consumed.
struct Conn {
    stream: Option<Stream>,
    partial: Vec<u8>,
    lines: VecDeque<String>,
    timeout: Duration,
}

impl Conn {
    fn stream(&mut self) -> Result<&mut Stream, SmtpError> {
        self.stream.as_mut().ok_or_else(|| SmtpError::new(SmtpErrorKind::Connection, "connection closed"))
    }

    fn protocol(message: &str) -> SmtpError {
        SmtpError::new(SmtpErrorKind::Protocol, message)
    }

    /// Adds received bytes; lines are Latin-1 text without their CR LF.
    fn feed(&mut self, bytes: &[u8]) -> Result<(), SmtpError> {
        self.partial.extend_from_slice(bytes);
        if self.partial.len() > MAX_BUFFERED {
            return Err(Conn::protocol("reply too long"));
        }
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            let line = &line[..i];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            self.lines.push_back(line.iter().map(|&b| char::from(b)).collect());
        }
        if self.lines.len() > MAX_LINES {
            return Err(Conn::protocol("reply too long"));
        }
        Ok(())
    }

    /// A complete reply: lines `ddd-text` ... `ddd text` (or a bare `ddd`).
    fn take_reply(&mut self) -> Result<Option<SmtpReply>, SmtpError> {
        for (k, l) in self.lines.iter().enumerate() {
            let b = l.as_bytes();
            let well_formed = b.len() >= 3
                && b[..3].iter().all(u8::is_ascii_digit)
                && (b.len() == 3 || b[3] == b' ' || b[3] == b'-');
            if !well_formed {
                return Err(Conn::protocol("malformed reply"));
            }
            if b.len() == 3 || b[3] == b' ' {
                let lines: Vec<String> = self.lines.drain(..=k).collect();
                let code = lines[0][..3].parse().expect("three ASCII digits");
                let lines = lines.iter().map(|x| x.get(4..).unwrap_or("").to_string()).collect();
                return Ok(Some(SmtpReply { code, lines }));
            }
        }
        Ok(None)
    }

    async fn read_reply(&mut self) -> Result<SmtpReply, SmtpError> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(r) = self.take_reply()? {
                return Ok(r);
            }
            let wait = self.timeout;
            let n = match timeout(wait, self.stream()?.read(&mut chunk)).await {
                Err(_) => return Err(SmtpError::new(SmtpErrorKind::Timeout, "SMTP server timeout")),
                Ok(Err(e)) => return Err(SmtpError::io(&e)),
                Ok(Ok(0)) => {
                    return Err(SmtpError::new(SmtpErrorKind::Connection, "connection closed by the server"));
                }
                Ok(Ok(n)) => n,
            };
            self.feed(&chunk[..n])?;
        }
    }

    async fn write(&mut self, text: &str) -> Result<(), SmtpError> {
        let bytes = Zeroizing::new(latin1(text));
        let wait = self.timeout;
        match timeout(wait, self.stream()?.write_all(&bytes)).await {
            Err(_) => Err(SmtpError::new(SmtpErrorKind::Timeout, "SMTP server timeout")),
            Ok(Err(e)) => Err(SmtpError::io(&e)),
            Ok(Ok(())) => Ok(()),
        }
    }

    /// Sends a command and checks the reply code. `label` names the command in errors (default:
    /// its first word), so that no credential ends up in a message.
    async fn cmd(&mut self, line: &str, expect: &[u16], label: Option<&str>) -> Result<SmtpReply, SmtpError> {
        let mut text = Zeroizing::new(String::with_capacity(line.len() + 2));
        text.push_str(line);
        text.push_str("\r\n");
        self.write(&text).await?;
        let r = self.read_reply().await?;
        if !expect.contains(&r.code) {
            let label = label.unwrap_or_else(|| line.split(' ').next().unwrap_or(line));
            return Err(SmtpError::rejected(format!("{label}: {} {}", r.code, r.lines.join(" ")), r));
        }
        Ok(r)
    }

    fn has_pending(&self) -> bool {
        !self.partial.is_empty() || !self.lines.is_empty()
    }
}

impl Drop for Conn {
    /// Closes the connection in the background (TLS close_notify and FIN, at most one second).
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move {
                let _ = timeout(Duration::from_secs(1), stream.shutdown()).await;
            });
        }
    }
}

/// The EHLO keywords (upper case) and their parameters (upper case).
fn extensions(reply: &SmtpReply) -> HashMap<String, Vec<String>> {
    let mut ext = HashMap::new();
    for l in reply.lines.iter().skip(1) {
        let mut words = js_trim(l).split(js_is_space).filter(|w| !w.is_empty());
        if let Some(k) = words.next() {
            ext.insert(k.to_uppercase(), words.map(str::to_uppercase).collect());
        }
    }
    ext
}

fn server_name(host: &str) -> Result<ServerName<'static>, SmtpError> {
    ServerName::try_from(host.to_string())
        .map_err(|_| SmtpError::new(SmtpErrorKind::Tls, format!("invalid TLS server name: {host}")))
}

async fn connect_plain(cfg: &SmtpConfig) -> Result<TcpStream, SmtpError> {
    match timeout(cfg.timeout, TcpStream::connect((cfg.host.as_str(), cfg.port))).await {
        Err(_) => Err(SmtpError::new(SmtpErrorKind::Timeout, "SMTP connection timeout")),
        Ok(Err(e)) => Err(SmtpError::io(&e)),
        Ok(Ok(s)) => Ok(s),
    }
}

async fn handshake(cfg: &SmtpConfig, tcp: TcpStream) -> Result<Stream, SmtpError> {
    let name = server_name(&cfg.host)?;
    let connector = TlsConnector::from(cfg.tls.clone());
    match timeout(cfg.timeout, connector.connect(name, tcp)).await {
        Err(_) => Err(SmtpError::new(SmtpErrorKind::Timeout, "TLS handshake timeout")),
        Ok(Err(e)) => Err(SmtpError::new(SmtpErrorKind::Tls, e.to_string())),
        Ok(Ok(s)) => Ok(Stream::Tls(Box::new(s))),
    }
}

/// Sends one message. `from` and `to` are bare envelope addresses; `raw` is the message of
/// `message::build_message`. Returns the server's answer to the message.
pub async fn send_smtp(cfg: &SmtpConfig, from: &str, to: &str, raw: &str) -> Result<SmtpReply, SmtpError> {
    let tcp = connect_plain(cfg).await?;
    let stream = match cfg.security {
        SmtpSecurity::Tls => handshake(cfg, tcp).await?,
        SmtpSecurity::Starttls | SmtpSecurity::None => Stream::Plain(tcp),
    };
    let mut c =
        Conn { stream: Some(stream), partial: Vec::new(), lines: VecDeque::new(), timeout: cfg.timeout };
    let helo: String =
        cfg.helo_name.chars().filter(|ch| ch.is_ascii_alphanumeric() || *ch == '.' || *ch == '-').collect();
    let helo = if helo.is_empty() { "localhost".to_string() } else { helo };

    let greeting = c.read_reply().await?;
    if greeting.code != 220 {
        return Err(SmtpError::rejected(format!("greeting: {}", greeting.code), greeting));
    }
    let ehlo = format!("EHLO {helo}");
    let mut ext = extensions(&c.cmd(&ehlo, &[250], None).await?);
    if cfg.security == SmtpSecurity::Starttls {
        if !ext.contains_key("STARTTLS") {
            return Err(SmtpError::new(
                SmtpErrorKind::StarttlsUnavailable,
                "the SMTP server does not offer STARTTLS",
            ));
        }
        c.cmd("STARTTLS", &[220], None).await?;
        if c.has_pending() {
            return Err(Conn::protocol("data received before the TLS handshake (STARTTLS injection)"));
        }
        let Some(Stream::Plain(tcp)) = c.stream.take() else {
            unreachable!("STARTTLS runs on the plain connection");
        };
        c.stream = Some(handshake(cfg, tcp).await?);
        ext = extensions(&c.cmd(&ehlo, &[250], None).await?);
    }
    if !cfg.user.is_empty() {
        let mechanisms = ext.get("AUTH").map(Vec::as_slice).unwrap_or_default();
        if mechanisms.iter().any(|m| m == "PLAIN") {
            let token = Zeroizing::new(format!("\0{}\0{}", cfg.user, cfg.password.as_str()));
            let line = Zeroizing::new(format!("AUTH PLAIN {}", STANDARD.encode(token.as_bytes())));
            c.cmd(&line, &[235], Some("AUTH PLAIN")).await?;
        } else if mechanisms.iter().any(|m| m == "LOGIN") {
            c.cmd("AUTH LOGIN", &[334], None).await?;
            c.cmd(&STANDARD.encode(cfg.user.as_bytes()), &[334], Some("AUTH LOGIN user")).await?;
            let password = Zeroizing::new(STANDARD.encode(cfg.password.as_bytes()));
            c.cmd(&password, &[235], Some("AUTH LOGIN password")).await?;
        } else {
            return Err(SmtpError::new(
                SmtpErrorKind::AuthUnavailable,
                "the SMTP server offers neither AUTH PLAIN nor AUTH LOGIN",
            ));
        }
    }
    c.cmd(&format!("MAIL FROM:<{from}>"), &[250], None).await?;
    c.cmd(&format!("RCPT TO:<{to}>"), &[250, 251], None).await?;
    c.cmd("DATA", &[354], None).await?;
    c.write(&dot_stuff(raw)).await?;
    let done = c.read_reply().await?;
    if done.code != 250 {
        return Err(SmtpError::rejected(format!("message: {} {}", done.code, done.lines.join(" ")), done));
    }
    // The message is accepted already: a failed QUIT changes nothing.
    let _ = c.cmd("QUIT", &[221], None).await;
    Ok(done)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use parking_lot::Mutex;
    use rustls::ServerConfig;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::mail::message::{BuiltMessage, MessageParts, build_message};

    /// A self-signed certificate for `localhost` and `127.0.0.1`, and a client configuration
    /// that trusts it.
    pub(crate) struct TestCert {
        pub(crate) server: Arc<ServerConfig>,
        pub(crate) client: Arc<ClientConfig>,
    }

    pub(crate) fn test_cert() -> TestCert {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
        let cert: CertificateDer<'static> = ck.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        TestCert { server: Arc::new(server), client: tls_client_config(roots) }
    }

    /// What the fake server saw.
    #[derive(Debug, Default)]
    pub(crate) struct Rec {
        pub(crate) commands: Vec<String>,
        pub(crate) messages: Vec<String>,
        pub(crate) auth: Option<(String, String, String)>,
        pub(crate) secure_at_mail: Option<bool>,
    }

    /// The behaviour of the fake server.
    #[derive(Clone)]
    pub(crate) struct Fake {
        pub(crate) ext: Vec<&'static str>,
        pub(crate) starttls: bool,
        pub(crate) implicit_tls: bool,
        pub(crate) inject: &'static str,
        pub(crate) rcpt_code: u16,
        pub(crate) silent: bool,
        pub(crate) user: &'static str,
        pub(crate) pass: &'static str,
    }

    impl Default for Fake {
        fn default() -> Fake {
            Fake {
                ext: vec!["8BITMIME", "AUTH PLAIN LOGIN"],
                starttls: false,
                implicit_tls: false,
                inject: "",
                rcpt_code: 250,
                silent: false,
                user: "mailer",
                pass: "p4ss w0rd",
            }
        }
    }

    pub(crate) struct FakeServer {
        pub(crate) port: u16,
        pub(crate) rec: Arc<Mutex<Rec>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    enum Outcome<S> {
        Done,
        StartTls(S),
    }

    fn b64_text(s: &str) -> String {
        String::from_utf8_lossy(&STANDARD.decode(s.trim()).unwrap_or_default()).into_owned()
    }

    async fn session<S: AsyncRead + AsyncWrite + Unpin>(
        stream: S,
        secure: bool,
        greet: bool,
        fake: &Fake,
        rec: &Mutex<Rec>,
    ) -> Outcome<S> {
        let mut io = BufReader::new(stream);
        if greet && !fake.silent {
            let _ = io.get_mut().write_all(b"220 fake.test ESMTP ready\r\n").await;
        }
        let (mut data_mode, mut data, mut auth_step, mut auth_user) = (false, Vec::new(), 0, String::new());
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match io.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => return Outcome::Done,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string();
            let reply = |s: String| s + "\r\n";
            let out: String = if data_mode {
                if line == "." {
                    rec.lock().messages.push(data.join("\r\n"));
                    data.clear();
                    data_mode = false;
                    reply("250 2.0.0 queued as 42".into())
                } else {
                    data.push(line);
                    continue;
                }
            } else if auth_step == 1 {
                auth_user = b64_text(&line);
                auth_step = 2;
                reply("334 UGFzc3dvcmQ6".into())
            } else if auth_step == 2 {
                let pass = b64_text(&line);
                auth_step = 0;
                let ok = auth_user == fake.user && pass == fake.pass;
                rec.lock().auth = Some(("LOGIN".into(), auth_user.clone(), pass));
                reply(if ok { "235 2.7.0 ok".into() } else { "535 5.7.8 bad credentials".into() })
            } else {
                rec.lock().commands.push(line.clone());
                let verb = line.split(' ').next().unwrap_or("").to_ascii_uppercase();
                match verb.as_str() {
                    "EHLO" => {
                        let mut kws: Vec<&str> = vec!["fake.test greets you"];
                        kws.extend(&fake.ext);
                        if fake.starttls && !secure {
                            kws.push("STARTTLS");
                        }
                        let n = kws.len();
                        kws.iter()
                            .enumerate()
                            .map(|(k, l)| format!("250{}{l}\r\n", if k == n - 1 { ' ' } else { '-' }))
                            .collect()
                    }
                    "STARTTLS" => {
                        let msg = format!("220 2.0.0 go ahead\r\n{}", fake.inject);
                        let _ = io.get_mut().write_all(msg.as_bytes()).await;
                        if !fake.inject.is_empty() {
                            // Keep the connection open until the client gives up.
                            let mut sink = Vec::new();
                            let _ = io.read_to_end(&mut sink).await;
                            return Outcome::Done;
                        }
                        return Outcome::StartTls(io.into_inner());
                    }
                    "AUTH" => {
                        let parts: Vec<&str> = line.split(' ').collect();
                        match parts.get(1).copied() {
                            Some("PLAIN") => {
                                let decoded = b64_text(parts.get(2).copied().unwrap_or(""));
                                let fields: Vec<&str> = decoded.split('\0').collect();
                                let (u, p) = (
                                    fields.get(1).copied().unwrap_or(""),
                                    fields.get(2).copied().unwrap_or(""),
                                );
                                let ok = u == fake.user && p == fake.pass;
                                rec.lock().auth = Some(("PLAIN".into(), u.into(), p.into()));
                                reply(if ok {
                                    "235 2.7.0 ok".into()
                                } else {
                                    "535 5.7.8 bad credentials".into()
                                })
                            }
                            Some("LOGIN") => {
                                auth_step = 1;
                                reply("334 VXNlcm5hbWU6".into())
                            }
                            _ => reply("504 unrecognized".into()),
                        }
                    }
                    "MAIL" => {
                        rec.lock().secure_at_mail = Some(secure);
                        reply("250 2.1.0 ok".into())
                    }
                    "RCPT" if fake.rcpt_code == 250 => reply("250 2.1.5 ok".into()),
                    "RCPT" => reply(format!("{} 5.1.1 no such user", fake.rcpt_code)),
                    "DATA" => {
                        data_mode = true;
                        reply("354 end with <CRLF>.<CRLF>".into())
                    }
                    "QUIT" => {
                        let _ = io.get_mut().write_all(b"221 bye\r\n").await;
                        let _ = io.get_mut().shutdown().await;
                        return Outcome::Done;
                    }
                    _ => reply("502 5.5.2 unknown command".into()),
                }
            };
            if io.get_mut().write_all(out.as_bytes()).await.is_err() {
                return Outcome::Done;
            }
        }
    }

    pub(crate) async fn fake_smtp(fake: Fake, cert: Option<&TestCert>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let rec = Arc::new(Mutex::new(Rec::default()));
        let acceptor = cert.map(|c| TlsAcceptor::from(c.server.clone()));
        let rec2 = rec.clone();
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (fake, rec, acceptor) = (fake.clone(), rec2.clone(), acceptor.clone());
                tokio::spawn(async move {
                    if fake.implicit_tls {
                        let Ok(tls) = acceptor.expect("a certificate").accept(tcp).await else { return };
                        session(tls, true, true, &fake, &rec).await;
                    } else if let Outcome::StartTls(tcp) = session(tcp, false, true, &fake, &rec).await {
                        let Ok(tls) = acceptor.expect("a certificate").accept(tcp).await else { return };
                        session(tls, true, false, &fake, &rec).await;
                    }
                });
            }
        });
        FakeServer { port, rec, task }
    }

    pub(crate) fn message() -> BuiltMessage {
        build_message(&MessageParts {
            from: "Scacelith <no-reply@chess.example.org>",
            to: "alice@example.com",
            subject: "Hello",
            text: "line one\n.starts with a dot\n..two dots\nend",
            date_ms: 0,
            message_id: Some("<00112233445566778899aabbccddeeff@chess.example.org>"),
        })
        .unwrap()
    }

    pub(crate) fn config(port: u16, security: SmtpSecurity) -> SmtpConfig {
        SmtpConfig {
            host: "127.0.0.1".into(),
            port,
            security,
            user: String::new(),
            password: Zeroizing::new(String::new()),
            helo_name: "test.local".into(),
            timeout: Duration::from_secs(3),
            tls: default_tls_config(),
        }
    }

    fn with_auth(mut cfg: SmtpConfig, user: &str, pass: &str) -> SmtpConfig {
        cfg.user = user.into();
        cfg.password = Zeroizing::new(pass.into());
        cfg
    }

    async fn send(cfg: &SmtpConfig) -> Result<SmtpReply, SmtpError> {
        let m = message();
        send_smtp(cfg, &m.from, &m.to, &m.raw).await
    }

    fn verbs(rec: &Mutex<Rec>) -> Vec<String> {
        rec.lock().commands.iter().map(|c| c.split(' ').next().unwrap().to_string()).collect()
    }

    #[test]
    fn dot_stuffing_and_terminator() {
        assert_eq!(dot_stuff("a\n.b\r\n..c"), "a\r\n..b\r\n...c\r\n.\r\n");
        assert_eq!(dot_stuff("x\r\n"), "x\r\n.\r\n");
        let v = crate::mail::message::tests::vectors();
        for p in v["stuff"].as_array().unwrap() {
            assert_eq!(dot_stuff(p[0].as_str().unwrap()), p[1].as_str().unwrap(), "{:?}", p[0]);
        }
        assert_eq!(dot_stuff("a\r.b"), "a\r..b\r\n.\r\n", "after a lone CR too");
    }

    #[test]
    fn security_of_the_port() {
        use SmtpSecurity::*;
        assert_eq!(resolve_security(465, Starttls, false), Tls);
        assert_eq!(resolve_security(587, Starttls, false), Starttls);
        assert_eq!(resolve_security(465, Starttls, true), Starttls);
        assert_eq!(resolve_security(25, None, true), None);
        let mut cfg = Config::for_tests();
        cfg.smtp_port = 465;
        assert_eq!(SmtpConfig::from_config(&cfg).security, Tls);
        cfg.smtp_security = None;
        assert_eq!(SmtpConfig::from_config(&cfg).security, None);
        cfg.smtp_port = 587;
        cfg.smtp_security = Starttls;
        cfg.smtp_password = Some(crate::config::SecretText::new("hunter2".into()));
        let c = SmtpConfig::from_config(&cfg);
        assert_eq!((c.security, c.password.as_str()), (Starttls, "hunter2"));
        assert!(!format!("{c:?}").contains("hunter2"));
        assert!(!system_helo_name().is_empty());
    }

    #[test]
    fn replies_are_parsed_and_capped() {
        let mut c =
            Conn { stream: None, partial: Vec::new(), lines: VecDeque::new(), timeout: DEFAULT_TIMEOUT };
        c.feed(b"250-first\r\n250-SIZE 1000\r\n250").unwrap();
        assert_eq!(c.take_reply().unwrap(), None);
        c.feed(b"\r\n").unwrap();
        assert_eq!(
            c.take_reply().unwrap(),
            Some(SmtpReply { code: 250, lines: vec!["first".into(), "SIZE 1000".into(), "".into()] })
        );
        c.feed(b"220 caf\xe9\n").unwrap();
        assert_eq!(c.take_reply().unwrap().unwrap().lines, ["café"]);
        c.feed(b"25x oops\r\n").unwrap();
        assert_eq!(c.take_reply().unwrap_err().kind, SmtpErrorKind::Protocol);
        let mut c =
            Conn { stream: None, partial: Vec::new(), lines: VecDeque::new(), timeout: DEFAULT_TIMEOUT };
        assert!(c.feed(&vec![b'x'; MAX_BUFFERED + 1]).is_err());
        let mut c =
            Conn { stream: None, partial: Vec::new(), lines: VecDeque::new(), timeout: DEFAULT_TIMEOUT };
        assert!(c.feed("220-x\r\n".repeat(MAX_LINES).as_bytes()).is_ok());
        assert_eq!(c.feed(b"220-x\r\n").unwrap_err().message, "reply too long");
        let ext = extensions(&SmtpReply {
            code: 250,
            lines: vec!["hi".into(), " auth  plain login ".into(), "STARTTLS".into(), "".into()],
        });
        assert_eq!(ext.get("AUTH").unwrap(), &["PLAIN", "LOGIN"]);
        assert!(ext.contains_key("STARTTLS") && ext.len() == 2);
    }

    #[tokio::test]
    async fn plain_relay_with_auth_plain_full_dialogue_dot_stuffing() {
        let s = fake_smtp(Fake::default(), None).await;
        let r = send(&with_auth(config(s.port, SmtpSecurity::None), "mailer", "p4ss w0rd")).await.unwrap();
        assert_eq!(r.code, 250);
        assert_eq!(verbs(&s.rec), ["EHLO", "AUTH", "MAIL", "RCPT", "DATA", "QUIT"]);
        let rec = s.rec.lock();
        assert_eq!(rec.commands[0], "EHLO test.local");
        assert_eq!(rec.auth, Some(("PLAIN".into(), "mailer".into(), "p4ss w0rd".into())));
        assert_eq!(rec.commands[2], "MAIL FROM:<no-reply@chess.example.org>");
        assert_eq!(rec.commands[3], "RCPT TO:<alice@example.com>");
        let got = &rec.messages[0];
        // The server sees the stuffed lines; un-stuffing gives the original message back.
        assert!(got.contains("\r\n..starts with a dot\r\n...two dots\r\n"));
        let unstuffed: Vec<&str> = got.split("\r\n").map(|l| l.strip_prefix('.').unwrap_or(l)).collect();
        assert_eq!(unstuffed.join("\r\n"), message().raw);
    }

    #[tokio::test]
    async fn auth_login_when_plain_is_not_offered_and_no_auth_without_credentials() {
        let s = fake_smtp(Fake { ext: vec!["AUTH LOGIN"], ..Fake::default() }, None).await;
        send(&with_auth(config(s.port, SmtpSecurity::None), "mailer", "p4ss w0rd")).await.unwrap();
        assert_eq!(s.rec.lock().auth, Some(("LOGIN".into(), "mailer".into(), "p4ss w0rd".into())));
        let t = fake_smtp(Fake::default(), None).await;
        send(&config(t.port, SmtpSecurity::None)).await.unwrap();
        assert_eq!(t.rec.lock().auth, None);
        let u = fake_smtp(Fake { ext: vec!["8BITMIME"], ..Fake::default() }, None).await;
        let e = send(&with_auth(config(u.port, SmtpSecurity::None), "mailer", "x")).await.unwrap_err();
        assert_eq!(e.code(), "auth_unavailable");
    }

    #[tokio::test]
    async fn wrong_credentials_and_refused_recipients_are_errors() {
        let s = fake_smtp(Fake::default(), None).await;
        let e = send(&with_auth(config(s.port, SmtpSecurity::None), "mailer", "nope")).await.unwrap_err();
        assert_eq!(e.kind, SmtpErrorKind::Rejected);
        assert_eq!(e.message, "AUTH PLAIN: 535 5.7.8 bad credentials", "no credential in the message");
        assert_eq!(e.reply.as_ref().map(|r| r.code), Some(535));
        assert!(s.rec.lock().messages.is_empty());
        let t = fake_smtp(Fake { rcpt_code: 550, ..Fake::default() }, None).await;
        let e = send(&config(t.port, SmtpSecurity::None)).await.unwrap_err();
        assert_eq!(e.code(), "rejected");
        assert_eq!(e.to_string(), "RCPT: 550 5.1.1 no such user");
    }

    #[tokio::test]
    async fn starttls_required_refused_when_not_offered() {
        let s = fake_smtp(Fake::default(), None).await;
        let e = send(&with_auth(config(s.port, SmtpSecurity::Starttls), "mailer", "p4ss w0rd"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), "starttls_unavailable");
        assert_eq!(verbs(&s.rec), ["EHLO"], "no credentials or message sent in clear");
    }

    #[tokio::test]
    async fn starttls_upgrade_then_ehlo_again_auth_and_message_over_tls() {
        let cert = test_cert();
        let s = fake_smtp(Fake { starttls: true, ..Fake::default() }, Some(&cert)).await;
        let mut cfg = with_auth(config(s.port, SmtpSecurity::Starttls), "mailer", "p4ss w0rd");
        cfg.tls = cert.client.clone();
        send(&cfg).await.unwrap();
        assert_eq!(verbs(&s.rec), ["EHLO", "STARTTLS", "EHLO", "AUTH", "MAIL", "RCPT", "DATA", "QUIT"]);
        assert_eq!(s.rec.lock().secure_at_mail, Some(true));
        assert_eq!(s.rec.lock().messages.len(), 1);
        // Host name verification: the certificate holds localhost too.
        cfg.host = "localhost".into();
        send(&cfg).await.unwrap();
        assert_eq!(s.rec.lock().messages.len(), 2);
    }

    #[tokio::test]
    async fn starttls_untrusted_certificate_is_refused() {
        let cert = test_cert();
        let s = fake_smtp(Fake { starttls: true, ..Fake::default() }, Some(&cert)).await;
        let e = send(&config(s.port, SmtpSecurity::Starttls)).await.unwrap_err();
        assert_eq!(e.code(), "tls", "{e}");
        assert!(s.rec.lock().messages.is_empty());
        // Another certificate's roots do not help either.
        let mut cfg = config(s.port, SmtpSecurity::Starttls);
        cfg.tls = test_cert().client;
        assert_eq!(send(&cfg).await.unwrap_err().code(), "tls");
    }

    #[tokio::test]
    async fn starttls_response_injection_is_detected() {
        let cert = test_cert();
        let s =
            fake_smtp(Fake { starttls: true, inject: "250 injected\r\n", ..Fake::default() }, Some(&cert))
                .await;
        let mut cfg = config(s.port, SmtpSecurity::Starttls);
        cfg.tls = cert.client.clone();
        let e = send(&cfg).await.unwrap_err();
        assert_eq!(e.code(), "protocol", "{e}");
        assert!(e.message.contains("STARTTLS injection"));
    }

    #[tokio::test]
    async fn implicit_tls_port_465_style() {
        let cert = test_cert();
        let s = fake_smtp(Fake { implicit_tls: true, ..Fake::default() }, Some(&cert)).await;
        let mut cfg = with_auth(config(s.port, SmtpSecurity::Tls), "mailer", "p4ss w0rd");
        cfg.tls = cert.client.clone();
        send(&cfg).await.unwrap();
        assert_eq!(s.rec.lock().secure_at_mail, Some(true));
        assert_eq!(s.rec.lock().messages.len(), 1);
        // Untrusted: refused during the handshake.
        let e = send(&config(s.port, SmtpSecurity::Tls)).await.unwrap_err();
        assert_eq!(e.code(), "tls");
    }

    #[tokio::test]
    async fn a_silent_server_times_out() {
        let s = fake_smtp(Fake { silent: true, ..Fake::default() }, None).await;
        let mut cfg = config(s.port, SmtpSecurity::None);
        cfg.timeout = Duration::from_millis(200);
        let t0 = Instant::now();
        let e = send(&cfg).await.unwrap_err();
        assert_eq!(e.code(), "timeout");
        assert_eq!(e.message, "SMTP server timeout");
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_reply_that_never_ends_is_refused() {
        // Continuation lines ("220-...") and never the last one: the server keeps the connection
        // busy, so the inactivity timeout does not end it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let chunk = "220-fake.test\r\n".repeat(50);
            while sock.write_all(chunk.as_bytes()).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let r = timeout(Duration::from_secs(3), send(&config(port, SmtpSecurity::None))).await;
        let e = r.expect("still reading after 3 s").unwrap_err();
        assert_eq!((e.code(), e.message.as_str()), ("protocol", "reply too long"));
        server.abort();
    }

    #[tokio::test]
    async fn connection_refused_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let e = send(&config(port, SmtpSecurity::None)).await.unwrap_err();
        assert_eq!(e.code(), "connection");
        let e = send(&config(port, SmtpSecurity::Tls)).await.unwrap_err();
        assert_eq!(e.code(), "connection");
    }

    #[tokio::test]
    async fn the_greeting_must_be_220() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = sock.write_all(b"554 go away\r\n").await;
            let mut sink = Vec::new();
            let _ = sock.read_to_end(&mut sink).await;
        });
        let e = send(&config(port, SmtpSecurity::None)).await.unwrap_err();
        assert_eq!((e.code(), e.message.as_str()), ("rejected", "greeting: 554"));
        server.abort();
    }
}
