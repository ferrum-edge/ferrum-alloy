//! Manifest validation and gateway configuration generation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_edge::export::{file_mode_document, file_mode_yaml, gitforgeops_files, resources};
use ferrum_alloy_edge::manifest::ServiceManifest;
use ferrum_alloy_edge::yaml;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/manifests")
}

fn manifest() -> ServiceManifest {
    ServiceManifest::from_toml(
        &std::fs::read_to_string(fixtures().join("orders-api.toml")).unwrap(),
    )
    .unwrap()
}

/// Field names of Ferrum Edge v0.9.7 resources (`src/config/types.rs`,
/// `deny_unknown_fields`) that the generator may emit.
const PROXY_FIELDS: &[&str] = &[
    "id",
    "name",
    "namespace",
    "listen_path",
    "backend_scheme",
    "backend_host",
    "backend_port",
    "backend_path",
    "strip_listen_path",
    "backend_connect_timeout_ms",
    "backend_read_timeout_ms",
    "backend_write_timeout_ms",
    "backend_tls_client_cert_path",
    "backend_tls_client_key_path",
    "backend_tls_verify_server_cert",
    "backend_tls_server_ca_cert_path",
    "upstream_id",
    "plugins",
    "labels",
];
const UPSTREAM_FIELDS: &[&str] = &[
    "id",
    "name",
    "namespace",
    "algorithm",
    "targets",
    "health_checks",
    "backend_tls_client_cert_path",
    "backend_tls_client_key_path",
    "backend_tls_verify_server_cert",
    "backend_tls_server_ca_cert_path",
    "labels",
];
const PLUGIN_CONFIG_FIELDS: &[&str] = &[
    "id",
    "plugin_name",
    "namespace",
    "config",
    "scope",
    "proxy_id",
    "enabled",
];
/// `otel_tracing` ALLOWED_CONFIG_KEYS in Edge v0.9.7.
const OTEL_TRACING_KEYS: &[&str] = &[
    "endpoint",
    "service_name",
    "deployment_environment",
    "generate_trace_id",
    "headers",
    "authorization",
    "batch_size",
    "flush_interval_ms",
    "buffer_capacity",
    "buffer_max_bytes",
    "max_attribute_bytes",
    "max_retries",
    "retry_delay_ms",
    "trace_context_trust",
    "root_sampling",
    "root_sampling_ratio",
    "include_url_path",
];

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

#[test]
fn generated_resources_use_only_existing_edge_fields() {
    let resources = resources(&manifest());
    for key in keys(&resources.proxy) {
        assert!(
            PROXY_FIELDS.contains(&key),
            "proxy field {key} is not an Edge v0.9.7 field"
        );
    }
    let upstream = resources.upstream.as_ref().unwrap();
    for key in keys(upstream) {
        assert!(UPSTREAM_FIELDS.contains(&key), "upstream field {key}");
    }
    for plugin in &resources.plugin_configs {
        for key in keys(plugin) {
            assert!(
                PLUGIN_CONFIG_FIELDS.contains(&key),
                "plugin_config field {key}"
            );
        }
        if plugin["plugin_name"] == "otel_tracing" {
            for key in keys(&plugin["config"]) {
                assert!(OTEL_TRACING_KEYS.contains(&key), "otel_tracing key {key}");
            }
        }
        if plugin["plugin_name"] == "correlation_id" {
            assert_eq!(
                keys(&plugin["config"]),
                vec!["echo_downstream", "header_name"]
            );
        }
    }
    let document = file_mode_document(&resources);
    assert_eq!(document["version"], "1");
    for key in ["proxies", "plugin_configs", "consumers", "upstreams"] {
        assert!(document.get(key).is_some(), "{key}");
    }
}

#[test]
fn backend_tls_moves_to_the_upstream_when_one_is_used() {
    let resources = resources(&manifest());
    assert_eq!(resources.proxy["upstream_id"], "orders-api-upstream");
    assert!(
        resources
            .proxy
            .get("backend_tls_client_cert_path")
            .is_none(),
        "Edge ignores proxy TLS with upstream_id"
    );
    assert!(resources.proxy.get("backend_host").is_none());
    let upstream = resources.upstream.unwrap();
    assert_eq!(
        upstream["backend_tls_client_cert_path"],
        "/etc/ferrum/edge-client.pem"
    );
    assert_eq!(upstream["health_checks"]["active"]["http_path"], "/readyz");
    assert_eq!(upstream["health_checks"]["active"]["use_tls"], true);
    assert_eq!(
        upstream["targets"],
        json!([{ "host": "orders.internal", "port": 8443, "weight": 1 }])
    );
}

