//! AI-agent tool metadata for documented operations (feature `openapi`).
//!
//! [`AgentTool`] describes how one operation is offered to AI agents. It
//! becomes the operation's `x-ferrum-mcp` OpenAPI extension, which Ferrum
//! Edge v0.9.9 reads on `POST /api-specs` to publish the operation as an MCP
//! tool. Declare it next to the route:
//!
//! ```ignore
//! use ferrum_alloy::agents::AgentTool;
//!
//! /// Fetch an order.
//! #[utoipa::path(
//!     get,
//!     path = "/orders/{id}",
//!     params(("id" = u64, Path, description = "Order identifier")),
//!     responses((status = 200, description = "The order", body = Order)),
//!     extensions(("x-ferrum-mcp" = json!(AgentTool::expose()
//!         .description("Fetch one of the caller's orders by its identifier"))))
//! )]
//! async fn get_order(Path(id): Path<u64>) -> Json<Order> { ... }
//! ```
//!
//! `json!(...)` there is utoipa's attribute syntax: it takes any expression
//! that serializes to the extension's value. Code that builds operations
//! without the macro can use [`AgentTool::extensions`].
//!
//! Nothing is exposed by default, and a state-changing operation is exposed
//! only by an explicit [`AgentTool::expose`] on it. `ferrum-alloy openapi
//! export` adds the method's default MCP annotations (`GET`: read-only,
//! `DELETE`: destructive, `PUT`: idempotent) where the tool sets none, stamps
//! the document-level extension from the service manifest's `[agents]`
//! section, and refuses metadata Edge would reject or an agent could not use.
//! See `docs/agent-tools.md` for the safety guidance.

use serde::Serialize;
use serde_json::Value;
use utoipa::openapi::extensions::{Extensions, ExtensionsBuilder};

/// The OpenAPI extension key Ferrum Edge reads, at operation and document
/// level.
pub const X_FERRUM_MCP: &str = "x-ferrum-mcp";

/// How one operation is offered to AI agents: the operation's `x-ferrum-mcp`
/// extension (Edge v0.9.9 keys `expose`, `name`, `title`, `description`, and
/// `annotations`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[must_use]
pub struct AgentTool {
    expose: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "ToolAnnotations::is_empty")]
    annotations: ToolAnnotations,
}

/// MCP tool annotations. Hints only: an agent may rely on them to decide
/// whether to ask its user before calling a tool, so they must not understate
/// what the operation does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    read_only_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destructive_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotent_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl AgentTool {
    /// Publishes the operation to agents (`expose: true`) once the service
    /// manifest's `[agents]` section is enabled. This is the explicit opt-in
    /// a `POST`, `PUT`, `PATCH`, or `DELETE` operation needs.
    pub fn expose() -> Self {
        Self::with_expose(true)
    }

    /// Never publishes the operation to agents (`expose: false`), whatever
    /// the document-level selection says.
    pub fn hide() -> Self {
        Self::with_expose(false)
    }

    fn with_expose(expose: bool) -> Self {
        Self {
            expose,
            name: None,
            title: None,
            description: None,
            annotations: ToolAnnotations::default(),
        }
    }

    /// The tool name (1-128 characters of `A-Za-z0-9_.-`). Defaults to the
    /// `operationId`, which utoipa sets to the handler's function name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// A human-readable title. Defaults to the operation summary.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// What the tool does, written for an agent choosing between tools.
    /// Defaults to the operation description, then its summary.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// `readOnlyHint`: the tool does not change state. Defaults to `true`
    /// for `GET` and `false` otherwise.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.annotations.read_only_hint = Some(read_only);
        self
    }

    /// `destructiveHint`: a state change may delete or overwrite data.
    /// Defaults to `true` for `DELETE`; MCP clients assume `true` for any
    /// other tool that is not read-only and does not say otherwise.
    pub fn destructive(mut self, destructive: bool) -> Self {
        self.annotations.destructive_hint = Some(destructive);
        self
    }

    /// `idempotentHint`: repeating a call with the same arguments has no
    /// further effect. Defaults to `true` for `PUT`.
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.annotations.idempotent_hint = Some(idempotent);
        self
    }

    /// `openWorldHint`: the tool reaches beyond this service (for example,
    /// a third-party API). MCP clients assume `true` when unset.
    pub fn open_world(mut self, open_world: bool) -> Self {
        self.annotations.open_world_hint = Some(open_world);
        self
    }

    /// The extension's JSON value.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The operation extensions carrying this tool, for operations built
    /// without `#[utoipa::path]`.
    pub fn extensions(&self) -> Extensions {
        ExtensionsBuilder::new()
            .add(X_FERRUM_MCP, self.to_value())
            .build()
    }
}

impl From<AgentTool> for Value {
    fn from(tool: AgentTool) -> Self {
        tool.to_value()
    }
}
