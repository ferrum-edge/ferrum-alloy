//! Client certificate revocation over real mTLS: CRLs from
//! `server.tls.client_crl_paths` refuse revoked certificates at the
//! handshake, and missing, unparsable, or expired CRLs fail startup.

#![cfg(feature = "tls")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::config::{
    AlloyConfig, ClientAuth, CrlDepth, CrlExpiration, CrlUnknownStatus, TlsSettings,
};
use ferrum_alloy::{AlloyApp, AlloyError, TelemetryInit};
use http::{Request, StatusCode};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use support::pki::{self, Ca};

const CLIENT: &str = "spiffe://ferrum.test/ns/edge/sa/gateway";

struct Pki {
    dir: tempfile::TempDir,
    ca: Ca,
    tls: TlsSettings,
}

impl Pki {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::new("crl-test-ca");
        let server = ca.server();
        let tls = TlsSettings {
            cert_path: pki::write(dir.path(), "server.pem", &server.cert_pem),
            key_path: pki::write(dir.path(), "server.key", &server.key_pem),
            client_ca_path: Some(pki::write(dir.path(), "ca.pem", &ca.cert_pem)),
            client_auth: ClientAuth::Required,
            handshake_timeout_ms: 2_000,
            client_crl_paths: Vec::new(),
            client_crl_depth: CrlDepth::default(),
            client_crl_unknown_status: CrlUnknownStatus::default(),
            client_crl_expiration: CrlExpiration::default(),
        };
        Self { dir, ca, tls }
    }

    fn file(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        pki::write(self.dir.path(), name, contents)
    }

    fn config(&self, crl_paths: Vec<PathBuf>) -> AlloyConfig {
        let mut config = support::config();
        let mut tls = self.tls.clone();
        tls.client_crl_paths = crl_paths;
        config.server.tls = Some(tls);
        config
    }
}

fn app() -> AlloyApp {
    AlloyApp::new("crl").router(Router::new().route("/hello", get(|| async { "hello" })))
}

fn startup_error(config: AlloyConfig) -> AlloyError {
    app()
        .config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap_err()
}

/// One TLS handshake and one request. With TLS 1.3 the server's refusal of
/// a client certificate can arrive after the client considers the handshake
/// done, so the request is part of the probe.
async fn get_hello(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
) -> Result<StatusCode, String> {
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let stream = tokio_rustls::TlsConnector::from(client)
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    let request = Request::get("/hello")
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    Ok(response.status())
}

