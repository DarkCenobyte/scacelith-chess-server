//! TLS settings of the client: which certificates it trusts.
//!
//! * [`TlsConfig::webpki_roots`]: the public Web PKI (a server with a certificate from a public
//!   authority, as the official server);
//! * [`TlsConfig::with_root_pem`] / [`TlsConfig::with_root_der`]: only the given certificates,
//!   the way a player pins a community server's self-signed certificate (also what tests and
//!   the benchmark use with a throw-away certificate);
//! * [`TlsConfig::dangerous_accept_any_certificate`]: no certificate check at all, for tests
//!   only. The handshake signatures are still verified, so the connection is encrypted, but any
//!   server can impersonate the real one.
//!
//! The cryptography is rustls with the *ring* provider, TLS 1.2 and 1.3.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

use crate::error::{ClientError, Result};

/// The certificates a client trusts, as a shareable rustls configuration.
#[derive(Clone)]
pub struct TlsConfig {
    config: Arc<ClientConfig>,
    accepts_any: bool,
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConfig")
            .field("accepts_any_certificate", &self.accepts_any)
            .finish_non_exhaustive()
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn builder() -> rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier> {
    ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions")
}

impl TlsConfig {
    /// Trusts the public Web PKI roots (Mozilla's list, compiled in).
    pub fn webpki_roots() -> TlsConfig {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        Self::from_roots(roots)
    }

    /// Trusts only the certificates of a PEM text (a self-signed server certificate, or a private
    /// authority).
    pub fn with_root_pem(pem: &[u8]) -> Result<TlsConfig> {
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|e| ClientError::Tls(format!("invalid PEM certificate: {e}")))?;
            roots.add(cert).map_err(|e| ClientError::Tls(format!("unusable certificate: {e}")))?;
        }
        if roots.is_empty() {
            return Err(ClientError::Tls("no certificate in the PEM text".into()));
        }
        Ok(Self::from_roots(roots))
    }

    /// Trusts only the certificates of a PEM file.
    pub fn with_root_file(path: impl AsRef<Path>) -> Result<TlsConfig> {
        let path = path.as_ref();
        let pem = std::fs::read(path)
            .map_err(|e| ClientError::Tls(format!("cannot read {}: {e}", path.display())))?;
        Self::with_root_pem(&pem)
    }

    /// Trusts only this DER certificate.
    pub fn with_root_der(der: &[u8]) -> Result<TlsConfig> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(der.to_vec()))
            .map_err(|e| ClientError::Tls(format!("unusable certificate: {e}")))?;
        Ok(Self::from_roots(roots))
    }

    /// **Tests only.** Accepts any server certificate, expired, self-signed or issued for another
    /// name. The handshake signatures are still checked against the certificate the server shows,
    /// but nothing ties that certificate to the server: never use it outside a test machine.
    pub fn dangerous_accept_any_certificate() -> TlsConfig {
        let provider = provider();
        let config = builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth();
        TlsConfig { config: Arc::new(config), accepts_any: true }
    }

    /// Uses a rustls configuration built elsewhere.
    pub fn from_rustls(config: Arc<ClientConfig>) -> TlsConfig {
        TlsConfig { config, accepts_any: false }
    }

    /// The same trust without TLS session resumption: every connection makes a full handshake
    /// (what a client connecting for the first time costs the server; the benchmark's default).
    pub fn without_resumption(self) -> TlsConfig {
        let mut config = (*self.config).clone();
        config.resumption = Resumption::disabled();
        TlsConfig { config: Arc::new(config), accepts_any: self.accepts_any }
    }

    /// The rustls configuration.
    pub fn rustls(&self) -> &Arc<ClientConfig> {
        &self.config
    }

    /// Whether this configuration skips the certificate check.
    pub fn accepts_any_certificate(&self) -> bool {
        self.accepts_any
    }

    fn from_roots(roots: RootCertStore) -> TlsConfig {
        let config = builder().with_root_certificates(roots).with_no_client_auth();
        TlsConfig { config: Arc::new(config), accepts_any: false }
    }
}

/// The verifier of [`TlsConfig::dangerous_accept_any_certificate`].
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
