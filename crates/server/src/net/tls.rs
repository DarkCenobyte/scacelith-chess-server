//! Native TLS (DESIGN 5.7): the rustls configuration of the API and WebSocket ports and the
//! certificate behind it, reloaded without a restart.
//!
//! * TLS 1.2 and 1.3 (`TLS_MIN_VERSION`), the server's cipher order (Node's default list minus the
//!   CBC suites rustls does not have), ALPN `http/1.1` only (a client offering only `h2` gets the
//!   `no_application_protocol` alert), session resumption by tickets (rustls's rotating ticketer:
//!   random keys, never derived from `SERVER_SECRET`).
//! * The certificate lives in an [`ArcSwap`]: a reload only affects new handshakes. Triggers:
//!   [`TlsContext::reload_on_sighup`] (the application's SIGHUP handler) and a stat poll of both
//!   files every 10 s, which reloads 1 s after the last change it saw ([`TlsContext::spawn_watcher`]).
//!   A reload that fails (unreadable file, bad PEM, key not matching the certificate) keeps the
//!   current certificate.

use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use rustls::crypto::CryptoProvider;
use rustls::crypto::ring::cipher_suite as suites;
use rustls::server::{ClientHello, ResolvesServerCert, ServerConfig};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::config::{Config, TlsMinVersion};
use crate::log::Logger;
use crate::{log_error, log_info};

/// How often the certificate and key files are checked for a change.
pub const RELOAD_POLL: Duration = Duration::from_secs(10);
/// A reload waits this long after the last change seen (both files are usually replaced together).
pub const RELOAD_DEBOUNCE: Duration = Duration::from_secs(1);
/// Log message of a failed reload.
pub const RELOAD_FAILED: &str = "certificate reload failed, keeping the current certificate";
/// Log message of a successful reload.
pub const RELOADED: &str = "certificate reloaded";
/// Log message of a reload asked by SIGHUP.
pub const SIGHUP_RELOAD: &str = "SIGHUP: reloading certificates";

/// The ALPN protocol of every TLS connection.
pub const ALPN_HTTP11: &[u8] = b"http/1.1";

/// The *ring* provider with the cipher suites in the order of the Node server's default list
/// (the server's preference wins: `ignore_client_order`).
pub fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: LazyLock<Arc<CryptoProvider>> = LazyLock::new(|| {
        let mut p = rustls::crypto::ring::default_provider();
        p.cipher_suites = vec![
            suites::TLS13_AES_256_GCM_SHA384,
            suites::TLS13_CHACHA20_POLY1305_SHA256,
            suites::TLS13_AES_128_GCM_SHA256,
            suites::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            suites::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            suites::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            suites::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            suites::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        ];
        Arc::new(p)
    });
    PROVIDER.clone()
}

/// Why a certificate and its key could not be loaded.
#[derive(Debug)]
pub enum CertError {
    /// A file could not be read.
    Read {
        /// The file.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
    /// The certificate file holds no certificate.
    NoCertificate(PathBuf),
    /// The certificate file is not valid PEM.
    BadCertificate(PathBuf, String),
    /// The key file holds no private key.
    NoKey(PathBuf),
    /// The key file is not valid PEM.
    BadKey(PathBuf, String),
    /// The key does not belong to the certificate.
    KeyMismatch,
    /// rustls refused the pair (unsupported key type...).
    Rejected(rustls::Error),
}

impl fmt::Display for CertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CertError::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            CertError::NoCertificate(p) => write!(f, "no certificate in {}", p.display()),
            CertError::BadCertificate(p, e) => write!(f, "invalid certificate PEM in {}: {e}", p.display()),
            CertError::NoKey(p) => write!(f, "no private key in {}", p.display()),
            CertError::BadKey(p, e) => write!(f, "invalid private key PEM in {}: {e}", p.display()),
            CertError::KeyMismatch => f.write_str("the private key does not match the certificate"),
            CertError::Rejected(e) => write!(f, "certificate or key refused: {e}"),
        }
    }
}

impl std::error::Error for CertError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CertError::Read { source, .. } => Some(source),
            CertError::Rejected(e) => Some(e),
            _ => None,
        }
    }
}

