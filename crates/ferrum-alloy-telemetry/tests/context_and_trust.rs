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
    SpanId, TraceContextError, TraceId, parse_traceparent, validate_tracestate,
};
use ferrum_alloy_telemetry::{
    AcceptPolicy, PeerTrust, RequestContext, ServerTimingPolicy, TelemetryConfig, TelemetryLayer,
    TraceDecision,
};
use http::{HeaderMap, Method, Request, Response, StatusCode};
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
    run_with_status(layer, request, StatusCode::OK, response_headers).await
}

async fn run_with_status(
    layer: TelemetryLayer,
    request: Request<Empty<Bytes>>,
    status: StatusCode,
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
            *response.status_mut() = status;
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
    let mut trust = TrustedPeersConfig::default();
    trust.identities = vec!["spiffe://ferrum.test/ns/edge/sa/gateway".into()];
    trust.networks = vec!["10.0.0.0/8".parse().unwrap()];
    let peers = TrustedPeers::new(&trust).unwrap();
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
    // From a trusted gateway, such as Ferrum Edge with its correlation id.
    let request = request_from("10.0.0.5:1", None, &[("x-request-id", "edge-req.42_A-b")]);
    let (seen, response) = run(
        trusted_layer(TelemetryConfig::default()),
        request,
        &[("cache-control", "no-store")],
    )
    .await;
    assert_eq!(seen.context.request_id.as_str(), "edge-req.42_A-b");
    assert_eq!(
        seen.context.request_id_source,
        ferrum_alloy_telemetry::request_id::RequestIdSource::Accepted
    );
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
    // The default, like trace context: a caller cannot choose the id its
    // request is logged, traced, and retained under.
    let config = TelemetryConfig::default();
    let policy = config.request_id.accept_incoming;
    assert_eq!(policy, AcceptPolicy::TrustedPeers);
    let layer = trusted_layer(config.clone());
    let metrics = layer.metrics();
    let request = request_from("203.0.113.9:1", None, &[("x-request-id", "client-chosen")]);
    let (seen, response) = run(layer, request, NO_STORE).await;
    let request_id = seen.context.request_id.as_str();
    assert_ne!(request_id, "client-chosen");
    assert_eq!(request_id.len(), 36, "generated UUID");
    assert_eq!(
        seen.context.request_id_source,
        ferrum_alloy_telemetry::request_id::RequestIdSource::ReplacedUntrusted
    );
    assert_eq!(seen.headers["x-request-id"], request_id);
    assert_eq!(response.headers()["x-request-id"], request_id);
    assert_eq!(metrics.request_id_decisions.get("replaced_untrusted"), 1);

    let request = request_from("10.0.0.5:1", None, &[("x-request-id", "gateway-chosen")]);
    let (seen, _) = run(trusted_layer(config), request, &[]).await;
    assert_eq!(seen.context.request_id.as_str(), "gateway-chosen");
}

#[tokio::test]
async fn never_and_any_request_id_policies_ignore_trust() {
    let mut config = TelemetryConfig::default();
    config.request_id.accept_incoming = AcceptPolicy::Never;
    let request = request_from("10.0.0.5:1", None, &[("x-request-id", "gateway-chosen")]);
    let (seen, _) = run(trusted_layer(config.clone()), request, &[]).await;
    assert_ne!(seen.context.request_id.as_str(), "gateway-chosen");

    config.request_id.accept_incoming = AcceptPolicy::Any;
    let request = request_from("203.0.113.9:1", None, &[("x-request-id", "client-chosen")]);
    let (seen, _) = run(trusted_layer(config), request, &[]).await;
    assert_eq!(seen.context.request_id.as_str(), "client-chosen");
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
    let (seen, response) = run(
        TelemetryLayer::new(config).unwrap(),
        request,
        &[("cache-control", "no-store")],
    )
    .await;
    let request_id = seen.context.request_id.as_str();
    assert_eq!(response.headers()["x-request-id"], request_id);
    let timing = response.headers()["server-timing"].to_str().unwrap();
    assert!(timing.starts_with("alloy;dur="), "{timing}");
}

const NO_STORE: &[(&str, &str)] = &[("cache-control", "no-store")];
const LAST_MODIFIED: (&str, &str) = ("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT");

