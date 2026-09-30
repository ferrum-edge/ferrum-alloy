//! OTLP endpoint environment variables are validated without exposing values.

#![cfg(all(feature = "otel", feature = "axum"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::sync::Arc;

use ferrum_alloy_telemetry::metrics::Metrics;
use ferrum_alloy_telemetry::otel::{OtelPipeline, OtlpConfig, ServiceResource};

#[test]
fn environment_endpoint_errors_never_disclose_the_endpoint() {
    let cases = vec![
        (
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT".to_owned(),
            "http://sentinel-user:sentinel-password@collector:4318/v1/traces".to_owned(),
        ),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(),
            "http://collector:4318/v1/traces sentinel-value".to_owned(),
        ),
        (
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT".to_owned(),
            format!("http://collector/{}sentinel-value", "x".repeat(2_048)),
        ),
    ];

    for (name, endpoint) in cases {
        let output = Command::new(std::env::current_exe().unwrap())
            .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env(&name, endpoint)
            .args([
                "--exact",
                "environment_endpoint_error_child_never_discloses_the_endpoint",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{name}: child test failed\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
        assert!(!stderr.contains("sentinel-user"), "{stderr}");
        assert!(!stderr.contains("sentinel-password"), "{stderr}");
        assert!(!stderr.contains("sentinel-value"), "{stderr}");
    }
}

#[test]
#[ignore = "runs in an isolated process with endpoint environment variables set"]
fn environment_endpoint_error_child_never_discloses_the_endpoint() {
    let resource = ServiceResource {
        name: "env-endpoint-test".to_owned(),
        version: None,
        instance_id: None,
        environment: None,
    };
    let error = OtelPipeline::otlp(
        &resource,
        &OtlpConfig::default(),
        Arc::new(Metrics::default()),
    )
    .unwrap_err();
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(!display.contains("sentinel-user"), "{display}");
    assert!(!display.contains("sentinel-password"), "{display}");
    assert!(!display.contains("sentinel-value"), "{display}");
    assert!(!debug.contains("sentinel-user"), "{debug}");
    assert!(!debug.contains("sentinel-password"), "{debug}");
    assert!(!debug.contains("sentinel-value"), "{debug}");
}
