//! CORS, compression, and OpenAPI serving.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ferrum_alloy::AlloyApp;
use support::{config, fetch_with, start};

fn router() -> Router {
    let big = "x".repeat(8 * 1024);
    let (a, b, c, d) = (big.clone(), big.clone(), big.clone(), big);
    Router::new()
        .route(
            "/big",
            get(move || {
                let a = a.clone();
                async move { a }
            }),
        )
        .route(
            "/big-no-store",
            get(move || {
                let b = b.clone();
                async move { ([("cache-control", "no-store")], b).into_response() }
            }),
        )
        .route(
            "/big-cookie",
            get(move || {
                let c = c.clone();
                async move { ([("set-cookie", "session=1")], c).into_response() }
            }),
        )
        .route(
            "/events",
            get(move || {
                let d = d.clone();
                async move {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(d))
                        .unwrap()
                }
            }),
        )
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn compression_skips_streams_and_secret_bearing_responses() {
    let mut cfg = config();
    cfg.compression.enabled = true;
    let server = start(AlloyApp::new("layers").router(router()), cfg).await;
    let gzip = [("accept-encoding", "gzip")];
    let compressed = fetch_with(&server.url("/big"), &gzip).await;
    assert_eq!(compressed.headers["content-encoding"], "gzip");
    assert!(compressed.body.len() < 8 * 1024);
    for path in ["/big-no-store", "/big-cookie", "/events"] {
        let reply = fetch_with(&server.url(path), &gzip).await;
        assert!(
            !reply.headers.contains_key("content-encoding"),
            "{path} must not be compressed"
        );
        assert_eq!(reply.body.len(), 8 * 1024, "{path}");
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn compression_is_off_by_default() {
    let server = start(AlloyApp::new("layers").router(router()), config()).await;
    let reply = fetch_with(&server.url("/big"), &[("accept-encoding", "gzip")]).await;
    assert!(!reply.headers.contains_key("content-encoding"));
    server.shutdown().await.unwrap();
}

#[cfg(feature = "cors")]
#[tokio::test]
async fn cors_allows_only_listed_origins() {
    use bytes::Bytes;
    use http::Request;
    use http_body_util::Full;
    use support::send;

    let mut cfg = config();
    cfg.cors.enabled = true;
    cfg.cors.allowed_origins = vec!["https://app.example".into()];
    cfg.cors.allowed_methods = vec!["GET".into()];
    let server = start(AlloyApp::new("layers").router(router()), cfg).await;
    let preflight = |origin: &'static str| {
        Request::options(server.url("/big"))
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .body(Full::<Bytes>::default())
            .unwrap()
    };
    let allowed = send(preflight("https://app.example")).await;
    assert_eq!(
        allowed.headers["access-control-allow-origin"],
        "https://app.example"
    );
    let denied = send(preflight("https://evil.example")).await;
    assert!(!denied.headers.contains_key("access-control-allow-origin"));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn cors_is_off_by_default() {
    let server = start(AlloyApp::new("layers").router(router()), config()).await;
    let reply = fetch_with(&server.url("/big"), &[("origin", "https://evil.example")]).await;
    assert!(!reply.headers.contains_key("access-control-allow-origin"));
    server.shutdown().await.unwrap();
}

#[cfg(feature = "openapi")]
fn document() -> ferrum_alloy::utoipa::openapi::OpenApi {
    ferrum_alloy::utoipa::openapi::OpenApiBuilder::new()
        .info(
            ferrum_alloy::utoipa::openapi::InfoBuilder::new()
                .title("layers")
                .version("1")
                .build(),
        )
        .build()
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn openapi_is_protected_unless_explicitly_public() {
    use support::{TOKEN, fetch};

    let server = start(
        AlloyApp::new("layers")
            .router(router())
            .openapi(&document()),
        config(),
    )
    .await;
    assert_eq!(
        fetch(&server.management_url("/openapi.json")).await.status,
        401
    );
    let bearer = format!("Bearer {TOKEN}");
    let reply = fetch_with(
        &server.management_url("/openapi.json"),
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["info"]["title"], "layers");
    assert_eq!(
        fetch(&server.url("/openapi.json")).await.status,
        404,
        "not on the public listener"
    );
    server.shutdown().await.unwrap();

    let mut cfg = config();
    cfg.openapi.public = true;
    let server = start(
        AlloyApp::new("layers")
            .router(router())
            .openapi(&document()),
        cfg,
    )
    .await;
    let reply = fetch(&server.url("/openapi.json")).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.headers["cache-control"], "no-store");
    server.shutdown().await.unwrap();
}
