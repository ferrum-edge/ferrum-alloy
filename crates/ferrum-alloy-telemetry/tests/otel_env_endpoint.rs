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
            vec![(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://sentinel-user:sentinel-password@collector:4318/v1/traces".to_owned(),
            )],
            "without credentials",
        ),
        (
            vec![(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://collector:4318/v1/traces sentinel-value".to_owned(),
            )],
            "without credentials",
        ),
        (
            vec![(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                format!("http://collector/{}sentinel-value", "x".repeat(2_048)),
            )],
            "2048 bytes",
        ),
        (
            vec![
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", String::new()),
                (
                    "OTEL_EXPORTER_OTLP_ENDPOINT",
                    format!("http://collector/{}sentinel-value", "x".repeat(2_048)),
                ),
            ],
            "2048 bytes",
        ),
    ];

    for (endpoints, expected_message) in cases {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        for (name, endpoint) in &endpoints {
            command.env(name, endpoint);
        }
        let output = command
            .args([
                "--exact",
                "environment_endpoint_error_child_never_discloses_the_endpoint",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert!(stdout.contains(expected_message), "{stdout}");
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
    println!("{display}");
    assert!(!display.contains("sentinel-user"), "{display}");
    assert!(!display.contains("sentinel-password"), "{display}");
    assert!(!display.contains("sentinel-value"), "{display}");
    assert!(!debug.contains("sentinel-user"), "{debug}");
    assert!(!debug.contains("sentinel-password"), "{debug}");
    assert!(!debug.contains("sentinel-value"), "{debug}");
}
