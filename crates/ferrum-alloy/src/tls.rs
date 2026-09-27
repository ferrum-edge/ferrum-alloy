//! rustls-based TLS termination with verified client identity handoff.
//!
//! When `client_auth` is `optional` or `required`, client certificates are
//! verified against `client_ca_path`, and against the certificate revocation
//! lists in `client_crl_paths` when any are configured. Only a certificate
//! that rustls verified produces a [`TlsPeer`]; identity is never read from
//! headers. The `ring` provider is passed explicitly; no process-wide crypto
//! provider is installed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ferrum_alloy_telemetry::peer::TlsPeer;
use rustls::RootCertStore;
use rustls::crypto::CryptoProvider;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, UnixTime};
use tokio_rustls::TlsAcceptor;

use crate::config::{ClientAuth, CrlDepth, CrlExpiration, CrlUnknownStatus, TlsSettings};

/// TLS configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A certificate, key, or CRL file could not be read or parsed.
    #[error("{what} {path}: {message}")]
    Pem {
        /// Which file.
        what: &'static str,
        /// Path.
        path: String,
        /// Reason.
        message: String,
    },
    /// rustls rejected the configuration.
    #[error("TLS configuration rejected: {0}")]
    Config(String),
}

/// A ready TLS acceptor.
#[derive(Clone)]
pub struct TlsServer {
    pub(crate) acceptor: TlsAcceptor,
    pub(crate) handshake_timeout: Duration,
}

impl std::fmt::Debug for TlsServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsServer")
            .field("handshake_timeout", &self.handshake_timeout)
            .finish_non_exhaustive()
    }
}

fn pem_error(what: &'static str, path: &Path, message: impl ToString) -> TlsError {
    TlsError::Pem {
        what,
        path: path.display().to_string(),
        message: message.to_string(),
    }
}

fn certificates(what: &'static str, path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| pem_error(what, path, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_error(what, path, e))?;
    if certs.is_empty() {
        return Err(pem_error(what, path, "no certificates found"));
    }
    Ok(certs)
}

/// Loads certificates, key, and client verification policy.
pub fn load(settings: &TlsSettings) -> Result<TlsServer, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let chain = certificates("certificate chain", &settings.cert_path)?;
    let key = PrivateKeyDer::from_pem_file(&settings.key_path)
        .map_err(|e| pem_error("private key", &settings.key_path, e))?;
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Config(e.to_string()))?;
    let builder = match client_verifier(settings, &provider, UnixTime::now())? {
        Some(verifier) => builder.with_client_cert_verifier(verifier),
        None => builder.with_no_client_auth(),
    };
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| TlsError::Config(e.to_string()))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsServer {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        handshake_timeout: Duration::from_millis(settings.handshake_timeout_ms),
    })
}

/// Builds the client certificate verifier that `settings` describe, or
/// `None` without client authentication. Every file is read on each call, so
/// a reload can build a replacement verifier from the same settings. `now`
/// decides whether a CRL has expired.
pub(crate) fn client_verifier(
    settings: &TlsSettings,
    provider: &Arc<CryptoProvider>,
    now: UnixTime,
) -> Result<Option<Arc<dyn ClientCertVerifier>>, TlsError> {
    let ca_path = match (settings.client_auth, &settings.client_ca_path) {
        (ClientAuth::None, _) => return Ok(None),
        (_, None) => {
            return Err(TlsError::Config(
                "client_auth requires client_ca_path".into(),
            ));
        }
        (_, Some(ca_path)) => ca_path,
    };
    let mut roots = RootCertStore::empty();
    for cert in certificates("client CA bundle", ca_path)? {
        roots
            .add(cert)
            .map_err(|e| pem_error("client CA bundle", ca_path, e))?;
    }
    let roots = Arc::new(roots);
    let mut crls = Vec::new();
    for path in &settings.client_crl_paths {
        let file = revocation_lists(path)?;
        // The verifier's own CRL parser, run per file so a rejected file is
        // named.
        WebPkiClientVerifier::builder_with_provider(Arc::clone(&roots), Arc::clone(provider))
            .with_crls(file.iter().cloned())
            .build()
            .map_err(|e| pem_error(CRL, path, e))?;
        if settings.client_crl_expiration == CrlExpiration::Enforce {
            check_not_expired(path, &file, now)?;
        }
        crls.extend(file);
    }
    let mut verifier = WebPkiClientVerifier::builder_with_provider(roots, Arc::clone(provider))
        .with_crls(crls);
    if settings.client_auth == ClientAuth::Optional {
        verifier = verifier.allow_unauthenticated();
    }
    if settings.client_crl_depth == CrlDepth::EndEntity {
        verifier = verifier.only_check_end_entity_revocation();
    }
    if settings.client_crl_unknown_status == CrlUnknownStatus::Allow {
        verifier = verifier.allow_unknown_revocation_status();
    }
    if settings.client_crl_expiration == CrlExpiration::Enforce {
        verifier = verifier.enforce_revocation_expiration();
    }
    verifier
        .build()
        .map(Some)
        .map_err(|e| TlsError::Config(e.to_string()))
}

const CRL: &str = "client CRL";

/// Reads the revocation lists in one PEM or DER file. Errors name the file
/// but never quote its contents.
fn revocation_lists(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|e| pem_error(CRL, path, e))?;
    // A DER CRL is an ASN.1 SEQUENCE; anything else is read as PEM.
    let crls = if bytes.first() == Some(&0x30) {
        vec![CertificateRevocationListDer::from(bytes)]
    } else {
        // PEM errors can quote lines of the file, so none of them is kept.
        CertificateRevocationListDer::pem_slice_iter(&bytes)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| pem_error(CRL, path, "not a valid PEM file"))?
    };
    if crls.is_empty() {
        return Err(pem_error(CRL, path, "no CRLs found"));
    }
    Ok(crls)
}

/// Rejects a CRL whose `nextUpdate` time is not after `now`, as the
/// verifier would at handshake time.
fn check_not_expired(
    path: &Path,
    crls: &[CertificateRevocationListDer<'static>],
    now: UnixTime,
) -> Result<(), TlsError> {
    let now = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
    for crl in crls {
        let next_update = x509_parser::parse_x509_crl(crl.as_ref())
            .ok()
            .and_then(|(_, crl)| crl.next_update())
            .ok_or_else(|| pem_error(CRL, path, "the CRL's nextUpdate time cannot be read"))?;
        if next_update.timestamp() <= now {
            return Err(pem_error(
                CRL,
                path,
                "the CRL has expired (its nextUpdate time has passed)",
            ));
        }
    }
    Ok(())
}

/// Identity of the verified client certificate on a finished handshake.
pub(crate) fn peer_identity(connection: &rustls::ServerConnection) -> Option<TlsPeer> {
    // With a client verifier configured, rustls only completes the handshake
    // with a verified chain (or none, for optional client auth).
    let leaf = connection.peer_certificates()?.first()?;
    match TlsPeer::from_verified_leaf(leaf.as_ref()) {
        Ok(peer) => Some(peer),
        Err(error) => {
            tracing::warn!(target: "ferrum_alloy::tls", %error, "verified client certificate could not be parsed; treating peer as unidentified");
            None
        }
    }
}
