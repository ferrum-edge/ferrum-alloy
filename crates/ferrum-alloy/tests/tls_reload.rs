//! TLS material reload over real TLS: a rotated certificate and key, client
//! CA bundle, or CRL is picked up by new handshakes without a restart,
//! established connections keep their session, sessions are not resumed
//! across a reload, and invalid replacements leave the previous material
//! serving and are counted.

#![cfg(feature = "tls")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::{AlloyConfig, ClientAuth, TlsSettings};
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rustls::HandshakeKind;
use rustls_pki_types::CertificateDer;
use support::pki::{self, Ca, Leaf};

const CLIENT: &str = "spiffe://ferrum.test/ns/edge/sa/gateway";

/// `server.tls.reload_interval_ms` of these tests.
const RELOAD_INTERVAL_MS: u64 = 100;

struct Pki {
    dir: tempfile::TempDir,
    ca: Ca,
    /// The server certificate the listener starts with.
    server: Leaf,
    tls: TlsSettings,
}

impl Pki {
    fn new(client_auth: ClientAuth) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::new("reload-test-ca");
        let server = ca.server();
        let client_ca_path = (client_auth != ClientAuth::None)
            .then(|| pki::write(dir.path(), "ca.pem", &ca.cert_pem));
        let mut tls = TlsSettings::new(
            pki::write(dir.path(), "server.pem", &server.cert_pem),
            pki::write(dir.path(), "server.key", &server.key_pem),
        );
        tls.client_ca_path = client_ca_path;
        tls.client_auth = client_auth;
        tls.handshake_timeout_ms = 2_000;
        tls.reload_interval_ms = RELOAD_INTERVAL_MS;
        Self {
            dir,
            ca,
            server,
            tls,
        }
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

/// Replaces `path` atomically, as a rename or a secret volume update does,
/// so a reload never reads a half-written file.
fn replace(path: &Path, contents: impl AsRef<[u8]>) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, contents).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

fn app() -> AlloyApp {
    AlloyApp::new("reload").router(Router::new().route("/hello", get(|| async { "hello" })))
}

/// An open HTTP/1.1 connection over TLS.
struct Connection {
    sender: SendRequest<Empty<Bytes>>,
    /// The leaf certificate the server presented.
    server_cert: CertificateDer<'static>,
    /// Whether the handshake resumed an earlier session.
    handshake_kind: Option<HandshakeKind>,
}

async fn connect(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
) -> Result<Connection, String> {
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let stream = tokio_rustls::TlsConnector::from(client)
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let server_cert = stream.get_ref().1.peer_certificates().unwrap()[0].clone();
    let handshake_kind = stream.get_ref().1.handshake_kind();
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    Ok(Connection {
        sender,
        server_cert,
        handshake_kind,
    })
}

impl Connection {
    async fn get(&mut self) -> Result<StatusCode, String> {
        self.sender.ready().await.map_err(|e| e.to_string())?;
        let request = Request::get("/hello")
            .header("host", "localhost")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = self
            .sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(status)
    }
}

/// One new TLS handshake and one request, returning the server's leaf
/// certificate and the status. With TLS 1.3 the server's refusal of a client
/// certificate can arrive after the client considers the handshake done, so
/// the request is part of the probe.
async fn probe(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
) -> Result<(CertificateDer<'static>, StatusCode), String> {
    let mut connection = connect(addr, client).await?;
    let status = connection.get().await?;
    Ok((connection.server_cert, status))
}

