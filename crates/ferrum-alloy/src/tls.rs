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
//! bundle, and CRLs are read again. When their bytes changed, they are read
//! once more after a short settle delay, so that a file still being written
//! is not used, and only when both reads agree are they validated exactly as
//! at startup and swapped in as a whole new server configuration, which new
//! handshakes use. Established connections keep the session they negotiated.
//! Material that fails validation is never swapped in: the previous material
//! keeps serving, and the failure is logged and counted once the same files
//! fail twice in a row. The client authentication policy comes from the
//! settings, which a reload never changes, so a reload cannot turn client
//! authentication off.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

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
use zeroize::Zeroizing;

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
    /// The `notAfter` time of the leaf certificate, in Unix seconds.
    cert_not_after: Option<i64>,
    /// The client CRL that expires first, or `None` without CRLs or when no
    /// CRL has a `nextUpdate` time.
    earliest_crl: Option<EarliestCrl>,
}

/// The client CRL with the earliest `nextUpdate` time.
#[derive(Debug, Clone)]
struct EarliestCrl {
    path: PathBuf,
    next_update: ASN1Time,
}

/// When the serving material stops being valid, exported as gauges. Unknown
/// times are left out, never exported as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Expiry {
    /// The `notAfter` time of the serving leaf certificate, in Unix seconds.
    cert_not_after: Option<i64>,
    /// The earliest `nextUpdate` time of the serving client CRLs, in Unix
    /// seconds.
    crl_next_update: Option<i64>,
}

impl Expiry {
    /// Prometheus text for the known times.
    pub(crate) fn render_prometheus(&self, listener: &str) -> String {
        let mut out = String::new();
        for (name, help, value) in [
            (
                "ferrum_alloy_tls_server_cert_not_after_timestamp_seconds",
                "notAfter time of the serving TLS certificate, in Unix seconds.",
                self.cert_not_after,
            ),
            (
                "ferrum_alloy_tls_client_crl_next_update_timestamp_seconds",
                "Earliest nextUpdate time of the serving client CRLs, in Unix seconds.",
                self.crl_next_update,
            ),
        ] {
            let Some(value) = value else {
                continue;
            };
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
            out.push_str(&format!("{name}{{listener=\"{listener}\"}} {value}\n"));
        }
        out
    }
}

/// What a second read of changed files found.
#[derive(Debug)]
enum Settled {
    /// The files still had the fingerprint of the first read, and their
    /// material was swapped in.
    Swapped,
    /// The files changed again since the first read.
    Changing,
    /// The files still had the fingerprint of the first read, and their
    /// material failed validation.
    Invalid(TlsError),
}

impl TlsServer {
    /// An acceptor for one new connection, with the current material.
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(Arc::clone(&self.current().config))
    }

    fn current(&self) -> RwLockReadGuard<'_, Current> {
        self.material
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// When the serving material stops being valid.
    pub(crate) fn expiry(&self) -> Expiry {
        let current = self.current();
        Expiry {
            cert_not_after: current.cert_not_after,
            crl_next_update: current
                .earliest_crl
                .as_ref()
                .map(|crl| crl.next_update.timestamp()),
        }
    }

    /// Reads every file, and returns their fingerprint when it is not the
    /// one serving.
    fn changed(&self) -> Result<Option<u64>, TlsError> {
        let fingerprint = Sources::read(&self.material.settings)?.fingerprint();
        let serving = self.current().fingerprint;
        Ok((fingerprint != serving).then_some(fingerprint))
    }

    /// Reads every file again and, when they still have `fingerprint` and
    /// validate, swaps their material in. Fails only when a file cannot be
    /// read; on any error nothing changes.
    fn swap_if_settled(&self, fingerprint: u64, now: UnixTime) -> Result<Settled, TlsError> {
        let material = &self.material;
        let sources = Sources::read(&material.settings)?;
        if sources.fingerprint() != fingerprint {
            return Ok(Settled::Changing);
        }
        let current = match build(&material.settings, &sources, &material.provider, now) {
            Ok(current) => current,
            Err(error) => return Ok(Settled::Invalid(error)),
        };
        let mut serving = material
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *serving = current;
        Ok(Settled::Swapped)
    }

    /// Logs the earliest CRL `nextUpdate` time of the serving material, as
    /// [`log_next_update`] describes. Returns whether it warned.
    fn log_crl_next_update(&self, now: UnixTime, only_warn: bool) -> bool {
        let now = unix_secs(now);
        match &self.current().earliest_crl {
            Some(crl) => log_next_update(crl, now, only_warn),
            None => false,
        }
    }
}

