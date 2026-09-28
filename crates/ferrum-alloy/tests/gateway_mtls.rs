//! Verified gateway identity over real mTLS: trace context and consumer
//! identity are honored only from the configured SPIFFE identity; forged
//! headers and wrong identities are not.

#![cfg(all(feature = "tls", feature = "edge"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::{AlloyConfig, ClientAuth, EdgeMode, TlsSettings};
use ferrum_alloy::edge::GatewayContext;
use ferrum_alloy::telemetry::RequestContext;
use http::Request;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use support::pki::{self, Ca};

const GATEWAY: &str = "spiffe://ferrum.test/ns/edge/sa/gateway";
const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn router() -> Router {
    Router::new().route(
        "/whoami",
        get(|context: RequestContext, gateway: Option<GatewayContext>, headers: http::HeaderMap| async move {
            serde_json::json!({
                "peer_trust": context.peer_trust.label(),
                "trace_decision": context.trace_decision.as_str(),
                "trace_id": context.trace_id.to_hex(),
                "consumer": gateway.as_ref().and_then(|g| g.consumer_username.clone()),
                "gateway": gateway.as_ref().map(|g| g.gateway_identity.clone()),
                "client_address": gateway.as_ref().and_then(|g| g.client_address.map(|a| a.to_string())),
                "raw_consumer_header": headers.get("x-consumer-username").map(|v| v.to_str().unwrap().to_owned()),
            })
            .to_string()
        }),
    )
}

struct Pki {
    _dir: tempfile::TempDir,
    ca: Ca,
    tls: TlsSettings,
}

fn pki() -> Pki {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("alloy-test-ca");
    let server = ca.server();
    let mut tls = TlsSettings::new(
        pki::write(dir.path(), "server.pem", &server.cert_pem),
        pki::write(dir.path(), "server.key", &server.key_pem),
    );
    tls.client_ca_path = Some(pki::write(dir.path(), "ca.pem", &ca.cert_pem));
    tls.client_auth = ClientAuth::Optional;
    tls.handshake_timeout_ms = 2_000;
    tls.reload_interval_ms = 0;
    Pki { _dir: dir, ca, tls }
}

fn config(pki: &Pki, mode: EdgeMode) -> AlloyConfig {
    let mut config = support::config();
    config.server.tls = Some(pki.tls.clone());
    config.trust.identities = vec![GATEWAY.into()];
    config.edge.mode = mode;
    config.edge.accept_consumer_identity = true;
    config
}

struct Reply {
    status: http::StatusCode,
    body: String,
}

