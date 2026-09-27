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
use x509_parser::oid_registry::OID_X509_EXT_ISSUER_DISTRIBUTION_POINT;
use x509_parser::time::ASN1Time;

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
    let mut summaries = Vec::new();
    for path in &settings.client_crl_paths {
        let file = revocation_lists(path)?;
        // The verifier's own CRL parser, run per file so a rejected file is
        // named.
        WebPkiClientVerifier::builder_with_provider(Arc::clone(&roots), Arc::clone(provider))
            .with_crls(file.iter().cloned())
            .build()
            .map_err(|e| pem_error(CRL, path, e))?;
        for crl in &file {
            summaries.push(CrlSummary::parse(path, crl)?);
        }
        crls.extend(file);
    }
    check_one_crl_per_scope(&summaries)?;
    let now = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
    if settings.client_crl_expiration == CrlExpiration::Enforce {
        check_not_expired(&summaries, now)?;
    }
    log_next_update(&summaries, now);
    let mut verifier =
        WebPkiClientVerifier::builder_with_provider(roots, Arc::clone(provider)).with_crls(crls);
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

/// What the startup checks read from one CRL.
struct CrlSummary<'a> {
    path: &'a Path,
    issuer: Vec<u8>,
    distribution_point: Option<Vec<u8>>,
    next_update: Option<ASN1Time>,
}

impl<'a> CrlSummary<'a> {
    fn parse(path: &'a Path, crl: &CertificateRevocationListDer<'_>) -> Result<Self, TlsError> {
        let (_, parsed) = x509_parser::parse_x509_crl(crl.as_ref())
            .map_err(|_| pem_error(CRL, path, "the CRL cannot be parsed"))?;
        let distribution_point = parsed
            .extensions()
            .iter()
            .find(|extension| extension.oid == OID_X509_EXT_ISSUER_DISTRIBUTION_POINT)
            .map(|extension| extension.value.to_vec());
        Ok(Self {
            path,
            issuer: parsed.issuer().as_raw().to_vec(),
            distribution_point,
            next_update: parsed.next_update(),
        })
    }

    /// Whether both CRLs could be the CRL consulted for one certificate.
    fn overlaps(&self, other: &Self) -> bool {
        if self.issuer != other.issuer {
            return false;
        }
        match (&self.distribution_point, &other.distribution_point) {
            (Some(ours), Some(theirs)) => ours == theirs,
            // A CRL without an issuing distribution point covers every
            // certificate from its issuer.
            _ => true,
        }
    }
}