/// Builds a certificate from PEM text: the full chain (end-entity first) and an unencrypted
/// PKCS#8, PKCS#1 (RSA) or SEC1 (EC) key that matches it.
pub fn certified_key_from_pem(
    chain_pem: &[u8],
    key_pem: &[u8],
    cert_path: &Path,
    key_path: &Path,
) -> Result<CertifiedKey, CertError> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| CertError::BadCertificate(cert_path.to_path_buf(), e.to_string()))?;
    if chain.is_empty() {
        return Err(CertError::NoCertificate(cert_path.to_path_buf()));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| match e {
        rustls_pki_types::pem::Error::NoItemsFound => CertError::NoKey(key_path.to_path_buf()),
        other => CertError::BadKey(key_path.to_path_buf(), other.to_string()),
    })?;
    CertifiedKey::from_der(chain, key, &provider()).map_err(|e| match e {
        rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch) => CertError::KeyMismatch,
        other => CertError::Rejected(other),
    })
}

/// Reads and builds the certificate of `cert_path` and `key_path` (blocking file reads).
pub fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<CertifiedKey, CertError> {
    let read =
        |p: &Path| std::fs::read(p).map_err(|source| CertError::Read { path: p.to_path_buf(), source });
    certified_key_from_pem(&read(cert_path)?, &read(key_path)?, cert_path, key_path)
}

/// Serves the current certificate to every handshake, whatever the SNI.
#[derive(Debug)]
struct CertResolver(ArcSwap<CertifiedKey>);

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.load_full())
    }
}

/// The rustls configuration of the native TLS listeners.
fn server_config(resolver: Arc<CertResolver>, min: TlsMinVersion) -> Result<ServerConfig, rustls::Error> {
    let versions: &[&rustls::SupportedProtocolVersion] = match min {
        TlsMinVersion::Tls12 => &[&rustls::version::TLS13, &rustls::version::TLS12],
        TlsMinVersion::Tls13 => &[&rustls::version::TLS13],
    };
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    cfg.alpn_protocols = vec![ALPN_HTTP11.to_vec()];
    cfg.ignore_client_order = true;
    cfg.ticketer = rustls::crypto::ring::Ticketer::new()?;
    Ok(cfg)
}

/// What a stat of a watched file says: modification time, size and inode (zeros when the file is
/// missing, which is a change too).
type FileStamp = (Option<SystemTime>, u64, u64);

async fn stamp(path: &Path) -> FileStamp {
    match tokio::fs::metadata(path).await {
        Ok(m) => (m.modified().ok(), m.len(), m.ino()),
        Err(_) => (None, 0, 0),
    }
}

/// The TLS state of the process: the configuration shared by every native listener, and the
/// certificate it serves.
pub struct TlsContext {
    resolver: Arc<CertResolver>,
    config: Arc<ServerConfig>,
    acceptor: TlsAcceptor,
    cert_path: PathBuf,
    key_path: PathBuf,
    reload_lock: parking_lot::Mutex<()>,
    log: Logger,
}

impl fmt::Debug for TlsContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsContext").field("cert", &self.cert_path).field("key", &self.key_path).finish()
    }
}

impl TlsContext {
    /// Loads `TLS_CERT_FILE` and `TLS_KEY_FILE` (blocking reads: call it at startup) and builds
    /// the configuration for `TLS_MIN_VERSION`.
    pub fn new(config: &Config, log: Logger) -> Result<Arc<TlsContext>, CertError> {
        TlsContext::from_files(
            Path::new(&config.tls_cert_file),
            Path::new(&config.tls_key_file),
            config.tls_min_version,
            log,
        )
    }

    /// Same as [`TlsContext::new`] with explicit files.
    pub fn from_files(
        cert_path: &Path,
        key_path: &Path,
        min: TlsMinVersion,
        log: Logger,
    ) -> Result<Arc<TlsContext>, CertError> {
        let key = load_certified_key(cert_path, key_path)?;
        let resolver = Arc::new(CertResolver(ArcSwap::from_pointee(key)));
        let config = Arc::new(server_config(resolver.clone(), min).map_err(CertError::Rejected)?);
        Ok(Arc::new(TlsContext {
            resolver,
            acceptor: TlsAcceptor::from(config.clone()),
            config,
            cert_path: cert_path.to_path_buf(),
            key_path: key_path.to_path_buf(),
            reload_lock: parking_lot::Mutex::new(()),
            log,
        }))
    }

