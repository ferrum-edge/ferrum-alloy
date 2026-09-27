//! rustls-based TLS termination with verified client identity handoff.
//!
//! When `client_auth` is `optional` or `required`, client certificates are
//! verified against `client_ca_path`, and against the certificate revocation
//! lists in `client_crl_paths` when any are configured. Only a certificate
//! that rustls verified produces a [`TlsPeer`]; identity is never read from
//! headers. The `ring` provider is passed explicitly; no process-wide crypto
//! provider is installed.
//!
//! Every `reload_interval_ms`, the certificate chain, private key, client CA
//! bundle, and CRLs are read again. When their bytes changed, they are
//! validated exactly as at startup and swapped in as a whole new server
//! configuration, which new handshakes use. Established connections keep the
//! session they negotiated. Material that fails validation is never swapped
//! in: the previous material keeps serving, and the failure is logged and
//! counted. The client authentication policy comes from the settings, which
//! a reload never changes, so a reload cannot turn client authentication
//! off.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use ferrum_alloy_telemetry::peer::TlsPeer;
use rustls::RootCertStore;
use rustls::crypto::CryptoProvider;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;
use rustls_pki_types::pem::{self, PemObject};
use rustls_pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, UnixTime};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use x509_parser::time::ASN1Time;

use crate::config::{ClientAuth, CrlDepth, CrlExpiration, CrlUnknownStatus, TlsSettings};
use crate::server::ServerStats;

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

/// A ready TLS acceptor whose material can be reloaded.
#[derive(Clone)]
pub struct TlsServer {
    material: Arc<Material>,
    pub(crate) handshake_timeout: Duration,
    reload_interval: Option<Duration>,
}

impl std::fmt::Debug for TlsServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsServer")
            .field("handshake_timeout", &self.handshake_timeout)
            .field("reload_interval", &self.reload_interval)
            .finish_non_exhaustive()
    }
}

/// Where the material comes from, and the material new handshakes use.
struct Material {
    settings: TlsSettings,
    provider: Arc<CryptoProvider>,
    current: RwLock<Current>,
}

struct Current {
    /// A whole server configuration, so a certificate, its key, and the
    /// client verifier always change together. A new configuration also
    /// starts with an empty session cache, so no session resumed after a
    /// reload skips the new client verifier.
    config: Arc<rustls::ServerConfig>,
    /// [`Sources::fingerprint`] of the files `config` was built from.
    fingerprint: u64,
}

/// What a successful reload did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reloaded {
    /// The files changed and the new material was swapped in.
    Swapped,
    /// The files hold the material already serving.
    Unchanged,
}

impl TlsServer {
    /// An acceptor for one new connection, with the current material.
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        let current = self
            .material
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        TlsAcceptor::from(Arc::clone(&current.config))
    }

    /// Reads every file again and, when the bytes changed and validate,
    /// swaps the new material in. On error nothing changes.
    fn reload(&self, now: UnixTime) -> Result<Reloaded, TlsError> {
        let material = &self.material;
        let sources = Sources::read(&material.settings)?;
        let fingerprint = sources.fingerprint();
        let serving = material
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .fingerprint;
        if fingerprint == serving {
            return Ok(Reloaded::Unchanged);
        }
        let config = build(&material.settings, &sources, &material.provider, now)?;
        let mut current = material
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *current = Current {
            config,
            fingerprint,
        };
        Ok(Reloaded::Swapped)
    }
}