/// Changed files are read a second time after the reload interval, or after
/// this delay when the interval is longer.
const MAX_SETTLE_DELAY: Duration = Duration::from_secs(1);

/// While the material is unchanged, a CRL that expires within a day is
/// warned about again this often.
const CRL_WARNING_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// What one reload attempt did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Changed files that validated were swapped in.
    Swapped,
    /// The files hold the material already serving.
    Unchanged,
    /// The files changed between the two reads; the next attempt retries.
    Changing,
    /// The files could not be read or failed validation, but the previous
    /// attempt did not fail the same way, so a writer still replacing them
    /// may explain it. Not counted; the next attempt retries.
    Unconfirmed(String),
    /// The files failed the same way as at the previous attempt. Counted.
    Failed(String),
}

/// What failed at a reload attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Attempt {
    /// A file could not be read, with this error.
    Read(String),
    /// The files with this fingerprint failed validation.
    Validate(u64),
}

/// The state that reload attempts carry from one to the next.
struct Reloader {
    tls: TlsServer,
    /// The delay between the two reads of changed files.
    settle: Duration,
    /// What failed at the previous attempt, if it failed.
    last_failure: Option<Attempt>,
    /// When the CRL expiry was last logged.
    crl_logged_at: Instant,
}

impl Reloader {
    fn new(tls: TlsServer, settle: Duration) -> Self {
        Self {
            tls,
            settle,
            last_failure: None,
            // Loading the material logged it.
            crl_logged_at: Instant::now(),
        }
    }

    /// Reads the files and, when they changed, settled, and validate, swaps
    /// their material in.
    async fn reload(&mut self, now: UnixTime) -> Outcome {
        let outcome = self.attempt(now).await;
        match outcome {
            Outcome::Swapped => {
                self.tls.log_crl_next_update(now, false);
                self.crl_logged_at = Instant::now();
            }
            _ if self.crl_logged_at.elapsed() >= CRL_WARNING_INTERVAL
                && self.tls.log_crl_next_update(now, true) =>
            {
                self.crl_logged_at = Instant::now();
            }
            _ => {}
        }
        outcome
    }

    async fn attempt(&mut self, now: UnixTime) -> Outcome {
        let tls = self.tls.clone();
        // Reading files and parsing certificates block.
        let fingerprint = match tokio::task::spawn_blocking(move || tls.changed()).await {
            Ok(Ok(Some(fingerprint))) => fingerprint,
            Ok(Ok(None)) => {
                self.last_failure = None;
                return Outcome::Unchanged;
            }
            Ok(Err(error)) => return self.failed(Attempt::Read(error.to_string()), error),
            Err(error) => return Outcome::Failed(format!("the reload did not finish: {error}")),
        };
        // A writer may not be done: a certificate can be read before its
        // key is replaced, and a truncated CA bundle or CRL file can still
        // parse, with trust anchors or CRLs missing. Only files that read
        // the same twice, a settle delay apart, are validated.
        tokio::time::sleep(self.settle).await;
        let tls = self.tls.clone();
        let settled = tokio::task::spawn_blocking(move || tls.swap_if_settled(fingerprint, now));
        match settled.await {
            Ok(Ok(Settled::Swapped)) => {
                self.last_failure = None;
                Outcome::Swapped
            }
            Ok(Ok(Settled::Changing)) => Outcome::Changing,
            Ok(Ok(Settled::Invalid(error))) => self.failed(Attempt::Validate(fingerprint), error),
            Ok(Err(error)) => self.failed(Attempt::Read(error.to_string()), error),
            Err(error) => Outcome::Failed(format!("the reload did not finish: {error}")),
        }
    }

    /// A failure is counted only when the previous attempt failed the same
    /// way, so files caught halfway through a replacement are not counted.
    fn failed(&mut self, attempt: Attempt, error: TlsError) -> Outcome {
        let repeated = self.last_failure.as_ref() == Some(&attempt);
        self.last_failure = Some(attempt);
        if repeated {
            Outcome::Failed(error.to_string())
        } else {
            Outcome::Unconfirmed(error.to_string())
        }
    }
}

/// Exports when the serving material of `tls` stops being valid.
fn export_expiry(tls: &TlsServer, stats: &ServerStats) {
    let mut expiry = stats
        .tls_expiry
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    *expiry = tls.expiry();
}

