//! AI-agent tool metadata (`x-ferrum-mcp`): stamping from the manifest's
//! `[agents]` section, default annotations, and the checks against Ferrum
//! Edge v0.9.9's admission rules and Alloy's agent-safety rules.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ferrum_alloy_edge::agents::{
    AgentToolError, MAX_TEXT_BYTES, MAX_TOOLS, X_FERRUM_MCP, default_annotations,
    fill_default_annotations, lint, prepare, stamp, warnings,
};
use ferrum_alloy_edge::manifest::ServiceManifest;
use serde_json::{Value, json};

const MANIFEST: &str = r#"
schema = "ferrum.service_manifest"
schema_version = "1.0"
[service]
name = "orders-api"
[api]
public_path = "/shop"
[upstream]
host = "orders.internal"
port = 8080
scheme = "http"
"#;

fn manifest_with(agents: &str) -> ServiceManifest {
    ServiceManifest::from_toml(&format!("{MANIFEST}{agents}")).unwrap()
}

fn operation(id: &str, extension: Value) -> Value {
    let mut operation = json!({
        "operationId": id,
        "summary": format!("The {id} operation."),
        "responses": { "200": { "description": "OK" } }
    });
    if !extension.is_null() {
        operation[X_FERRUM_MCP] = extension;
    }
    operation
}

/// Orders with a declared tool, an undeclared `GET`, and an undeclared
/// `DELETE`.
fn orders() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": { "title": "orders-api", "version": "1" },
        "paths": {
            "/orders": {
                "get": operation("list_orders", json!({ "expose": true })),
            },
            "/orders/{id}": {
                "get": operation("get_order", Value::Null),
                "delete": operation("cancel_order", Value::Null),
            },
        }
    })
}

fn assert_problem(problems: &[String], expected: &str) {
    assert!(
        problems.iter().any(|problem| problem.contains(expected)),
        "no problem contains {expected:?}: {problems:#?}"
    );
}

#[test]
fn default_annotations_follow_the_method_like_edge() {
    let hints = |method| Value::Object(default_annotations(method).unwrap());
    assert_eq!(hints("get"), json!({ "readOnlyHint": true }));
    assert_eq!(
        hints("delete"),
        json!({ "readOnlyHint": false, "destructiveHint": true })
    );
    assert_eq!(
        hints("put"),
        json!({ "readOnlyHint": false, "idempotentHint": true })
    );
    assert_eq!(hints("post"), json!({ "readOnlyHint": false }));
    assert_eq!(hints("patch"), json!({ "readOnlyHint": false }));
    for method in ["head", "options", "trace"] {
        assert!(default_annotations(method).is_none(), "{method}");
    }
}

#[test]
fn exposed_operations_get_default_annotations_and_keep_their_own() {
    let mut document = json!({
        "paths": {
            "/a": {
                "get": { X_FERRUM_MCP: true },
                "put": {
                    X_FERRUM_MCP: { "expose": true, "annotations": { "idempotentHint": false } }
                },
                "delete": { X_FERRUM_MCP: { "expose": false } },
                "post": { X_FERRUM_MCP: { "description": "not exposed" } },
                "head": { X_FERRUM_MCP: true },
            }
        }
    });
    fill_default_annotations(&mut document);
    let item = &document["paths"]["/a"];
    assert_eq!(
        item["get"][X_FERRUM_MCP],
        json!({ "expose": true, "annotations": { "readOnlyHint": true } })
    );
    assert_eq!(
        item["put"][X_FERRUM_MCP]["annotations"],
        json!({ "idempotentHint": false, "readOnlyHint": false })
    );
    // Only operations exposed explicitly are filled in.
    assert_eq!(item["delete"][X_FERRUM_MCP], json!({ "expose": false }));
    assert_eq!(
        item["post"][X_FERRUM_MCP],
        json!({ "description": "not exposed" })
    );
    assert_eq!(item["head"][X_FERRUM_MCP], json!(true));
}