/// Waits until `counter` reaches `target`.
async fn wait_for(counter: &AtomicU64, target: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while counter.load(Ordering::Relaxed) < target {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the reload happened in time");
}

#[tokio::test]
async fn a_rotated_certificate_serves_new_handshakes_and_established_connections_keep_theirs() {
    let pki = Pki::new(ClientAuth::None);
    let server = support::start(app(), pki.config(Vec::new())).await;
    let client = pki::client_config(&pki.ca, None);
    let mut established = connect(server.addr, Arc::clone(&client)).await.unwrap();
    assert_eq!(established.get().await.unwrap(), StatusCode::OK);
    assert_eq!(established.server_cert, pki.server.cert_der);

    let rotated = pki.ca.server();
    replace(&pki.tls.cert_path, &rotated.cert_pem);
    replace(&pki.tls.key_path, &rotated.key_pem);
    wait_for(&server.stats.tls_reloads, 1).await;

    let (cert, status) = probe(server.addr, client).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cert, rotated.cert_der, "the new certificate serves");
    assert_eq!(
        established.get().await.unwrap(),
        StatusCode::OK,
        "a connection established before the reload keeps its session"
    );
    drop(established);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_rotated_client_ca_refuses_old_client_certificates_and_accepts_new_ones() {
    let pki = Pki::new(ClientAuth::Required);
    let server = support::start(app(), pki.config(Vec::new())).await;
    let old = pki::client_config(&pki.ca, Some(&pki.ca.client(CLIENT)));
    let new_ca = Ca::new("reload-test-new-client-ca");
    // Trusts the server's CA, and presents a certificate from the new
    // client CA.
    let new = pki::client_config(&pki.ca, Some(&new_ca.client(CLIENT)));
    let mut established = connect(server.addr, Arc::clone(&old)).await.unwrap();
    assert_eq!(established.get().await.unwrap(), StatusCode::OK);
    assert!(
        probe(server.addr, Arc::clone(&new)).await.is_err(),
        "the new client CA is not trusted before the reload"
    );
    // Until the reload, the old certificate resumes its session, so the
    // refusal after it shows that the reload dropped the session cache.
    let resumed = connect(server.addr, Arc::clone(&old)).await.unwrap();
    assert_eq!(resumed.handshake_kind, Some(HandshakeKind::Resumed));
    drop(resumed);

    replace(pki.tls.client_ca_path.as_ref().unwrap(), &new_ca.cert_pem);
    wait_for(&server.stats.tls_reloads, 1).await;

    assert!(
        probe(server.addr, old).await.is_err(),
        "a certificate from the removed CA fails new handshakes"
    );
    let (_, status) = probe(server.addr, new).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        established.get().await.unwrap(),
        StatusCode::OK,
        "a connection established before the reload keeps its session"
    );
    drop(established);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_serial_added_to_the_crl_is_refused_after_the_reload() {
    let pki = Pki::new(ClientAuth::Required);
    let client = pki.ca.client(CLIENT);
    let other = pki.ca.client(CLIENT);
    let crl_path = pki.file("ca.crl.pem", pki.ca.crl(&[]).pem().unwrap());
    let server = support::start(app(), pki.config(vec![crl_path.clone()])).await;
    let config = pki::client_config(&pki.ca, Some(&client));
    let (_, status) = probe(server.addr, Arc::clone(&config)).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    // Enforced CRL expiration disables resumption so every new connection
    // checks that the loaded CRL is still current.
    let second = connect(server.addr, Arc::clone(&config)).await.unwrap();
    assert_eq!(second.handshake_kind, Some(HandshakeKind::Full));
    drop(second);

    replace(&crl_path, pki.ca.crl(&[&client.serial]).pem().unwrap());
    wait_for(&server.stats.tls_reloads, 1).await;

    assert!(
        probe(server.addr, config).await.is_err(),
        "a newly revoked certificate fails new handshakes"
    );
    let other = pki::client_config(&pki.ca, Some(&other));
    let (_, status) = probe(server.addr, other).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_replacements_keep_the_previous_material_serving_and_are_counted() {
    for case in [
        "mismatched key",
        "unparsable CRL",
        "two CRLs from one issuer",
    ] {
        let pki = Pki::new(ClientAuth::Required);
        let revoked = pki.ca.client(CLIENT);
        let valid = pki.ca.client(CLIENT);
        let crl = pki.ca.crl(&[&revoked.serial]).pem().unwrap();
        let ca_crl = pki.file("ca.crl.pem", crl);
        let other_ca = Ca::new("reload-test-other-ca");
        let other_crl = pki.file("other.crl.pem", other_ca.crl(&[]).pem().unwrap());
        let crls = vec![ca_crl.clone(), other_crl.clone()];
        let server = support::start(app(), pki.config(crls)).await;

        match case {
            "mismatched key" => replace(&pki.tls.key_path, pki.ca.server().key_pem),
            // Valid PEM and base64 around bytes that are not a CRL.
            "unparsable CRL" => replace(
                &ca_crl,
                "-----BEGIN X509 CRL-----\nbm90IGEgQ1JM\n-----END X509 CRL-----\n",
            ),
            // A second CRL from the issuer of the first.
            _ => replace(&other_crl, pki.ca.crl(&[]).pem().unwrap()),
        }
        wait_for(&server.stats.tls_reload_failures, 1).await;

        let reloads = server.stats.tls_reloads.load(Ordering::Relaxed);
        assert_eq!(reloads, 0, "{case}");
        let valid = pki::client_config(&pki.ca, Some(&valid));
        let (cert, status) = probe(server.addr, valid).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{case}");
        assert_eq!(cert, pki.server.cert_der, "{case}: new certificate");
        let revoked = pki::client_config(&pki.ca, Some(&revoked));
        assert!(
            probe(server.addr, revoked).await.is_err(),
            "{case}: the previous CRL no longer revokes"
        );
        let bearer = format!("Bearer {}", support::TOKEN);
        let metrics = support::fetch_with(
            &server.management_url("/metrics"),
            &[("authorization", &bearer)],
        )
        .await
        .text();
        assert!(
            metrics.contains("ferrum_alloy_tls_reloads_total{listener=\"app\"} 0\n"),
            "{case}: {metrics}"
        );
        assert!(
            !metrics.contains("ferrum_alloy_tls_reload_failures_total{listener=\"app\"} 0\n"),
            "{case}: {metrics}"
        );
        assert!(
            metrics.contains("ferrum_alloy_tls_reload_stalls_total{listener=\"app\"} 0\n"),
            "{case}: {metrics}"
        );
        for gauge in [
            "ferrum_alloy_tls_server_cert_not_after_timestamp_seconds{",
            "ferrum_alloy_tls_client_crl_next_update_timestamp_seconds{",
        ] {
            assert!(metrics.contains(gauge), "{case}: {metrics}");
        }
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_zero_interval_disables_reloading() {
    let mut pki = Pki::new(ClientAuth::None);
    pki.tls.reload_interval_ms = 0;
    let server = support::start(app(), pki.config(Vec::new())).await;

    let rotated = pki.ca.server();
    replace(&pki.tls.cert_path, &rotated.cert_pem);
    replace(&pki.tls.key_path, &rotated.key_pem);
    tokio::time::sleep(Duration::from_millis(10 * RELOAD_INTERVAL_MS)).await;

    let client = pki::client_config(&pki.ca, None);
    let (cert, status) = probe(server.addr, client).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cert, pki.server.cert_der, "the old certificate serves");
    let stats = &server.stats;
    assert_eq!(stats.tls_reloads.load(Ordering::Relaxed), 0);
    assert_eq!(stats.tls_reload_failures.load(Ordering::Relaxed), 0);
    server.shutdown().await.unwrap();
}

#[test]
fn unparsable_keys_and_chains_fail_startup_without_quoting_them() {
    const MARKER: &str = "NOT-PEM-MARKER";
    let pki = Pki::new(ClientAuth::None);
    let startup_error = |config: AlloyConfig| {
        app()
            .config(config)
            .telemetry(ferrum_alloy::TelemetryInit::ApplicationOwned)
            .into_parts()
            .unwrap_err()
            .to_string()
    };

    let key = pki.file(
        "garbage.key",
        format!("-----BEGIN PRIVATE KEY-----\n{MARKER}!\n-----END PRIVATE KEY-----\n"),
    );
    let mut config = pki.config(Vec::new());
    if let Some(tls) = config.server.tls.as_mut() {
        tls.key_path = key.clone();
    }
    let error = startup_error(config);
    assert!(error.contains("private key"), "{error}");
    assert!(error.contains(&key.display().to_string()), "{error}");
    assert!(!error.contains(MARKER), "quoted: {error}");

    // A valid leaf followed by a PEM section that is not a certificate.
    let chain = pki.file(
        "garbage-chain.pem",
        format!(
            "{}-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydGlmaWNhdGU=\n-----END CERTIFICATE-----\n",
            pki.server.cert_pem
        ),
    );
    let mut config = pki.config(Vec::new());
    if let Some(tls) = config.server.tls.as_mut() {
        tls.cert_path = chain.clone();
    }
    let error = startup_error(config);
    assert!(error.contains("certificate 2 cannot"), "{error}");
    assert!(error.contains(&chain.display().to_string()), "{error}");
}