    /// The acceptor of new connections.
    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }

    /// The rustls configuration.
    pub fn server_config(&self) -> &Arc<ServerConfig> {
        &self.config
    }

    /// The certificate served now.
    pub fn certificate(&self) -> Arc<CertifiedKey> {
        self.resolver.0.load_full()
    }

    /// Reads both files again and serves the new certificate to new handshakes; on failure logs
    /// the error and keeps the current one. Blocking: use [`TlsContext::reload`] from async code.
    pub fn reload_blocking(&self) -> bool {
        let _one_at_a_time = self.reload_lock.lock();
        match load_certified_key(&self.cert_path, &self.key_path) {
            Ok(key) => {
                self.resolver.0.store(Arc::new(key));
                log_info!(self.log, RELOADED);
                true
            }
            Err(e) => {
                log_error!(self.log, RELOAD_FAILED, {"err": {"message": e.to_string()}});
                false
            }
        }
    }

    /// [`TlsContext::reload_blocking`] on a blocking thread.
    pub async fn reload(self: &Arc<Self>) -> bool {
        let ctx = self.clone();
        tokio::task::spawn_blocking(move || ctx.reload_blocking()).await.unwrap_or(false)
    }

    /// The reload of the SIGHUP handler (`ExecReload=/bin/kill -HUP $MAINPID`).
    pub async fn reload_on_sighup(self: &Arc<Self>) -> bool {
        log_info!(self.log, SIGHUP_RELOAD);
        self.reload().await
    }

    /// Watches both files: a stat every 10 s, a reload 1 s after the last change seen. The task
    /// ends with the context (it holds a weak reference) or when aborted.
    pub fn spawn_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        self.spawn_watcher_with(RELOAD_POLL, RELOAD_DEBOUNCE)
    }

    /// [`TlsContext::spawn_watcher`] with other periods (tests).
    pub fn spawn_watcher_with(
        self: &Arc<Self>,
        poll: Duration,
        debounce: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        let (cert, key) = (self.cert_path.clone(), self.key_path.clone());
        tokio::spawn(watch(weak, cert, key, poll, debounce))
    }
}

