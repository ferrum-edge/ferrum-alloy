//! Rate limits on the management listener: the `429` shape, per-client
//! budgets keyed by transport address, separate probe budgets, exempt
//! networks, and a bounded client table.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::{IpAddr, SocketAddr};

use axum::Router;
use axum::body::Body;
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::telemetry::PeerInfo;
use ferrum_alloy::{AlloyApp, AlloyParts, TelemetryInit};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use support::{TOKEN, config, fetch, fetch_with, start};
use tower::ServiceExt;

fn app() -> AlloyApp {
    AlloyApp::new("rate-limit-test").router(Router::new())
}

fn parts(config: AlloyConfig) -> AlloyParts {
    app()
        .config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

/// Sends a request to the management router as if it arrived from `peer`.
async fn call(parts: &AlloyParts, peer: IpAddr, path: &str, headers: &[(&str, &str)]) -> Reply {
    let router = parts.management_router.clone().unwrap();
    let mut builder = Request::get(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
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
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

/// The value of an exact Prometheus series in `text`.
fn metric(text: &str, series: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("{series} is missing from:\n{text}"))
}

#[tokio::test]
async fn a_burst_beyond_the_limit_gets_a_429_problem() {
    let mut cfg = config();
    cfg.management.rate_limit.requests_per_second = 1;
    cfg.management.rate_limit.burst = 3;
    let server = start(app(), cfg).await;
    let url = server.management_url("/metrics");

    // Failed token attempts count against the budget like any request.
    for _ in 0..3 {
        let reply = fetch_with(&url, &[("authorization", "Bearer wrong")]).await;
        assert_eq!(reply.status, 401);
    }
    let bearer = format!("Bearer {TOKEN}");
    let mut limited = None;
    // At one token per second, ten requests cannot all be admitted.
    for _ in 0..10 {
        let reply = fetch_with(&url, &[("authorization", &bearer)]).await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(reply);
            break;
        }
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    let reply = limited.expect("a request beyond the burst is rejected");
    assert_eq!(reply.headers["content-type"], "application/problem+json");
    assert_eq!(reply.headers["cache-control"], "no-store");
    let retry_after = reply.headers["retry-after"].to_str().unwrap();
    assert_eq!(retry_after, "1", "one token per second");
    let body = reply.json();
    assert_eq!(
        body["type"],
        "tag:ferrumedge.com,2026:alloy/problem/rate-limited"
    );
    assert_eq!(body["title"], "Too many requests");
    assert_eq!(body["status"], 429);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn probes_are_served_while_metrics_is_saturated() {
    let mut cfg = config();
    let limit = &mut cfg.management.rate_limit;
    limit.requests_per_second = 1;
    limit.burst = 2;
    limit.global_requests_per_second = 1;
    limit.global_burst = 2;
    let server = start(app(), cfg).await;

    let mut saturated = false;
    for _ in 0..10 {
        let reply = fetch(&server.management_url("/metrics")).await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            saturated = true;
            break;
        }
    }
    assert!(saturated, "/metrics is saturated");
    for _ in 0..10 {
        assert_eq!(fetch(&server.management_url("/livez")).await.status, 200);
        assert_eq!(fetch(&server.management_url("/readyz")).await.status, 200);
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn clients_are_limited_independently_by_transport_address() {
    let mut cfg = config();
    cfg.management.rate_limit.requests_per_second = 1;
    cfg.management.rate_limit.burst = 2;
    let parts = parts(cfg);
    let (a, b) = (ip("192.0.2.1"), ip("192.0.2.2"));

    // Unknown paths are charged too.
    for _ in 0..2 {
        assert_eq!(call(&parts, a, "/nope", &[]).await.status, 404);
    }
    let reply = call(&parts, a, "/metrics", &[]).await;
    assert_eq!(reply.status, 429, "{}", reply.body);
    assert!(reply.headers.contains_key("retry-after"));

    // Forwarded headers never make a request another client.
    let forwarded = [
        ("x-forwarded-for", "198.51.100.7"),
        ("forwarded", "for=198.51.100.7"),
        ("x-real-ip", "198.51.100.7"),
    ];
    assert_eq!(call(&parts, a, "/metrics", &forwarded).await.status, 429);

    // Another address has its own budget.
    assert_eq!(call(&parts, b, "/metrics", &[]).await.status, 401);
    // So does another IPv6 /64, while one /64 is one client.
    let c = ip("2001:db8::1");
    let d = ip("2001:db8::2");
    let e = ip("2001:db8:0:1::1");
    assert_eq!(call(&parts, c, "/nope", &[]).await.status, 404);
    assert_eq!(call(&parts, d, "/nope", &[]).await.status, 404);
    assert_eq!(call(&parts, c, "/nope", &[]).await.status, 429);
    assert_eq!(call(&parts, e, "/nope", &[]).await.status, 404);
}

#[tokio::test]
async fn the_client_table_stays_bounded_under_many_distinct_peers() {
    let mut cfg = config();
    let limit = &mut cfg.management.rate_limit;
    limit.requests_per_second = 1;
    limit.burst = 2;
    // At one listener token per second, 500 requests are not all admitted
    // unless they take minutes.
    limit.global_requests_per_second = 1;
    limit.global_burst = 8;
    limit.max_clients = 8;
    // Exempt loopback explicitly so the scrape can inspect the saturated table.
    limit.exempt_networks.push("127.0.0.0/8".parse().unwrap());
    let parts = parts(cfg);

    for n in 0..500u32 {
        let [_, _, high, low] = n.to_be_bytes();
        let peer = IpAddr::from([10, 0, high, low]);
        let status = call(&parts, peer, "/nope", &[]).await.status;
        assert!(status == 404 || status == 429, "{status}");
    }

    let bearer = format!("Bearer {TOKEN}");
    let auth = [("authorization", bearer.as_str())];
    let reply = call(&parts, ip("127.0.0.1"), "/metrics", &auth).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let text = reply.body;
    let tracked_series = "ferrum_alloy_management_rate_limit_clients{budget=\"endpoints\"}";
    let tracked = metric(&text, tracked_series);
    assert!(tracked <= 8, "tracked clients: {tracked}");
    let name = "ferrum_alloy_management_rate_limited_total";
    let mut rejected = 0;
    for scope in ["client", "shared", "global"] {
        let series = format!("{name}{{budget=\"endpoints\",scope=\"{scope}\"}}");
        rejected += metric(&text, &series);
    }
    assert!(rejected > 0, "some of the 500 requests were rejected");
    let probes = metric(
        &text,
        "ferrum_alloy_management_rate_limited_total{budget=\"probes\",scope=\"client\"}",
    );
    assert_eq!(probes, 0);
}

#[tokio::test]
async fn loopback_and_exempt_networks_are_not_limited() {
    let mut cfg = config();
    let limit = &mut cfg.management.rate_limit;
    limit.requests_per_second = 1;
    limit.burst = 1;
    limit.probe_requests_per_second = 1;
    limit.probe_burst = 1;
    limit.exempt_networks = vec![
        "127.0.0.0/8".parse().unwrap(),
        "::1/128".parse().unwrap(),
    ];
    // A node network, so kubelet probes are never refused.
    limit.exempt_networks.push("10.244.0.0/16".parse().unwrap());
    let parts = parts(cfg);

    // Loopback includes sidecars such as Istio, which connect from 127.0.0.6.
    for peer in ["127.0.0.1", "127.0.0.6", "::1", "10.244.3.7"] {
        for _ in 0..20 {
            assert_eq!(call(&parts, ip(peer), "/livez", &[]).await.status, 200);
            assert_eq!(call(&parts, ip(peer), "/nope", &[]).await.status, 404);
        }
    }
    // Any other peer is limited.
    let other = ip("10.245.0.1");
    let mut limited = false;
    for _ in 0..10 {
        limited |= call(&parts, other, "/livez", &[]).await.status == 429;
    }
    assert!(limited, "other peers are limited");
}

#[tokio::test]
async fn rate_limiting_can_be_disabled() {
    let mut cfg = config();
    cfg.management.rate_limit.enabled = false;
    cfg.management.rate_limit.burst = 0;
    let parts = parts(cfg);
    let peer = ip("192.0.2.1");
    for _ in 0..100 {
        assert_eq!(call(&parts, peer, "/nope", &[]).await.status, 404);
    }
    let bearer = format!("Bearer {TOKEN}");
    let auth = [("authorization", bearer.as_str())];
    let reply = call(&parts, peer, "/metrics", &auth).await;
    assert_eq!(reply.status, 200);
    assert!(!reply.body.contains("ferrum_alloy_management_rate_limit"));
}
