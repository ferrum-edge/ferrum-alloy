//! Trace context, request id, and peer trust decisions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::field_reassign_with_default
)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use ferrum_alloy_telemetry::peer::{PeerInfo, TlsPeer, TrustedPeers, TrustedPeersConfig};
use ferrum_alloy_telemetry::trace_context::{
    TraceContextError, parse_traceparent, validate_tracestate,
};
use ferrum_alloy_telemetry::{
    AcceptPolicy, PeerTrust, RequestContext, ServerTimingPolicy, TelemetryConfig, TelemetryLayer,
    TraceDecision,
};
use http::{HeaderMap, Request, Response};
use http_body_util::Empty;
use tower::{Layer, ServiceExt, service_fn};

const INCOMING: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

struct Seen {
    context: RequestContext,
    headers: HeaderMap,
}

async fn run(
    layer: TelemetryLayer,
    request: Request<Empty<Bytes>>,
    response_headers: &[(&'static str, &'static str)],
) -> (Seen, Response<()>) {
    let seen = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&seen);
    let response_headers: Vec<_> = response_headers.to_vec();
    let service = layer.layer(service_fn(move |req: Request<Empty<Bytes>>| {
        let captured = Arc::clone(&captured);
        let response_headers = response_headers.clone();
        async move {
            *captured.lock().unwrap() = Some(Seen {
                context: req.extensions().get::<RequestContext>().unwrap().clone(),
                headers: req.headers().clone(),
            });
            let mut response = Response::new(Empty::<Bytes>::new());
            for (name, value) in response_headers {
                response.headers_mut().insert(name, value.parse().unwrap());
            }
            Ok::<_, Infallible>(response)
        }
    }));
    let response = service.oneshot(request).await.unwrap();
    let (parts, _) = response.into_parts();
    let seen = seen.lock().unwrap().take().unwrap();
    (seen, Response::from_parts(parts, ()))
}

fn request_from(
    addr: &str,
    tls: Option<TlsPeer>,
    headers: &[(&str, &str)],
) -> Request<Empty<Bytes>> {
    let mut request = Request::builder().uri("/");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Empty::new()).unwrap();
    request.extensions_mut().insert(PeerInfo {
        remote_addr: Some(addr.parse::<SocketAddr>().unwrap()),
        tls,
    });
    request
}

fn trusted_layer(config: TelemetryConfig) -> TelemetryLayer {
    let peers = TrustedPeers::new(&TrustedPeersConfig {
        identities: vec!["spiffe://ferrum.test/ns/edge/sa/gateway".into()],
        networks: vec!["10.0.0.0/8".parse().unwrap()],
    })
    .unwrap();
    TelemetryLayer::new(config)
        .unwrap()
        .with_classifier(Arc::new(peers))
}

