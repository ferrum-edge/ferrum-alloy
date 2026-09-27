//! Authorized, tenant-scoped diagnostic retrieval (ADR 0008): one uniform
//! `404` for denied callers, failed authorizers, other tenants, and unknown,
//! malformed, or evicted ids; `no-store`; the management rate limit, which
//! no peer is exempt from; a loopback management listener; bounded
//! retention; and reports that hold labels and timings only.

#![cfg(feature = "diagnostics")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::routing::get;
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::diagnostics::{
    DiagnosticsAccess, DiagnosticsAuthorizer, DiagnosticsRequest, TenantTag,
};
use ferrum_alloy::telemetry::PeerInfo;
use ferrum_alloy::{AlloyApp, AlloyParts, TelemetryInit};
use ferrum_alloy_diagnostics::model::{Availability, CollectionMethod, Confidence};
use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use support::{TOKEN, config, fetch_with, start};
use tower::ServiceExt;

const TOKEN_A: &str = "tenant-a-diagnostics-credential";
const TOKEN_B: &str = "tenant-b-diagnostics-credential";

/// Stands in for an application's authorizer: each credential names one
/// tenant.
async fn authorize(request: DiagnosticsRequest) -> DiagnosticsAccess {
    match request.bearer_token() {
        Some(TOKEN_A) => DiagnosticsAccess::tenant("tenant-a"),
        Some(TOKEN_B) => DiagnosticsAccess::tenant("tenant-b"),
        _ => DiagnosticsAccess::Deny,
    }
}

/// An application that tags each request with the tenant in its path, in
/// place of its own authentication.
fn app() -> AlloyApp {
    let router = Router::new()
        .route(
            "/tenants/{tenant}/orders/{id}",
            get(
                |Path((tenant, _)): Path<(String, u32)>, tag: TenantTag| async move {
                    assert!(tag.set(&tenant));
                    "ok"
                },
            ),
        )
        .route("/public", get(|| async { "ok" }));
    AlloyApp::new("orders").router(router)
}

fn settings() -> AlloyConfig {
    let mut cfg = config();
    cfg.management.rate_limit.burst = 100;
    cfg
}

fn parts_with(authorizer: impl DiagnosticsAuthorizer, cfg: AlloyConfig) -> AlloyParts {
    app()
        .diagnostics_authorizer(authorizer)
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap()
}

fn parts(cfg: AlloyConfig) -> AlloyParts {
    parts_with(authorize, cfg)
}

/// Serves one application request, including its whole body.
async fn serve(parts: &AlloyParts, uri: &str, request_id: &str) -> StatusCode {
    let mut request = Request::get(uri)
        .header("x-request-id", request_id)
        .header("authorization", "Bearer app-user-secret")
        .header("cookie", "session=cookie-secret")
        .body(Body::empty())
        .unwrap();
    let peer = PeerInfo {
        remote_addr: Some("198.51.100.77:40000".parse().unwrap()),
        ..PeerInfo::default()
    };
    request.extensions_mut().insert(peer);
    let response = parts.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    response.into_body().collect().await.unwrap();
    status
}

async fn order(parts: &AlloyParts, tenant: &str, request_id: &str) {
    let uri = format!("/tenants/{tenant}/orders/7?card=4111111111111111");
    assert_eq!(serve(parts, &uri, request_id).await, StatusCode::OK);
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Sends a management request from `peer`.
async fn manage(parts: &AlloyParts, peer: IpAddr, path: &str, token: Option<&str>) -> Reply {
    let router = parts.management_router.clone().unwrap();
    let mut builder = Request::get(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let mut request = builder.body(Body::empty()).unwrap();
    let info = PeerInfo {
        remote_addr: Some(SocketAddr::new(peer, 40_000)),
        ..PeerInfo::default()
    };
    request.extensions_mut().insert(info);
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: body.to_vec(),
    }
}

fn operator() -> IpAddr {
    "192.0.2.10".parse().unwrap()
}

async fn retrieve(parts: &AlloyParts, request_id: &str, token: Option<&str>) -> Reply {
    let path = format!("/diagnostics/v1/requests/{request_id}");
    manage(parts, operator(), &path, token).await
}

async fn metrics(parts: &AlloyParts) -> String {
    let reply = manage(parts, operator(), "/metrics", Some(TOKEN)).await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.text()
}

/// The value of an exact Prometheus series in `text`.
fn metric(text: &str, series: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("{series} is missing from:\n{text}"))
}

fn assert_not_found(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    assert_eq!(reply.headers["cache-control"], "no-store");
    assert_eq!(reply.headers["content-type"], "application/problem+json");
}

