//! Writes the Ferrum Edge file-mode configuration for the end-to-end stack.
//!
//! ```text
//! gen-e2e-edge-config <manifest.toml> <cli-export.yaml> <output.yaml>
//! ```
//!
//! The service's proxy, upstream, and plugins are exactly what
//! `ferrum-alloy edge export` generates: this program rebuilds them with the
//! same library call and stops unless its rendering is byte-identical to the
//! CLI's output. It then appends proxies that exist only for the attempt and
//! connection cases, all inside the compose network:
//!
//! * `e2e-retry` (`/retry`): retries a `503` from Alloy once (Edge `retry`
//!   block, default fixed 100 ms backoff);
//! * `e2e-reuse` (`/reuse`): reaches Alloy through the `alloy-reuse` network
//!   alias, a backend host no other proxy uses, so its first request needs a
//!   new connection and later requests can reuse or multiplex it;
//! * `e2e-refused` (`/refused`): a port on the Alloy container where nothing
//!   listens, so every connection attempt is refused.
//!
//! Every field is in the proxy schema of Ferrum Edge v0.9.9 and v0.9.8
//! (`src/config/types.rs` `Proxy` and `RetryConfig`, `docs/retry.md`), which
//! are identical for these fields.

use ferrum_alloy::edge::export;
use ferrum_alloy::edge::manifest::ServiceManifest;
use ferrum_alloy::edge::yaml;
use serde_json::{Map, Value, json};

/// A port on the Alloy container with no listener.
const REFUSED_PORT: u16 = 8444;

const USAGE: &str = "usage: gen-e2e-edge-config <manifest.toml> <cli-export.yaml> <output.yaml>";

type Failure = Box<dyn std::error::Error>;

/// One additional proxy.
struct Extra {
    id: &'static str,
    listen_path: &'static str,
    host: &'static str,
    port: u16,
    tls: bool,
    retry: Option<Value>,
}

fn main() -> Result<(), Failure> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [manifest_path, cli_path, output] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let manifest_text = std::fs::read_to_string(manifest_path)?;
    let manifest = ServiceManifest::from_toml(&manifest_text).map_err(|e| e.to_string())?;
    let resources = export::resources(&manifest);
    let base = export::file_mode_yaml(&resources);
    if std::fs::read_to_string(cli_path)? != base {
        let message = format!("{cli_path} differs from the library export of {manifest_path}");
        return Err(message.into());
    }

    let main_proxy = resources
        .proxy
        .as_object()
        .ok_or("the exported proxy is not a mapping")?;
    // Backend TLS settings live on the upstream when the manifest declares a
    // health check, and on the proxy otherwise.
    let tls: Map<String, Value> = resources
        .upstream
        .iter()
        .chain(std::iter::once(&resources.proxy))
        .filter_map(Value::as_object)
        .flat_map(|map| map.iter())
        .filter(|(key, _)| key.starts_with("backend_tls_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let extras = [
        Extra {
            id: "e2e-retry",
            listen_path: "/retry",
            host: "alloy",
            port: 8443,
            tls: true,
            retry: Some(json!({
                "max_retries": 1,
                "retryable_status_codes": [503],
                "retryable_methods": ["GET"],
                "retry_on_connect_failure": true,
            })),
        },
        Extra {
            id: "e2e-reuse",
            listen_path: "/reuse",
            host: "alloy-reuse",
            port: 8443,
            tls: true,
            retry: None,
        },
        Extra {
            id: "e2e-refused",
            listen_path: "/refused",
            host: "alloy",
            port: REFUSED_PORT,
            tls: false,
            retry: None,
        },
    ];

    let mut proxies = vec![resources.proxy.clone()];
    let mut plugin_configs = resources.plugin_configs.clone();
    for extra in &extras {
        let mut plugin_refs = Vec::new();
        for plugin in &resources.plugin_configs {
            let mut plugin = plugin.clone();
            let name = plugin
                .get("plugin_name")
                .and_then(Value::as_str)
                .unwrap_or("plugin")
                .replace('_', "-");
            let id = format!("{}-{name}", extra.id);
            if let Some(map) = plugin.as_object_mut() {
                map.insert("id".into(), json!(id));
                map.insert("proxy_id".into(), json!(extra.id));
            }
            plugin_refs.push(json!({ "plugin_config_id": id }));
            plugin_configs.push(plugin);
        }
        let mut proxy = Map::new();
        for key in [
            "namespace",
            "strip_listen_path",
            "backend_connect_timeout_ms",
            "backend_read_timeout_ms",
            "backend_write_timeout_ms",
        ] {
            if let Some(value) = main_proxy.get(key) {
                proxy.insert(key.into(), value.clone());
            }
        }
        proxy.insert("id".into(), json!(extra.id));
        proxy.insert("name".into(), json!(extra.id));
        proxy.insert("listen_path".into(), json!(extra.listen_path));
        proxy.insert("backend_host".into(), json!(extra.host));
        proxy.insert("backend_port".into(), json!(extra.port));
        if extra.tls {
            proxy.insert("backend_scheme".into(), json!("https"));
            proxy.extend(tls.clone());
        } else {
            proxy.insert("backend_scheme".into(), json!("http"));
        }
        if let Some(retry) = &extra.retry {
            proxy.insert("retry".into(), retry.clone());
        }
        proxy.insert("labels".into(), json!({ "generated-by": "edge-e2e" }));
        proxy.insert("plugins".into(), Value::Array(plugin_refs));
        proxies.push(Value::Object(proxy));
    }

    let mut document = export::file_mode_document(&resources);
    if let Some(map) = document.as_object_mut() {
        map.insert("proxies".into(), Value::Array(proxies));
        map.insert("plugin_configs".into(), Value::Array(plugin_configs));
    }
    let mut text: String = base
        .lines()
        .take_while(|line| line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str("# Extended by gen-e2e-edge-config with the proxies e2e-retry, e2e-reuse,\n");
    text.push_str("# and e2e-refused for the end-to-end attempt and connection cases.\n");
    text.push_str(&yaml::to_string(&document));
    std::fs::write(output, text)?;
    Ok(())
}
