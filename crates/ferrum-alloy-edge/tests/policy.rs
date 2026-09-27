//! Gateway trust policy: identity handoff only from verified identities.

#![cfg(feature = "axum")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use ferrum_alloy_edge::{
    DeploymentMode, EdgeLayer, EdgePolicy, EdgePolicyConfig, GatewayContext, GatewayVerification,
};
use ferrum_alloy_telemetry::peer::{PeerInfo, TlsPeer, TrustedPeers, TrustedPeersConfig};
use http::{Request, Response};
use http_body_util::BodyExt;
use tower::{Layer, ServiceExt, service_fn};

const GATEWAY: &str = "spiffe://ferrum.test/ns/edge/sa/gateway";

fn policy(mode: DeploymentMode, accept: bool) -> EdgePolicy {
    let mut trust = TrustedPeersConfig::default();
    trust.identities = vec![GATEWAY.into()];
    trust.networks = vec!["10.0.0.0/8".parse().unwrap()];
    let peers = TrustedPeers::new(&trust).unwrap();
    let mut config = EdgePolicyConfig::default();
    config.mode = mode;
    config.accept_consumer_identity = accept;
    EdgePolicy::new(config, Arc::new(peers)).with_exempt_paths(vec!["/readyz".into()])
}

fn request(path: &str, peer: PeerInfo, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::get(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    request.extensions_mut().insert(peer);
    request
}

fn verified() -> PeerInfo {
    PeerInfo {
        remote_addr: Some("198.51.100.2:443".parse::<SocketAddr>().unwrap()),
        tls: Some(TlsPeer {
            client_cert_verified: true,
            spiffe_id: Some(GATEWAY.into()),
            dns_names: vec![],
        }),
    }
}

fn network_trusted() -> PeerInfo {
    PeerInfo {
        remote_addr: Some("10.1.2.3:443".parse::<SocketAddr>().unwrap()),
        tls: None,
    }
}

fn untrusted() -> PeerInfo {
    PeerInfo {
        remote_addr: Some("203.0.113.5:443".parse::<SocketAddr>().unwrap()),
        tls: None,
    }
}

#[derive(Debug, Clone)]
struct Seen {
    context: Option<GatewayContext>,
    verification: Option<GatewayVerification>,
    username_header: Option<String>,
}

async fn run(
    policy: EdgePolicy,
    request: Request<Body>,
) -> (http::StatusCode, Option<Seen>, String) {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let captured = Arc::clone(&seen);
    let service = EdgeLayer::new(policy).layer(service_fn(move |req: Request<Body>| {
        let captured = Arc::clone(&captured);
        async move {
            *captured.lock().unwrap() = Some(Seen {
                context: req.extensions().get::<GatewayContext>().cloned(),
                verification: req.extensions().get::<GatewayVerification>().cloned(),
                username_header: req
                    .headers()
                    .get("x-consumer-username")
                    .map(|v| v.to_str().unwrap().to_owned()),
            });
            Ok::<_, Infallible>(Response::new(Body::empty()))
        }
    }));
    let response = service.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let seen = seen.lock().unwrap().clone();
    (status, seen, String::from_utf8_lossy(&body).into_owned())
}

const IDENTITY: &[(&str, &str)] = &[
    ("x-consumer-username", "alice"),
    ("x-consumer-custom-id", "cust-7"),
    ("x-forwarded-for", "192.0.2.1, 203.0.113.44"),
];

#[tokio::test]
async fn verified_gateway_identity_is_handed_off() {
    let (status, seen, _) = run(
        policy(DeploymentMode::GatewayPreferred, true),
        request("/x", verified(), IDENTITY),
    )
    .await;
    assert_eq!(status, 200);
    let seen = seen.unwrap();
    let context = seen.context.unwrap();
    assert_eq!(context.gateway_identity, GATEWAY);
    assert_eq!(context.consumer_username.as_deref(), Some("alice"));
    assert_eq!(context.consumer_custom_id.as_deref(), Some("cust-7"));
    assert_eq!(context.client_address.unwrap().to_string(), "203.0.113.44");
    assert_eq!(
        seen.verification,
        Some(GatewayVerification::Verified {
            identity: GATEWAY.into()
        })
    );
}

#[tokio::test]
async fn network_trust_never_authorizes_identity_headers() {
    let policy = policy(DeploymentMode::GatewayPreferred, true);
    let stats = policy.stats();
    let (_, seen, _) = run(policy, request("/x", network_trusted(), IDENTITY)).await;
    let seen = seen.unwrap();
    assert!(seen.context.is_none());
    assert_eq!(seen.username_header, None, "header removed");
    assert_eq!(seen.verification, Some(GatewayVerification::NetworkTrusted));
    assert_eq!(
        stats.stripped_unverified_identity.load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn standalone_mode_still_removes_forged_identity() {
    let (status, seen, _) = run(
        policy(DeploymentMode::Standalone, false),
        request("/x", untrusted(), IDENTITY),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(seen.unwrap().username_header, None);
}

#[tokio::test]
async fn identity_handoff_can_be_disabled_even_for_the_gateway() {
    let (_, seen, _) = run(
        policy(DeploymentMode::GatewayPreferred, false),
        request("/x", verified(), IDENTITY),
    )
    .await;
    let seen = seen.unwrap();
    assert!(seen.context.is_none());
    assert_eq!(seen.username_header, None);
}

#[tokio::test]
async fn gateway_required_rejects_unverified_peers_with_a_problem() {
    let policy = policy(DeploymentMode::GatewayRequired, true);
    let stats = policy.stats();
    for peer in [untrusted(), network_trusted()] {
        let (status, seen, body) = run(policy.clone(), request("/orders", peer, &[])).await;
        assert_eq!(status, 403);
        assert!(seen.is_none(), "the handler must not run");
        let problem: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(problem["status"], 403);
        assert_eq!(
            problem["type"],
            ferrum_alloy_edge::policy::GATEWAY_REQUIRED_TYPE
        );
    }
    assert_eq!(stats.rejected_without_gateway.load(Ordering::Relaxed), 2);
    let (status, seen, _) = run(policy.clone(), request("/readyz", untrusted(), &[])).await;
    assert_eq!(status, 200, "exempt health path");
    assert!(seen.is_some());
    let (status, _, _) = run(policy, request("/orders", verified(), &[])).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn unverified_certificates_and_ambiguous_values_are_not_identity() {
    let mut forged = verified();
    if let Some(tls) = forged.tls.as_mut() {
        tls.client_cert_verified = false;
    }
    let (_, seen, _) = run(
        policy(DeploymentMode::GatewayPreferred, true),
        request("/x", forged, IDENTITY),
    )
    .await;
    assert!(seen.unwrap().context.is_none());

    let long = "a".repeat(600);
    let (_, seen, _) = run(
        policy(DeploymentMode::GatewayPreferred, true),
        request(
            "/x",
            verified(),
            &[
                ("x-consumer-username", "alice"),
                ("x-consumer-username", "bob"),
                ("x-consumer-custom-id", long.as_str()),
            ],
        ),
    )
    .await;
    let context = seen.unwrap().context.unwrap();
    assert_eq!(
        context.consumer_username, None,
        "duplicate values are ambiguous"
    );
    assert_eq!(
        context.consumer_custom_id, None,
        "oversized values are rejected"
    );
}
