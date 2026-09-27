//! PostgreSQL integration (feature `postgres`), built on SQLx.
//!
//! * [`connect`] builds an ordinary `sqlx::PgPool`. The pool is exposed
//!   as-is; use SQLx queries and transactions directly.
//! * [`acquire`] measures time waiting for a pooled connection separately
//!   from the database call itself (`alloy.db.pool_wait_ms`).
//! * [`query`] marks an explicitly instrumented database call. Its duration
//!   is application-observed call time (pool wait, network, driver, server),
//!   not database server execution time.
//! * [`readiness`] is a `SELECT 1` check for Alloy's cached readiness.
//! * [`migrate`] runs migrations only when called. Application schema
//!   migrations are the application's responsibility; they are unrelated to
//!   Ferrum Edge's own database rules.
//!
//! Connections are established lazily, so a database outage at startup makes
//! the service *not ready* rather than crash-looping; an invalid URL still
//! fails at startup.

use std::str::FromStr;
use std::time::{Duration, Instant};

use ferrum_alloy_telemetry::operation::{Operation, record_pool_wait};
use sqlx::migrate::Migrator;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres};

use crate::config::DatabaseSettings;
use crate::error::AlloyError;
use crate::health::{CheckError, CheckFuture, HealthCheck};

/// Builds a lazily connecting pool from `[database]`. Must be called
/// inside a Tokio runtime (the pool runs maintenance tasks).
pub fn connect(settings: &DatabaseSettings, application_name: &str) -> Result<PgPool, AlloyError> {
    let url = settings
        .url
        .as_ref()
        .ok_or_else(|| AlloyError::Integration("database.url is not configured".into()))?;
    // The URL is a secret: never include it in errors.
    let text = url.expose();
    if !(text.starts_with("postgres://") || text.starts_with("postgresql://")) {
        return Err(AlloyError::Integration(
            "database.url is not a valid PostgreSQL URL (expected postgres:// or postgresql://)"
                .into(),
        ));
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(AlloyError::Integration(
            "ferrum_alloy::postgres::connect must be called inside a Tokio runtime".into(),
        ));
    }
    let mut options = PgConnectOptions::from_str(text)
        .map_err(|_| AlloyError::Integration("database.url is not a valid PostgreSQL URL".into()))?
        .application_name(application_name);
    if let Some(timeout) = settings.statement_timeout_ms {
        options = options.options([("statement_timeout", timeout.to_string())]);
    }
    let pool = PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .min_connections(settings.min_connections)
        .acquire_timeout(Duration::from_millis(settings.acquire_timeout_ms))
        .idle_timeout(settings.idle_timeout_ms.map(Duration::from_millis))
        .max_lifetime(settings.max_lifetime_ms.map(Duration::from_millis))
        .test_before_acquire(true)
        .connect_lazy_with(options);
    Ok(pool)
}

/// Acquires a connection and records the wait on the current operation span.
pub async fn acquire(pool: &PgPool) -> Result<PoolConnection<Postgres>, sqlx::Error> {
    let started = Instant::now();
    let connection = pool.acquire().await;
    record_pool_wait(started.elapsed());
    connection
}

/// An instrumented database operation. Use a static, parameter-free
/// `summary` (e.g. `"SELECT orders by id"`); never pass SQL values.
pub fn query(name: &'static str, operation: &'static str, summary: &'static str) -> Operation {
    Operation::new(name)
        .db("postgresql", operation)
        .summary(summary)
}

/// Runs embedded migrations. Call it deliberately (for example from a
/// separate migration command or job), not implicitly on every start.
pub async fn migrate(pool: &PgPool, migrator: &Migrator) -> Result<(), AlloyError> {
    migrator
        .run(pool)
        .await
        .map_err(|e| AlloyError::Integration(format!("migration failed: {e}")))
}

/// A readiness check running `SELECT 1`. Alloy caches its result, so
/// health traffic never floods the database.
pub fn readiness(pool: PgPool) -> impl HealthCheck {
    PgReadiness(pool)
}

struct PgReadiness(PgPool);

impl HealthCheck for PgReadiness {
    fn check(&self) -> CheckFuture {
        let pool = self.0.clone();
        Box::pin(async move {
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .map(|_| ())
                // Error text can include host names; it is shown only on the
                // protected management listener.
                .map_err(|e| CheckError::new(format!("postgres: {e}")))
        })
    }
}