async fn watch(weak: Weak<TlsContext>, cert: PathBuf, key: PathBuf, poll: Duration, debounce: Duration) {
    let stamps = || async { (stamp(&cert).await, stamp(&key).await) };
    let mut last = stamps().await;
    loop {
        tokio::time::sleep(poll).await;
        if weak.strong_count() == 0 {
            return;
        }
        let now = stamps().await;
        if now == last {
            continue;
        }
        last = now;
        // Wait until the files stay unchanged for `debounce`.
        loop {
            tokio::time::sleep(debounce).await;
            let again = stamps().await;
            if again == last {
                break;
            }
            last = again;
        }
        let Some(ctx) = weak.upgrade() else { return };
        ctx.reload().await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::Engine as _;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

    /// A test certificate and its key, as PEM.
    pub(crate) struct TestCert {
        pub cert_pem: String,
        pub key_pem: String,
        pub der: Vec<u8>,
    }

    fn pem(label: &str, der: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        let mut out = format!("-----BEGIN {label}-----\n");
        for line in b64.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            out.push('\n');
        }
        out.push_str(&format!("-----END {label}-----\n"));
        out
    }

    /// A self-signed certificate for `name` (ECDSA P-256).
    pub(crate) fn test_cert(name: &str) -> TestCert {
        let c = rcgen::generate_simple_self_signed(vec![name.to_string()]).expect("a test certificate");
        TestCert {
            cert_pem: pem("CERTIFICATE", c.cert.der()),
            key_pem: pem("PRIVATE KEY", &c.signing_key.serialize_der()),
            der: c.cert.der().to_vec(),
        }
    }

    /// A directory removed when dropped.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> TempDir {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("scacelith-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("a temporary directory");
            TempDir(dir)
        }

        pub(crate) fn write(&self, name: &str, text: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, text).expect("a temporary file");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Accepts any server certificate (the tests read it from the connection instead).
    #[derive(Debug)]
    struct AcceptAny;

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            provider().signature_verification_algorithms.supported_schemes()
        }
    }

    /// A client configuration for the tests: any certificate, the given versions and ALPN.
    pub(crate) fn client_config(
        versions: &[&'static rustls::SupportedProtocolVersion],
        alpn: &[&[u8]],
    ) -> Arc<ClientConfig> {
        let mut c = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(versions)
            .expect("valid versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny))
            .with_no_client_auth();
        c.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Arc::new(c)
    }

    /// The default test client: TLS 1.2 and 1.3, ALPN `http/1.1`.
    pub(crate) fn test_client() -> Arc<ClientConfig> {
        client_config(&[&rustls::version::TLS13, &rustls::version::TLS12], &[ALPN_HTTP11])
    }

    /// A context serving `cert` from files in `dir`.
    pub(crate) fn context(dir: &TempDir, cert: &TestCert) -> Arc<TlsContext> {
        let c = dir.write("live.crt", &cert.cert_pem);
        let k = dir.write("live.key", &cert.key_pem);
        TlsContext::from_files(&c, &k, TlsMinVersion::Tls12, Logger::root().child("tls-test"))
            .expect("a context")
    }

    #[test]
    fn loads_pem_and_refuses_broken_pairs() {
        let a = test_cert("first.test");
        let b = test_cert("second.test");
        let (cp, kp) = (Path::new("c.pem"), Path::new("k.pem"));
        let ok = certified_key_from_pem(a.cert_pem.as_bytes(), a.key_pem.as_bytes(), cp, kp).expect("valid");
        assert_eq!(ok.cert[0].as_ref(), a.der.as_slice());
        let mismatch = certified_key_from_pem(a.cert_pem.as_bytes(), b.key_pem.as_bytes(), cp, kp);
        assert!(matches!(mismatch, Err(CertError::KeyMismatch)), "{mismatch:?}");
        assert!(matches!(
            certified_key_from_pem(b"", a.key_pem.as_bytes(), cp, kp),
            Err(CertError::NoCertificate(_))
        ));
        assert!(matches!(
            certified_key_from_pem(a.cert_pem.as_bytes(), b"junk", cp, kp),
            Err(CertError::NoKey(_))
        ));
        let truncated = &a.cert_pem[..a.cert_pem.len() - 20];
        assert!(certified_key_from_pem(truncated.as_bytes(), a.key_pem.as_bytes(), cp, kp).is_err());
        let missing = load_certified_key(Path::new("/nonexistent/c.pem"), kp);
        assert!(matches!(missing, Err(CertError::Read { .. })));
        assert_eq!(CertError::KeyMismatch.to_string(), "the private key does not match the certificate");
    }

    #[test]
    fn the_configuration_follows_the_node_server() {
        let dir = TempDir::new("tls-config");
        let ctx = context(&dir, &test_cert("first.test"));
        let cfg = ctx.server_config();
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(cfg.ignore_client_order);
        assert!(cfg.ticketer.enabled());
        assert_eq!(cfg.max_early_data_size, 0);
        let names: Vec<String> =
            provider().cipher_suites.iter().map(|s| format!("{:?}", s.suite())).collect();
        assert_eq!(names[0], "TLS13_AES_256_GCM_SHA384");
        assert_eq!(names[3], "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256");
    }

    #[test]
    fn a_reload_swaps_the_certificate_and_a_failed_one_keeps_it() {
        let dir = TempDir::new("tls-reload");
        let a = test_cert("first.test");
        let b = test_cert("second.test");
        let ctx = context(&dir, &a);
        assert_eq!(ctx.certificate().cert[0].as_ref(), a.der.as_slice());
        crate::log::capture(true);
        dir.write("live.crt", &b.cert_pem);
        dir.write("live.key", &b.key_pem);
        assert!(ctx.reload_blocking());
        assert_eq!(ctx.certificate().cert[0].as_ref(), b.der.as_slice());
        dir.write("live.key", &a.key_pem);
        assert!(!ctx.reload_blocking(), "the key no longer matches");
        assert_eq!(ctx.certificate().cert[0].as_ref(), b.der.as_slice(), "the current one is kept");
        std::fs::remove_file(dir.0.join("live.crt")).expect("remove");
        assert!(!ctx.reload_blocking(), "a missing file");
        assert_eq!(ctx.certificate().cert[0].as_ref(), b.der.as_slice());
        let lines = crate::log::capture(true);
        assert!(lines.iter().any(|l| l.contains(RELOADED)), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains(RELOAD_FAILED)));
    }

    #[tokio::test]
    async fn the_watcher_reloads_after_a_change_and_stops_with_the_context() {
        let dir = TempDir::new("tls-watch");
        let a = test_cert("first.test");
        let b = test_cert("second.test");
        let ctx = context(&dir, &a);
        let task = ctx.spawn_watcher_with(Duration::from_millis(40), Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(60)).await;
        dir.write("live.crt", &b.cert_pem);
        dir.write("live.key", &b.key_pem);
        let mut swapped = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if ctx.certificate().cert[0].as_ref() == b.der.as_slice() {
                swapped = true;
                break;
            }
        }
        assert!(swapped, "the new certificate is served");
        drop(ctx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the watcher ends")
            .expect("no panic");
    }

    #[tokio::test]
    async fn sighup_reloads() {
        let dir = TempDir::new("tls-hup");
        let a = test_cert("first.test");
        let b = test_cert("second.test");
        let ctx = context(&dir, &a);
        dir.write("live.crt", &b.cert_pem);
        dir.write("live.key", &b.key_pem);
        assert!(ctx.reload_on_sighup().await);
        assert_eq!(ctx.certificate().cert[0].as_ref(), b.der.as_slice());
    }
}
