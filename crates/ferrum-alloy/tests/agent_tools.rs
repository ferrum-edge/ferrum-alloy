//! AI-agent tool metadata (feature `openapi`): `AgentTool` declared in
//! `#[utoipa::path]` becomes the operation's `x-ferrum-mcp` extension with
//! Ferrum Edge v0.9.9's keys, and nothing is exposed without a declaration.

#![cfg(feature = "openapi")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ferrum_alloy::agents::{AgentTool, X_FERRUM_MCP};
use serde_json::{Value, json};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// List orders.
#[utoipa::path(
    get,
    path = "/orders",
    responses((status = 200, description = "Orders")),
    extensions(("x-ferrum-mcp" = json!(AgentTool::expose()
        .description("List the caller's orders, newest first"))))
)]
async fn list_orders() -> &'static str {
    "[]"
}

/// Cancel an order.
#[utoipa::path(
    delete,
    path = "/orders/{id}",
    params(("id" = u64, Path, description = "Order identifier")),
    responses((status = 204, description = "Cancelled")),
    extensions(("x-ferrum-mcp" = json!(AgentTool::expose()
        .description("Cancel one of the caller's open orders")
        .idempotent(true))))
)]
async fn cancel_order() {}

/// Reconcile the order ledger.
#[utoipa::path(
    post,
    path = "/orders/reconcile",
    responses((status = 202, description = "Accepted")),
    extensions(("x-ferrum-mcp" = json!(AgentTool::hide())))
)]
async fn reconcile() {}

/// Order queue state.
#[utoipa::path(
    get,
    path = "/orders/queue",
    responses((status = 200, description = "Queue state"))
)]
async fn queue() -> &'static str {
    "ok"
}

fn document() -> Value {
    let (_, document) = OpenApiRouter::<()>::new()
        .routes(routes!(list_orders))
        .routes(routes!(cancel_order))
        .routes(routes!(reconcile))
        .routes(routes!(queue))
        .split_for_parts();
    serde_json::to_value(document).unwrap()
}

#[test]
fn declared_tools_become_operation_extensions() {
    let document = document();
    let paths = &document["paths"];
    assert_eq!(
        paths["/orders"]["get"][X_FERRUM_MCP],
        json!({ "expose": true, "description": "List the caller's orders, newest first" })
    );
    assert_eq!(
        paths["/orders/{id}"]["delete"][X_FERRUM_MCP],
        json!({
            "expose": true,
            "description": "Cancel one of the caller's open orders",
            "annotations": { "idempotentHint": true }
        })
    );
    assert_eq!(
        paths["/orders/reconcile"]["post"][X_FERRUM_MCP],
        json!({ "expose": false })
    );
    // No declaration, no extension: nothing is exposed by default.
    assert!(paths["/orders/queue"]["get"].get(X_FERRUM_MCP).is_none());
    // The document is otherwise what utoipa produces: the tool name defaults
    // to the operationId, the handler's name.
    assert_eq!(paths["/orders"]["get"]["operationId"], "list_orders");
}

#[test]
fn every_field_serializes_to_the_edge_keys() {
    let tool = AgentTool::expose()
        .name("orders.search")
        .title("Search orders")
        .description("Search the caller's orders by item")
        .read_only(true)
        .destructive(false)
        .idempotent(true)
        .open_world(false);
    let expected = json!({
        "expose": true,
        "name": "orders.search",
        "title": "Search orders",
        "description": "Search the caller's orders by item",
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    });
    assert_eq!(tool.to_value(), expected);
    assert_eq!(Value::from(tool.clone()), expected);
    assert_eq!(tool.extensions().get(X_FERRUM_MCP), Some(&expected));
    assert_eq!(AgentTool::hide().to_value(), json!({ "expose": false }));
}