/// Reloads the material of `tls` every reload interval until
/// `stop_accepting` is cancelled, counting reloads in `stats`. Returns at
/// once when reloading is disabled.
pub(crate) async fn reload_until(
    tls: TlsServer,
    stats: Arc<ServerStats>,
    stop_accepting: CancellationToken,
) {
    let Some(interval) = tls.reload_interval else {
        return;
    };
    loop {
        tokio::select! {
            () = stop_accepting.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        let server = tls.clone();
        // Reading files and parsing certificates block.
        let reload = tokio::task::spawn_blocking(move || server.reload(UnixTime::now()));
        match reload.await {
            Ok(Ok(Reloaded::Swapped)) => {
                stats.tls_reloads.fetch_add(1, Ordering::Relaxed);
                tracing::info!(target: "ferrum_alloy::tls", "TLS material reloaded; new handshakes use it");
            }
            Ok(Ok(Reloaded::Unchanged)) => {}
            Ok(Err(error)) => {
                stats.tls_reload_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(target: "ferrum_alloy::tls", %error, "TLS reload failed; the previous material keeps serving");
            }
            Err(error) => {
                stats.tls_reload_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(target: "ferrum_alloy::tls", %error, "TLS reload did not finish; the previous material keeps serving");
            }
        }
    }
}

fn pem_error(what: &'static str, path: &Path, message: impl ToString) -> TlsError {
    TlsError::Pem {
        what,
        path: path.display().to_string(),
        message: message.to_string(),
    }
}

/// Why a PEM file was rejected, without the parts of a PEM error that quote
/// the file (a line, or a byte the base64 decoder refused).
fn pem_reason(error: &pem::Error) -> &'static str {
    match error {
        pem::Error::NoItemsFound => "no PEM section found",
        pem::Error::SectionTooLarge => "a PEM section is too large",
        _ => "not a valid PEM file",
    }
}

const CHAIN: &str = "certificate chain";
const KEY: &str = "private key";
const CLIENT_CA: &str = "client CA bundle";

/// The bytes of every file the settings name, each read once. A reload
/// compares and parses the same bytes, so the material it swaps in is the
/// material it compared.
pub(crate) struct Sources<'a> {
    cert: Vec<u8>,
    key: Vec<u8>,
    client_ca: Option<Vec<u8>>,
    crls: Vec<(&'a Path, Vec<u8>)>,
}

impl<'a> Sources<'a> {
    pub(crate) fn read(settings: &'a TlsSettings) -> Result<Self, TlsError> {
        let cert = read_file(CHAIN, &settings.cert_path)?;
        let key = read_file(KEY, &settings.key_path)?;
        let client_auth = settings.client_auth != ClientAuth::None;
        let client_ca = match &settings.client_ca_path {
            Some(path) if client_auth => Some(read_file(CLIENT_CA, path)?),
            _ => None,
        };
        let mut crls = Vec::new();
        if client_auth {
            for path in &settings.client_crl_paths {
                crls.push((path.as_path(), read_file(CRL, path)?));
            }
        }
        Ok(Self {
            cert,
            key,
            client_ca,
            crls,
        })
    }

    /// Changes whenever any file's bytes change. Keeps no copy of the key.
    fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.cert.hash(&mut hasher);
        self.key.hash(&mut hasher);
        self.client_ca.hash(&mut hasher);
        for (_, crl) in &self.crls {
            crl.hash(&mut hasher);
        }
        hasher.finish()
    }
}

fn read_file(what: &'static str, path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|e| pem_error(what, path, e))
}

fn certificates(
    what: &'static str,
    path: &Path,
    bytes: &[u8],
) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs = CertificateDer::pem_slice_iter(bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_error(what, path, pem_reason(&e)))?;
    if certs.is_empty() {
        return Err(pem_error(what, path, "no certificates found"));
    }
    Ok(certs)
}

/// Loads certificates, key, and client verification policy.
pub fn load(settings: &TlsSettings) -> Result<TlsServer, TlsError> {
    load_at(settings, UnixTime::now())
}

/// [`load`], with `now` deciding whether a CRL has expired.
fn load_at(settings: &TlsSettings, now: UnixTime) -> Result<TlsServer, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let sources = Sources::read(settings)?;
    let config = build(settings, &sources, &provider, now)?;
    let current = Current {
        config,
        fingerprint: sources.fingerprint(),
    };
    let reload_interval = match settings.reload_interval_ms {
        0 => None,
        ms => Some(Duration::from_millis(ms)),
    };
    Ok(TlsServer {
        material: Arc::new(Material {
            settings: settings.clone(),
            provider,
            current: RwLock::new(current),
        }),
        handshake_timeout: Duration::from_millis(settings.handshake_timeout_ms),
        reload_interval,
    })
}