#[test]
fn the_manifest_selects_only_declared_operations() {
    let enabled = manifest_with("[agents]\nenabled = true\n");
    let mut document = orders();
    stamp(&mut document, &enabled).unwrap();
    // The explicit include turns off Edge's default of publishing every GET,
    // so the undeclared `get_order` stays private.
    assert_eq!(
        document[X_FERRUM_MCP],
        json!({
            "enabled": true,
            "include": { "operations": ["list_orders"] },
            "namespace": "orders-api"
        })
    );
    assert!(lint(&document, Some("/shop")).is_empty());

    let mut document = orders();
    let agents =
        "[agents]\nenabled = true\nendpoint_path = \"/shop/agents\"\nnamespace = \"orders\"\n";
    stamp(&mut document, &manifest_with(agents)).unwrap();
    assert_eq!(document[X_FERRUM_MCP]["endpoint"]["path"], "/shop/agents");
    assert_eq!(document[X_FERRUM_MCP]["namespace"], "orders");

    let disabled = manifest_with("[agents]\nenabled = false\n");
    let mut document = orders();
    stamp(&mut document, &disabled).unwrap();
    assert_eq!(document[X_FERRUM_MCP], json!(false));

    // Without an [agents] section the document is left as the code wrote it.
    let mut document = orders();
    stamp(&mut document, &manifest_with("")).unwrap();
    assert_eq!(document, orders());
}

#[test]
fn stamping_refuses_a_second_source_and_an_empty_selection() {
    let enabled = manifest_with("[agents]\nenabled = true\n");
    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    let error = stamp(&mut document, &enabled).unwrap_err();
    assert!(error.0[0].contains("already sets"), "{error}");

    let mut document = orders();
    document["paths"]["/orders"]["get"][X_FERRUM_MCP] = json!({ "expose": false });
    let error = stamp(&mut document, &enabled).unwrap_err();
    assert!(error.0[0].contains("no operation"), "{error}");
}

#[test]
fn prepare_stamps_fills_and_reports_every_problem() {
    let mut document = orders();
    let enabled = manifest_with("[agents]\nenabled = true\n");
    assert!(prepare(&mut document, Some(&enabled)).unwrap().is_empty());
    assert_eq!(
        document["paths"]["/orders"]["get"][X_FERRUM_MCP],
        json!({ "expose": true, "annotations": { "readOnlyHint": true } })
    );

    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    let list = &mut document["paths"]["/orders"]["get"];
    let list = list.as_object_mut().unwrap();
    list.remove("summary");
    list.remove("operationId");
    let error: AgentToolError = prepare(&mut document, None).unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.starts_with("invalid AI-agent tool metadata (x-ferrum-mcp):\n  - "),
        "{rendered}"
    );
    assert_problem(
        &error.0,
        "paths./orders.get is exposed to agents but has no operationId",
    );
    assert_problem(
        &error.0,
        "paths./orders.get is exposed to agents but has no description",
    );

    // Without a document-level extension nothing is published, so only the
    // shapes are checked.
    document[X_FERRUM_MCP] = Value::Null;
    assert!(lint(&document, None).is_empty());
}

#[test]
fn edge_default_selection_publishes_every_get_and_is_checked() {
    // `x-ferrum-mcp: true` written by hand, as Edge reads it: every GET is
    // selected, and the DELETE is not.
    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    assert!(lint(&document, None).is_empty());
    let get = &mut document["paths"]["/orders/{id}"]["get"];
    get.as_object_mut().unwrap().remove("summary");
    assert_problem(
        &lint(&document, None),
        "paths./orders/{id}.get is exposed to agents but has no description",
    );
}

#[test]
fn state_changing_operations_need_an_explicit_opt_in() {
    let mut document = orders();
    document["paths"]["/orders/{id}"]["delete"]["tags"] = json!(["orders"]);
    document[X_FERRUM_MCP] = json!({ "include": { "tags": ["orders"] } });
    assert_problem(
        &lint(&document, None),
        "paths./orders/{id}.delete changes state and is selected only by tag",
    );

    // Naming the operation, or declaring it, is explicit.
    document[X_FERRUM_MCP] = json!({ "include": { "operations": ["cancel_order"] } });
    assert!(lint(&document, None).is_empty());
    document[X_FERRUM_MCP] = json!({ "include": { "tags": ["orders"] } });
    document["paths"]["/orders/{id}"]["delete"][X_FERRUM_MCP] = json!({ "expose": true });
    assert!(lint(&document, None).is_empty());
}

#[test]
fn head_options_and_trace_cannot_be_exposed() {
    let mut document = orders();
    let probe = operation("probe_orders", json!({ "expose": true }));
    document["paths"]["/orders"]["head"] = probe;
    document[X_FERRUM_MCP] = json!(true);
    assert_problem(
        &lint(&document, None),
        "paths./orders.head cannot be exposed to agents",
    );

    // Selected by a tag, too.
    let mut document = orders();
    let mut probe = operation("probe_orders", Value::Null);
    probe["tags"] = json!(["orders"]);
    document["paths"]["/orders"]["options"] = probe;
    document[X_FERRUM_MCP] = json!({ "include": { "tags": ["orders"] } });
    assert_problem(
        &lint(&document, None),
        "paths./orders.options cannot be exposed to agents",
    );
}

