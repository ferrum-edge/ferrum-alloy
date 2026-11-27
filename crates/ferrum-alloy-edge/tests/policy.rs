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
use ferrum_alloy_telemetry::peer::{
    ClassifiedPeer, PeerInfo, PeerTrust, TlsPeer, TrustClassifier, TrustedPeers, TrustedPeersConfig,
};
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
    let mut tls = TlsPeer::new(true);
    tls.spiffe_id = Some(GATEWAY.into());
    PeerInfo::new("198.51.100.2:443".parse::<SocketAddr>().unwrap()).with_tls(tls)
}

fn network_trusted() -> PeerInfo {
    PeerInfo::new("10.1.2.3:443".parse::<SocketAddr>().unwrap())
}

fn untrusted() -> PeerInfo {
    PeerInfo::new("203.0.113.5:443".parse::<SocketAddr>().unwrap())
}

#[derive(Debug, Clone)]
struct Seen {
    context: Option<GatewayContext>,
    verification: Option<GatewayVerification>,
    username_header: Option<String>,
    headers: http::HeaderMap,
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
                headers: req.headers().clone(),
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
        context.edge_peer_address.unwrap().to_string(),
        "203.0.113.44"
    );
    assert_eq!(
        seen.verification,
        Some(GatewayVerification::Verified {
            identity: GATEWAY.into()
        })
    );
}

/// Forged copies of every gateway-asserted header, not just the two
/// Consumer names (GHSA-hh7p-48xw-gwqg), in Edge's spellings.
const FORGED: &[(&str, &str)] = &[
    ("x-authenticated-identity", "admin@example.com"),
    ("x-consumer-role", "admin"),
    ("X_Consumer_Username", "mallory"),
    ("x-geo-country", "US"),
    (
        "x-ferrum-peer-spiffe-id",
        "spiffe://ferrum.test/ns/pay/sa/payments",
    ),
    (
        "x-ferrum-source-principal",
        "spiffe://ferrum.test/ns/pay/sa/payments",
    ),
    ("x-ferrum-hops", "1"),
    ("X_Path_Param_Id", "someone-else"),
    ("x-api-version", "2"),
];

fn assertion_names(headers: &http::HeaderMap) -> Vec<String> {
    headers
        .keys()
        .filter(|name| ferrum_alloy_edge::contract::is_gateway_written_header(name.as_str()))
        .map(|name| name.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn every_forged_gateway_assertion_is_removed_without_a_verified_gateway() {
    for (label, mode, accept, peer) in [
        ("standalone", DeploymentMode::Standalone, false, untrusted()),
        (
            "preferred",
            DeploymentMode::GatewayPreferred,
            true,
            untrusted(),
        ),
        (
            "network",
            DeploymentMode::GatewayPreferred,
            true,
            network_trusted(),
        ),
        ("exempt", DeploymentMode::GatewayRequired, true, untrusted()),
        (
            "handoff off",
            DeploymentMode::GatewayPreferred,
            false,
            verified(),
        ),
    ] {
        let policy = policy(mode, accept);
        let stats = policy.stats();
        let (status, seen, _) = run(policy, request("/readyz", peer, FORGED)).await;
        assert_eq!(status, 200, "{label}");
        let seen = seen.unwrap();
        assert!(seen.context.is_none(), "{label}");
        assert_eq!(
            assertion_names(&seen.headers),
            Vec::<String>::new(),
            "{label}"
        );
        assert_eq!(
            seen.headers.get("x-api-version").unwrap(),
            "2",
            "{label}: ordinary headers stay"
        );
        assert_eq!(
            stats.stripped_unverified_identity.load(Ordering::Relaxed),
            1,
            "{label}"
        );
    }
}

/// Driven by the pinned vocabulary, so a header it adds is covered here
/// as soon as `contract::GATEWAY_ASSERTION_HEADERS` lists it.
#[tokio::test]
async fn every_pinned_gateway_assertion_header_is_removed_from_direct_callers() {
    let vocabulary: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../contracts/ferrum-contracts/vocabularies/gateway-headers.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let names: Vec<String> = vocabulary["headers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| {
            entry["role"] == "gateway_assertion"
                || (entry["role"] == "ferrum_owned" && entry["direction"] == "gateway_to_backend")
        })
        .map(|entry| {
            let name = entry["name"].as_str().unwrap().to_ascii_lowercase();
            if entry["match"] == "prefix" {
                format!("{name}groups")
            } else {
                name
            }
        })
        .collect();
    assert!(names.len() >= 11, "{names:?}");
    assert!(
        names.contains(&"x-path-param-groups".to_owned()),
        "{names:?}"
    );
    let forged: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "forged")).collect();
    let (_, seen, _) = run(
        policy(DeploymentMode::GatewayPreferred, true),
        request("/x", untrusted(), &forged),
    )
    .await;
    let seen = seen.unwrap();
    for name in &names {
        assert!(
            !seen.headers.contains_key(name.as_str()),
            "{name} reached the handler"
        );
    }
}