/// Reloads the material of `tls` every reload interval until
/// `stop_accepting` is cancelled, counting reloads in `stats`. Returns at
/// once when reloading is disabled.
pub(crate) async fn reload_until(
    tls: TlsServer,
    stats: Arc<ServerStats>,
    stop_accepting: CancellationToken,
) {
    export_expiry(&tls, &stats);
    let Some(interval) = tls.reload_interval else {
        return;
    };
    let mut reloader = Reloader::new(tls, interval.min(MAX_SETTLE_DELAY));
    // The error last logged at error level. A failure that persists is
    // counted at every interval, but logged at error level only when it
    // starts or its error changes.
    let mut logged: Option<String> = None;
    loop {
        tokio::select! {
            () = stop_accepting.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        match reloader.reload(UnixTime::now()).await {
            Outcome::Swapped => {
                logged = None;
                stats.tls_reloads.fetch_add(1, Ordering::Relaxed);
                export_expiry(&reloader.tls, &stats);
                tracing::info!(target: "ferrum_alloy::tls", "TLS material reloaded; new handshakes use it");
            }
            Outcome::Unchanged => logged = None,
            Outcome::Changing => {
                tracing::debug!(target: "ferrum_alloy::tls", "TLS files changed while being read; retrying at the next reload");
            }
            Outcome::Unconfirmed(error) => {
                tracing::debug!(target: "ferrum_alloy::tls", %error, "TLS reload failed; retrying at the next reload before counting it, in case the files are still being written");
            }
            Outcome::Failed(error) => {
                stats.tls_reload_failures.fetch_add(1, Ordering::Relaxed);
                if logged.as_ref() == Some(&error) {
                    tracing::debug!(target: "ferrum_alloy::tls", %error, "TLS reload failed again; the previous material keeps serving");
                } else {
                    tracing::error!(target: "ferrum_alloy::tls", %error, "TLS reload failed; the previous material keeps serving");
                    logged = Some(error);
                }
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
    /// Zeroed when dropped.
    key: Zeroizing<Vec<u8>>,
    client_ca: Option<Vec<u8>>,
    crls: Vec<(&'a Path, Vec<u8>)>,
}

impl<'a> Sources<'a> {
    pub(crate) fn read(settings: &'a TlsSettings) -> Result<Self, TlsError> {
        let cert = read_file(CHAIN, &settings.cert_path)?;
        let key = Zeroizing::new(read_file(KEY, &settings.key_path)?);
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

    /// Changes whenever any file's bytes change. The serving material keeps
    /// this 64-bit hash, not the bytes of the files, the key among them.
    /// `DefaultHasher` is not collision-resistant: files crafted to collide
    /// with the serving ones would not be reloaded. That is acceptable,
    /// because whoever writes these files is trusted with the private key.
    fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.cert.hash(&mut hasher);
        self.key.as_slice().hash(&mut hasher);
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
    let current = build(settings, &sources, &provider, now)?;
    let reload_interval = match settings.reload_interval_ms {
        0 => None,
        ms => Some(Duration::from_millis(ms)),
    };
    let server = TlsServer {
        material: Arc::new(Material {
            settings: settings.clone(),
            provider,
            current: RwLock::new(current),
        }),
        handshake_timeout: Duration::from_millis(settings.handshake_timeout_ms),
        reload_interval,
    };
    server.log_crl_next_update(now, false);
    Ok(server)
}

/// Parses and validates `sources` into a server configuration: every
/// certificate of the chain parses, the key matches the leaf, and the client
/// CA bundle and CRLs pass the checks of [`client_verifier`].
fn build(
    settings: &TlsSettings,
    sources: &Sources<'_>,
    provider: &Arc<CryptoProvider>,
    now: UnixTime,
) -> Result<Current, TlsError> {
    let chain = certificates(CHAIN, &settings.cert_path, &sources.cert)?;
    let mut cert_not_after = None;
    for (index, cert) in chain.iter().enumerate() {
        let Ok((_, parsed)) = x509_parser::parse_x509_certificate(cert.as_ref()) else {
            let message = format!("certificate {} cannot be parsed", index + 1);
            return Err(pem_error(CHAIN, &settings.cert_path, message));
        };
        if index == 0 {
            cert_not_after = Some(parsed.validity().not_after.timestamp());
        }
    }
    let key = PrivateKeyDer::from_pem_slice(&sources.key)
        .map_err(|e| pem_error(KEY, &settings.key_path, pem_reason(&e)))?;
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Config(e.to_string()))?;
    // `None` only when the settings, which a reload never changes, ask for
    // no client authentication.
    let (builder, earliest_crl) = match client_verifier(settings, sources, provider, now)? {
        Some(client) => (
            builder.with_client_cert_verifier(client.verifier),
            client.earliest_crl,
        ),
        None => (builder.with_no_client_auth(), None),
    };
    // Fails when the key does not match the leaf certificate.
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| chain_and_key_error(settings, e))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Current {
        config: Arc::new(config),
        fingerprint: sources.fingerprint(),
        cert_not_after,
        earliest_crl,
    })
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

/// A client certificate verifier, and the CRL it uses that expires first.
pub(crate) struct ClientVerifier {
    pub(crate) verifier: Arc<dyn ClientCertVerifier>,
    earliest_crl: Option<EarliestCrl>,
}

/// Builds the client certificate verifier that `settings` describe from the
/// files in `sources`, or `None` without client authentication. `now`
/// decides whether a CRL has expired.
pub(crate) fn client_verifier(
    settings: &TlsSettings,
    sources: &Sources<'_>,
    provider: &Arc<CryptoProvider>,
    now: UnixTime,
) -> Result<Option<ClientVerifier>, TlsError> {
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
    if settings.client_crl_expiration == CrlExpiration::Enforce {
        check_not_expired(&summaries, unix_secs(now))?;
    }
    let earliest_crl = summaries
        .iter()
        .filter_map(|summary| Some((summary.path, summary.next_update?)))
        .min_by_key(|(_, next_update)| *next_update)
        .map(|(path, next_update)| EarliestCrl {
            path: path.to_path_buf(),
            next_update,
        });
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
    let verifier = verifier
        .build()
        .map_err(|e| TlsError::Config(e.to_string()))?;
    Ok(Some(ClientVerifier {
        verifier,
        earliest_crl,
    }))
}

fn unix_secs(time: UnixTime) -> i64 {
    i64::try_from(time.as_secs()).unwrap_or(i64::MAX)
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

/// Logs the earliest CRL `nextUpdate` time: as a warning when it is less
/// than a day away, and otherwise at info unless `only_warn` is set. With
/// expiration enforced, an expired CRL fails every handshake it covers until
/// a fresh CRL is loaded. Returns whether it warned.
fn log_next_update(crl: &EarliestCrl, now: i64, only_warn: bool) -> bool {
    let next_update = crl.next_update;
    let remaining_secs = next_update.timestamp().saturating_sub(now);
    let path = crl.path.display();
    let warn = remaining_secs < CRL_EXPIRY_WARNING_SECS;
    if warn {
        tracing::warn!(
            target: "ferrum_alloy::tls",
            path = %path,
            next_update = %next_update,
            remaining_secs,
            "a client CRL expires within 24 hours or has expired; publish a fresh CRL, which the next TLS reload (or, with reloading disabled, a restart) loads"
        );
    } else if !only_warn {
        tracing::info!(
            target: "ferrum_alloy::tls",
            path = %path,
            next_update = %next_update,
            remaining_secs,
            "earliest client CRL nextUpdate"
        );
    }
    warn
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
#[allow(clippy::unwrap_used, clippy::panic)]
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
            .unwrap()
            .verifier;
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

    /// A server whose certificate and key, issued by `issuer`, are written
    /// to `dir`, and that requires client certificates from the CA at
    /// `ca_path`. Returns the server and the certificate and key paths.
    fn reloadable(
        dir: &Path,
        ca_path: PathBuf,
        issuer: &Issuer<'static, KeyPair>,
    ) -> (TlsServer, PathBuf, PathBuf) {
        let (cert, key) = server_pem(issuer);
        let cert_path = dir.join("server.pem");
        let key_path = dir.join("server.key");
        std::fs::write(&cert_path, cert).unwrap();
        std::fs::write(&key_path, key).unwrap();
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
        (load(&settings).unwrap(), cert_path, key_path)
    }

    #[tokio::test]
    async fn a_reload_swaps_only_changed_material_that_validates() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let (server, cert_path, key_path) = reloadable(dir.path(), ca_path, &issuer);
        assert_eq!(server.reload_interval, Some(Duration::from_secs(1)));
        let initial = serving(&server);
        let metrics = server.expiry().render_prometheus("app");
        assert!(
            metrics.contains("cert_not_after_timestamp_seconds"),
            "{metrics}"
        );
        assert!(
            !metrics.contains("crl_next_update"),
            "unknown is not zero: {metrics}"
        );
        let mut reloader = Reloader::new(server.clone(), Duration::ZERO);

        let now = UnixTime::now();
        assert_eq!(reloader.reload(now).await, Outcome::Unchanged);
        assert!(Arc::ptr_eq(&initial, &serving(&server)));

        // A key that does not match the certificate is refused, and counted
        // once the same files fail again.
        let (_, other_key) = server_pem(&issuer);
        std::fs::write(&key_path, &other_key).unwrap();
        let outcome = reloader.reload(now).await;
        assert!(matches!(outcome, Outcome::Unconfirmed(_)), "{outcome:?}");
        let Outcome::Failed(error) = reloader.reload(now).await else {
            panic!("the same failure was not counted");
        };
        assert!(error.contains("private key"), "{error}");
        assert!(
            !error.contains("PRIVATE KEY"),
            "the key was quoted: {error}"
        );
        assert!(Arc::ptr_eq(&initial, &serving(&server)));

        let (new_cert, new_key) = server_pem(&issuer);
        std::fs::write(&cert_path, &new_cert).unwrap();
        std::fs::write(&key_path, &new_key).unwrap();
        assert_eq!(reloader.reload(now).await, Outcome::Swapped);
        assert!(!Arc::ptr_eq(&initial, &serving(&server)));
        assert_eq!(reloader.reload(now).await, Outcome::Unchanged);
    }

    #[tokio::test]
    async fn a_certificate_read_before_its_key_is_neither_swapped_in_nor_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let (server, cert_path, key_path) = reloadable(dir.path(), ca_path, &issuer);
        let initial = serving(&server);
        let mut reloader = Reloader::new(server.clone(), Duration::ZERO);
        let now = UnixTime::now();

        // Only the certificate has been written so far.
        let (new_cert, new_key) = server_pem(&issuer);
        std::fs::write(&cert_path, &new_cert).unwrap();
        let outcome = reloader.reload(now).await;
        assert!(matches!(outcome, Outcome::Unconfirmed(_)), "{outcome:?}");
        assert!(Arc::ptr_eq(&initial, &serving(&server)));

        std::fs::write(&key_path, &new_key).unwrap();
        assert_eq!(reloader.reload(now).await, Outcome::Swapped);
        assert!(!Arc::ptr_eq(&initial, &serving(&server)));
    }

    #[test]
    fn files_that_change_between_the_two_reads_are_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, issuer) = ca(dir.path());
        let (server, cert_path, key_path) = reloadable(dir.path(), ca_path, &issuer);
        let initial = serving(&server);
        let now = UnixTime::now();

        let (new_cert, new_key) = server_pem(&issuer);
        std::fs::write(&cert_path, &new_cert).unwrap();
        let fingerprint = server.changed().unwrap().unwrap();
        std::fs::write(&key_path, &new_key).unwrap();
        let settled = server.swap_if_settled(fingerprint, now).unwrap();
        assert!(matches!(settled, Settled::Changing), "{settled:?}");
        assert!(Arc::ptr_eq(&initial, &serving(&server)));

        let fingerprint = server.changed().unwrap().unwrap();
        let settled = server.swap_if_settled(fingerprint, now).unwrap();
        assert!(matches!(settled, Settled::Swapped), "{settled:?}");
        assert!(!Arc::ptr_eq(&initial, &serving(&server)));
        assert_eq!(server.changed().unwrap(), None);
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
        Err(rustls::Error::General(
            "the handshake did not finish".into(),
        ))
    }

    #[tokio::test]
    async fn an_expired_crl_is_replaced_at_the_next_reload() {
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
        let metrics = server.expiry().render_prometheus("app");
        let gauge = "ferrum_alloy_tls_client_crl_next_update_timestamp_seconds";
        let sample = format!("{gauge}{{listener=\"app\"}} {NEXT_UPDATE_SECS}\n");
        assert!(metrics.contains(&sample), "{metrics}");

        let mut reloader = Reloader::new(server.clone(), Duration::ZERO);
        let now = UnixTime::now();
        assert_eq!(reloader.reload(now).await, Outcome::Unchanged);
        // Another expired CRL is refused, and the old one keeps serving.
        std::fs::write(&crl_path, crl_pem(&issuer, 2022)).unwrap();
        let Outcome::Unconfirmed(error) = reloader.reload(now).await else {
            panic!("an expired CRL was not refused");
        };
        assert!(error.contains("expired"), "{error}");

        std::fs::write(&crl_path, crl_pem(&issuer, 2100)).unwrap();
        assert_eq!(reloader.reload(now).await, Outcome::Swapped);
        handshake(serving(&server), client).unwrap();
        // 2100-01-01T00:00:00Z.
        assert_eq!(server.expiry().crl_next_update, Some(4_102_444_800));
    }
}