#[test]
fn annotations_may_only_state_more_risk() {
    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    let create = json!({ "expose": true, "annotations": { "readOnlyHint": true } });
    document["paths"]["/orders"]["post"] = operation("create_order", create);
    let cancel = json!({ "expose": true, "annotations": { "destructiveHint": false } });
    document["paths"]["/orders/{id}"]["delete"] = operation("cancel_order", cancel);
    let problems = lint(&document, None);
    assert_problem(
        &problems,
        "paths./orders.post changes state but claims `readOnlyHint: true`",
    );
    assert_problem(
        &problems,
        "paths./orders/{id}.delete is a DELETE but claims `destructiveHint: false`",
    );

    // Stating more risk than the default is always allowed.
    let get = json!({ "expose": true, "annotations": { "readOnlyHint": false } });
    document["paths"]["/orders"]["get"] = operation("list_orders", get);
    let create = json!({ "expose": true, "annotations": { "destructiveHint": true } });
    document["paths"]["/orders"]["post"] = operation("create_order", create);
    let cancel = json!({ "expose": true, "annotations": { "idempotentHint": true } });
    document["paths"]["/orders/{id}"]["delete"] = operation("cancel_order", cancel);
    assert!(lint(&document, None).is_empty());

    let title = "t".repeat(MAX_TEXT_BYTES + 1);
    let get = json!({ "expose": true, "annotations": { "title": title } });
    document["paths"]["/orders"]["get"] = operation("list_orders", get);
    assert_problem(&lint(&document, None), "is longer than 8192 bytes");
}

#[test]
fn a_selection_without_include_is_a_warning() {
    // Hand-written `x-ferrum-mcp: true` publishes every GET, including the
    // undeclared `get_order`: valid for Edge, so a warning, not a problem.
    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    assert!(lint(&document, None).is_empty());
    let found = warnings(&document);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("paths./orders/{id}.get"), "{found:?}");
    assert!(!found[0].contains("paths./orders.get"), "{found:?}");

    let mut stamped = orders();
    let enabled = manifest_with("[agents]\nenabled = true\n");
    assert!(prepare(&mut stamped, Some(&enabled)).unwrap().is_empty());
    document[X_FERRUM_MCP] = json!({ "include": { "tags": ["orders"] } });
    assert!(warnings(&document).is_empty());
    document[X_FERRUM_MCP] = json!(false);
    assert!(warnings(&document).is_empty());
}

#[test]
fn request_bodies_must_be_json() {
    let mut document = orders();
    document["components"] = json!({
        "requestBodies": {
            "Order": { "required": true, "content": { "application/vnd.order+json": {} } },
            "Upload": { "content": { "multipart/form-data": {} } }
        }
    });
    document["paths"]["/orders"]["post"] = operation("create_order", json!({ "expose": true }));
    let body = &mut document["paths"]["/orders"]["post"]["requestBody"];
    *body = json!({ "$ref": "#/components/requestBodies/Order" });
    document[X_FERRUM_MCP] = json!(true);
    assert!(lint(&document, None).is_empty(), "+json is JSON");

    let body = &mut document["paths"]["/orders"]["post"]["requestBody"];
    *body = json!({ "$ref": "#/components/requestBodies/Upload" });
    assert_problem(
        &lint(&document, None),
        "paths./orders.post takes a request body with no JSON media type",
    );
    let body = &mut document["paths"]["/orders"]["post"]["requestBody"];
    *body = json!({ "$ref": "#/components/requestBodies/Missing" });
    assert_problem(&lint(&document, None), "does not resolve");
}

#[test]
fn selection_is_bounded() {
    let mut paths = serde_json::Map::new();
    for index in 0..=MAX_TOOLS {
        let get = operation(&format!("r{index}"), Value::Null);
        paths.insert(format!("/r{index}"), json!({ "get": get }));
    }
    let mut document = json!({ "paths": paths, X_FERRUM_MCP: true });
    assert_problem(&lint(&document, None), "selects 257 operations");

    document[X_FERRUM_MCP] = json!({ "include": { "operations": ["none_of_these"] } });
    assert_problem(&lint(&document, None), "selects no operation");
}

#[test]
fn tool_names_are_valid_and_unique() {
    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    let get = &mut document["paths"]["/orders/{id}"]["get"];
    get[X_FERRUM_MCP] = json!({ "name": "list_orders" });
    let duplicate = "both produce the tool name \"list_orders\"";
    assert_problem(&lint(&document, None), duplicate);

    let get = &mut document["paths"]["/orders/{id}"]["get"];
    get[X_FERRUM_MCP] = json!({ "name": "get order" });
    assert_problem(&lint(&document, None), "name` must be 1-128 characters");

    // Edge's slug: a run of other characters becomes one `_`.
    let get = &mut document["paths"]["/orders/{id}"]["get"];
    get[X_FERRUM_MCP] = Value::Null;
    get["operationId"] = json!("list orders");
    document["paths"]["/orders"]["get"]["operationId"] = json!("list__orders");
    assert_problem(&lint(&document, None), duplicate);
}