/// Parses and validates `sources` into a server configuration: every
/// certificate of the chain parses, the key matches the leaf, and the client
/// CA bundle and CRLs pass the checks of [`client_verifier`].
fn build(
    settings: &TlsSettings,
    sources: &Sources<'_>,
    provider: &Arc<CryptoProvider>,
    now: UnixTime,
) -> Result<Arc<rustls::ServerConfig>, TlsError> {
    let chain = certificates(CHAIN, &settings.cert_path, &sources.cert)?;
    for (index, cert) in chain.iter().enumerate() {
        if x509_parser::parse_x509_certificate(cert.as_ref()).is_err() {
            let message = format!("certificate {} cannot be parsed", index + 1);
            return Err(pem_error(CHAIN, &settings.cert_path, message));
        }
    }
    let key = PrivateKeyDer::from_pem_slice(&sources.key)
        .map_err(|e| pem_error(KEY, &settings.key_path, pem_reason(&e)))?;
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Config(e.to_string()))?;
    // `None` only when the settings, which a reload never changes, ask for
    // no client authentication.
    let builder = match client_verifier(settings, sources, provider, now)? {
        Some(verifier) => builder.with_client_cert_verifier(verifier),
        None => builder.with_no_client_auth(),
    };
    // Fails when the key does not match the leaf certificate.
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| chain_and_key_error(settings, e))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Why rustls refused the certificate chain and key, such as a key that
/// does not match the leaf certificate.
fn chain_and_key_error(settings: &TlsSettings, error: rustls::Error) -> TlsError {
    TlsError::Config(format!(
        "certificate chain {} and private key {}: {error}",
        settings.cert_path.display(),
        settings.key_path.display()
    ))
}

