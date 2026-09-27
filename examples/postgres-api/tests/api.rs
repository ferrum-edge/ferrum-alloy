//! CRUD against a real database (`FERRUM_ALLOY_TEST_DATABASE_URL`, provided
//! by CI's PostgreSQL service container), plus an OpenAPI parity check that
//! needs no database.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn documented_paths_match_routes() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/x")
        .unwrap();
    let (_, document) = example_postgres_api::api(pool);
    let mut paths: Vec<&str> = document.paths.paths.keys().map(String::as_str).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["/orders", "/orders/{id}"]);
}

async fn call(router: &axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn crud_round_trip() {
    let url = std::env::var("FERRUM_ALLOY_TEST_DATABASE_URL").unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS orders")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS _sqlx_migrations")
        .execute(&pool)
        .await
        .unwrap();
    ferrum_alloy::postgres::migrate(&pool, &example_postgres_api::MIGRATOR)
        .await
        .unwrap();
    let (router, _) = example_postgres_api::api(pool);

    let (status, created) = call(
        &router,
        Request::post("/orders")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"item":"tea","quantity":2}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_i64().unwrap();

    let (status, fetched) = call(
        &router,
        Request::get(format!("/orders/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["item"], "tea");

    let (status, invalid) = call(
        &router,
        Request::post("/orders")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"item":"","quantity":0}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid["errors"].as_array().unwrap().len(), 2);

    let (status, list) = call(
        &router,
        Request::get("/orders?limit=5").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, _) = call(
        &router,
        Request::delete(format!("/orders/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, missing) = call(
        &router,
        Request::get(format!("/orders/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["title"], "Order not found");
}