#[tokio::test]
async fn a_revoked_client_certificate_is_refused_and_others_are_accepted() {
    let pki = Pki::new();
    let revoked = pki.ca.client(CLIENT);
    let valid = pki.ca.client(CLIENT);
    let crl = pki.ca.crl(&[&revoked.serial]);
    let crl_path = pki.file("ca.crl.pem", crl.pem().unwrap());
    let server = support::start(app(), pki.config(vec![crl_path])).await;

    let refused = get_hello(server.addr, pki::client_config(&pki.ca, Some(&revoked))).await;
    assert!(
        refused.is_err(),
        "a revoked certificate must fail the handshake"
    );
    let accepted = get_hello(server.addr, pki::client_config(&pki.ca, Some(&valid)))
        .await
        .unwrap();
    assert_eq!(accepted, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn optional_client_auth_refuses_revoked_certificates_and_accepts_anonymous_clients() {
    let pki = Pki::new();
    let revoked = pki.ca.client(CLIENT);
    let crl_path = pki.file("ca.crl.pem", pki.ca.crl(&[&revoked.serial]).pem().unwrap());
    let mut config = pki.config(vec![crl_path]);
    if let Some(tls) = config.server.tls.as_mut() {
        tls.client_auth = ClientAuth::Optional;
    }
    let server = support::start(app(), config).await;

    let refused = get_hello(server.addr, pki::client_config(&pki.ca, Some(&revoked))).await;
    assert!(
        refused.is_err(),
        "optional client auth still refuses a revoked certificate"
    );
    let anonymous = get_hello(server.addr, pki::client_config(&pki.ca, None))
        .await
        .unwrap();
    assert_eq!(anonymous, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn der_crl_files_are_read() {
    let pki = Pki::new();
    let revoked = pki.ca.client(CLIENT);
    let valid = pki.ca.client(CLIENT);
    let crl = pki.ca.crl(&[&revoked.serial]);
    let crl_path = pki.file("ca.crl.der", crl.der());
    let server = support::start(app(), pki.config(vec![crl_path])).await;

    let refused = get_hello(server.addr, pki::client_config(&pki.ca, Some(&revoked))).await;
    assert!(refused.is_err(), "a DER CRL must be enforced too");
    let accepted = get_hello(server.addr, pki::client_config(&pki.ca, Some(&valid)))
        .await
        .unwrap();
    assert_eq!(accepted, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_revocation_status_is_refused_unless_allowed() {
    let pki = Pki::new();
    let client = pki.ca.client(CLIENT);
    // The only CRL comes from another CA, so no CRL covers the client.
    let other = Ca::new("crl-test-other-ca");
    let crl_path = pki.file("other.crl.pem", other.crl(&[]).pem().unwrap());

    let server = support::start(app(), pki.config(vec![crl_path.clone()])).await;
    let refused = get_hello(server.addr, pki::client_config(&pki.ca, Some(&client))).await;
    assert!(
        refused.is_err(),
        "unknown revocation status is refused by default"
    );
    server.shutdown().await.unwrap();

    let mut config = pki.config(vec![crl_path]);
    if let Some(tls) = config.server.tls.as_mut() {
        tls.client_crl_unknown_status = CrlUnknownStatus::Allow;
    }
    let server = support::start(app(), config).await;
    let accepted = get_hello(server.addr, pki::client_config(&pki.ca, Some(&client)))
        .await
        .unwrap();
    assert_eq!(accepted, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn chain_depth_checks_intermediates_and_end_entity_depth_does_not() {
    let pki = Pki::new();
    let intermediate = pki.ca.intermediate("crl-test-intermediate");
    let client = intermediate.client(CLIENT);
    let revoked_leaf = intermediate.client(CLIENT);
    // The root revokes the intermediate; the intermediate revokes one leaf.
    let root_crl = pki.file(
        "root.crl.pem",
        pki.ca.crl(&[&intermediate.serial]).pem().unwrap(),
    );
    let intermediate_crl = pki.file(
        "intermediate.crl.pem",
        intermediate.crl(&[&revoked_leaf.serial]).pem().unwrap(),
    );
    let crls = vec![root_crl, intermediate_crl];
    let chain = |leaf: &pki::Leaf| pki::client_config_with_chain(&pki.ca, leaf, &[&intermediate]);

    let server = support::start(app(), pki.config(crls.clone())).await;
    let refused = get_hello(server.addr, chain(&client)).await;
    assert!(
        refused.is_err(),
        "the default chain depth refuses a revoked intermediate"
    );
    server.shutdown().await.unwrap();

    let mut config = pki.config(crls);
    if let Some(tls) = config.server.tls.as_mut() {
        tls.client_crl_depth = CrlDepth::EndEntity;
    }
    let server = support::start(app(), config).await;
    let accepted = get_hello(server.addr, chain(&client)).await.unwrap();
    assert_eq!(
        accepted,
        StatusCode::OK,
        "end_entity depth ignores the intermediate's revocation"
    );
    let refused = get_hello(server.addr, chain(&revoked_leaf)).await;
    assert!(refused.is_err(), "end_entity depth still checks the leaf");
    server.shutdown().await.unwrap();

    // Once the root no longer revokes the intermediate, the chain passes.
    let root_crl = pki.file("root.crl.pem", pki.ca.crl(&[]).pem().unwrap());
    let intermediate_crl = pki.file(
        "intermediate.crl.pem",
        intermediate.crl(&[&revoked_leaf.serial]).pem().unwrap(),
    );
    let server = support::start(app(), pki.config(vec![root_crl, intermediate_crl])).await;
    let accepted = get_hello(server.addr, chain(&client)).await.unwrap();
    assert_eq!(accepted, StatusCode::OK);
    let refused = get_hello(server.addr, chain(&revoked_leaf)).await;
    assert!(refused.is_err(), "chain depth checks the leaf too");
    server.shutdown().await.unwrap();
}

#[test]
fn a_missing_crl_file_fails_startup() {
    let pki = Pki::new();
    let missing = pki.dir.path().join("missing.crl.pem");
    let error = startup_error(pki.config(vec![missing.clone()])).to_string();
    assert!(error.contains("client CRL"), "{error}");
    assert!(error.contains(&missing.display().to_string()), "{error}");
}

#[test]
fn an_unparsable_crl_file_fails_startup_without_quoting_it() {
    const MARKER: &str = "NOT-A-CRL-MARKER";
    let pki = Pki::new();
    let cases = [
        (
            "garbage.pem",
            format!("-----BEGIN X509 CRL-----\n{MARKER}!\n-----END X509 CRL-----\n").into_bytes(),
        ),
        (
            "unterminated.pem",
            format!("-----BEGIN X509 CRL-----\n{MARKER}\n").into_bytes(),
        ),
        // DER framing (an ASN.1 SEQUENCE) around bytes that are not a CRL.
        (
            "garbage.der",
            [&[0x30, 0x10][..], MARKER.as_bytes()].concat(),
        ),
        // PEM without any CRL section.
        ("certificate.pem", pki.ca.cert_pem.clone().into_bytes()),
        ("empty.pem", Vec::new()),
    ];
    for (name, contents) in cases {
        let path = pki.file(name, &contents);
        let error = startup_error(pki.config(vec![path.clone()])).to_string();
        assert!(error.contains("client CRL"), "{name}: {error}");
        assert!(
            error.contains(&path.display().to_string()),
            "{name}: {error}"
        );
        assert!(!error.contains(MARKER), "{name} was quoted: {error}");
    }
}

#[tokio::test]
async fn an_expired_crl_fails_startup_unless_expiration_is_ignored() {
    let pki = Pki::new();
    let revoked = pki.ca.client(CLIENT);
    let valid = pki.ca.client(CLIENT);
    let crl_path = pki.file(
        "expired.crl.pem",
        pki.ca.expired_crl(&[&revoked.serial]).pem().unwrap(),
    );

    let error = startup_error(pki.config(vec![crl_path.clone()])).to_string();
    assert!(error.contains("expired"), "{error}");
    assert!(error.contains(&crl_path.display().to_string()), "{error}");

    // With expiration ignored, the stale CRL is still enforced.
    let mut config = pki.config(vec![crl_path]);
    if let Some(tls) = config.server.tls.as_mut() {
        tls.client_crl_expiration = CrlExpiration::Ignore;
    }
    let server = support::start(app(), config).await;
    let refused = get_hello(server.addr, pki::client_config(&pki.ca, Some(&revoked))).await;
    assert!(refused.is_err(), "a stale CRL still revokes");
    let accepted = get_hello(server.addr, pki::client_config(&pki.ca, Some(&valid)))
        .await
        .unwrap();
    assert_eq!(accepted, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[test]
fn one_bad_file_among_several_is_named() {
    let pki = Pki::new();
    let good = pki.file("good.crl.pem", pki.ca.crl(&[]).pem().unwrap());
    let bad = pki.file("bad.crl.pem", "not a crl");
    let error = startup_error(pki.config(vec![good.clone(), bad.clone()])).to_string();
    assert!(error.contains(&bad.display().to_string()), "{error}");
    assert!(!error.contains(&good.display().to_string()), "{error}");
}

#[test]
fn two_crls_from_one_issuer_fail_startup() {
    let pki = Pki::new();
    let full_pem = pki.ca.crl(&[]).pem().unwrap();
    let newer_pem = pki.ca.crl(&[]).pem().unwrap();
    let full = pki.file("full.crl.pem", &full_pem);
    let newer = pki.file("newer.crl.pem", &newer_pem);
    let partition_a = pki.file(
        "a.crl.pem",
        pki.ca.partition_crl("http://crl.test/a").pem().unwrap(),
    );
    let partition_b = pki.file(
        "b.crl.pem",
        pki.ca.partition_crl("http://crl.test/b").pem().unwrap(),
    );

    // Only the first CRL whose issuer matches is consulted, so a revocation
    // listed only in the other one would be missed. Partitions are refused
    // too: each also covers certificates without a CRL distribution point.
    let pairs = [
        (&full, &newer),
        (&full, &partition_a),
        (&partition_a, &full),
        (&partition_a, &partition_b),
    ];
    for (earlier, later) in pairs {
        let error = startup_error(pki.config(vec![earlier.clone(), later.clone()])).to_string();
        assert!(error.contains("same issuer"), "{error}");
        assert!(error.contains(&earlier.display().to_string()), "{error}");
        assert!(error.contains(&later.display().to_string()), "{error}");
    }

    let both = pki.file("both.crl.pem", format!("{full_pem}{newer_pem}"));
    let error = startup_error(pki.config(vec![both.clone()])).to_string();
    assert!(error.contains("same issuer"), "{error}");
    assert!(error.contains(&both.display().to_string()), "{error}");
}

#[tokio::test]
async fn crls_from_different_issuers_are_accepted() {
    let pki = Pki::new();
    let intermediate = pki.ca.intermediate("crl-test-intermediate");
    let root_crl = pki.file("root.crl.pem", pki.ca.crl(&[]).pem().unwrap());
    let intermediate_pem = intermediate.crl(&[]).pem().unwrap();
    let intermediate_crl = pki.file("intermediate.crl.pem", intermediate_pem);
    let server = support::start(app(), pki.config(vec![root_crl, intermediate_crl])).await;
    server.shutdown().await.unwrap();
}