/// Builds the client certificate verifier that `settings` describe from the
/// files in `sources`, or `None` without client authentication. `now`
/// decides whether a CRL has expired.
pub(crate) fn client_verifier(
    settings: &TlsSettings,
    sources: &Sources<'_>,
    provider: &Arc<CryptoProvider>,
    now: UnixTime,
) -> Result<Option<Arc<dyn ClientCertVerifier>>, TlsError> {
    if settings.client_auth == ClientAuth::None {
        return Ok(None);
    }
    let (Some(ca_path), Some(ca)) = (&settings.client_ca_path, &sources.client_ca) else {
        return Err(TlsError::Config(
            "client_auth requires client_ca_path".into(),
        ));
    };
    let mut roots = RootCertStore::empty();
    for cert in certificates(CLIENT_CA, ca_path, ca)? {
        roots
            .add(cert)
            .map_err(|e| pem_error(CLIENT_CA, ca_path, e))?;
    }
    let roots = Arc::new(roots);
    let mut crls = Vec::new();
    let mut summaries = Vec::new();
    for (path, bytes) in &sources.crls {
        let file = revocation_lists(path, bytes)?;
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
    check_one_crl_per_issuer(&summaries)?;
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

/// Parses the revocation lists in the bytes of one PEM or DER file. Errors
/// name the file but never quote its contents.
fn revocation_lists(
    path: &Path,
    bytes: &[u8],
) -> Result<Vec<CertificateRevocationListDer<'static>>, TlsError> {
    // A DER CRL is an ASN.1 SEQUENCE; anything else is read as PEM.
    let crls = if bytes.first() == Some(&0x30) {
        vec![CertificateRevocationListDer::from(bytes.to_vec())]
    } else {
        // PEM errors can quote lines of the file, so none of them is kept.
        CertificateRevocationListDer::pem_slice_iter(bytes)
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
    next_update: Option<ASN1Time>,
}

impl<'a> CrlSummary<'a> {
    fn parse(path: &'a Path, crl: &CertificateRevocationListDer<'_>) -> Result<Self, TlsError> {
        let (_, parsed) = x509_parser::parse_x509_crl(crl.as_ref())
            .map_err(|_| pem_error(CRL, path, "the CRL cannot be parsed"))?;
        Ok(Self {
            path,
            issuer: parsed.issuer().as_raw().to_vec(),
            next_update: parsed.next_update(),
        })
    }
}

/// Rejects two CRLs from the same issuer. The verifier consults only the
/// first CRL whose issuer matches a certificate, so a revocation listed only
/// in a later one (a newer CRL added during rotation, or another partition)
/// would be missed. Partitioned CRLs are refused too: a partition also
/// covers certificates without a CRL distribution point, and partitions
/// can overlap in ways their issuing distribution points do not show.
fn check_one_crl_per_issuer(summaries: &[CrlSummary<'_>]) -> Result<(), TlsError> {
    for (index, later) in summaries.iter().enumerate() {
        for earlier in &summaries[..index] {
            if earlier.issuer == later.issuer {
                return Err(same_issuer_error(earlier.path, later.path));
            }
        }
    }
    Ok(())
}

fn same_issuer_error(earlier: &Path, later: &Path) -> TlsError {
    let message = "two CRLs have the same issuer, and only the first would be consulted; \
                   provide exactly one full CRL per issuing CA (combine partitioned CRLs \
                   into one)";
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
            return Err(pem_error(
                CRL,
                summary.path,
                "the CRL has no nextUpdate time",
            ));
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
            "a client CRL expires within 24 hours or has expired; publish a fresh CRL, which the next TLS reload (or, with reloading disabled, a restart) loads"
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
    use rustls_pki_types::PrivatePkcs8KeyDer;

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
        let path = dir.join("ca.crl.pem");
        std::fs::write(&path, crl_pem(issuer, 2021)).unwrap();
        path
    }

    /// An empty CRL from `issuer`, current from mid-2020 until the start of
    /// `next_update_year`.
    fn crl_pem(issuer: &Issuer<'static, KeyPair>, next_update_year: i32) -> String {
        let params = CertificateRevocationListParams {
            this_update: date_time_ymd(2020, 6, 1),
            next_update: date_time_ymd(next_update_year, 1, 1),
            crl_number: rcgen::SerialNumber::from(1_u64),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
            key_identifier_method: KeyIdMethod::Sha256,
        };
        params.signed_by(issuer).unwrap().pem().unwrap()
    }

    #[test]
    fn a_crl_that_expires_after_startup_fails_later_handshakes() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let client = client_cert(&issuer);
        // The verifier never reads the server certificate or key.
        let settings = TlsSettings {
            cert_path: ca_path.clone(),
            key_path: ca_path.clone(),
            client_ca_path: Some(ca_path),
            client_auth: ClientAuth::Required,
            handshake_timeout_ms: 2_000,
            client_crl_paths: vec![crl(dir.path(), &issuer)],
            client_crl_depth: CrlDepth::default(),
            client_crl_unknown_status: CrlUnknownStatus::default(),
            client_crl_expiration: CrlExpiration::Enforce,
            reload_interval_ms: 0,
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // Startup in September 2020, while the CRL is current.
        let startup = UnixTime::since_unix_epoch(Duration::from_secs(1_600_000_000));
        let sources = Sources::read(&settings).unwrap();
        let verifier = client_verifier(&settings, &sources, &provider, startup)
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

    /// A server certificate and key issued by `issuer`, as PEM.
    fn server_pem(issuer: &Issuer<'static, KeyPair>) -> (String, String) {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, issuer).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn serving(server: &TlsServer) -> Arc<rustls::ServerConfig> {
        Arc::clone(&server.material.current.read().unwrap().config)
    }

    #[test]
    fn a_reload_swaps_only_changed_material_that_validates() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let (cert, key) = server_pem(&issuer);
        let cert_path = dir.path().join("server.pem");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, &cert).unwrap();
        std::fs::write(&key_path, &key).unwrap();
        let settings = TlsSettings {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
            client_ca_path: Some(ca_path),
            client_auth: ClientAuth::Required,
            handshake_timeout_ms: 2_000,
            client_crl_paths: Vec::new(),
            client_crl_depth: CrlDepth::default(),
            client_crl_unknown_status: CrlUnknownStatus::default(),
            client_crl_expiration: CrlExpiration::default(),
            reload_interval_ms: 1_000,
        };
        let server = load(&settings).unwrap();
        assert_eq!(server.reload_interval, Some(Duration::from_secs(1)));
        let initial = serving(&server);

        let now = UnixTime::now();
        assert_eq!(server.reload(now).unwrap(), Reloaded::Unchanged);
        assert!(Arc::ptr_eq(&initial, &serving(&server)));

        // A key that does not match the certificate is refused.
        let (_, other_key) = server_pem(&issuer);
        std::fs::write(&key_path, &other_key).unwrap();
        let error = server.reload(now).unwrap_err().to_string();
        assert!(error.contains("private key"), "{error}");
        assert!(!error.contains("PRIVATE KEY"), "the key was quoted: {error}");
        assert!(Arc::ptr_eq(&initial, &serving(&server)));

        let (new_cert, new_key) = server_pem(&issuer);
        std::fs::write(&cert_path, &new_cert).unwrap();
        std::fs::write(&key_path, &new_key).unwrap();
        assert_eq!(server.reload(now).unwrap(), Reloaded::Swapped);
        assert!(!Arc::ptr_eq(&initial, &serving(&server)));
        assert_eq!(server.reload(now).unwrap(), Reloaded::Unchanged);
    }

    /// A client configuration that trusts the CA at `ca_path` and presents
    /// a new certificate from `issuer`.
    fn client_config(
        ca_path: &Path,
        issuer: &Issuer<'static, KeyPair>,
    ) -> Arc<rustls::ClientConfig> {
        let mut params = CertificateParams::default();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, issuer).unwrap().der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut roots = RootCertStore::empty();
        let ca = CertificateDer::from_pem_file(ca_path).unwrap();
        roots.add(ca).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![cert], key)
            .unwrap();
        Arc::new(config)
    }

    /// Runs a handshake between `client` and `server` in memory. The server
    /// verifies the client certificate at the current time.
    fn handshake(
        server: Arc<rustls::ServerConfig>,
        client: Arc<rustls::ClientConfig>,
    ) -> Result<(), rustls::Error> {
        let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
        let mut client = rustls::ClientConnection::new(client, name)?;
        let mut server = rustls::ServerConnection::new(server)?;
        for _ in 0..8 {
            let mut flight = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut flight).unwrap();
            }
            let mut flight = flight.as_slice();
            while !flight.is_empty() {
                server.read_tls(&mut flight).unwrap();
                server.process_new_packets()?;
            }
            let mut flight = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut flight).unwrap();
            }
            let mut flight = flight.as_slice();
            while !flight.is_empty() {
                client.read_tls(&mut flight).unwrap();
                client.process_new_packets()?;
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
        }
        Err(rustls::Error::General("the handshake did not finish".into()))
    }

    #[test]
    fn an_expired_crl_is_replaced_at_the_next_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let (cert, key) = server_pem(&issuer);
        let cert_path = dir.path().join("server.pem");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, cert).unwrap();
        std::fs::write(&key_path, key).unwrap();
        let crl_path = crl(dir.path(), &issuer);
        let settings = TlsSettings {
            cert_path,
            key_path,
            client_ca_path: Some(ca_path.clone()),
            client_auth: ClientAuth::Required,
            handshake_timeout_ms: 2_000,
            client_crl_paths: vec![crl_path.clone()],
            client_crl_depth: CrlDepth::default(),
            client_crl_unknown_status: CrlUnknownStatus::default(),
            client_crl_expiration: CrlExpiration::Enforce,
            reload_interval_ms: 1_000,
        };
        let client = client_config(&ca_path, &issuer);
        // Loaded in September 2020, while the CRL was current. It has
        // expired since, so handshakes fail.
        let startup = UnixTime::since_unix_epoch(Duration::from_secs(1_600_000_000));
        let server = load_at(&settings, startup).unwrap();
        let refused = handshake(serving(&server), Arc::clone(&client));
        let refused = refused.unwrap_err().to_string();
        assert!(refused.contains("revocation list expired"), "{refused}");

        let now = UnixTime::now();
        assert_eq!(server.reload(now).unwrap(), Reloaded::Unchanged);
        // Another expired CRL is refused, and the old one keeps serving.
        std::fs::write(&crl_path, crl_pem(&issuer, 2022)).unwrap();
        let error = server.reload(now).unwrap_err().to_string();
        assert!(error.contains("expired"), "{error}");

        std::fs::write(&crl_path, crl_pem(&issuer, 2100)).unwrap();
        assert_eq!(server.reload(now).unwrap(), Reloaded::Swapped);
        handshake(serving(&server), client).unwrap();
    }
}
