//! PostgreSQL integration. Tests marked `#[ignore]` need a real database at
//! `FERRUM_ALLOY_TEST_DATABASE_URL`; CI provides one in a service container.

#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use ferrum_alloy::config::{DatabaseSettings, Secret};
use ferrum_alloy::health::HealthCheck;
use ferrum_alloy::postgres;

fn settings(url: &str) -> DatabaseSettings {
    DatabaseSettings {
        url: Some(Secret::new(url)),
        acquire_timeout_ms: 500,
        ..DatabaseSettings::default()
    }
}

fn database_url() -> String {
    std::env::var("FERRUM_ALLOY_TEST_DATABASE_URL")
        .expect("set FERRUM_ALLOY_TEST_DATABASE_URL to run database tests")
}

#[tokio::test]
async fn invalid_urls_fail_without_revealing_the_secret() {
    let error = postgres::connect(&settings("mysql://user:hunter2@db/x"), "test").unwrap_err();
    let text = error.to_string();
    assert!(text.contains("not a valid PostgreSQL URL"), "{text}");
    assert!(!text.contains("hunter2"));
    assert!(
        postgres::connect(&DatabaseSettings::default(), "test").is_err(),
        "missing url"
    );
    assert!(postgres::connect(&settings("postgres://u:hunter2@[::1:bad/x"), "test").is_err());
}

#[test]
fn connect_outside_a_runtime_is_an_error_not_a_panic() {
    let error = postgres::connect(
        &settings("postgres://alloy:alloy@127.0.0.1:1/alloy"),
        "test",
    )
    .unwrap_err();
    assert!(error.to_string().contains("Tokio runtime"));
}

#[tokio::test]
async fn an_unreachable_database_is_not_ready_within_the_acquire_timeout() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let pool = postgres::connect(
        &settings(&format!(
            "postgres://alloy:alloy@127.0.0.1:{}/alloy",
            closed.port()
        )),
        "test",
    )
    .unwrap();
    let started = Instant::now();
    let result = postgres::readiness(pool).check().await;
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded by acquire_timeout_ms"
    );
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn migrations_transactions_and_timeouts_behave() {
    let mut cfg = settings(&database_url());
    cfg.statement_timeout_ms = Some(200);
    let pool = postgres::connect(&cfg, "ferrum-alloy-test").unwrap();
    postgres::readiness(pool.clone()).check().await.unwrap();

    sqlx::query("DROP TABLE IF EXISTS alloy_test_items")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS _sqlx_migrations")
        .execute(&pool)
        .await
        .unwrap();
    let migrator = sqlx::migrate::Migrator::new(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/migrations"
    )))
    .await
    .unwrap();
    postgres::migrate(&pool, &migrator).await.unwrap();
    postgres::migrate(&pool, &migrator).await.unwrap(); // idempotent

    // Rolled-back transactions leave no rows.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO alloy_test_items (name) VALUES ($1)")
        .bind("rolled back")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM alloy_test_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Committed work through an instrumented, measured acquire.
    let mut connection = postgres::acquire(&pool).await.unwrap();
    let inserted: i64 = postgres::query("items.insert", "INSERT", "INSERT alloy_test_items")
        .run_result(
            sqlx::query_scalar("INSERT INTO alloy_test_items (name) VALUES ($1) RETURNING id")
                .bind("kept")
                .fetch_one(&mut *connection),
        )
        .await
        .unwrap();
    assert!(inserted > 0);

    // statement_timeout is enforced server-side.
    let slow = sqlx::query("SELECT pg_sleep(1)").execute(&pool).await;
    let message = slow.unwrap_err().to_string();
    assert!(message.contains("statement timeout"), "{message}");
}
