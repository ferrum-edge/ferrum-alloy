#![cfg(feature = "otel")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::AlloyConfig;

#[test]
fn rejected_otlp_endpoint_credentials_do_not_reach_alloy_startup_errors() {
    let mut config = AlloyConfig::default();
    config.otlp.enabled = true;
    config.otlp.endpoint =
        Some("http://sentinel-user:sentinel-password@collector:4318/v1/traces".to_owned());
    let config_debug = format!("{config:?}");
    assert!(!config_debug.contains("sentinel-user"));
    assert!(!config_debug.contains("sentinel-password"));

    let error = match AlloyApp::new("otlp-credentials")
        .config(config)
        .prepare()
    {
        Ok(_) => panic!("credential-bearing endpoint was accepted"),
        Err(error) => error,
    };
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(display.contains("without credentials"));
    assert!(!display.contains("sentinel-user"));
    assert!(!display.contains("sentinel-password"));
    assert!(!debug.contains("sentinel-user"));
    assert!(!debug.contains("sentinel-password"));
}
