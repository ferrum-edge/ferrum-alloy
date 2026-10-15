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
//!
//! TLS: for a host that is not local (a Unix socket or a literal loopback
//! address; DNS names, `localhost` included, are not), `sslmode=prefer`,
//! libpq's and SQLx's default when the URL and `PGSSLMODE` name none, is
//! raised to `require`, so a server or attacker that
//! declines TLS gets an error instead of a silent plaintext fallback. Use
//! `verify-full` (with `sslrootcert` for a private CA) to authenticate the
//! server too. Plaintext to a remote host needs an explicit `sslmode=disable`
//! or `allow`, and logs a warning when the pool is built.
//!
//! URL query parameters SQLx does not recognize are rejected before SQLx
//! parses the URL: SQLx would log each one with its value (a libpq
//! `sslpassword` is a secret) and silently ignore it (a typo such as
//! `sslmod=verify-full` would not apply). The error names the parameter,
//! never its value.

use std::str::FromStr;
use std::time::{Duration, Instant};

use ferrum_alloy_telemetry::operation::{Operation, record_pool_wait};
use sqlx::migrate::Migrator;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
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
    reject_unrecognized_parameters(text)?;
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
    if !is_local(&options) {
        match options.get_ssl_mode() {
            // No plaintext fallback off this host (#182).
            PgSslMode::Prefer => options = options.ssl_mode(PgSslMode::Require),
            PgSslMode::Disable | PgSslMode::Allow => tracing::warn!(
                target: "ferrum_alloy::postgres",
                "database.url permits plaintext to a non-local host (sslmode=disable or allow): \
                 credentials and data can cross the network unencrypted; use sslmode=verify-full"
            ),
            _ => {}
        }
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

/// Query parameters SQLx 0.9 recognizes in a connection URL (its
/// `options/parse.rs`), besides the `options[<name>]` form handled below.
const RECOGNIZED_PARAMETERS: &[&str] = &[
    "sslmode",
    "ssl-mode",
    "sslrootcert",
    "ssl-root-cert",
    "ssl-ca",
    "sslcert",
    "ssl-cert",
    "sslkey",
    "ssl-key",
    "statement-cache-capacity",
    "host",
    "hostaddr",
    "port",
    "dbname",
    "user",
    "password",
    "application_name",
    "options",
];

/// Rejects URL query parameters SQLx does not recognize, before SQLx sees
/// the URL. SQLx logs every unrecognized parameter *with its value* and
/// then ignores it, so a libpq parameter it does not know (for example
/// `sslpassword`, a secret) would reach the logs, and a typo in a security
/// parameter (for example `sslmod=verify-full`) would silently not apply.
/// The URL is parsed with the same parser SQLx uses, so normalization
/// (tab/newline stripping, percent-decoding) cannot split a name past
/// this check. The error names the parameter, never its value, and only
/// when the name is a plain parameter-shaped token: anything else could
/// be a fragment of a malformed URL's credential, so it is withheld.
fn reject_unrecognized_parameters(text: &str) -> Result<(), AlloyError> {
    let url = url::Url::parse(text).map_err(|_| {
        AlloyError::Integration("database.url is not a valid PostgreSQL URL".into())
    })?;
    for (key, _value) in url.query_pairs() {
        let key = key.as_ref();
        let options_entry = key
            .strip_prefix("options[")
            .and_then(|rest| rest.strip_suffix(']'))
            .is_some_and(|name| !name.is_empty());
        if !RECOGNIZED_PARAMETERS.contains(&key) && !options_entry {
            let named = if is_plain_parameter_name(key) {
                format!(" {key:?}")
            } else {
                String::new()
            };
            return Err(AlloyError::Integration(format!(
                "database.url: unrecognized connection parameter{named} \
                 (SQLx ignores parameters it does not recognize)"
            )));
        }
    }
    Ok(())
}

/// Whether a parameter name is safe to repeat in an error: short and
/// made only of the characters real parameter names use. Anything else
/// (an `@`, a `/`, whitespace) is more likely a piece of a malformed
/// URL's userinfo than a parameter name.
fn is_plain_parameter_name(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'[' | b']'))
}

/// Whether the connection stays on this host: a Unix socket or a literal
/// loopback address. DNS names, `localhost` included, do not qualify: the
/// resolver decides where they connect (the same rule as `auth.jwt.jwks_url`).
fn is_local(options: &PgConnectOptions) -> bool {
    if options.get_socket().is_some() {
        return true;
    }
    let host = options.get_host();
    // SQLx keeps the brackets of an IPv6 literal.
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.starts_with('/')
        || literal
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Acquires a connection and records the wait on the current operation span.
/// Call it inside the future given to [`query`]'s `run`/`run_result`, so the
/// wait lands on that operation; outside an operation span nothing is
/// recorded.
///
/// ```ignore
/// let row = postgres::query("orders.fetch", "SELECT", "SELECT orders by id")
///     .run_result(async {
///         let mut connection = postgres::acquire(&pool).await?;
///         sqlx::query("SELECT 1").execute(&mut *connection).await
///     })
///     .await?;
/// ```
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
