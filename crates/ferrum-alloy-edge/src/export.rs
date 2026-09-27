//! Generates reviewable Ferrum Edge configuration from a service manifest.
//!
//! Output uses only fields that exist in the configuration schema of Ferrum Edge
//! v0.9.8 and v0.9.7 (`src/config/types.rs`: `Proxy`, `Upstream`, `PluginConfig`,
//! `GatewayConfig`, all `deny_unknown_fields`) and GitForgeOps's per-resource
//! `kind`/`spec` wrapper. Nothing is sent to a running gateway: artifacts are
//! written for review and applied through the operator's normal process.
//!
//! When the manifest declares a health check, the backend is expressed as an
//! `Upstream` with an active HTTP health check, and backend TLS settings are
//! placed on the upstream (Edge ignores a proxy's own `backend_tls_*` fields
//! when `upstream_id` is set).

use serde_json::{Map, Value, json};

use crate::manifest::ServiceManifest;
use crate::yaml;

/// Generated gateway resources.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeResources {
    /// The proxy.
    pub proxy: Value,
    /// The upstream, when a health check is declared.
    pub upstream: Option<Value>,
    /// Plugin configurations attached to the proxy.
    pub plugin_configs: Vec<Value>,
}

/// One file of a GitForgeOps resource tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceFile {
    /// Path relative to the repository root, e.g.
    /// `resources/ferrum/proxies/orders-api.yaml`.
    pub path: String,
    /// YAML content.
    pub content: String,
}

fn insert_some(map: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), Value::String(value.clone()));
    }
}

/// Builds Edge resources from a validated manifest.
pub fn resources(manifest: &ServiceManifest) -> EdgeResources {
    let proxy_id = manifest.proxy_id().to_owned();
    let namespace = manifest.gateway.namespace.clone();
    let upstream_id = format!("{proxy_id}-upstream");
    let tls = |map: &mut Map<String, Value>| {
        if manifest.upstream.scheme == "https" {
            map.insert("backend_tls_verify_server_cert".into(), Value::Bool(true));
        }
        insert_some(
            map,
            "backend_tls_client_cert_path",
            manifest.upstream.gateway_client_cert_path.as_ref(),
        );
        insert_some(
            map,
            "backend_tls_client_key_path",
            manifest.upstream.gateway_client_key_path.as_ref(),
        );
        insert_some(
            map,
            "backend_tls_server_ca_cert_path",
            manifest.upstream.server_ca_path.as_ref(),
        );
    };

    let mut plugin_configs = Vec::new();
    let mut plugin_refs = Vec::new();
    if manifest.gateway.correlation_id {
        let id = format!("{proxy_id}-correlation-id");
        plugin_configs.push(json!({
            "id": id,
            "plugin_name": "correlation_id",
            "namespace": namespace,
            "scope": "proxy",
            "proxy_id": proxy_id,
            "enabled": true,
            "config": { "header_name": "x-request-id", "echo_downstream": true },
        }));
        plugin_refs.push(json!({ "plugin_config_id": id }));
    }
    if let Some(endpoint) = &manifest.gateway.otel_endpoint {
        let id = format!("{proxy_id}-otel-tracing");
        let mut config = json!({
            "endpoint": endpoint,
            "service_name": format!("ferrum-edge-{namespace}"),
            // The gateway does not trust callers' trace context; it starts a
            // new trace and propagates its own SERVER span to the service.
            "trace_context_trust": "untrusted",
            "include_url_path": false,
        });
        if let Some(ratio) = manifest.gateway.otel_root_sampling_ratio
            && let Some(map) = config.as_object_mut()
        {
            map.insert("root_sampling".into(), json!("ratio"));
            map.insert("root_sampling_ratio".into(), json!(ratio));
        }
        plugin_configs.push(json!({
            "id": id,
            "plugin_name": "otel_tracing",
            "namespace": namespace,
            "scope": "proxy",
            "proxy_id": proxy_id,
            "enabled": true,
            "config": config,
        }));
        plugin_refs.push(json!({ "plugin_config_id": id }));
    }

    let mut proxy = Map::new();
    proxy.insert("id".into(), json!(proxy_id));
    proxy.insert("name".into(), json!(manifest.service.name));
    proxy.insert("namespace".into(), json!(namespace));
    proxy.insert("listen_path".into(), json!(manifest.api.public_path));
    proxy.insert("backend_scheme".into(), json!(manifest.upstream.scheme));
    proxy.insert(
        "strip_listen_path".into(),
        json!(manifest.api.strip_public_path),
    );
    if manifest.api.service_base_path != "/" {
        proxy.insert(
            "backend_path".into(),
            json!(manifest.api.service_base_path.trim_end_matches('/')),
        );
    }
    proxy.insert(
        "backend_connect_timeout_ms".into(),
        json!(manifest.timeouts.connect_ms),
    );
    proxy.insert(
        "backend_read_timeout_ms".into(),
        json!(manifest.timeouts.read_ms),
    );
    proxy.insert(
        "backend_write_timeout_ms".into(),
        json!(manifest.timeouts.write_ms),
    );
    proxy.insert("labels".into(), json!({ "generated-by": "ferrum-alloy" }));

    let upstream = match &manifest.health {
        Some(health) => {
            proxy.insert("upstream_id".into(), json!(upstream_id));
            let mut upstream = Map::new();
            upstream.insert("id".into(), json!(upstream_id));
            upstream.insert(
                "name".into(),
                json!(format!("{} service", manifest.service.name)),
            );
            upstream.insert("namespace".into(), json!(namespace));
            upstream.insert("algorithm".into(), json!("round_robin"));
            upstream.insert(
                "targets".into(),
                json!([{ "host": manifest.upstream.host, "port": manifest.upstream.port, "weight": 1 }]),
            );
            upstream.insert(
                "health_checks".into(),
                json!({
                    "active": {
                        "probe_type": "http",
                        "http_path": health.path,
                        "interval_seconds": health.interval_seconds,
                        "timeout_ms": health.timeout_ms,
                        "healthy_status_codes": [200],
                        "use_tls": manifest.upstream.scheme == "https",
                    }
                }),
            );
            tls(&mut upstream);
            upstream.insert("labels".into(), json!({ "generated-by": "ferrum-alloy" }));
            Some(Value::Object(upstream))
        }
        None => {
            proxy.insert("backend_host".into(), json!(manifest.upstream.host));
            proxy.insert("backend_port".into(), json!(manifest.upstream.port));
            tls(&mut proxy);
            None
        }
    };
    if !plugin_refs.is_empty() {
        proxy.insert("plugins".into(), Value::Array(plugin_refs));
    }
    EdgeResources {
        proxy: Value::Object(proxy),
        upstream,
        plugin_configs,
    }
}