#[tokio::test]
async fn verified_gateway_hands_off_the_external_authenticated_identity() {
    let (_, seen, _) = run(
        policy(DeploymentMode::GatewayPreferred, true),
        request("/x", verified(), FORGED),
    )
    .await;
    let seen = seen.unwrap();
    let context = seen.context.unwrap();
    assert_eq!(
        context.authenticated_identity.as_deref(),
        Some("admin@example.com")
    );
    assert_eq!(context.consumer_username, None);
    assert_eq!(
        seen.headers.get("x-geo-country").unwrap(),
        "US",
        "a verified gateway's assertions reach the handler"
    );
}

fn policy_with_trusted_proxies(trusted: &[&str]) -> EdgePolicy {
    let mut trust = TrustedPeersConfig::default();
    trust.identities = vec![GATEWAY.into()];
    let peers = TrustedPeers::new(&trust).unwrap();
    let mut config = EdgePolicyConfig::default();
    config.mode = DeploymentMode::GatewayPreferred;
    config.accept_consumer_identity = true;
    config.trusted_proxies = trusted.iter().map(|net| net.parse().unwrap()).collect();
    EdgePolicy::new(config, Arc::new(peers))
}

/// Edge's outbound `X-Forwarded-For` forms (`build_xff_value`) resolved the
/// way Edge resolves them (`resolve_client_ip_field_lines`) with the same
/// trusted list (#308, sol F1): Edge appends the address it accepted the
/// connection from, so the walk starts at Edge's peer and stops at the
/// first untrusted address. A position-based pick would hand out a
/// caller-chosen entry whenever the number of trusted hops varies.
#[tokio::test]
async fn client_address_repeats_edges_forwarded_for_resolution() {
    const LB: &[&str] = &["10.0.0.0/8"];
    const CDN_AND_LB: &[&str] = &["10.0.0.0/8", "192.0.2.0/24"];
    for (row, trusted, xff, client, peer) in [
        // No proxy in front of Edge: `peer`.
        (
            "direct",
            &[][..],
            vec!["203.0.113.7"],
            Some("203.0.113.7"),
            Some("203.0.113.7"),
        ),
        // Trusted LB appending to a caller-supplied chain: `spoof, client, lb`.
        (
            "lb with xff",
            LB,
            vec!["198.51.100.1, 203.0.113.7, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        // A caller that pads the chain cannot move the boundary: the walk
        // stops at the address the trusted LB vouched for.
        (
            "padded chain",
            LB,
            vec!["192.0.2.99, 198.51.100.1, 203.0.113.7, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        // A request that reached Edge through the CDN and the LB.
        (
            "cdn and lb",
            CDN_AND_LB,
            vec!["203.0.113.7, 192.0.2.10, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        // The same deployment, a request that skipped the CDN.
        (
            "lb only",
            CDN_AND_LB,
            vec!["203.0.113.8, 10.0.0.5"],
            Some("203.0.113.8"),
            Some("10.0.0.5"),
        ),
        // A request that bypassed the balancer: Edge dropped its chain and
        // wrote only the untrusted peer, which Edge resolved as the client.
        (
            "bypassed lb",
            LB,
            vec!["198.51.100.66"],
            Some("198.51.100.66"),
            Some("198.51.100.66"),
        ),
        // Without the trusted list the walk stops at Edge's peer.
        (
            "lb, list empty",
            &[][..],
            vec!["203.0.113.7, 10.0.0.5"],
            Some("10.0.0.5"),
            Some("10.0.0.5"),
        ),
        // Only trusted entries: Edge falls back to its peer.
        (
            "all trusted",
            LB,
            vec!["10.0.0.9, 10.0.0.5"],
            Some("10.0.0.5"),
            Some("10.0.0.5"),
        ),
        // A malformed boundary falls back to the peer, as in Edge, never to a
        // caller-chosen neighbour further left.
        (
            "malformed boundary",
            LB,
            vec!["198.51.100.1, unknown, 10.0.0.5"],
            Some("10.0.0.5"),
            Some("10.0.0.5"),
        ),
        // Field lines are one chain in order; empty entries are skipped.
        (
            "split lines",
            LB,
            vec!["2001:db8::7", ", 10.0.0.5"],
            Some("2001:db8::7"),
            Some("10.0.0.5"),
        ),
        // Obs-text in a caller-supplied entry or field line is one malformed
        // entry left of the boundary, as in Edge, not a reason to drop the
        // gateway-written suffix (round 2 sol F1).
        (
            "obs-text entry",
            LB,
            vec!["caf\u{e9}, 203.0.113.7, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        (
            "obs-text line",
            LB,
            vec!["caf\u{e9}", "203.0.113.7, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        // Edge's CidrSet reads an IPv4-mapped rule of /96 or longer as the
        // IPv4 network, and a shorter one as matching nothing (round 2 sol F2).
        (
            "mapped rule",
            &["::ffff:10.0.0.0/104"][..],
            vec!["203.0.113.7, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        (
            "short mapped rule",
            &["::ffff:0:0/80"][..],
            vec!["203.0.113.7, 10.0.0.5"],
            Some("10.0.0.5"),
            Some("10.0.0.5"),
        ),
        // A mapped query matches an IPv4 rule by its embedded address, and
        // addresses are reported canonically, as Edge's identities are.
        (
            "mapped lb",
            LB,
            vec!["203.0.113.7, ::ffff:10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        (
            "mapped client",
            LB,
            vec!["203.0.113.7, ::ffff:203.0.113.7, 10.0.0.5"],
            Some("203.0.113.7"),
            Some("10.0.0.5"),
        ),
        // A native IPv4 address never matches an IPv6 rule.
        (
            "v6 rule, v4 lb",
            &["::/0"][..],
            vec!["203.0.113.7, 10.0.0.5"],
            Some("10.0.0.5"),
            Some("10.0.0.5"),
        ),
    ] {
        let headers: Vec<(&str, &str)> =
            xff.iter().map(|line| ("x-forwarded-for", *line)).collect();
        let (_, seen, _) = run(
            policy_with_trusted_proxies(trusted),
            request("/x", verified(), &headers),
        )
        .await;
        let context = seen.unwrap().context.unwrap();
        assert_eq!(
            context.client_address.map(|a| a.to_string()).as_deref(),
            client,
            "{row}"
        );
        assert_eq!(
            context.edge_peer_address.map(|a| a.to_string()).as_deref(),
            peer,
            "{row}"
        );
    }
}

/// Edge's real-IP-header mode keeps a trusted LB's passed-through chain and
/// seeds the authoritative client only when the chain lacks it, so the chain
/// alone cannot show which entry Edge chose (sol F1). Alloy cannot repeat
/// that resolution; the documented setting for this layout is an empty
/// trusted list, which reports Edge's peer, never a caller-chosen entry.
#[tokio::test]
async fn real_ip_header_layouts_never_report_a_caller_chosen_entry() {
    // Edge resolved 203.0.113.7 from the LB's real-IP header; the caller
    // supplied `203.0.113.7, 192.0.2.99`.
    let headers = [("x-forwarded-for", "203.0.113.7, 192.0.2.99, 10.0.0.5")];
    let (_, seen, _) = run(
        policy_with_trusted_proxies(&[]),
        request("/x", verified(), &headers),
    )
    .await;
    let context = seen.unwrap().context.unwrap();
    assert_eq!(context.client_address.unwrap().to_string(), "10.0.0.5");
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
async fn exempt_paths_match_the_exact_path_only() {
    let policy = policy(DeploymentMode::GatewayRequired, true);
    for path in [
        "/readyz/",
        "//readyz",
        "/%72eadyz",
        "/READYZ",
        "/readyz/../orders",
    ] {
        let (status, seen, _) = run(policy.clone(), request(path, untrusted(), &[])).await;
        assert_eq!(status, 403, "{path} is not the exempt path");
        assert!(seen.is_none(), "{path} must not reach the handler");
    }
    let (status, seen, _) = run(policy, request("/readyz?x=1", untrusted(), &[])).await;
    assert_eq!(status, 200, "the query is not part of the path");
    assert!(seen.is_some());
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

/// Counts classifications while delegating to the real peers policy.
#[derive(Debug)]
struct CountingClassifier {
    peers: TrustedPeers,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl TrustClassifier for CountingClassifier {
    fn classify(&self, extensions: &http::Extensions) -> PeerTrust {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.peers.classify(extensions)
    }
}

/// A transport-provided per-connection classification is reused, not
/// recomputed: the classifier runs only when no `ClassifiedPeer` is
/// present, so Edge and telemetry judge by one classification.
#[tokio::test]
async fn a_preclassified_peer_skips_the_edge_classifier() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut trust = TrustedPeersConfig::default();
    trust.identities = vec![GATEWAY.into()];
    let counting = CountingClassifier {
        peers: TrustedPeers::new(&trust).unwrap(),
        calls: Arc::clone(&calls),
    };
    let mut config = EdgePolicyConfig::default();
    config.mode = DeploymentMode::GatewayRequired;
    config.accept_consumer_identity = true;
    let classifier: ferrum_alloy_telemetry::peer::SharedClassifier = Arc::new(counting);
    let policy = EdgePolicy::new(config, Arc::clone(&classifier));

    // The peer is untrusted on its own, but the transport already
    // classified the connection as the verified gateway with the very
    // classifier this policy holds.
    let mut preclassified = request("/x", untrusted(), IDENTITY);
    preclassified.extensions_mut().insert(ClassifiedPeer::new(
        Arc::clone(&classifier),
        PeerTrust::VerifiedIdentity(GATEWAY.into()),
    ));
    let (status, seen, _) = run(policy.clone(), preclassified).await;
    assert_eq!(status, http::StatusCode::OK);
    assert!(seen.unwrap().context.is_some(), "identity was accepted");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the classifier must not run when a classification is present"
    );

    // Without one, the classifier runs and the same peer is refused.
    let (status, _, _) = run(policy, request("/x", untrusted(), IDENTITY)).await;
    assert_eq!(status, http::StatusCode::FORBIDDEN);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the fallback classification runs exactly once"
    );
}

/// A cached classification from a *different, broader* classifier must
/// not satisfy this policy: the server trusts gateway G and service S,
/// this policy trusts only G, and S's cached `VerifiedIdentity` used to
/// pass `gateway_required` without the policy's classifier running (#214).
#[tokio::test]
async fn cached_trust_from_another_classifier_does_not_satisfy_a_stricter_policy() {
    const SERVICE: &str = "spiffe://ferrum.test/ns/shop/sa/orders";
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut trust = TrustedPeersConfig::default();
    trust.identities = vec![GATEWAY.into()];
    let counting = CountingClassifier {
        peers: TrustedPeers::new(&trust).unwrap(),
        calls: Arc::clone(&calls),
    };
    let mut config = EdgePolicyConfig::default();
    config.mode = DeploymentMode::GatewayRequired;
    config.accept_consumer_identity = true;
    let policy = EdgePolicy::new(config, Arc::new(counting));

    // The broader server classifier trusts the service identity too.
    let mut broad_config = TrustedPeersConfig::default();
    broad_config.identities = vec![GATEWAY.into(), SERVICE.into()];
    let broader: ferrum_alloy_telemetry::peer::SharedClassifier =
        Arc::new(TrustedPeers::new(&broad_config).unwrap());

    let mut service_tls = TlsPeer::new(true);
    service_tls.spiffe_id = Some(SERVICE.into());
    let service_peer =
        PeerInfo::new("198.51.100.7:443".parse::<SocketAddr>().unwrap()).with_tls(service_tls);
    let mut preclassified = request("/x", service_peer, IDENTITY);
    preclassified.extensions_mut().insert(ClassifiedPeer::new(
        broader,
        PeerTrust::VerifiedIdentity(SERVICE.into()),
    ));
    let (status, seen, _) = run(policy, preclassified).await;
    assert_eq!(status, http::StatusCode::FORBIDDEN);
    assert!(seen.is_none(), "the handler never runs");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the policy's own classifier runs instead of trusting the cache"
    );
}