#[tokio::test]
async fn a_tenant_retrieves_its_own_request_as_a_no_store_report() {
    let parts = parts(settings());
    order(&parts, "tenant-a", "req-a-1").await;

    let reply = retrieve(&parts, "req-a-1", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.headers["cache-control"], "no-store");
    assert_eq!(reply.headers["content-type"], "application/json");

    // Redaction: labels and timings only.
    let text = reply.text();
    for secret in [
        "4111111111111111",
        "app-user-secret",
        "cookie-secret",
        "tenants/tenant-a/orders/7",
        "198.51.100.77",
        "192.0.2.10",
        "Bearer",
        TOKEN_A,
    ] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }

    let parsed = parse_offline(&reply.body, &Limits::default()).unwrap();
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let report = parsed.report;
    assert_eq!(report.collection.method, CollectionMethod::LiveExport);
    assert_eq!(report.subject.request_id.as_deref(), Some("req-a-1"));
    assert_eq!(report.subject.service.as_deref(), Some("orders"));
    let route = report.subject.route.as_deref();
    assert_eq!(route, Some("/tenants/{tenant}/orders/{id}"));
    let head = report
        .observations
        .iter()
        .find(|o| o.name == "alloy.server.time_to_headers")
        .unwrap();
    assert_eq!(head.availability, Availability::Measured);
    let response = report
        .observations
        .iter()
        .find(|o| o.name == "alloy.response")
        .unwrap();
    assert_eq!(response.attr("status"), Some("200"));
    assert_eq!(response.attr("peer_trust"), Some("untrusted"));

    // A live report is service telemetry, never verified evidence.
    for finding in analyze(&report, &Thresholds::default()) {
        assert_ne!(finding.confidence, Confidence::Confirmed, "{finding:?}");
    }
}

