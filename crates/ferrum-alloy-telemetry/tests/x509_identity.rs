//! Peer identity extraction from verified certificates.

#![cfg(feature = "x509")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ferrum_alloy_telemetry::peer::{PeerCertificateError, TlsPeer};
use rcgen::{CertificateParams, KeyPair, SanType};

fn leaf(sans: Vec<SanType>) -> Vec<u8> {
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans;
    let key = KeyPair::generate().unwrap();
    params.self_signed(&key).unwrap().der().to_vec()
}

fn uri(value: &str) -> SanType {
    SanType::URI(value.try_into().unwrap())
}

#[test]
fn reads_single_spiffe_id_and_dns_names() {
    let der = leaf(vec![
        uri("spiffe://ferrum.test/ns/edge/sa/gateway"),
        SanType::DnsName("Gateway.Internal".try_into().unwrap()),
    ]);
    let peer = TlsPeer::from_verified_leaf(&der).unwrap();
    assert!(peer.client_cert_verified);
    assert_eq!(
        peer.spiffe_id.as_deref(),
        Some("spiffe://ferrum.test/ns/edge/sa/gateway")
    );
    assert_eq!(peer.dns_names, vec!["gateway.internal".to_owned()]);
}

#[test]
fn multiple_spiffe_ids_are_ambiguous() {
    let der = leaf(vec![uri("spiffe://a.test/x"), uri("spiffe://b.test/y")]);
    assert_eq!(TlsPeer::from_verified_leaf(&der).unwrap().spiffe_id, None);
}

#[test]
fn non_spiffe_uris_are_ignored() {
    let der = leaf(vec![uri("https://example.test/")]);
    assert_eq!(TlsPeer::from_verified_leaf(&der).unwrap().spiffe_id, None);
}

#[test]
fn garbage_is_rejected() {
    assert_eq!(
        TlsPeer::from_verified_leaf(b"not a certificate"),
        Err(PeerCertificateError::Malformed)
    );
    let mut der = leaf(vec![]);
    der.push(0);
    assert_eq!(
        TlsPeer::from_verified_leaf(&der),
        Err(PeerCertificateError::Malformed)
    );
}