#[tokio::test]
async fn untrusted_trace_context_is_rerooted_and_not_forwarded() {
    let layer = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let metrics = layer.metrics();
    let request = request_from(
        "203.0.113.9:4000",
        None,
        &[("traceparent", INCOMING), ("tracestate", "vendor=value")],
    );
    let (seen, _) = run(layer, request, &[]).await;
    assert_eq!(
        seen.context.trace_decision,
        TraceDecision::RerootedUntrusted
    );
    assert_ne!(
        seen.context.trace_id.to_hex(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert!(
        !seen.context.sampled,
        "an untrusted sampled flag must not force sampling"
    );
    assert!(seen.context.remote_parent.is_none());
    assert!(
        !seen.headers.contains_key("traceparent"),
        "rejected context must not reach the handler"
    );
    assert!(!seen.headers.contains_key("tracestate"));
    assert_eq!(metrics.trace_decisions.get("rerooted_untrusted"), 1);
}

#[tokio::test]
async fn trusted_network_peer_becomes_the_parent() {
    let request = request_from("10.1.2.3:4000", None, &[("traceparent", INCOMING)]);
    let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert_eq!(seen.context.trace_decision, TraceDecision::AcceptedRemote);
    assert_eq!(
        seen.context.trace_id.to_hex(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_ne!(
        seen.context.span_id.to_hex(),
        "00f067aa0ba902b7",
        "a fresh span id per hop"
    );
    assert!(seen.context.sampled);
    assert!(matches!(
        seen.context.peer_trust,
        PeerTrust::NetworkBoundary(_)
    ));
    let child = seen.context.child_traceparent(seen.context.span_id);
    assert_eq!(child.trace_id, seen.context.trace_id);
}

#[tokio::test]
async fn ipv4_mapped_ipv6_peers_match_ipv4_networks() {
    let request = request_from("[::ffff:10.9.8.7]:4000", None, &[("traceparent", INCOMING)]);
    let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert_eq!(seen.context.trace_decision, TraceDecision::AcceptedRemote);
}

#[tokio::test]
async fn verified_spiffe_identity_is_trusted_but_unverified_certificates_are_not() {
    let identity = |verified| TlsPeer {
        client_cert_verified: verified,
        spiffe_id: Some("spiffe://ferrum.test/ns/edge/sa/gateway".into()),
        dns_names: vec![],
    };
    let request = request_from(
        "198.51.100.4:4000",
        Some(identity(true)),
        &[("traceparent", INCOMING)],
    );
    let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert_eq!(
        seen.context.peer_trust,
        PeerTrust::VerifiedIdentity("spiffe://ferrum.test/ns/edge/sa/gateway".into())
    );
    assert_eq!(seen.context.trace_decision, TraceDecision::AcceptedRemote);

    let request = request_from(
        "198.51.100.4:4000",
        Some(identity(false)),
        &[("traceparent", INCOMING)],
    );
    let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert_eq!(seen.context.peer_trust, PeerTrust::Untrusted);
    assert_eq!(
        seen.context.trace_decision,
        TraceDecision::RerootedUntrusted
    );
}

#[tokio::test]
async fn wrong_gateway_identity_is_untrusted() {
    let tls = TlsPeer {
        client_cert_verified: true,
        spiffe_id: Some("spiffe://ferrum.test/ns/other/sa/impostor".into()),
        dns_names: vec!["gateway".into()],
    };
    let request = request_from("198.51.100.4:4000", Some(tls), &[("traceparent", INCOMING)]);
    let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert_eq!(seen.context.peer_trust, PeerTrust::Untrusted);
}

#[tokio::test]
async fn forwarded_headers_never_establish_trust() {
    let request = request_from(
        "203.0.113.9:4000",
        None,
        &[
            ("traceparent", INCOMING),
            ("x-forwarded-for", "10.0.0.1"),
            ("forwarded", "for=10.0.0.1"),
            ("x-real-ip", "10.0.0.1"),
        ],
    );
    let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert_eq!(seen.context.peer_trust, PeerTrust::Untrusted);
}

#[tokio::test]
async fn malformed_and_duplicate_trace_context_is_rerooted() {
    for headers in [
        vec![(
            "traceparent",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
        )],
        vec![("traceparent", INCOMING), ("traceparent", INCOMING)],
        vec![("traceparent", "garbage")],
    ] {
        let request = request_from("10.1.2.3:4000", None, &headers);
        let (seen, _) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
        assert_eq!(
            seen.context.trace_decision,
            TraceDecision::RerootedInvalid,
            "{headers:?}"
        );
    }
}

#[tokio::test]
async fn never_policy_ignores_even_trusted_context() {
    let mut config = TelemetryConfig::default();
    config.trace_context.accept_incoming = AcceptPolicy::Never;
    let request = request_from("10.1.2.3:4000", None, &[("traceparent", INCOMING)]);
    let (seen, _) = run(trusted_layer(config), request, &[]).await;
    assert_eq!(seen.context.trace_decision, TraceDecision::IgnoredByPolicy);
}

#[tokio::test]
async fn valid_request_ids_are_kept_and_echoed() {
    let request = request_from(
        "203.0.113.9:1",
        None,
        &[("x-request-id", "edge-req.42_A-b")],
    );
    let (seen, response) = run(
        TelemetryLayer::new(TelemetryConfig::default()).unwrap(),
        request,
        &[],
    )
    .await;
    assert_eq!(seen.context.request_id.as_str(), "edge-req.42_A-b");
    assert_eq!(seen.headers["x-request-id"], "edge-req.42_A-b");
    assert_eq!(response.headers()["x-request-id"], "edge-req.42_A-b");
}

#[tokio::test]
async fn invalid_or_oversized_request_ids_are_replaced() {
    let long = "a".repeat(257);
    for value in ["has space", "semi;colon", long.as_str(), "ünicode"] {
        let mut request = request_from("203.0.113.9:1", None, &[]);
        request.headers_mut().insert(
            "x-request-id",
            http::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
        );
        let (seen, _) = run(
            TelemetryLayer::new(TelemetryConfig::default()).unwrap(),
            request,
            &[],
        )
        .await;
        assert_ne!(seen.context.request_id.as_str(), value);
        assert_eq!(seen.context.request_id.as_str().len(), 36, "generated UUID");
        assert_eq!(
            seen.context.request_id_source,
            ferrum_alloy_telemetry::request_id::RequestIdSource::ReplacedInvalid
        );
        assert_eq!(
            seen.headers["x-request-id"],
            seen.context.request_id.as_str()
        );
    }
}

#[tokio::test]
async fn trusted_only_request_ids_are_replaced_for_untrusted_peers() {
    let mut config = TelemetryConfig::default();
    config.request_id.accept_incoming = AcceptPolicy::TrustedPeers;
    let request = request_from("203.0.113.9:1", None, &[("x-request-id", "client-chosen")]);
    let (seen, _) = run(trusted_layer(config.clone()), request, &[]).await;
    assert_ne!(seen.context.request_id.as_str(), "client-chosen");
    let request = request_from("10.0.0.5:1", None, &[("x-request-id", "gateway-chosen")]);
    let (seen, _) = run(trusted_layer(config), request, &[]).await;
    assert_eq!(seen.context.request_id.as_str(), "gateway-chosen");
}

#[tokio::test]
async fn request_specific_headers_are_withheld_from_shared_cacheable_responses() {
    let mut config = TelemetryConfig::default();
    config.server_timing = ServerTimingPolicy::Always;
    let layer = TelemetryLayer::new(config.clone()).unwrap();
    let metrics = layer.metrics();
    let request = request_from("203.0.113.9:1", None, &[("x-request-id", "r1")]);
    let (_, response) = run(layer, request, &[("cache-control", "public, max-age=60")]).await;
    assert!(!response.headers().contains_key("x-request-id"));
    assert!(!response.headers().contains_key("server-timing"));
    assert_eq!(metrics.header_suppressions.get("shared_cacheable"), 1);

    let request = request_from("203.0.113.9:1", None, &[("x-request-id", "r2")]);
    let (_, response) = run(
        TelemetryLayer::new(config).unwrap(),
        request,
        &[("cache-control", "no-store")],
    )
    .await;
    assert_eq!(response.headers()["x-request-id"], "r2");
    let timing = response.headers()["server-timing"].to_str().unwrap();
    assert!(timing.starts_with("alloy;dur="), "{timing}");
}

#[tokio::test]
async fn server_timing_is_off_by_default_and_trusted_only_when_configured() {
    let request = request_from("10.0.0.5:1", None, &[]);
    let (_, response) = run(trusted_layer(TelemetryConfig::default()), request, &[]).await;
    assert!(!response.headers().contains_key("server-timing"));

    let mut config = TelemetryConfig::default();
    config.server_timing = ServerTimingPolicy::TrustedPeers;
    let request = request_from("203.0.113.9:1", None, &[]);
    let (_, response) = run(trusted_layer(config.clone()), request, &[]).await;
    assert!(
        !response.headers().contains_key("server-timing"),
        "untrusted caller"
    );
    let request = request_from("10.0.0.5:1", None, &[]);
    let (_, response) = run(trusted_layer(config), request, &[]).await;
    assert!(
        response.headers().contains_key("server-timing"),
        "trusted gateway"
    );
}

#[test]
fn reserved_request_id_headers_are_rejected() {
    for header in [
        "authorization",
        "cookie",
        "traceparent",
        "x-consumer-username",
        "bad header",
    ] {
        let mut config = TelemetryConfig::default();
        config.request_id.header = header.into();
        assert!(TelemetryLayer::new(config).is_err(), "{header}");
    }
}

#[test]
fn trusted_peer_configuration_is_validated() {
    for identity in ["gateway", "spiffe://", "dns:", "http://x"] {
        assert!(
            TrustedPeers::new(&TrustedPeersConfig {
                identities: vec![identity.into()],
                networks: vec![],
            })
            .is_err(),
            "{identity}"
        );
    }
    assert!(
        TrustedPeers::new(&TrustedPeersConfig {
            identities: vec![],
            networks: vec!["0.0.0.0/0".parse().unwrap()],
        })
        .is_err(),
        "trusting every address must be rejected"
    );
}

#[test]
fn traceparent_parsing_follows_the_w3c_grammar() {
    let ok = parse_traceparent(INCOMING).unwrap();
    assert_eq!(ok.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(ok.parent_id.to_hex(), "00f067aa0ba902b7");
    assert!(ok.sampled());
    assert_eq!(ok.to_header_value(), INCOMING);

    // Future versions may append fields.
    assert!(parse_traceparent(&format!("01{}-extra", &INCOMING[2..])).is_ok());
    let invalid = [
        format!("ff{}", &INCOMING[2..]),
        format!("{INCOMING}-extra"),
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01".to_owned(),
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01".to_owned(),
        INCOMING.to_uppercase(),
        INCOMING.replace('-', "_"),
        INCOMING[..54].to_owned(),
        format!("01{}x", &INCOMING[2..]),
        String::new(),
    ];
    for value in &invalid {
        assert_eq!(
            parse_traceparent(value),
            Err(TraceContextError::Malformed),
            "{value:?}"
        );
    }
}

#[test]
fn tracestate_validation_follows_the_w3c_grammar() {
    assert_eq!(
        validate_tracestate("rojo=00f067aa0ba902b7, congo=t61rcWkgMzE")
            .unwrap()
            .as_str(),
        "rojo=00f067aa0ba902b7,congo=t61rcWkgMzE"
    );
    assert!(validate_tracestate("tenant@system=value").is_some());
    for invalid in [
        "Upper=value",
        "key=",
        "key=val,key=dup",
        "=value",
        "key=bad,value",
        "key=a=b",
        &(0..33)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(","),
        &format!("k={}", "x".repeat(600)),
    ] {
        assert!(validate_tracestate(invalid).is_none(), "{invalid:?}");
    }
}

/// Deterministic mutation test: parsing arbitrary input never panics.
#[test]
fn traceparent_parser_never_panics_on_hostile_input() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let alphabet = b"0123456789abcdefABCDEF-xz\x00\xff ";
    for length in 0..120 {
        for _ in 0..50 {
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    alphabet[(state % alphabet.len() as u64) as usize]
                })
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            let _ = parse_traceparent(&text);
            let _ = validate_tracestate(&text);
        }
    }
}
