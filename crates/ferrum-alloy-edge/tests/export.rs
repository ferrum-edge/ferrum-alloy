//! Manifest validation and gateway configuration generation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_edge::export::{
    ExportError, file_mode_document, file_mode_yaml, gitforgeops_files, resources,
};
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

/// Field names of the supported Ferrum Edge releases' resources
/// (`docs/compatibility.json`; `src/config/types.rs`, `deny_unknown_fields`)
/// that the generator may emit. `allow_path_parameters` and
/// `websocket_permessage_deflate` are deliberately absent: their defaults are
/// wanted (see `src/export.rs`).
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
/// `otel_tracing` ALLOWED_CONFIG_KEYS in the supported Edge releases.
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
    let resources = resources(&manifest()).unwrap();
    for key in keys(&resources.proxy) {
        assert!(
            PROXY_FIELDS.contains(&key),
            "proxy field {key} is not a field of the supported Edge releases"
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
    let resources = resources(&manifest()).unwrap();
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
    let resources = resources(&manifest).unwrap();
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
    for name in ["orders-api", "plain-http", "minimal"] {
        let manifest = ServiceManifest::from_toml(
            &std::fs::read_to_string(fixtures().join(format!("{name}.toml"))).unwrap(),
        )
        .unwrap();
        let rendered = file_mode_yaml(&resources(&manifest).unwrap());
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
    let resources = resources(&manifest).unwrap();
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
    let files = gitforgeops_files(&resources(&manifest()).unwrap(), "ferrum");
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

    let resources = resources(&manifest).unwrap();
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

/// The `otel_tracing` plugin is generated for every manifest (#307), so its
/// suffix bounds the proxy id even without health checks, correlation, or
/// an exporter; the shorter `-upstream` suffix never binds.
#[test]
fn each_generated_id_suffix_is_checked_at_its_boundary() {
    for (family, suffix, base_length) in [
        (
            "upstream",
            "-otel-tracing",
            EDGE_RESOURCE_ID_MAX_LENGTH - "-otel-tracing".len(),
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
        (
            "propagation-only",
            "-otel-tracing",
            EDGE_RESOURCE_ID_MAX_LENGTH - "-otel-tracing".len(),
        ),
    ] {
        let mut manifest = manifest();
        if family != "upstream" {
            manifest.health = None;
        }
        manifest.gateway.correlation_id = family == "correlation";
        manifest.gateway.otel_endpoint = (family == "otel").then(|| "https://otel".to_owned());
        manifest.gateway.proxy_id = Some("a".repeat(base_length));
        manifest.validate().unwrap();

        let resources = resources(&manifest).unwrap();
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
fn the_trace_plugin_bounds_a_proxy_id_without_other_derived_resources() {
    let mut manifest = manifest();
    manifest.health = None;
    manifest.gateway.correlation_id = false;
    manifest.gateway.otel_endpoint = None;
    let max = EDGE_RESOURCE_ID_MAX_LENGTH - "-otel-tracing".len();
    manifest.gateway.proxy_id = Some("a".repeat(max));

    manifest.validate().unwrap();
    let resources = resources(&manifest).unwrap();
    assert_eq!(
        generated_ids(&resources),
        vec![format!("{}-otel-tracing", "a".repeat(max)).as_str()]
    );

    manifest.gateway.proxy_id = Some("a".repeat(EDGE_RESOURCE_ID_MAX_LENGTH));
    let error = manifest.validate().unwrap_err().to_string();
    assert!(error.contains("-otel-tracing"), "{error}");
}

/// Without `gateway.otel_endpoint`, export still attaches `otel_tracing`,
/// in Edge's propagation-only mode: it distrusts the caller's context and
/// generates none, so Edge's `before_proxy` removes every casing of the
/// caller's `traceparent`/`tracestate` and forwards no trace context (#307).
#[test]
fn export_without_an_otel_endpoint_never_relays_caller_trace_context() {
    let mut manifest = manifest();
    manifest.gateway.otel_endpoint = None;
    manifest.gateway.otel_root_sampling_ratio = None;
    let resources = resources(&manifest).unwrap();
    let otel = resources
        .plugin_configs
        .iter()
        .find(|plugin| plugin["plugin_name"] == "otel_tracing")
        .expect("otel_tracing is always attached");
    assert_eq!(
        otel["config"],
        json!({
            "service_name": "ferrum-edge-ferrum",
            "trace_context_trust": "untrusted",
            "generate_trace_id": false,
            "include_url_path": false,
        })
    );
    assert_eq!(otel["enabled"], true);
    assert!(
        resources.proxy["plugins"]
            .as_array()
            .unwrap()
            .contains(&json!({ "plugin_config_id": otel["id"] })),
        "the proxy references the plugin"
    );

    // With an endpoint, Edge distrusts the caller's context and starts its
    // own trace (the default `generate_trace_id: true`).
    let with_endpoint = resources_for_endpoint();
    assert_eq!(with_endpoint["trace_context_trust"], "untrusted");
    assert!(with_endpoint.get("generate_trace_id").is_none());
}

fn resources_for_endpoint() -> Value {
    let resources = resources(&manifest()).unwrap();
    resources
        .plugin_configs
        .into_iter()
        .find(|plugin| plugin["plugin_name"] == "otel_tracing")
        .unwrap()["config"]
        .clone()
}

/// Literal paths outside Alloy's canonical subset. The first four are
/// ones Ferrum Edge itself refuses as a `listen_path`: a `;` path
/// parameter (v0.9.9 without `allow_path_parameters`, which export
/// never sets), a `.` segment, a percent-escape, and a backslash. The
/// next four Edge's `policy_path` admits, but export only writes plain
/// ASCII `pchar` paths, so they are refused here instead. The last two
/// are `pchar` paths that the shared `service-manifest` v1 schema refuses
/// (`~` and `*`), so Alloy refuses them too
/// (ferrum-edge/ferrum-contracts#58).
const REFUSED_PATHS: &[&str] = &[
    "/orders;v=1",
    "/orders/./items",
    "/orders%2Fitems",
    "/orders\\items",
    "/orders items",
    "/orders|items",
    "/orders\"items",
    "/ordérs",
    "/items~draft",
    "/v1/*",
];

#[test]
fn paths_outside_the_canonical_subset_are_rejected() {
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
fn ordinary_pchar_paths_are_accepted() {
    let fixture = std::fs::read_to_string(fixtures().join("orders-api.toml")).unwrap();
    for path in ["/orders-v2/items.json", "/a:b@c"] {
        let text = fixture.replace(
            "public_path = \"/orders\"",
            &format!("public_path = {path:?}"),
        );
        assert_ne!(text, fixture);
        assert!(
            ServiceManifest::from_toml(&text).is_ok(),
            "{path} must be accepted"
        );
    }
}

/// `gateway` auth is accepted by the manifest schema, but export cannot
/// generate Edge authentication yet. Export refuses rather than produce an
/// unauthenticated proxy.
#[test]
fn gateway_auth_refuses_export() {
    let fixture = std::fs::read_to_string(fixtures().join("orders-api.toml")).unwrap();
    for mode in ["service", "none"] {
        let text = fixture.replace("mode = \"service\"", &format!("mode = {mode:?}"));
        let manifest = ServiceManifest::from_toml(&text).unwrap();
        assert!(resources(&manifest).is_ok(), "{mode}");
    }
    let text = fixture.replace("[auth]\nmode = \"service\"\n", "");
    assert_ne!(text, fixture);
    let manifest = ServiceManifest::from_toml(&text).unwrap();
    assert!(resources(&manifest).is_ok(), "auth.mode absent");

    let text = fixture.replace("mode = \"service\"", "mode = \"gateway\"");
    assert_ne!(text, fixture);
    let manifest = ServiceManifest::from_toml(&text).unwrap();
    let ExportError(reasons) = resources(&manifest).unwrap_err();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].contains("auth.mode is \"gateway\""),
        "{reasons:?}"
    );
    assert!(reasons[0].contains("unauthenticated"), "{reasons:?}");

    // A manifest that skipped validation still fails closed.
    let mut manifest = manifest;
    manifest.auth.mode = Some("Gateway".into());
    let ExportError(reasons) = resources(&manifest).unwrap_err();
    assert!(reasons[0].contains("\"Gateway\""), "{reasons:?}");
}

/// Keys YAML would read back as a bool, null, or number are quoted, in
/// any letter case; plain identifiers stay bare.
#[test]
fn yaml_keys_that_would_change_type_are_quoted() {
    let out = yaml::to_string(&json!({
        "True": 1,
        "NULL": 2,
        "Off": 3,
        "-123": 4,
        "-0xFF": 5,
        "-1e3": 6,
        "plain_key": 7,
    }));
    for expected in [
        "\"True\": 1",
        "\"NULL\": 2",
        "\"Off\": 3",
        "\"-123\": 4",
        "\"-0xFF\": 5",
        "\"-1e3\": 6",
        "plain_key: 7",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
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

/// A sequence nested in a sequence continues its parent's `- ` line, and its
/// later items line up under its first, as YAML requires.
#[test]
fn yaml_nested_sequences_are_valid_block_sequences() {
    for (value, expected) in [
        (json!([[1, 2]]), "- - 1\n  - 2\n"),
        (json!([[1, [2, 3]], 4]), "- - 1\n  - - 2\n    - 3\n- 4\n"),
        (
            json!([[{ "a": 1, "b": [true] }], []]),
            "- - a: 1\n    b:\n      - true\n- []\n",
        ),
        (
            json!({ "k": [["x"], { "z": [[null]] }] }),
            "k:\n  - - \"x\"\n  - z:\n      - - null\n",
        ),
    ] {
        assert_eq!(yaml::to_string(&value), expected, "{value}");
    }
}

/// `gateway.otel_endpoint` is held to Ferrum Edge main's OTLP endpoint
/// rules (`validate_endpoint_for_provider` without `allow_plaintext`): Edge
/// omits an `otel_tracing` plugin whose endpoint it refuses, and with it the
/// strip of caller trace context (#319).
#[test]
fn cleartext_otel_endpoints_are_limited_to_loopback_ip_literals() {
    let with_endpoint = |endpoint: &str| {
        let mut manifest = manifest();
        manifest.gateway.otel_endpoint = Some(endpoint.to_owned());
        manifest.validate().map_err(|error| error.to_string())
    };
    for accepted in [
        "https://otel-collector:4318/v1/traces",
        "https://otel.example.com/v1/traces",
        "https://10.0.0.5/v1/traces",
        "https://[2001:db8::1]:4318/v1/traces",
        "https://otel.example.com?contact=ops@example.com",
        "https://otel.example.com/v1/traces?tenant=a#frag",
        "HTTPS://otel.example.com/v1/traces",
        "http://127.0.0.1:4318/v1/traces",
        "http://127.0.0.1",
        "http://127.0.0.1#frag",
        "http://127.0.0.1?q=1",
        "http://127.8.9.10/v1/traces",
        "http://127.0.0.1:65535/v1/traces",
        "http://[::1]:4318/v1/traces",
        "http://[::1]",
        "HTTP://127.0.0.1:4318/v1/traces",
    ] {
        assert!(
            with_endpoint(accepted).is_ok(),
            "{accepted} was refused: {:?}",
            with_endpoint(accepted)
        );
    }
    for refused in [
        "http://otel-collector:4318/v1/traces",
        "http://localhost:4318/v1/traces",
        "http://10.0.0.5:4318/v1/traces",
        "http://[::2]:4318/v1/traces",
        "http://[0:0:0:0:0:0:0:1]:4318/v1/traces",
        "http://127.1:4318/v1/traces",
        "http://0x7f.0.0.1:4318/v1/traces",
        "http://2130706433:4318/v1/traces",
        "http://[::ffff:127.0.0.1]:4318/v1/traces",
    ] {
        let error = with_endpoint(refused).unwrap_err();
        assert!(error.contains("loopback IP literal"), "{refused}: {error}");
    }
    for refused in [
        "http://[::1/v1/traces",
        "http://[::1]garbage/v1/traces",
        "http://[::1]foo/v1/traces",
        "http://[::1]:notaport/v1/traces",
        "http://127.0.0.1:notaport/v1/traces",
        "http://127.0.0.1:99999/v1/traces",
        "http://127.0.0.1:0/v1/traces",
        "https://otel.example.com:0/v1/traces",
        "https://user:pass@otel.example.com/v1/traces",
        "https://@otel.example.com/v1/traces",
        "http://user@127.0.0.1:4318/v1/traces",
        "ftp://127.0.0.1/v1/traces",
        "otel.example.com/v1/traces",
        "https:///v1/traces",
        "https://",
        "http:127.0.0.1:4318",
    ] {
        let error = with_endpoint(refused).unwrap_err();
        assert!(
            error.contains("must be an http(s) URL with a host, a valid port, and no credentials"),
            "{refused}: {error}"
        );
    }
    for refused in [
        "http://127.0.0.1:4318/v1/tr\naces",
        "https://otel.example.com/v1/\ttraces",
        " https://otel.example.com/v1/traces",
        "https://otel.example.com/v1/traces\u{7f}",
        "https://otel.example.com/v1/traces\u{85}",
    ] {
        let error = with_endpoint(refused).unwrap_err();
        assert!(
            error.contains("must not contain control or whitespace characters"),
            "{refused:?}: {error}"
        );
    }
}

/// Edge's upper bounds and control-character rules (#309): every field at
/// its limit is accepted and exports exactly that value; one past it is
/// refused with a message naming the field, instead of export writing
/// configuration Edge refuses to load.
#[test]
fn manifests_are_held_to_edges_upper_bounds() {
    use ferrum_alloy_edge::manifest::{
        EDGE_MAX_BACKEND_HOST_LENGTH, EDGE_MAX_BACKEND_PATH_LENGTH,
        EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS, EDGE_MAX_LISTEN_PATH_LENGTH, EDGE_MAX_TIMEOUT_MS,
    };
    assert_eq!(EDGE_MAX_TIMEOUT_MS, 86_400_000);
    assert_eq!(EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS, 3_600);
    assert_eq!(EDGE_MAX_LISTEN_PATH_LENGTH, 500);
    assert_eq!(EDGE_MAX_BACKEND_PATH_LENGTH, 2_048);
    assert_eq!(EDGE_MAX_BACKEND_HOST_LENGTH, 255);
    let path_of = |len: usize| format!("/{}", "p".repeat(len - 1));

    let mut at_limit = manifest();
    at_limit.timeouts.connect_ms = EDGE_MAX_TIMEOUT_MS;
    at_limit.timeouts.read_ms = EDGE_MAX_TIMEOUT_MS;
    at_limit.timeouts.write_ms = EDGE_MAX_TIMEOUT_MS;
    let health = at_limit.health.as_mut().unwrap();
    health.interval_seconds = EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS;
    health.timeout_ms = EDGE_MAX_TIMEOUT_MS;
    health.path = path_of(EDGE_MAX_BACKEND_PATH_LENGTH);
    at_limit.api.public_path = path_of(EDGE_MAX_LISTEN_PATH_LENGTH);
    // Export writes it without the trailing `/` as `backend_path`, which Edge
    // bounds, so the manifest value may be one byte longer.
    at_limit.api.service_base_path = format!("{}/", path_of(EDGE_MAX_BACKEND_PATH_LENGTH));
    at_limit.upstream.host = "h".repeat(EDGE_MAX_BACKEND_HOST_LENGTH);
    at_limit.validate().unwrap();
    let exported = resources(&at_limit).unwrap();
    assert_eq!(
        exported.proxy["backend_read_timeout_ms"],
        EDGE_MAX_TIMEOUT_MS
    );
    assert_eq!(
        exported.proxy["listen_path"].as_str().unwrap().len(),
        EDGE_MAX_LISTEN_PATH_LENGTH
    );
    assert_eq!(
        exported.proxy["backend_path"].as_str().unwrap().len(),
        EDGE_MAX_BACKEND_PATH_LENGTH
    );
    let upstream = exported.upstream.unwrap();
    assert_eq!(
        upstream["health_checks"]["active"]["interval_seconds"],
        EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS
    );
    assert_eq!(
        upstream["targets"][0]["host"].as_str().unwrap().len(),
        EDGE_MAX_BACKEND_HOST_LENGTH
    );

    let refused = |change: &dyn Fn(&mut ServiceManifest), field: &str| {
        let mut manifest = manifest();
        change(&mut manifest);
        let error = manifest.validate().unwrap_err().to_string();
        assert!(error.contains(field), "{field}: {error}");
    };
    refused(
        &|m| m.timeouts.read_ms = 100_000_000,
        "timeouts.read_ms must not exceed",
    );
    refused(
        &|m| m.timeouts.connect_ms = EDGE_MAX_TIMEOUT_MS + 1,
        "timeouts.connect_ms must not exceed",
    );
    refused(
        &|m| m.timeouts.write_ms = EDGE_MAX_TIMEOUT_MS + 1,
        "timeouts.write_ms must not exceed",
    );
    refused(
        &|m| m.health.as_mut().unwrap().interval_seconds = 7_200,
        "health.interval_seconds must not exceed",
    );
    refused(
        &|m| m.health.as_mut().unwrap().timeout_ms = EDGE_MAX_TIMEOUT_MS + 1,
        "health.timeout_ms must not exceed",
    );
    refused(
        &|m| m.health.as_mut().unwrap().path = path_of(EDGE_MAX_BACKEND_PATH_LENGTH + 1),
        "health.path must not exceed",
    );
    refused(
        &|m| m.api.public_path = path_of(600),
        "api.public_path must not exceed 500 bytes",
    );
    refused(
        &|m| m.api.service_base_path = format!("{}/", path_of(EDGE_MAX_BACKEND_PATH_LENGTH + 1)),
        "api.service_base_path must not exceed",
    );
    refused(
        &|m| m.upstream.host = "h".repeat(EDGE_MAX_BACKEND_HOST_LENGTH + 1),
        "upstream.host must not exceed 255 bytes",
    );
    for host in [
        "orders\ninternal",
        "orders\tinternal",
        "orders\u{7f}",
        "orders\u{85}x",
    ] {
        refused(
            &|m| m.upstream.host = host.to_owned(),
            "upstream.host must not contain control or whitespace characters",
        );
    }
}
