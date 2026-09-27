//! Global subscriber conflicts are detected instead of silently losing
//! telemetry. This test binary owns its process-wide subscriber.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::Router;
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::{AlloyApp, TelemetryInit};

#[test]
fn an_existing_global_subscriber_is_respected_and_conflicts_are_reported() {
    // The application installs its own subscriber first.
    tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();

    // Without OTLP export, Alloy uses the existing subscriber.
    let parts = AlloyApp::new("conflict")
        .router(Router::new())
        .config(AlloyConfig::default())
        .into_parts()
        .unwrap();
    assert_eq!(parts.telemetry.mode, "existing");
    drop(parts);

    // OTLP export requested, but the existing subscriber has no
    // OpenTelemetry layer: traces would be lost, so startup fails.
    let mut config = AlloyConfig::default();
    config.otlp.enabled = true;
    let result = AlloyApp::new("conflict")
        .router(Router::new())
        .config(config.clone())
        .into_parts();
    if cfg!(feature = "otel") {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("already installed"), "{error}");
    } else {
        assert!(result.unwrap_err().to_string().contains("`otel` feature"));
    }

    // Application-owned telemetry with OTLP enabled is contradictory.
    let error = AlloyApp::new("conflict")
        .router(Router::new())
        .config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("otlp") || error.contains("`otel` feature"),
        "{error}"
    );
}
