//! Generates a throwaway demo PKI:
//!
//! * `ca.pem`: the demo CA (trusted by both Alloy and Edge);
//! * `alloy.pem` / `alloy.key`: Alloy's server certificate
//!   (DNS `alloy`, `alloy-reuse`, `localhost`, IP `127.0.0.1`; `alloy-reuse`
//!   is a compose network alias for the connection-reuse case);
//! * `edge-client.pem` / `edge-client.key`: the certificate Edge presents to
//!   Alloy, with the SPIFFE URI SAN `spiffe://ferrum.demo/ns/edge/sa/gateway`.
//!
//! Demo only: keys are written world-readable so container users can read
//! them. Never use these files outside a disposable environment.

use std::path::PathBuf;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

/// The gateway identity the demo trusts.
const GATEWAY_SPIFFE_ID: &str = "spiffe://ferrum.demo/ns/edge/sa/gateway";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: gen-demo-certs <output-directory>")?,
    );
    std::fs::create_dir_all(&out)?;

    let mut ca_params = CertificateParams::default();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Ferrum Alloy demo CA (disposable)");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;
    std::fs::write(out.join("ca.pem"), ca_cert.pem())?;
    let issuer = Issuer::new(ca_params, ca_key);

    let mut server = CertificateParams::default();
    server.distinguished_name.push(DnType::CommonName, "alloy");
    server.subject_alt_names = vec![
        SanType::DnsName("alloy".try_into()?),
        SanType::DnsName("alloy-reuse".try_into()?),
        SanType::DnsName("localhost".try_into()?),
        SanType::IpAddress("127.0.0.1".parse()?),
    ];
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let server_key = KeyPair::generate()?;
    let server_cert = server.signed_by(&server_key, &issuer)?;
    std::fs::write(out.join("alloy.pem"), server_cert.pem())?;
    std::fs::write(out.join("alloy.key"), server_key.serialize_pem())?;

    let mut client = CertificateParams::default();
    client
        .distinguished_name
        .push(DnType::CommonName, GATEWAY_SPIFFE_ID);
    client.subject_alt_names = vec![SanType::URI(GATEWAY_SPIFFE_ID.try_into()?)];
    client.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ClientAuth,
        ExtendedKeyUsagePurpose::ServerAuth,
    ];
    client.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let client_key = KeyPair::generate()?;
    let client_cert = client.signed_by(&client_key, &issuer)?;
    std::fs::write(out.join("edge-client.pem"), client_cert.pem())?;
    std::fs::write(out.join("edge-client.key"), client_key.serialize_pem())?;
    Ok(())
}