#[tokio::test]
async fn other_tenants_denied_callers_and_unknown_ids_get_the_same_404() {
    let parts = parts(settings());
    order(&parts, "tenant-a", "req-a-1").await;
    order(&parts, "tenant-b", "req-b-1").await;

    let attempts: [(&str, Option<&str>); 9] = [
        // Another tenant's request, whether or not its id is known.
        ("req-b-1", Some(TOKEN_A)),
        ("req-a-1", Some(TOKEN_B)),
        // No credential, a wrong one, and the management token, which is
        // not tenant-scoped.
        ("req-a-1", None),
        ("req-a-1", Some("not-a-credential")),
        ("req-a-1", Some(TOKEN)),
        // Unknown and malformed ids.
        ("req-a-2", Some(TOKEN_A)),
        ("never-seen", Some(TOKEN_A)),
        ("bad%20id", Some(TOKEN_A)),
        ("%FF", Some(TOKEN_A)),
    ];
    let first = retrieve(&parts, attempts[0].0, attempts[0].1).await;
    assert_not_found(&first);
    for (request_id, token) in attempts {
        let reply = retrieve(&parts, request_id, token).await;
        assert_not_found(&reply);
        assert_eq!(reply.body, first.body, "{request_id} {token:?}");
        assert_eq!(reply.headers, first.headers, "{request_id} {token:?}");
    }

    // Each tenant still reads its own.
    let reply = retrieve(&parts, "req-a-1", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = retrieve(&parts, "req-b-1", Some(TOKEN_B)).await;
    assert_eq!(reply.status, StatusCode::OK);

    // Denials are counted with the misses they look like.
    let text = metrics(&parts).await;
    assert!(!text.contains(r#"outcome="denied""#), "{text}");
    let not_found = r#"ferrum_alloy_diagnostics_retrievals_total{outcome="not_found"}"#;
    assert_eq!(metric(&text, not_found), 10);
    let served = r#"ferrum_alloy_diagnostics_retrievals_total{outcome="served"}"#;
    assert_eq!(metric(&text, served), 2);
}

#[tokio::test]
async fn a_request_id_shared_by_two_tenants_discloses_only_the_callers_own() {
    let parts = parts(settings());
    // Callers choose request ids, so two tenants can use the same one.
    order(&parts, "tenant-a", "shared-id").await;
    order(&parts, "tenant-b", "shared-id").await;
    order(&parts, "tenant-b", "shared-id").await;

    let reply = retrieve(&parts, "shared-id", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let report = parse_offline(&reply.body, &Limits::default())
        .unwrap()
        .report;
    let attempts = report
        .observations
        .iter()
        .filter(|o| o.name == "alloy.response")
        .count();
    assert_eq!(attempts, 1, "only tenant-a's request");

    let reply = retrieve(&parts, "shared-id", Some(TOKEN_B)).await;
    let report = parse_offline(&reply.body, &Limits::default())
        .unwrap()
        .report;
    let attempts = report
        .observations
        .iter()
        .filter(|o| o.name == "alloy.response")
        .count();
    assert_eq!(attempts, 2, "both of tenant-b's requests");
}

#[tokio::test]
async fn evicted_and_untagged_requests_are_not_found_and_counted() {
    let mut cfg = settings();
    cfg.diagnostics.max_records = 2;
    let parts = parts(cfg);
    for request_id in ["req-1", "req-2", "req-3"] {
        order(&parts, "tenant-a", request_id).await;
    }
    let status = serve(&parts, "/public", "req-public").await;
    assert_eq!(status, StatusCode::OK);

    assert_not_found(&retrieve(&parts, "req-1", Some(TOKEN_A)).await);
    assert_not_found(&retrieve(&parts, "req-public", Some(TOKEN_A)).await);
    for request_id in ["req-2", "req-3"] {
        let reply = retrieve(&parts, request_id, Some(TOKEN_A)).await;
        assert_eq!(reply.status, StatusCode::OK, "{request_id}");
    }

    let text = metrics(&parts).await;
    assert_eq!(metric(&text, "ferrum_alloy_diagnostics_records"), 2);
    let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
    assert_eq!(metric(&text, evicted), 1);
    let untagged = r#"ferrum_alloy_diagnostics_skipped_total{reason="untagged"}"#;
    assert_eq!(metric(&text, untagged), 1);
}

#[tokio::test]
async fn retrieval_is_rate_limited_before_the_authorizer_runs() {
    let mut cfg = config();
    cfg.management.rate_limit.requests_per_second = 1;
    cfg.management.rate_limit.burst = 2;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let parts = parts_with(
        move |request: DiagnosticsRequest| {
            counted.fetch_add(1, Ordering::Relaxed);
            authorize(request)
        },
        cfg,
    );
    order(&parts, "tenant-a", "req-a-1").await;

    for _ in 0..2 {
        let reply = retrieve(&parts, "guess", Some(TOKEN_A)).await;
        assert_not_found(&reply);
    }
    let reply = retrieve(&parts, "req-a-1", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.headers["cache-control"], "no-store");
    assert!(reply.headers.contains_key("retry-after"));
    assert_eq!(calls.load(Ordering::Relaxed), 2, "never authorized");

    // Another client has its own budget.
    let other: IpAddr = "192.0.2.11".parse().unwrap();
    let path = "/diagnostics/v1/requests/req-a-1";
    let reply = manage(&parts, other, path, Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn the_authorizer_sees_the_transport_peer_and_never_trusts_headers() {
    let trusted: IpAddr = "10.0.0.5".parse().unwrap();
    let parts = parts_with(
        move |request: DiagnosticsRequest| async move {
            let peer = request.peer.and_then(|peer| peer.remote_addr);
            if peer.map(|addr| addr.ip()) == Some(trusted) {
                DiagnosticsAccess::tenant("tenant-a")
            } else {
                DiagnosticsAccess::Deny
            }
        },
        settings(),
    );
    order(&parts, "tenant-a", "req-a-1").await;
    let path = "/diagnostics/v1/requests/req-a-1";
    let reply = manage(&parts, trusted, path, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_not_found(&manage(&parts, operator(), path, None).await);
}

#[tokio::test]
async fn a_panicking_authorizer_denies_like_any_refusal() {
    let parts = parts_with(
        |request: DiagnosticsRequest| {
            // In the call itself, before any future exists.
            if request.bearer_token() == Some("panic-now") {
                panic!("authorizer bug");
            }
            async move {
                if request.bearer_token() == Some("panic-later") {
                    panic!("authorizer bug");
                }
                authorize(request).await
            }
        },
        settings(),
    );
    order(&parts, "tenant-a", "req-a-1").await;

    let denied = retrieve(&parts, "req-a-1", None).await;
    assert_not_found(&denied);
    for token in ["panic-now", "panic-later", "panic-now"] {
        let reply = retrieve(&parts, "req-a-1", Some(token)).await;
        assert_not_found(&reply);
        assert_eq!(reply.body, denied.body, "{token}");
        assert_eq!(reply.headers, denied.headers, "{token}");
    }
    // The endpoint keeps serving.
    let reply = retrieve(&parts, "req-a-1", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::OK);

    let text = metrics(&parts).await;
    assert!(!text.contains("authorizer bug"), "{text}");
    let panics = r#"ferrum_alloy_diagnostics_authorizer_failures_total{reason="panic"}"#;
    assert_eq!(metric(&text, panics), 3);
    let timeouts = r#"ferrum_alloy_diagnostics_authorizer_failures_total{reason="timeout"}"#;
    assert_eq!(metric(&text, timeouts), 0);
    let not_found = r#"ferrum_alloy_diagnostics_retrievals_total{outcome="not_found"}"#;
    assert_eq!(metric(&text, not_found), 4);
}

#[tokio::test]
async fn exempt_networks_never_bypass_the_retrieval_rate_limit() {
    let mut cfg = config();
    cfg.management.rate_limit.requests_per_second = 1;
    cfg.management.rate_limit.burst = 2;
    cfg.management.rate_limit.exempt_networks = vec!["192.0.2.0/24".parse().unwrap()];
    let parts = parts(cfg);
    order(&parts, "tenant-a", "req-a-1").await;

    // The exemption holds elsewhere on the listener.
    for _ in 0..5 {
        let reply = manage(&parts, operator(), "/metrics", Some(TOKEN)).await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    for _ in 0..2 {
        assert_not_found(&retrieve(&parts, "guess", Some(TOKEN_A)).await);
    }
    let reply = retrieve(&parts, "req-a-1", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn an_authorizer_granting_an_invalid_tenant_denies() {
    let parts = parts_with(
        |_: DiagnosticsRequest| async { DiagnosticsAccess::tenant("tenant a") },
        settings(),
    );
    order(&parts, "tenant-a", "req-a-1").await;
    assert_not_found(&retrieve(&parts, "req-a-1", Some(TOKEN_A)).await);
}

#[tokio::test]
async fn without_an_authorizer_nothing_is_retained_or_served() {
    let parts = app()
        .config(settings())
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap();
    order(&parts, "tenant-a", "req-a-1").await;
    let reply = retrieve(&parts, "req-a-1", Some(TOKEN_A)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(!metrics(&parts).await.contains("ferrum_alloy_diagnostics"));
}

#[test]
fn startup_requires_the_management_listener_and_its_rate_limit() {
    let mut cfg = settings();
    cfg.management.rate_limit.enabled = false;
    let error = app()
        .diagnostics_authorizer(authorize)
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap_err()
        .to_string();
    assert!(error.contains("management.rate_limit.enabled"), "{error}");

    let mut cfg = settings();
    cfg.management.enabled = false;
    let error = app()
        .diagnostics_authorizer(authorize)
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap_err()
        .to_string();
    assert!(error.contains("management.enabled"), "{error}");
}

#[test]
fn startup_requires_a_loopback_management_listener() {
    // The listener has no TLS; the authorizer's credentials would cross the
    // network in cleartext.
    for bind in ["0.0.0.0:9090", "192.0.2.10:9090", "[::]:9090"] {
        let mut cfg = settings();
        cfg.management.bind = bind.parse().unwrap();
        let error = app()
            .diagnostics_authorizer(authorize)
            .config(cfg)
            .telemetry(TelemetryInit::ApplicationOwned)
            .into_parts()
            .unwrap_err()
            .to_string();
        assert!(error.contains("loopback management.bind"), "{bind}: {error}");
    }

    // Without retrieval, a non-loopback listener with a token is allowed.
    let mut cfg = settings();
    cfg.management.bind = "0.0.0.0:9090".parse().unwrap();
    let parts = app()
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts();
    assert!(parts.is_ok(), "{:?}", parts.err().map(|e| e.to_string()));
}

#[tokio::test]
async fn retrieval_works_over_real_connections() {
    let app = app().diagnostics_authorizer(authorize);
    let server = start(app, settings()).await;
    let url = server.url("/tenants/tenant-a/orders/7");
    let reply = fetch_with(&url, &[("x-request-id", "req-live-1")]).await;
    assert_eq!(reply.status, StatusCode::OK);

    let url = server.management_url("/diagnostics/v1/requests/req-live-1");
    let bearer = format!("Bearer {TOKEN_A}");
    let reply = fetch_with(&url, &[("authorization", &bearer)]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.headers["cache-control"], "no-store");
    let parsed = parse_offline(&reply.body, &Limits::default()).unwrap();
    let request_id = parsed.report.subject.request_id.as_deref();
    assert_eq!(request_id, Some("req-live-1"));

    let bearer = format!("Bearer {TOKEN_B}");
    let reply = fetch_with(&url, &[("authorization", &bearer)]).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    server.shutdown().await.unwrap();
}
