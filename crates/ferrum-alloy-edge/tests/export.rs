//! Manifest validation and gateway configuration generation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_edge::export::{file_mode_document, file_mode_yaml, gitforgeops_files, resources};
use ferrum_alloy_edge::manifest::{EDGE_RESOURCE_ID_MAX_LENGTH, ServiceManifest};
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

fn generated_ids(resources: &ferrum_alloy_edge::export::EdgeResources) -> Vec<&str> {
    let mut ids = Vec::new();
    if let Some(upstream) = &resources.upstream {
        ids.push(upstream["id"].as_str().unwrap());
    }
    ids.extend(
        resources
            .plugin_configs
            .iter()
            .map(|plugin| plugin["id"].as_str().unwrap()),
    );
    ids
}

/// Field names of Ferrum Edge v0.9.11 and v0.9.10 resources
/// (`src/config/types.rs`, `deny_unknown_fields`) that the generator may emit.
/// v0.9.9's `allow_path_parameters` and `websocket_permessage_deflate` are
/// deliberately absent: v0.9.8 rejects them, and their defaults are wanted.
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
/// `otel_tracing` ALLOWED_CONFIG_KEYS in Edge v0.9.11 and v0.9.10 (unchanged).
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
            "proxy field {key} is not an Edge v0.9.11 and v0.9.10 field"
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
fn proxy_id_boundary_accounts_for_the_longest_generated_resource_id() {
    let mut manifest = manifest();
    let longest_suffix_length = "-correlation-id".len();
    let max_proxy_id_length = EDGE_RESOURCE_ID_MAX_LENGTH - longest_suffix_length;
    manifest.gateway.proxy_id = Some("a".repeat(max_proxy_id_length));
    manifest.validate().unwrap();

    let resources = resources(&manifest);
    assert!(
        generated_ids(&resources)
            .iter()
            .all(|id| id.len() <= EDGE_RESOURCE_ID_MAX_LENGTH)
    );
    let longest_id = format!("{}-correlation-id", manifest.proxy_id());
    assert!(generated_ids(&resources).contains(&longest_id.as_str()));

    manifest.gateway.proxy_id = Some("a".repeat(max_proxy_id_length + 1));
    let error = manifest.validate().unwrap_err().to_string();
    assert!(error.contains("254-character limit"), "{error}");
    assert!(error.contains("-correlation-id"), "{error}");
}

#[test]
fn each_generated_id_suffix_is_checked_at_its_boundary() {
    for (family, suffix, base_length) in [
        (
            "upstream",
            "-upstream",
            EDGE_RESOURCE_ID_MAX_LENGTH - "-upstream".len(),
        ),
        (
            "correlation",
            "-correlation-id",
            EDGE_RESOURCE_ID_MAX_LENGTH - "-correlation-id".len(),
        ),
        (
            "otel",
            "-otel-tracing",
            EDGE_RESOURCE_ID_MAX_LENGTH - "-otel-tracing".len(),
        ),
    ] {
        let mut manifest = manifest();
        if family != "upstream" {
            manifest.health = None;
        }
        manifest.gateway.correlation_id = family == "correlation";
        manifest.gateway.otel_endpoint = (family == "otel").then(|| "http://otel".to_owned());
        manifest.gateway.proxy_id = Some("a".repeat(base_length));
        manifest.validate().unwrap();

        let resources = resources(&manifest);
        assert!(
            generated_ids(&resources)
                .iter()
                .all(|id| id.len() <= EDGE_RESOURCE_ID_MAX_LENGTH)
        );
        assert!(
            generated_ids(&resources)
                .iter()
                .any(|id| id.ends_with(suffix))
        );

        manifest.gateway.proxy_id = Some("a".repeat(base_length + 1));
        let error = manifest.validate().unwrap_err().to_string();
        assert!(error.contains("254-character limit"), "{family}: {error}");
        assert!(error.contains(suffix), "{family}: {error}");
    }
}

#[test]
fn direct_proxy_id_can_use_the_full_edge_limit_without_derived_resources() {
    let mut manifest = manifest();
    manifest.health = None;
    manifest.gateway.correlation_id = false;
    manifest.gateway.otel_endpoint = None;
    manifest.gateway.proxy_id = Some("a".repeat(EDGE_RESOURCE_ID_MAX_LENGTH));

    manifest.validate().unwrap();
    let resources = resources(&manifest);
    assert!(generated_ids(&resources).is_empty());
    assert_eq!(
        resources.proxy["id"].as_str().unwrap().len(),
        EDGE_RESOURCE_ID_MAX_LENGTH
    );
}

/// Literal paths Ferrum Edge refuses as a `listen_path`: a `;` path parameter
/// (v0.9.9 without `allow_path_parameters`, which export never sets), and on
/// both supported releases a `.` segment, a percent-escape, or a backslash.
const REFUSED_PATHS: &[&str] = &[
    "/orders;v=1",
    "/orders/./items",
    "/orders%2Fitems",
    "/orders\\items",
];

#[test]
fn paths_edge_would_refuse_are_rejected() {
    let fixture = std::fs::read_to_string(fixtures().join("orders-api.toml")).unwrap();
    for path in REFUSED_PATHS {
        let line = format!("public_path = {path:?}");
        let text = fixture.replace("public_path = \"/orders\"", &line);
        assert_ne!(text, fixture);
        let error = ServiceManifest::from_toml(&text).unwrap_err().to_string();
        assert!(error.contains("api.public_path"), "{path}: {error}");
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