#[test]
fn extensions_are_closed_like_edge() {
    let mut document = orders();
    document[X_FERRUM_MCP] = json!({
        "enabled": true,
        "namespace": "orders api",
        "include": { "operation": ["list_orders"] },
        "limits": { "max_request_body_bytes": 0, "max_body": 1 },
        "forward_request_headers": "x-api-version",
        "endpoints": {}
    });
    document["paths"]["/orders"]["get"][X_FERRUM_MCP] = json!({
        "expose": "yes",
        "readOnly": true,
        "annotations": { "readOnlyHint": "true", "dangerous": true }
    });
    let problems = lint(&document, None);
    for expected in [
        "`x-ferrum-mcp` has the unknown key \"endpoints\"",
        "`x-ferrum-mcp.namespace` must be",
        "`x-ferrum-mcp.include` has the unknown key \"operation\"",
        "`x-ferrum-mcp.limits.max_request_body_bytes` must be a positive integer",
        "`x-ferrum-mcp.limits` has the unknown key \"max_body\"",
        "`x-ferrum-mcp.forward_request_headers` must be an array",
        "`paths./orders.get.x-ferrum-mcp.expose` must be a boolean",
        "`paths./orders.get.x-ferrum-mcp` has the unknown key \"readOnly\"",
        "`paths./orders.get.x-ferrum-mcp.annotations.readOnlyHint` has the wrong type",
        "`paths./orders.get.x-ferrum-mcp.annotations` has the unknown key \"dangerous\"",
    ] {
        assert_problem(&problems, expected);
    }
    // `enabled: false` publishes nothing; operation shapes are still checked.
    document[X_FERRUM_MCP] = json!({ "enabled": false });
    let problems = lint(&document, None);
    assert_problem(&problems, "expose` must be a boolean");
    assert!(
        !problems.iter().any(|p| p.contains("namespace")),
        "{problems:?}"
    );
}

#[test]
fn the_endpoint_stays_under_the_public_path_and_clear_of_operations() {
    let mut document = orders();
    document[X_FERRUM_MCP] = json!({ "endpoint": { "path": "/elsewhere/mcp" } });
    assert_problem(
        &lint(&document, Some("/shop")),
        "is not under api.public_path \"/shop\"",
    );
    document[X_FERRUM_MCP] = json!({ "endpoint": { "path": "/shop/orders" } });
    assert_problem(
        &lint(&document, Some("/shop")),
        "paths./orders.get is served at \"/shop/orders\", inside the MCP endpoint",
    );
    // The default endpoint is the public path plus `/mcp`.
    document[X_FERRUM_MCP] = json!(true);
    document["paths"]["/mcp/tools"] = json!({ "get": operation("tools", Value::Null) });
    assert_problem(&lint(&document, Some("/shop/")), "inside the MCP endpoint");
}

#[test]
fn validation_and_tools_cannot_share_a_document() {
    let mut document = orders();
    document[X_FERRUM_MCP] = json!(true);
    document["x-ferrum-validate"] = json!(true);
    assert_problem(
        &lint(&document, None),
        "cannot be combined with `x-ferrum-validate`",
    );
}

#[test]
fn manifest_agents_section_is_validated() {
    for (agents, expected) in [
        (
            "endpoint_path = \"/other/mcp\"",
            "endpoint_path must be below",
        ),
        ("endpoint_path = \"/shop\"", "endpoint_path must be below"),
        (
            "endpoint_path = \"/shop/../mcp\"",
            "endpoint_path must be a literal",
        ),
        ("namespace = \"orders api\"", "agents.namespace"),
        ("namespace = \"\"", "agents.namespace"),
    ] {
        let text = format!("{MANIFEST}[agents]\nenabled = true\n{agents}\n");
        let error = ServiceManifest::from_toml(&text).unwrap_err().to_string();
        assert!(error.contains(expected), "{agents}: {error}");
    }
    let unknown = format!("{MANIFEST}[agents]\nenabled = true\nread_only = true\n");
    assert!(ServiceManifest::from_toml(&unknown).is_err());
    let agents = manifest_with("[agents]\n").agents.unwrap();
    assert!(!agents.enabled, "off unless enabled");
}
