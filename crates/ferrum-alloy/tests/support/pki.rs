//! Throwaway test PKI: a CA, intermediates, a server certificate, client
//! certificates with SPIFFE URI SANs, and certificate revocation lists.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationList,
    CertificateRevocationListParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyIdMethod,
    KeyPair, KeyUsagePurpose, RevocationReason, RevokedCertParams, SanType, SerialNumber,
    date_time_ymd,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Ca {
    pub cert_pem: String,
    pub cert_der: CertificateDer<'static>,
    pub serial: SerialNumber,
    issuer: Issuer<'static, KeyPair>,
}

pub struct Leaf {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
    pub serial: SerialNumber,
}

/// Distinct serial numbers, so CRL entries match exactly one certificate.
fn next_serial() -> SerialNumber {
    static NEXT: AtomicU64 = AtomicU64::new(1_000);
    SerialNumber::from(NEXT.fetch_add(1, Ordering::Relaxed))
}

fn ca_params(name: &str) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(next_serial());
    params
}

impl Ca {
    pub fn new(name: &str) -> Self {
        let params = ca_params(name);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Self {
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
            serial: params.serial_number.clone().unwrap(),
            issuer: Issuer::new(params, key),
        }
    }

    /// An intermediate CA issued by this one.
    pub fn intermediate(&self, name: &str) -> Self {
        let params = ca_params(name);
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        Self {
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
            serial: params.serial_number.clone().unwrap(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A CRL from this CA revoking `revoked`, valid until 2100.
    pub fn crl(&self, revoked: &[&SerialNumber]) -> CertificateRevocationList {
        self.crl_until(revoked, 2100, None)
    }

    /// A CRL from this CA revoking `revoked`, whose `nextUpdate` was in 2021.
    pub fn expired_crl(&self, revoked: &[&SerialNumber]) -> CertificateRevocationList {
        self.crl_until(revoked, 2021, None)
    }

    /// An empty CRL from this CA for the partition of its certificates
    /// published at `uri`, valid until 2100.
    pub fn partition_crl(&self, uri: &str) -> CertificateRevocationList {
        let partition = rcgen::CrlIssuingDistributionPoint {
            distribution_point: rcgen::CrlDistributionPoint {
                uris: vec![uri.to_owned()],
            },
            scope: None,
        };
        self.crl_until(&[], 2100, Some(partition))
    }

    fn crl_until(
        &self,
        revoked: &[&SerialNumber],
        next_update_year: i32,
        issuing_distribution_point: Option<rcgen::CrlIssuingDistributionPoint>,
    ) -> CertificateRevocationList {
        let revoked_certs = revoked
            .iter()
            .map(|serial| RevokedCertParams {
                serial_number: (*serial).clone(),
                revocation_time: date_time_ymd(2020, 6, 1),
                reason_code: Some(RevocationReason::KeyCompromise),
                invalidity_date: None,
            })
            .collect();
        let params = CertificateRevocationListParams {
            this_update: date_time_ymd(2020, 6, 1),
            next_update: date_time_ymd(next_update_year, 1, 1),
            crl_number: next_serial(),
            issuing_distribution_point,
            revoked_certs,
            key_identifier_method: KeyIdMethod::Sha256,
        };
        params.signed_by(&self.issuer).unwrap()
    }

    fn issue(&self, sans: Vec<SanType>, usage: ExtendedKeyUsagePurpose) -> Leaf {
        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params.extended_key_usages = vec![usage];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let serial = next_serial();
        params.serial_number = Some(serial.clone());
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        Leaf {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            cert_der: cert.der().clone(),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            serial,
        }
    }

    pub fn server(&self) -> Leaf {
        self.issue(
            vec![
                SanType::DnsName("localhost".try_into().unwrap()),
                SanType::IpAddress("127.0.0.1".parse().unwrap()),
            ],
            ExtendedKeyUsagePurpose::ServerAuth,
        )
    }

    pub fn client(&self, spiffe_id: &str) -> Leaf {
        self.issue(
            vec![SanType::URI(spiffe_id.try_into().unwrap())],
            ExtendedKeyUsagePurpose::ClientAuth,
        )
    }
}

pub fn write(dir: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

/// A rustls client config trusting `ca`, optionally presenting `identity`.
pub fn client_config(ca: &Ca, identity: Option<&Leaf>) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.cert_der.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    let config = match identity {
        Some(leaf) => builder
            .with_client_auth_cert(vec![leaf.cert_der.clone()], leaf.key_der.clone_key())
            .unwrap(),
        None => builder.with_no_client_auth(),
    };
    Arc::new(config)
}

/// A rustls client config trusting `ca` and presenting `leaf` followed by
/// `intermediates`.
pub fn client_config_with_chain(
    ca: &Ca,
    leaf: &Leaf,
    intermediates: &[&Ca],
) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.cert_der.clone()).unwrap();
    let mut chain = vec![leaf.cert_der.clone()];
    chain.extend(intermediates.iter().map(|ca| ca.cert_der.clone()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, leaf.key_der.clone_key())
        .unwrap();
    Arc::new(config)
}
