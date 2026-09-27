//! Throwaway PKI for the TLS transports, generated per run: a CA, a server
//! certificate for `localhost` and `127.0.0.1`, and a client certificate.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrum_alloy::config::{ClientAuth, TlsSettings};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tempfile::TempDir;

use crate::Failure;
use crate::dims::Transport;

struct Leaf {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    cert_pem: String,
    key_pem: String,
}

/// Server-side TLS for one run: a rustls configuration for the `plain`
/// scenario, and file-based settings for `AlloyApp`.
pub(crate) struct ServerTls {
    pub(crate) rustls: Arc<ServerConfig>,
    pub(crate) alloy: TlsSettings,
}

/// The generated certificates. PEM files for `AlloyApp` live in a private
/// temporary directory with an unpredictable name, removed on drop.
pub(crate) struct Pki {
    _dir: TempDir,
    ca: CertificateDer<'static>,
    ca_path: PathBuf,
    server: Leaf,
    server_cert_path: PathBuf,
    server_key_path: PathBuf,
    client: Leaf,
}

impl Pki {
    pub(crate) fn generate() -> Result<Self, Failure> {
        let mut ca_params = CertificateParams::default();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "alloy-bench CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate()?;
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let issuer = Issuer::new(ca_params, ca_key);
        let server = leaf(
            &issuer,
            vec![
                SanType::DnsName("localhost".try_into()?),
                SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            ],
            ExtendedKeyUsagePurpose::ServerAuth,
        )?;
        let client = leaf(
            &issuer,
            vec![SanType::URI("spiffe://alloy-bench/client".try_into()?)],
            ExtendedKeyUsagePurpose::ClientAuth,
        )?;

        let dir = tempfile::Builder::new().prefix("alloy-bench-").tempdir()?;
        let pki = Self {
            ca_path: dir.path().join("ca.pem"),
            server_cert_path: dir.path().join("server.pem"),
            server_key_path: dir.path().join("server-key.pem"),
            _dir: dir,
            ca: ca_cert.der().clone(),
            server,
            client,
        };
        std::fs::write(&pki.ca_path, ca_cert.pem())?;
        std::fs::write(&pki.server_cert_path, &pki.server.cert_pem)?;
        write_private(&pki.server_key_path, &pki.server.key_pem)?;
        Ok(pki)
    }

    fn roots(&self) -> Result<RootCertStore, Failure> {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca.clone())?;
        Ok(roots)
    }

    /// Server TLS offering `h2` and `http/1.1`, requiring a client
    /// certificate when `mtls` is set.
    pub(crate) fn server(&self, mtls: bool) -> Result<ServerTls, Failure> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?;
        let builder = if mtls {
            let roots = Arc::new(self.roots()?);
            let verifier = WebPkiClientVerifier::builder_with_provider(roots, provider);
            builder.with_client_cert_verifier(verifier.build()?)
        } else {
            builder.with_no_client_auth()
        };
        let chain = vec![self.server.cert.clone()];
        let mut config = builder.with_single_cert(chain, self.server.key.clone_key())?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let alloy = TlsSettings {
            cert_path: self.server_cert_path.clone(),
            key_path: self.server_key_path.clone(),
            client_ca_path: mtls.then(|| self.ca_path.clone()),
            client_auth: if mtls {
                ClientAuth::Required
            } else {
                ClientAuth::None
            },
            handshake_timeout_ms: 10_000,
            client_crl_paths: Vec::new(),
            client_crl_depth: Default::default(),
            client_crl_unknown_status: Default::default(),
            client_crl_expiration: Default::default(),
        };
        Ok(ServerTls {
            rustls: Arc::new(config),
            alloy,
        })
    }

    /// Client TLS for `transport`: trusts the generated CA, offers exactly
    /// one ALPN protocol, and presents the client certificate for mTLS.
    pub(crate) fn client(&self, transport: Transport) -> Result<Arc<ClientConfig>, Failure> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(self.roots()?);
        let mut config = if transport.mtls() {
            let chain = vec![self.client.cert.clone()];
            builder.with_client_auth_cert(chain, self.client.key.clone_key())?
        } else {
            builder.with_no_client_auth()
        };
        config.alpn_protocols = vec![transport.alpn().to_vec()];
        Ok(Arc::new(config))
    }
}

/// Creates `path` readable and writable by its owner only (on Unix), and
/// writes `contents` to it.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(contents.as_bytes())
}

fn leaf(
    issuer: &Issuer<'_, KeyPair>,
    sans: Vec<SanType>,
    usage: ExtendedKeyUsagePurpose,
) -> Result<Leaf, Failure> {
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![usage];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, issuer)?;
    Ok(Leaf {
        cert: cert.der().clone(),
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn files_live_in_a_private_directory_removed_on_drop() {
        let pki = Pki::generate().unwrap();
        let dir = pki.server_key_path.parent().unwrap().to_path_buf();
        assert!(pki.ca_path.is_file() && pki.server_cert_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(&pki.server_key_path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        drop(pki);
        assert!(!dir.exists());
    }
}
