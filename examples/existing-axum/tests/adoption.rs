#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn existing_router_state_and_middleware_keep_working() {
    let (service, metrics) = example_existing_axum::app().unwrap();
    let response = service
        .clone()
        .oneshot(
            Request::get("/hello/ada")
                .header("x-request-id", "abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["x-powered-by"], "existing-app");
    assert_eq!(response.headers()["cache-control"], "private");
    assert_eq!(response.headers()["x-request-id"], "abc");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, "hello ada (visit 1, request abc)");
    assert_eq!(metrics.request_count("GET", "/hello/{name}", 200), 1);

    let response = service
        .oneshot(Request::get("/missing").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        404,
        "the application's own 404 is untouched"
    );
    drop(response);
    assert_eq!(metrics.request_count("GET", "__unmatched__", 404), 1);
}
