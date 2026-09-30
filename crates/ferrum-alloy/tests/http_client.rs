//! Outbound client: destination-aware propagation, redirects, timeouts.

#![cfg(feature = "http-client")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::response::Redirect;
use axum::routing::get;
use ferrum_alloy::config::HttpClientSettings;
use ferrum_alloy::http_client::AlloyClient;
use ferrum_alloy::telemetry::context::{RequestContext, TraceDecision};
use ferrum_alloy::telemetry::trace_context::{SpanId, TraceId, validate_tracestate};

async fn server(other: Option<SocketAddr>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/echo",
            get(|headers: HeaderMap| async move {
                serde_json::json!({
                    "traceparent": headers.get("traceparent").map(|v| v.to_str().unwrap().to_owned()),
                    "tracestate": headers.get("tracestate").map(|v| v.to_str().unwrap().to_owned()),
                    "baggage": headers.get("baggage").map(|v| v.to_str().unwrap().to_owned()),
                    "authorization": headers.get("authorization").map(|v| v.to_str().unwrap().to_owned()),
                })
                .to_string()
            }),
        )
        .route("/same-origin", get(|| async { Redirect::temporary("/echo") }))
        .route(
            "/cross-origin",
            get(move || async move {
                Redirect::temporary(&format!("http://localhost:{}/echo", other.map_or(1, |a| a.port())))
            }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "slow"
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

fn context() -> RequestContext {
    RequestContext::new(TraceId([0x11; 16]), SpanId([0x22; 8]), true)
}

/// A context rooted by a trusted peer, with `state` as its accepted
/// `tracestate`, as the telemetry layer builds for [`TraceDecision::AcceptedRemote`].
fn accepted_context(state: Option<&str>) -> RequestContext {
    let mut context = context();
    context.trace_decision = TraceDecision::AcceptedRemote;
    context.tracestate = state.and_then(validate_tracestate);
    context
}

fn client(propagate: &[&str], redirects: usize) -> AlloyClient {
    let mut settings = HttpClientSettings::default();
    settings.connect_timeout_ms = 1_000;
    settings.request_timeout_ms = 500;
    settings.max_redirects = redirects;
    settings.propagate_trace_context_to = propagate.iter().map(|s| (*s).to_owned()).collect();
    AlloyClient::new(&settings).unwrap()
}

async fn echo_with(
    client: &AlloyClient,
    context: &RequestContext,
    url: String,
    headers: &[(&str, &str)],
) -> (u16, serde_json::Value) {
    let mut builder = client.request(reqwest::Method::GET, url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = client
        .execute(Some(context), builder.build().unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

async fn echo(
    client: &AlloyClient,
    url: String,
    headers: &[(&str, &str)],
) -> (u16, serde_json::Value) {
    echo_with(client, &context(), url, headers).await
}

#[tokio::test]
async fn trace_context_goes_only_to_listed_hosts() {
    let addr = server(None).await;
    let client = client(&["127.0.0.1"], 0);
    let (_, allowed) = echo(
        &client,
        format!("http://127.0.0.1:{}/echo", addr.port()),
        &[],
    )
    .await;
    let traceparent = allowed["traceparent"].as_str().unwrap();
    assert!(
        traceparent.starts_with("00-11111111111111111111111111111111-"),
        "{traceparent}"
    );
    assert!(traceparent.ends_with("-01"));

    let (_, other) = echo(
        &client,
        format!("http://localhost:{}/echo", addr.port()),
        &[
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
            ("baggage", "user=alice"),
        ],
    )
    .await;
    assert_eq!(
        other["traceparent"],
        serde_json::Value::Null,
        "caller context is not leaked to unlisted hosts"
    );
    assert_eq!(
        other["baggage"],
        serde_json::Value::Null,
        "baggage is never forwarded"
    );
}

#[tokio::test]
async fn absent_tracestate_stays_absent() {
    let addr = server(None).await;
    let client = client(&["127.0.0.1"], 0);
    let (_, value) = echo_with(
        &client,
        &accepted_context(None),
        format!("http://127.0.0.1:{}/echo", addr.port()),
        &[],
    )
    .await;
    assert!(value["traceparent"].is_string(), "{value}");
    assert_eq!(value["tracestate"], serde_json::Value::Null, "{value}");
}

#[tokio::test]
async fn rerooted_context_drops_caller_tracestate() {
    let addr = server(None).await;
    let client = client(&["127.0.0.1"], 0);
    // A rerooted request has no accepted state. A header copied from the
    // inbound request must not survive alongside the new traceparent.
    let mut rerooted = context();
    rerooted.trace_decision = TraceDecision::RerootedUntrusted;
    let (_, value) = echo_with(
        &client,
        &rerooted,
        format!("http://127.0.0.1:{}/echo", addr.port()),
        &[("tracestate", "attacker=opaque-value")],
    )
    .await;
    assert!(
        value["traceparent"].is_string(),
        "traceparent is still propagated: {value}"
    );
    assert_eq!(
        value["tracestate"],
        serde_json::Value::Null,
        "stale caller tracestate is dropped: {value}"
    );
}

#[tokio::test]
async fn accepted_tracestate_replaces_stale_caller_state() {
    let addr = server(None).await;
    let client = client(&["127.0.0.1"], 0);
    let (_, value) = echo_with(
        &client,
        &accepted_context(Some("accepted=canonical")),
        format!("http://127.0.0.1:{}/echo", addr.port()),
        &[("tracestate", "attacker=opaque-value")],
    )
    .await;
    assert_eq!(
        value["tracestate"].as_str(),
        Some("accepted=canonical"),
        "the accepted state wins over the caller-supplied header: {value}"
    );
}

#[tokio::test]
async fn tracestate_is_not_forwarded_to_unlisted_hosts() {
    let addr = server(None).await;
    let client = client(&["127.0.0.1"], 0);
    let (_, value) = echo_with(
        &client,
        &accepted_context(Some("accepted=canonical")),
        format!("http://localhost:{}/echo", addr.port()),
        &[
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
            ("tracestate", "attacker=opaque-value"),
        ],
    )
    .await;
    assert_eq!(value["traceparent"], serde_json::Value::Null, "{value}");
    assert_eq!(value["tracestate"], serde_json::Value::Null, "{value}");
}

#[tokio::test]
async fn suffix_rules_match_subdomains_only() {
    let addr = server(None).await;
    let client = client(&[".localhost"], 0);
    let (_, value) = echo(
        &client,
        format!("http://localhost:{}/echo", addr.port()),
        &[],
    )
    .await;
    assert_eq!(
        value["traceparent"],
        serde_json::Value::Null,
        ".localhost does not match localhost itself"
    );
}

#[tokio::test]
async fn redirects_are_same_origin_only() {
    let other = server(None).await;
    let origin = server(Some(other)).await;

    let none = client(&["127.0.0.1"], 0);
    let response = none
        .execute(
            Some(&context()),
            none.request(
                reqwest::Method::GET,
                format!("http://127.0.0.1:{}/same-origin", origin.port()),
            )
            .build()
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 307, "redirects are off by default");

    let follow = client(&["127.0.0.1"], 2);
    let (status, value) = echo(
        &follow,
        format!("http://127.0.0.1:{}/same-origin", origin.port()),
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert!(value["traceparent"].is_string());

    let response = follow
        .execute(
            Some(&context()),
            follow
                .request(
                    reqwest::Method::GET,
                    format!("http://127.0.0.1:{}/cross-origin", origin.port()),
                )
                .header("authorization", "Bearer secret")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        307,
        "cross-origin redirects are returned, not followed"
    );
}

#[tokio::test]
async fn request_timeouts_fail_fast() {
    let addr = server(None).await;
    let client = client(&[], 0);
    let started = std::time::Instant::now();
    let error = client
        .execute(
            None,
            client
                .request(
                    reqwest::Method::GET,
                    format!("http://127.0.0.1:{}/slow", addr.port()),
                )
                .build()
                .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(error.is_timeout(), "{error:?}");
    assert!(!error.is_connect(), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn refused_connections_are_reported_as_refusals() {
    // Deadlines well above the refusal time, so a refusal can never be
    // misreported as a timeout. Windows retries a refused connect for about
    // two seconds before reporting it, which the 500 ms request deadline of
    // the shared test client would otherwise cut short.
    let mut settings = HttpClientSettings::default();
    settings.connect_timeout_ms = 5_000;
    settings.request_timeout_ms = 10_000;
    let client = AlloyClient::new(&settings).unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let started = std::time::Instant::now();
    let error = client
        .execute(
            None,
            client
                .request(
                    reqwest::Method::GET,
                    format!("http://127.0.0.1:{}/", closed.port()),
                )
                .build()
                .unwrap(),
        )
        .await
        .unwrap_err();
    let mut refused = false;
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            refused |= io.kind() == std::io::ErrorKind::ConnectionRefused;
        }
        source = cause.source();
    }
    assert!(error.is_connect(), "{error:?}");
    assert!(!error.is_timeout(), "{error:?}");
    assert!(
        refused,
        "no ConnectionRefused in the error chain: {error:?}"
    );
    // The refusal arrives before the connect deadline on every platform.
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn invalid_host_rules_are_rejected() {
    for rule in ["http://x", "a/b", "*.example.com", "user@host"] {
        let mut settings = HttpClientSettings::default();
        settings.propagate_trace_context_to = vec![rule.into()];
        assert!(AlloyClient::new(&settings).is_err(), "{rule}");
    }
}
