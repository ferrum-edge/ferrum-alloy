//! rustls-based TLS termination with verified client identity handoff.
//!
//! When `client_auth` is `optional` or `required`, client certificates are
//! verified against `client_ca_path`. Only a certificate that rustls
//! verified produces a [`TlsPeer`]; identity is never read from headers.
//! The `ring` provider is passed explicitly; no process-wide crypto
//! provider is installed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ferrum_alloy_telemetry::peer::TlsPeer;
use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::config::{ClientAuth, TlsSettings};

/// TLS configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file could not be read or parsed.
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
    let builder = match (settings.client_auth, &settings.client_ca_path) {
        (ClientAuth::None, _) => builder.with_no_client_auth(),
        (_, None) => {
            return Err(TlsError::Config(
                "client_auth requires client_ca_path".into(),
            ));
        }
        (policy, Some(ca_path)) => {
            let mut roots = RootCertStore::empty();
            for cert in certificates("client CA bundle", ca_path)? {
                roots
                    .add(cert)
                    .map_err(|e| pem_error("client CA bundle", ca_path, e))?;
            }
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider));
            let verifier = if policy == ClientAuth::Optional {
                verifier.allow_unauthenticated()
            } else {
                verifier
            };
            builder.with_client_cert_verifier(
                verifier
                    .build()
                    .map_err(|e| TlsError::Config(e.to_string()))?,
            )
        }
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