#[test]
fn without_health_checks_the_proxy_targets_the_backend_directly() {
    let mut manifest = manifest();
    manifest.health = None;
    let resources = resources(&manifest);
    assert!(resources.upstream.is_none());
    assert_eq!(resources.proxy["backend_host"], "orders.internal");
    assert_eq!(resources.proxy["backend_port"], 8443);
    assert_eq!(
        resources.proxy["backend_tls_client_key_path"],
        "/etc/ferrum/edge-client.key"
    );
}

#[test]
fn file_mode_yaml_matches_snapshots() {
    for name in ["orders-api", "plain-http"] {
        let manifest = ServiceManifest::from_toml(
            &std::fs::read_to_string(fixtures().join(format!("{name}.toml"))).unwrap(),
        )
        .unwrap();
        let rendered = file_mode_yaml(&resources(&manifest));
        let path = fixtures().join(format!("{name}.edge.yaml"));
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(&path, &rendered).unwrap();
        }
        // CI also validates these files with the real `ferrum-edge validate`.
        assert_eq!(rendered, std::fs::read_to_string(&path).unwrap(), "{name}");
    }
}

#[test]
fn service_base_path_and_sampling_map_to_edge_fields() {
    let manifest = ServiceManifest::from_toml(
        &std::fs::read_to_string(fixtures().join("plain-http.toml")).unwrap(),
    )
    .unwrap();
    let resources = resources(&manifest);
    assert_eq!(resources.proxy["backend_path"], "/v1");
    assert_eq!(
        resources.proxy["backend_read_timeout_ms"], 0,
        "0 disables the read timeout for long streams"
    );
    let otel = resources
        .plugin_configs
        .iter()
        .find(|p| p["plugin_name"] == "otel_tracing")
        .unwrap();
    assert_eq!(otel["config"]["root_sampling"], "ratio");
    assert_eq!(otel["config"]["root_sampling_ratio"], 0.25);
    assert!(
        resources
            .proxy
            .get("backend_tls_verify_server_cert")
            .is_none(),
        "plain http has no TLS fields"
    );
}

#[test]
fn gitforgeops_files_use_kind_and_spec_only() {
    let files = gitforgeops_files(&resources(&manifest()), "ferrum");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "resources/ferrum/proxies/orders-api.yaml",
            "resources/ferrum/upstreams/orders-api-upstream.yaml",
            "resources/ferrum/plugins/orders-api-correlation-id.yaml",
            "resources/ferrum/plugins/orders-api-otel-tracing.yaml",
        ]
    );
    for file in &files {
        let top: Vec<&str> = file
            .content
            .lines()
            .filter(|l| !l.starts_with(' ') && !l.is_empty())
            .map(|l| l.split(':').next().unwrap())
            .collect();
        assert_eq!(top, vec!["kind", "spec"], "{}", file.path);
    }
}

#[test]
fn invalid_manifests_are_rejected_with_every_problem_listed() {
    let text = r#"
        schema = "ferrum.service_manifest"
        schema_version = "2.0"
        [service]
        name = "Orders_API"
        [api]
        public_path = "orders/../admin"
        [upstream]
        host = "https://orders.internal"
        port = 0
        scheme = "h2c"
        protocols = ["http3"]
        gateway_client_cert_path = "/cert.pem"
        [gateway]
        otel_endpoint = "http://user:pass@collector/v1/traces"
    "#;
    let error = ServiceManifest::from_toml(text).unwrap_err().to_string();
    for expected in [
        "schema_version",
        "service.name",
        "api.public_path",
        "upstream.host",
        "upstream.port",
        "upstream.scheme",
        "HTTP/3",
        "set together",
        "otel_endpoint",
    ] {
        assert!(error.contains(expected), "missing {expected:?} in {error}");
    }
    assert!(ServiceManifest::from_toml("schema = 1").is_err());
    assert!(
        ServiceManifest::from_toml(&format!(
            "{}\nunknown_key = true\n",
            std::fs::read_to_string(fixtures().join("orders-api.toml")).unwrap()
        ))
        .is_err()
    );
}

#[test]
fn yaml_strings_cannot_change_type_or_inject_structure() {
    let value = json!({
        "plain": "value",
        "looks_like_bool": "true",
        "looks_like_number": "0123",
        "injection": "a\"\n- evil: true",
        "colon": "a: b",
        "123key": 1,
        "yes": false,
        "nested": { "list": ["x", 2, true, null, {"k": "v"}], "empty": [], "obj": {} },
    });
    let text = yaml::to_string(&value);
    assert!(text.contains("looks_like_bool: \"true\""));
    assert!(text.contains("looks_like_number: \"0123\""));
    assert!(text.contains(r#"injection: "a\"\n- evil: true""#), "{text}");
    assert!(text.contains("\"123key\": 1"));
    assert!(text.contains("\"yes\": false"), "{text}");
    assert!(text.contains("empty: []") && text.contains("obj: {}"));
    assert!(
        !text.lines().any(|l| l.trim_start().starts_with("- evil")),
        "{text}"
    );
}
