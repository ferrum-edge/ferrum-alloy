//! CRUD against a real database (`FERRUM_ALLOY_TEST_DATABASE_URL`, provided
//! by CI's PostgreSQL service container), plus an OpenAPI parity check that
//! needs no database.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

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

/// Captures the last `alloy.db.pool_wait_ms` recorded on any span.
#[derive(Clone, Default)]
struct PoolWait(std::sync::Arc<std::sync::Mutex<Option<f64>>>);

impl PoolWait {
    fn get(&self) -> Option<f64> {
        *self.0.lock().expect("pool wait lock")
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PoolWait {
    fn on_record(
        &self,
        _span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        values.record(&mut PoolWaitVisitor(self));
    }
}

struct PoolWaitVisitor<'a>(&'a PoolWait);

impl tracing::field::Visit for PoolWaitVisitor<'_> {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "alloy.db.pool_wait_ms" {
            *self.0.0.lock().expect("pool wait lock") = Some(value);
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
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

    // With the only connection held, creating an order waits for it, and the
    // wait is recorded on the insert's operation span.
    {
        let wait = PoolWait::default();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(wait.clone()));
        let single = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let held = single.acquire().await.unwrap();
        let (contended, _) = example_postgres_api::api(single);
        let release = async move {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            drop(held);
        };
        let request = Request::post("/orders")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"item":"wait","quantity":1}"#))
            .unwrap();
        let ((), (status, waited)) = tokio::join!(release, call(&contended, request));
        assert_eq!(status, StatusCode::CREATED);
        assert!(
            wait.get().is_some(),
            "the insert records alloy.db.pool_wait_ms"
        );
        // Remove it so the listing below sees only this test's order.
        let id = waited["id"].as_i64().unwrap();
        let (status, _) = call(
            &router,
            Request::delete(format!("/orders/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

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
