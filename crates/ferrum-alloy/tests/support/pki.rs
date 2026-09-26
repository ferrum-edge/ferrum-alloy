//! Throwaway test PKI: a CA, a server certificate, and client certificates
//! with SPIFFE URI SANs.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Ca {
    pub cert_pem: String,
    pub cert_der: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
}

pub struct Leaf {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

impl Ca {
    pub fn new(name: &str) -> Self {
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Self {
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
            issuer: Issuer::new(params, key),
        }
    }

    fn issue(&self, sans: Vec<SanType>, usage: ExtendedKeyUsagePurpose) -> Leaf {
        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params.extended_key_usages = vec![usage];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        Leaf {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            cert_der: cert.der().clone(),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
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

pub fn write(dir: &Path, name: &str, pem: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, pem).unwrap();
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