/// Runs one request with `Server-Timing` always on and reports whether the
/// layer added its request-specific headers, checking that the id echo and
/// `Server-Timing` follow one decision and that a withheld pair is counted.
async fn adds_request_specific_headers(
    request: Request<Empty<Bytes>>,
    status: StatusCode,
    response_headers: &[(&'static str, &'static str)],
) -> bool {
    let mut config = TelemetryConfig::default();
    config.server_timing = ServerTimingPolicy::Always;
    let layer = TelemetryLayer::new(config).unwrap();
    let metrics = layer.metrics();
    let (_, response) = run_with_status(layer, request, status, response_headers).await;
    let echoed = response.headers().contains_key("x-request-id");
    assert_eq!(
        echoed,
        response.headers().contains_key("server-timing"),
        "{status} {response_headers:?}"
    );
    assert_eq!(
        metrics.header_suppressions.get("shared_cacheable"),
        u64::from(!echoed),
        "{status} {response_headers:?}"
    );
    echoed
}

fn get_request(headers: &[(&str, &str)]) -> Request<Empty<Bytes>> {
    let mut all = vec![("x-request-id", "r1")];
    all.extend_from_slice(headers);
    request_from("203.0.113.9:1", None, &all)
}

#[tokio::test]
async fn heuristically_cacheable_responses_withhold_request_specific_headers() {
    // RFC 9111 §4.2.2: these statuses are storable without explicit freshness,
    // and Last-Modified is only an input to the heuristic, not a requirement.
    for code in [200, 203, 204, 206, 300, 301, 308, 404, 405, 410, 414, 501] {
        let status = StatusCode::from_u16(code).unwrap();
        for headers in [&[][..], &[LAST_MODIFIED][..], &[("etag", "\"v1\"")][..]] {
            assert!(
                !adds_request_specific_headers(get_request(&[]), status, headers).await,
                "{code} {headers:?}"
            );
        }
        let mut head = get_request(&[]);
        *head.method_mut() = Method::HEAD;
        assert!(
            !adds_request_specific_headers(head, status, &[]).await,
            "HEAD {code}"
        );
    }
    // Directives that allow storage with revalidation do not prevent it.
    for directive in ["no-cache", "must-revalidate", "private=\"set-cookie\""] {
        let headers = [("cache-control", directive)];
        assert!(
            !adds_request_specific_headers(get_request(&[]), StatusCode::OK, &headers).await,
            "{directive}"
        );
    }
}

#[tokio::test]
async fn explicit_freshness_withholds_request_specific_headers_for_any_status() {
    for headers in [
        &[("cache-control", "public, max-age=60")][..],
        &[("cache-control", "max-age=60")][..],
        &[("cache-control", "s-maxage=60")][..],
        &[("expires", "Wed, 21 Oct 2037 07:28:00 GMT")][..],
    ] {
        for status in [StatusCode::OK, StatusCode::INTERNAL_SERVER_ERROR] {
            assert!(
                !adds_request_specific_headers(get_request(&[]), status, headers).await,
                "{status} {headers:?}"
            );
        }
    }
}

#[tokio::test]
async fn explicit_prohibitions_keep_request_specific_headers() {
    for directive in [
        "no-store",
        "private",
        "Private, max-age=60",
        "public, no-store, max-age=60",
    ] {
        let headers = [("cache-control", directive), LAST_MODIFIED];
        assert!(
            adds_request_specific_headers(get_request(&[]), StatusCode::OK, &headers).await,
            "{directive}"
        );
    }
}

#[tokio::test]
async fn statuses_and_methods_that_are_not_storable_keep_request_specific_headers() {
    for code in [201, 302, 400, 403, 500, 503] {
        let status = StatusCode::from_u16(code).unwrap();
        assert!(
            adds_request_specific_headers(get_request(&[]), status, &[LAST_MODIFIED]).await,
            "{code}"
        );
    }
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let mut request = get_request(&[]);
        *request.method_mut() = method.clone();
        assert!(
            adds_request_specific_headers(request, StatusCode::OK, &[LAST_MODIFIED]).await,
            "{method}"
        );
    }
    let mut post = get_request(&[]);
    *post.method_mut() = Method::POST;
    let headers = [("cache-control", "public, max-age=60")];
    assert!(
        !adds_request_specific_headers(post, StatusCode::OK, &headers).await,
        "POST with explicit freshness is storable"
    );
}