/// A complete Edge file-mode configuration document (`FERRUM_MODE=file`).
pub fn file_mode_document(resources: &EdgeResources) -> Value {
    json!({
        "version": "1",
        "proxies": [resources.proxy],
        "upstreams": resources.upstream.iter().collect::<Vec<_>>(),
        "consumers": [],
        "plugin_configs": resources.plugin_configs,
    })
}

/// Edge file-mode configuration as YAML.
pub fn file_mode_yaml(resources: &EdgeResources) -> String {
    let namespace = resources
        .proxy
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("ferrum");
    let header = format!(
        "# Generated by ferrum-alloy edge export for Ferrum Edge {} ({}).\n# Review before applying; nothing was sent to a gateway.\n# Resources are in namespace {namespace:?}: run Edge with FERRUM_NAMESPACE={namespace}.\n",
        crate::contract::EDGE_RELEASE,
        &crate::contract::EDGE_SOURCE_COMMIT[..12]
    );
    header + &yaml::to_string(&file_mode_document(resources))
}

/// GitForgeOps `resources/<namespace>/...` files.
pub fn gitforgeops_files(resources: &EdgeResources, namespace: &str) -> Vec<ResourceFile> {
    let id = |value: &Value| {
        value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resource")
            .to_owned()
    };
    let mut files = vec![ResourceFile {
        path: format!(
            "resources/{namespace}/proxies/{}.yaml",
            id(&resources.proxy)
        ),
        content: yaml::to_string(&json!({ "kind": "Proxy", "spec": resources.proxy })),
    }];
    if let Some(upstream) = &resources.upstream {
        files.push(ResourceFile {
            path: format!("resources/{namespace}/upstreams/{}.yaml", id(upstream)),
            content: yaml::to_string(&json!({ "kind": "Upstream", "spec": upstream })),
        });
    }
    for plugin in &resources.plugin_configs {
        files.push(ResourceFile {
            path: format!("resources/{namespace}/plugins/{}.yaml", id(plugin)),
            content: yaml::to_string(&json!({ "kind": "PluginConfig", "spec": plugin })),
        });
    }
    files
}