async fn tls_get(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<Reply, String> {
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let connector = tokio_rustls::TlsConnector::from(client);
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let stream = connector
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    let mut request = Request::get(path).header("host", "localhost");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok(Reply {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn json(reply: &Reply) -> serde_json::Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
}

#[tokio::test]
async fn the_verified_gateway_identity_is_trusted_end_to_end() {
    let pki = pki();
    let server = support::start(
        AlloyApp::new("mtls").router(router()),
        config(&pki, EdgeMode::GatewayRequired),
    )
    .await;
    let gateway = pki.ca.client(GATEWAY);
    let reply = tls_get(
        server.addr,
        pki::client_config(&pki.ca, Some(&gateway)),
        "/whoami",
        &[
            ("traceparent", TRACEPARENT),
            ("x-consumer-username", "alice"),
            ("x-forwarded-for", "198.51.100.7, 203.0.113.9"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = json(&reply);
    assert_eq!(body["peer_trust"], "verified_identity");
    assert_eq!(body["gateway"], GATEWAY);
    assert_eq!(body["trace_decision"], "accepted_remote");
    assert_eq!(body["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(body["consumer"], "alice");
    assert_eq!(
        body["client_address"], "203.0.113.9",
        "rightmost hop, written by the gateway"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn gateway_required_rejects_direct_callers_but_not_health_probes() {
    let pki = pki();
    let server = support::start(
        AlloyApp::new("mtls").router(router()),
        config(&pki, EdgeMode::GatewayRequired),
    )
    .await;
    let anonymous = pki::client_config(&pki.ca, None);
    let reply = tls_get(
        server.addr,
        Arc::clone(&anonymous),
        "/whoami",
        &[("x-consumer-username", "mallory")],
    )
    .await
    .unwrap();
    assert_eq!(reply.status, 403);
    assert_eq!(
        json(&reply)["type"],
        "tag:ferrumedge.com,2026:alloy/problem/gateway-required"
    );
    let ready = tls_get(server.addr, anonymous, "/readyz", &[])
        .await
        .unwrap();
    assert_eq!(ready.status, 200, "health paths are exempt: {}", ready.body);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_different_identity_from_the_same_ca_is_not_the_gateway() {
    let pki = pki();
    let server = support::start(
        AlloyApp::new("mtls").router(router()),
        config(&pki, EdgeMode::GatewayPreferred),
    )
    .await;
    let impostor = pki.ca.client("spiffe://ferrum.test/ns/apps/sa/billing");
    let reply = tls_get(
        server.addr,
        pki::client_config(&pki.ca, Some(&impostor)),
        "/whoami",
        &[
            ("traceparent", TRACEPARENT),
            ("x-consumer-username", "alice"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(reply.status, 200);
    let body = json(&reply);
    assert_eq!(body["peer_trust"], "untrusted");
    assert_eq!(body["trace_decision"], "rerooted_untrusted");
    assert_eq!(body["consumer"], serde_json::Value::Null);
    assert_eq!(
        body["raw_consumer_header"],
        serde_json::Value::Null,
        "forged header removed"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn direct_requests_still_work_in_preferred_mode_without_forged_identity() {
    let pki = pki();
    let server = support::start(
        AlloyApp::new("mtls").router(router()),
        config(&pki, EdgeMode::GatewayPreferred),
    )
    .await;
    let reply = tls_get(
        server.addr,
        pki::client_config(&pki.ca, None),
        "/whoami",
        &[
            ("x-consumer-username", "mallory"),
            ("x-consumer-custom-id", "admin"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(reply.status, 200);
    let body = json(&reply);
    assert_eq!(body["peer_trust"], "untrusted");
    assert_eq!(body["raw_consumer_header"], serde_json::Value::Null);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn certificates_from_another_ca_fail_the_handshake() {
    let pki = pki();
    let server = support::start(
        AlloyApp::new("mtls").router(router()),
        config(&pki, EdgeMode::GatewayPreferred),
    )
    .await;
    let rogue_ca = Ca::new("rogue");
    let rogue = rogue_ca.client(GATEWAY);
    // The client trusts the real server CA but presents a rogue client cert.
    let result = tls_get(
        server.addr,
        pki::client_config(&pki.ca, Some(&rogue)),
        "/whoami",
        &[],
    )
    .await;
    assert!(
        result.is_err(),
        "a certificate from an untrusted CA must be refused"
    );
    // The server stays healthy for legitimate peers.
    let gateway = pki.ca.client(GATEWAY);
    let ok = tls_get(
        server.addr,
        pki::client_config(&pki.ca, Some(&gateway)),
        "/whoami",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(ok.status, 200);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn required_client_auth_refuses_anonymous_handshakes() {
    let pki = pki();
    let mut cfg = config(&pki, EdgeMode::GatewayRequired);
    if let Some(tls) = cfg.server.tls.as_mut() {
        tls.client_auth = ClientAuth::Required;
    }
    let server = support::start(AlloyApp::new("mtls").router(router()), cfg).await;
    let result = tls_get(
        server.addr,
        pki::client_config(&pki.ca, None),
        "/readyz",
        &[],
    )
    .await;
    assert!(result.is_err(), "no client certificate, no handshake");
    server.shutdown().await.unwrap();
}

#[test]
fn tls_misconfiguration_fails_at_startup() {
    let pki = pki();
    let mut cfg = config(&pki, EdgeMode::GatewayPreferred);
    if let Some(tls) = cfg.server.tls.as_mut() {
        tls.cert_path = "/nonexistent/cert.pem".into();
    }
    let error = AlloyApp::new("mtls")
        .router(router())
        .config(cfg)
        .telemetry(ferrum_alloy::TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap_err();
    assert!(error.to_string().contains("certificate chain"), "{error}");
}