#[tokio::test]
async fn authenticated_requests_keep_headers_unless_shared_storage_is_explicit() {
    let authorized = || get_request(&[("authorization", "Bearer token")]);
    for headers in [
        &[][..],
        &[LAST_MODIFIED][..],
        &[("cache-control", "max-age=60")][..],
    ] {
        assert!(
            adds_request_specific_headers(authorized(), StatusCode::OK, headers).await,
            "{headers:?}"
        );
    }
    for directive in [
        "public, max-age=60",
        "s-maxage=60",
        "must-revalidate, max-age=60",
    ] {
        let headers = [("cache-control", directive)];
        assert!(
            !adds_request_specific_headers(authorized(), StatusCode::OK, &headers).await,
            "{directive}"
        );
    }
}

#[tokio::test]
async fn server_timing_is_off_by_default_and_trusted_only_when_configured() {
    let request = request_from("10.0.0.5:1", None, &[]);
    let layer = trusted_layer(TelemetryConfig::default());
    let (_, response) = run(layer, request, NO_STORE).await;
    assert!(!response.headers().contains_key("server-timing"));

    let mut config = TelemetryConfig::default();
    config.server_timing = ServerTimingPolicy::TrustedPeers;
    let request = request_from("203.0.113.9:1", None, &[]);
    let (_, response) = run(trusted_layer(config.clone()), request, NO_STORE).await;
    assert!(
        !response.headers().contains_key("server-timing"),
        "untrusted caller"
    );
    let request = request_from("10.0.0.5:1", None, &[]);
    let (_, response) = run(trusted_layer(config), request, NO_STORE).await;
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
        "x-authenticated-identity",
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
        let mut trust = TrustedPeersConfig::default();
        trust.identities = vec![identity.into()];
        assert!(TrustedPeers::new(&trust).is_err(), "{identity}");
    }
    let mut trust = TrustedPeersConfig::default();
    trust.networks = vec!["0.0.0.0/0".parse().unwrap()];
    assert!(
        TrustedPeers::new(&trust).is_err(),
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

#[test]
fn a_constructed_context_is_an_untrusted_root() {
    let context = RequestContext::new(TraceId([0x11; 16]), SpanId([0x22; 8]), true);
    assert_eq!(context.request_id.as_str().len(), 36, "generated UUID");
    assert_eq!(
        context.request_id_source,
        ferrum_alloy_telemetry::request_id::RequestIdSource::Generated
    );
    assert_eq!(context.trace_decision, TraceDecision::Root);
    assert_eq!(context.peer_trust, PeerTrust::Untrusted);
    assert!(!context.exported);
    assert!(context.remote_parent.is_none());
    assert!(context.tracestate.is_none());
    let parent = context.child_traceparent(SpanId([0x33; 8]));
    assert_eq!(
        parent.to_header_value(),
        "00-11111111111111111111111111111111-3333333333333333-01"
    );

    let unsampled = RequestContext::new(TraceId::random(), SpanId::random(), false);
    let parent = unsampled.child_traceparent(SpanId::random());
    assert!(parent.to_header_value().ends_with("-00"));
}

#[tokio::test]
async fn matching_accepted_remote_pairs_have_fresh_immutable_local_owners() {
    let layer = trusted_layer(TelemetryConfig::default());
    let headers = [
        ("x-request-id", "predictable"),
        ("traceparent", INCOMING),
        ("authorization", "Bearer test-user"),
    ];
    let (first, response) = run(
        layer.clone(),
        request_from("10.1.2.3:4000", None, &headers),
        &[],
    )
    .await;
    let (second, _) = run(layer, request_from("10.1.2.3:4000", None, &headers), &[]).await;
    assert_eq!(first.context.request_id, second.context.request_id);
    assert_eq!(first.context.trace_id, second.context.trace_id);
    assert_ne!(first.context.span_id, second.context.span_id);
    assert_ne!(
        first.context.diagnostic_id(),
        second.context.diagnostic_id()
    );
    assert_eq!(response.headers()["x-request-id"], "predictable");
    let owner = first.context.diagnostic_id().clone();
    let mut cloned = first.context.clone();
    cloned.request_id = ferrum_alloy_telemetry::RequestId::generate();
    cloned.trace_id = TraceId::random();
    cloned.span_id = SpanId::random();
    assert_eq!(cloned.diagnostic_id(), &owner);
}