/// Rejects two CRLs that cover the same certificates. The verifier consults
/// only the first CRL that covers a certificate, so a revocation listed only
/// in a later one (a newer CRL added during rotation, say) would be missed.
fn check_one_crl_per_scope(summaries: &[CrlSummary<'_>]) -> Result<(), TlsError> {
    for (index, later) in summaries.iter().enumerate() {
        for earlier in &summaries[..index] {
            if earlier.overlaps(later) {
                return Err(overlap_error(earlier.path, later.path));
            }
        }
    }
    Ok(())
}

fn overlap_error(earlier: &Path, later: &Path) -> TlsError {
    let message = "two CRLs from the same issuer cover the same certificates, and only the \
                   first would be consulted; configure one CRL per issuer, or per issuing \
                   distribution point";
    if earlier == later {
        return pem_error(CRL, later, message);
    }
    TlsError::Config(format!(
        "client CRLs {} and {}: {message}",
        earlier.display(),
        later.display()
    ))
}

/// Rejects a CRL whose `nextUpdate` time is not after `now`, as the
/// verifier would at handshake time.
fn check_not_expired(summaries: &[CrlSummary<'_>], now: i64) -> Result<(), TlsError> {
    for summary in summaries {
        let Some(next_update) = summary.next_update else {
            return Err(pem_error(CRL, summary.path, "the CRL has no nextUpdate time"));
        };
        if next_update.timestamp() <= now {
            return Err(pem_error(
                CRL,
                summary.path,
                "the CRL has expired (its nextUpdate time has passed)",
            ));
        }
    }
    Ok(())
}

/// Warn when the first CRL expires sooner than this.
const CRL_EXPIRY_WARNING_SECS: i64 = 24 * 60 * 60;

/// Logs the earliest CRL `nextUpdate` time, as a warning when it is less
/// than a day away. With expiration enforced, an expired CRL fails every
/// handshake it covers until a fresh CRL is loaded.
fn log_next_update(summaries: &[CrlSummary<'_>], now: i64) {
    let earliest = summaries
        .iter()
        .filter_map(|summary| Some((summary.path, summary.next_update?)))
        .min_by_key(|(_, next_update)| *next_update);
    let Some((path, next_update)) = earliest else {
        return;
    };
    let remaining_secs = next_update.timestamp().saturating_sub(now);
    let path = path.display();
    if remaining_secs < CRL_EXPIRY_WARNING_SECS {
        tracing::warn!(
            target: "ferrum_alloy::tls",
            path = %path,
            next_update = %next_update,
            remaining_secs,
            "a client CRL expires within 24 hours or has expired; publish a fresh CRL and restart"
        );
    } else {
        tracing::info!(
            target: "ferrum_alloy::tls",
            path = %path,
            next_update = %next_update,
            remaining_secs,
            "earliest client CRL nextUpdate"
        );
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;

    use rcgen::{
        BasicConstraints, CertificateParams, CertificateRevocationListParams,
        ExtendedKeyUsagePurpose, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose,
        date_time_ymd,
    };
    use rustls::CertificateError;

    use super::*;

    /// 2021-01-01T00:00:00Z, the `nextUpdate` time of the test CRL.
    const NEXT_UPDATE_SECS: u64 = 1_609_459_200;

    /// A self-signed CA written to `dir` as `ca.pem`, and its issuer.
    fn ca(dir: &Path) -> (PathBuf, Issuer<'static, KeyPair>) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let path = dir.join("ca.pem");
        std::fs::write(&path, cert.pem()).unwrap();
        (path, Issuer::new(params, key))
    }

    fn client_cert(issuer: &Issuer<'static, KeyPair>) -> CertificateDer<'static> {
        let mut params = CertificateParams::default();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let key = KeyPair::generate().unwrap();
        params.signed_by(&key, issuer).unwrap().der().clone()
    }

    /// An empty CRL from `issuer`, written to `dir` as `ca.crl.pem`, that
    /// was current from mid-2020 until [`NEXT_UPDATE_SECS`].
    fn crl(dir: &Path, issuer: &Issuer<'static, KeyPair>) -> PathBuf {
        let params = CertificateRevocationListParams {
            this_update: date_time_ymd(2020, 6, 1),
            next_update: date_time_ymd(2021, 1, 1),
            crl_number: rcgen::SerialNumber::from(1_u64),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
            key_identifier_method: KeyIdMethod::Sha256,
        };
        let path = dir.join("ca.crl.pem");
        let pem = params.signed_by(issuer).unwrap().pem().unwrap();
        std::fs::write(&path, pem).unwrap();
        path
    }

    #[test]
    fn a_crl_that_expires_after_startup_fails_later_handshakes() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let client = client_cert(&issuer);
        let settings = TlsSettings {
            cert_path: PathBuf::new(),
            key_path: PathBuf::new(),
            client_ca_path: Some(ca_path),
            client_auth: ClientAuth::Required,
            handshake_timeout_ms: 2_000,
            client_crl_paths: vec![crl(dir.path(), &issuer)],
            client_crl_depth: CrlDepth::default(),
            client_crl_unknown_status: CrlUnknownStatus::default(),
            client_crl_expiration: CrlExpiration::Enforce,
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // Startup in September 2020, while the CRL is current.
        let startup = UnixTime::since_unix_epoch(Duration::from_secs(1_600_000_000));
        let verifier = client_verifier(&settings, &provider, startup)
            .unwrap()
            .unwrap();
        verifier.verify_client_cert(&client, &[], startup).unwrap();

        let now = UnixTime::now();
        let expired = CertificateError::ExpiredRevocationListContext {
            time: now,
            next_update: UnixTime::since_unix_epoch(Duration::from_secs(NEXT_UPDATE_SECS)),
        };
        assert_eq!(
            verifier.verify_client_cert(&client, &[], now).err(),
            Some(rustls::Error::InvalidCertificate(expired))
        );
    }
}
