//! AI-agent tool metadata: the `x-ferrum-mcp` OpenAPI extension.
//!
//! Ferrum Edge v0.9.9 reads `x-ferrum-mcp` on `POST /api-specs` and publishes
//! the selected operations as MCP tools through a generated `mcp_gateway`
//! (Edge `docs/api_specs.md`, "`x-ferrum-mcp` (optional)";
//! `src/admin/api_specs/extractor.rs`, `parse_x_ferrum_mcp_extension` and
//! `extract_mcp_bridge_operations`; `src/plugins/mcp_openapi_bridge.rs`).
//! v0.9.8 ignores the extension.
//!
//! `ferrum-alloy openapi export` uses this module to:
//!
//! 1. stamp the document-level extension from the manifest's `[agents]`
//!    section ([`stamp`]), selecting only the operations whose handlers
//!    declare `expose: true`, so a `GET` without a declaration is never
//!    published by Edge's default;
//! 2. write the method's default MCP annotations into each exposed operation
//!    ([`fill_default_annotations`]), so the reviewed document shows the hints
//!    an agent will see;
//! 3. check the result against Edge's admission rules and Alloy's agent-safety
//!    rules ([`lint`]), and point out a hand-written selection that publishes
//!    every `GET` operation ([`warnings`]).
//!
//! Nothing here contacts a gateway.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::manifest::ServiceManifest;

/// The extension key, at document and operation level.
pub const X_FERRUM_MCP: &str = "x-ferrum-mcp";
/// Most operations one document may publish as tools (Edge
/// `MAX_BRIDGE_OPERATIONS`).
pub const MAX_TOOLS: usize = 256;
/// Most bytes in a tool namespace (Edge `MAX_MCP_BRIDGE_NAMESPACE_BYTES`).
pub const MAX_NAMESPACE_BYTES: usize = 64;
/// Most bytes in a tool name (Edge `MAX_MCP_BRIDGE_TOOL_NAME_BYTES`).
pub const MAX_TOOL_NAME_BYTES: usize = 128;

/// Document-level keys (Edge `X_FERRUM_MCP_KEYS`, a closed set).
const DOCUMENT_KEYS: &[&str] = &[
    "enabled",
    "endpoint",
    "exclude",
    "forward_request_headers",
    "include",
    "limits",
    "namespace",
];
const ENDPOINT_KEYS: &[&str] = &["path"];
const SELECTOR_KEYS: &[&str] = &["operations", "tags"];
const LIMIT_KEYS: &[&str] = &[
    "max_error_excerpt_bytes",
    "max_request_body_bytes",
    "max_response_body_bytes",
    "max_structured_content_bytes",
];
/// Per-operation keys (Edge `X_FERRUM_MCP_OPERATION_KEYS`).
const OPERATION_KEYS: &[&str] = &["annotations", "description", "expose", "name", "title"];
/// MCP tool annotation keys the bridge accepts (Edge `ANNOTATION_KEYS`).
const ANNOTATION_KEYS: &[&str] = &[
    "destructiveHint",
    "idempotentHint",
    "openWorldHint",
    "readOnlyHint",
    "title",
];
/// OpenAPI operation keys of a Path Item.
const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];
/// Methods whose responses a tool result cannot carry; Edge never bridges them.
const UNBRIDGED_METHODS: &[&str] = &["head", "options", "trace"];
/// Methods that change state; a tool for one needs an explicit opt-in.
const MUTATING_METHODS: &[&str] = &["post", "put", "patch", "delete"];
/// Most bytes in an annotation title (Edge `MAX_BRIDGE_TEXT_BYTES`).
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
/// `$ref` hops followed before giving up.
const MAX_REF_HOPS: usize = 16;
/// Most characters of a `$ref` value echoed in a problem.
const MAX_REF_EXCERPT_CHARS: usize = 80;
/// Most characters of a path, key, or endpoint echoed in a problem.
const MAX_TEXT_EXCERPT_CHARS: usize = 120;
/// Most problems [`lint`] lists; the rest are counted in a final line.
pub const MAX_PROBLEMS: usize = 100;
/// Most paths one warning names.
const MAX_WARNING_PATHS: usize = 20;

/// Problems found so far, held to [`MAX_PROBLEMS`] so that hostile input
/// cannot make the report grow with it: every message is bounded (input it
/// echoes is shortened) and only the first [`MAX_PROBLEMS`] are kept.
#[derive(Default)]
struct Problems {
    list: Vec<String>,
    suppressed: usize,
}

impl Problems {
    fn push(&mut self, problem: String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem);
        } else {
            self.suppressed += 1;
        }
    }

    /// Like [`Problems::push`], formatting the problem only when it is kept.
    fn push_with(&mut self, problem: impl FnOnce() -> String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem());
        } else {
            self.suppressed += 1;
        }
    }

    /// Keeps a document-wide problem even past the limit; called a fixed
    /// number of times per document.
    fn push_summary(&mut self, problem: String) {
        self.list.push(problem);
    }

    fn extend(&mut self, problems: impl IntoIterator<Item = String>) {
        for problem in problems {
            self.push(problem);
        }
    }

    fn into_list(mut self) -> Vec<String> {
        if self.suppressed > 0 {
            self.list
                .push(format!("{} further problems not listed", self.suppressed));
        }
        self.list
    }
}

/// `text` for a problem message, cut at [`MAX_TEXT_EXCERPT_CHARS`]; never
/// walks more of it than it keeps.
fn excerpt_text(text: &str) -> std::borrow::Cow<'_, str> {
    match text.char_indices().nth(MAX_TEXT_EXCERPT_CHARS) {
        None => std::borrow::Cow::Borrowed(text),
        Some((end, _)) => std::borrow::Cow::Owned(format!("{}…", &text[..end])),
    }
}

/// An operation's location in problems: `paths.<path>.<method>`, with a
/// long path shortened.
fn location(path: &str, method: &str) -> String {
    format!("paths.{}.{method}", excerpt_text(path))
}

/// Invalid or unsafe AI-agent tool metadata. Every problem is listed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid AI-agent tool metadata (x-ferrum-mcp):\n  - {}", .0.join("\n  - "))]
pub struct AgentToolError(pub Vec<String>);

/// The MCP tool annotations Edge derives from an operation's method
/// (`BridgeMethod::default_annotations`), keyed by the lower-case OpenAPI
/// method. `None` for a method the bridge does not carry.
pub fn default_annotations(method: &str) -> Option<Map<String, Value>> {
    let hints = match method {
        "get" => json!({ "readOnlyHint": true }),
        "delete" => json!({ "readOnlyHint": false, "destructiveHint": true }),
        "put" => json!({ "readOnlyHint": false, "idempotentHint": true }),
        "post" | "patch" => json!({ "readOnlyHint": false }),
        _ => return None,
    };
    match hints {
        Value::Object(hints) => Some(hints),
        _ => None,
    }
}

/// Stamps, fills, and checks an exported document, and returns its
/// [`warnings`]. See the module documentation. `manifest` is the service
/// manifest given to the export, if any.
pub fn prepare(
    document: &mut Value,
    manifest: Option<&ServiceManifest>,
) -> Result<Vec<String>, AgentToolError> {
    if let Some(manifest) = manifest {
        stamp(document, manifest)?;
    }
    fill_default_annotations(document);
    let problems = lint(document, manifest.map(|m| m.api.public_path.as_str()));
    if problems.is_empty() {
        Ok(warnings(document))
    } else {
        Err(AgentToolError(problems))
    }
}

/// Selections that are valid but publish more than any handler declared: an
/// enabled document-level `x-ferrum-mcp` without `include` publishes every
/// `GET` operation that does not set `expose: false`. `openapi export` never
/// writes one from `[agents]`; this catches a hand-written extension.
pub fn warnings(document: &Value) -> Vec<String> {
    let mut ignored = Problems::default();
    let Some(extension) = document_extension(document.get(X_FERRUM_MCP), &mut ignored) else {
        return Vec::new();
    };
    if !extension.include_operations.is_empty() || !extension.include_tags.is_empty() {
        return Vec::new();
    }
    let mut undeclared: Vec<String> = Vec::new();
    let mut unlisted = 0usize;
    for (path, method, operation) in operations(&mut Resolver::new(document)) {
        if method == "get" && explicit_expose(operation).is_none() {
            if undeclared.len() < MAX_WARNING_PATHS {
                undeclared.push(location(path, method));
            } else {
                unlisted += 1;
            }
        }
    }
    if undeclared.is_empty() {
        return Vec::new();
    }
    if unlisted > 0 {
        undeclared.push(format!("{unlisted} more"));
    }
    vec![format!(
        "`{X_FERRUM_MCP}` has no `include`, so Edge publishes every GET operation, including {} that no handler declared; declare tools with `AgentTool::expose()` and use the manifest's [agents] section to publish only those",
        undeclared.join(", ")
    )]
}

/// Writes the document-level `x-ferrum-mcp` from the manifest's `[agents]`
/// section. Without that section the document is left unchanged.
///
/// With `enabled = true`, the extension names the manifest's namespace (the
/// service name by default), its endpoint path when set, and an `include`
/// listing exactly the operations whose `x-ferrum-mcp` sets `expose: true`.
/// The explicit `include` replaces Edge's default of publishing every `GET`
/// operation, so only declared handlers become tools. With
/// `enabled = false`, the extension is `false`.
pub fn stamp(document: &mut Value, manifest: &ServiceManifest) -> Result<(), AgentToolError> {
    let Some(agents) = &manifest.agents else {
        return Ok(());
    };
    if document.get(X_FERRUM_MCP).is_some() {
        return Err(AgentToolError(vec![format!(
            "the document already sets a document-level `{X_FERRUM_MCP}`; with an [agents] section in the manifest, declare it there only"
        )]));
    }
    let extension = if agents.enabled {
        // Grants below are keyed by operationId alone, so a second
        // operation reusing an exposed id would inherit its grant (#216).
        let mut problems = Problems::default();
        duplicate_operation_id_problems(&operations(&mut Resolver::new(document)), &mut problems);
        let problems = problems.into_list();
        if !problems.is_empty() {
            return Err(AgentToolError(problems));
        }
        // Path Items shared by several paths are visited once, so the work
        // does not grow with the number of aliases.
        let mut seen = HashSet::new();
        let exposed: BTreeSet<String> = operations(&mut Resolver::new(document))
            .into_iter()
            .filter(|(_, _, operation)| seen.insert(std::ptr::from_ref(*operation)))
            .filter(|(_, _, operation)| explicit_expose(operation) == Some(true))
            .filter_map(|(_, _, operation)| operation_id(operation).map(str::to_owned))
            .collect();
        if exposed.is_empty() {
            return Err(AgentToolError(vec![format!(
                "[agents] is enabled, but no operation with an operationId sets `{X_FERRUM_MCP}` `expose: true` (declare one with `AgentTool::expose()`)"
            )]));
        }
        let namespace = agents
            .namespace
            .clone()
            .unwrap_or_else(|| manifest.service.name.clone());
        let mut extension = Map::new();
        extension.insert("enabled".to_owned(), Value::Bool(true));
        if let Some(path) = &agents.endpoint_path {
            extension.insert("endpoint".to_owned(), json!({ "path": path }));
        }
        extension.insert("include".to_owned(), json!({ "operations": exposed }));
        extension.insert("namespace".to_owned(), Value::String(namespace));
        Value::Object(extension)
    } else {
        Value::Bool(false)
    };
    if let Some(root) = document.as_object_mut() {
        root.insert(X_FERRUM_MCP.to_owned(), extension);
    }
    Ok(())
}

/// Adds the method's default annotations ([`default_annotations`]) to every
/// operation whose `x-ferrum-mcp` sets `expose: true`, keeping any hint the
/// operation already sets. `x-ferrum-mcp: true` becomes the equivalent
/// `{"expose": true, "annotations": {...}}`. Malformed values are left for
/// [`lint`] to report.
pub fn fill_default_annotations(document: &mut Value) {
    // Locally referenced Path Items hold their operations outside `paths`;
    // collect their locations first (immutable borrow), then fill inline
    // items and every referenced target. A shared target is filled once;
    // filling is idempotent and never replaces an explicit hint. Unresolvable
    // references are left for [`lint`] to report.
    let targets = referenced_path_items(document);
    if let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) {
        for item in paths.values_mut() {
            if let Some(item) = item.as_object_mut() {
                fill_item_annotations(item);
            }
        }
    }
    for pointer in targets {
        if let Some(item) = document
            .pointer_mut(&pointer)
            .and_then(Value::as_object_mut)
        {
            fill_item_annotations(item);
        }
    }
}

/// One JSON Pointer for each distinct Path Item the local `$ref` chains of
/// `paths` end at. Targets are told apart by the address of the value they
/// resolve to, so pointers spelled differently for one target (JSON Pointer
/// escapes allow several spellings) still yield it once, and no string is
/// hashed or copied per alias. Inline items and references that cannot be
/// followed are skipped.
fn referenced_path_items(document: &Value) -> Vec<String> {
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut resolver = Resolver::new(document);
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for item in paths.values() {
        if let Ok(Resolved {
            value,
            pointer: Some(pointer),
            ..
        }) = resolver.resolve(item)
            && seen.insert(std::ptr::from_ref(value))
        {
            targets.push(pointer.to_owned());
        }
    }
    targets
}

fn fill_item_annotations(item: &mut Map<String, Value>) {
    for (method, operation) in item.iter_mut() {
        let Some(defaults) = default_annotations(method) else {
            continue;
        };
        let Some(extension) = operation
            .as_object_mut()
            .and_then(|operation| operation.get_mut(X_FERRUM_MCP))
        else {
            continue;
        };
        if *extension == Value::Bool(true) {
            *extension = json!({ "expose": true, "annotations": defaults });
            continue;
        }
        let Value::Object(object) = extension else {
            continue;
        };
        if object.get("expose") != Some(&Value::Bool(true)) {
            continue;
        }
        let annotations = object
            .entry("annotations")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(annotations) = annotations {
            for (hint, value) in defaults {
                annotations.entry(hint).or_insert(value);
            }
        }
    }
}

/// Checks a document's `x-ferrum-mcp` metadata and returns every problem.
///
/// The shape of every `x-ferrum-mcp` value is checked against Edge's closed
/// schema. When the document-level extension is enabled, Edge's selection is
/// simulated, and each selected operation must have an `operationId` and a
/// description, must not be `HEAD`, `OPTIONS`, or `TRACE`, must take a JSON
/// request body if it takes one, must not set hints that state less risk than
/// its method (`readOnlyHint: true` on a state-changing operation,
/// `destructiveHint: false` on a `DELETE`), and, when it changes state, must
/// be selected explicitly (`expose: true` or by `operationId` in `include`,
/// never by tag alone). At least one and at most [`MAX_TOOLS`] operations must be selected,
/// tool names must be unique, and the extension cannot be combined with
/// `x-ferrum-validate`.
///
/// With `public_path` (the manifest's `api.public_path`, which a publisher
/// uses as the proxy's `listen_path`), the MCP endpoint must sit under it and
/// must not overlap a selected operation's path.
///
/// An operation reached through several paths (Path Item `$ref` aliases) is
/// parsed and checked once, and its problems name the first such path; only
/// the per-path checks (the selection count, tool-name uniqueness, and the
/// endpoint overlap) run for every path. The work therefore grows with the
/// document, not with the number of aliases times the size of what they
/// share.
pub fn lint(document: &Value, public_path: Option<&str>) -> Vec<String> {
    let mut problems = Problems::default();
    let extension = document_extension(document.get(X_FERRUM_MCP), &mut problems);
    let mut resolver = Resolver::new(document);
    let found = operations(&mut resolver);
    let mut parsed: HashMap<*const Map<String, Value>, OperationExtension<'_>> = HashMap::new();
    for &(path, method, operation) in &found {
        parsed
            .entry(std::ptr::from_ref(operation))
            .or_insert_with(|| {
                operation_extension(operation, &location(path, method), &mut problems)
            });
    }
    let Some(extension) = extension else {
        return problems.into_list();
    };
    duplicate_operation_id_problems(&found, &mut problems);
    path_item_problems(&mut resolver, &mut problems);
    if document
        .get("x-ferrum-validate")
        .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)))
    {
        problems.push(format!(
            "`{X_FERRUM_MCP}` cannot be combined with `x-ferrum-validate` in one document (Edge v0.9.9 refuses it)"
        ));
    }

    let mut selected = 0usize;
    let mut tool_names: BTreeMap<String, String> = BTreeMap::new();
    let mut public_paths = Vec::new();
    // Per operation: `None` when Edge would not publish it, otherwise its
    // valid tool name, if it has one.
    let mut tools: HashMap<*const Map<String, Value>, Option<Option<String>>> = HashMap::new();
    for &(path, method, operation) in &found {
        let location = location(path, method);
        let key = std::ptr::from_ref(operation);
        let tool = match tools.get(&key) {
            Some(tool) => tool.clone(),
            None => {
                let tool = parsed.get(&key).and_then(|operation_extension| {
                    check_selected(
                        &extension,
                        &mut resolver,
                        method,
                        operation,
                        operation_extension,
                        &location,
                        &mut problems,
                    )
                });
                tools.insert(key, tool.clone());
                tool
            }
        };
        let Some(name) = tool else {
            continue;
        };
        selected += 1;
        if let Some(name) = name
            && let Some(other) = tool_names.insert(name.clone(), location.clone())
        {
            problems.push(format!(
                "{location} and {other} both produce the tool name {name:?}; set `{X_FERRUM_MCP}.name` on one of them"
            ));
        }
        public_paths.push((path, location));
    }
    if selected == 0 {
        problems.push_summary(format!(
            "`{X_FERRUM_MCP}` is enabled but selects no operation Edge can publish; Edge refuses such a document"
        ));
    } else if selected > MAX_TOOLS {
        problems.push_summary(format!(
            "`{X_FERRUM_MCP}` selects {selected} operations; Edge publishes at most {MAX_TOOLS}, so narrow the selection"
        ));
    }
    if let Some(public_path) = public_path {
        endpoint_problems(&extension, public_path, &public_paths, &mut problems);
    }
    problems.into_list()
}

/// The checks of one operation Edge's selection would consider, run once per
/// operation however many paths reach it. Returns `None` when Edge would not
/// publish it, otherwise its valid tool name, if it has one.
fn check_selected<'a>(
    extension: &DocumentExtension,
    resolver: &mut Resolver<'a>,
    method: &str,
    operation: &'a Map<String, Value>,
    operation_extension: &OperationExtension<'_>,
    location: &str,
    problems: &mut Problems,
) -> Option<Option<String>> {
    if !extension.selects(operation, operation_extension, method) {
        return None;
    }
    if UNBRIDGED_METHODS.contains(&method) {
        problems.push(format!(
            "{location} cannot be exposed to agents: Edge bridges only GET, POST, PUT, PATCH, and DELETE operations"
        ));
        return None;
    }
    let id = operation_id(operation);
    let named = id.is_some_and(|id| extension.include_operations.contains(id));
    let explicit = operation_extension.expose == Some(true) || named;
    if MUTATING_METHODS.contains(&method) && !explicit {
        problems.push(format!(
            "{location} changes state and is selected only by tag; set `expose: true` on it (`AgentTool::expose()`) or name its operationId in `include` to publish it to agents"
        ));
    }
    problems.extend(understated_risk(method, operation_extension, location));
    let mut tool_name = None;
    match id {
        None => problems.push(format!(
            "{location} is exposed to agents but has no operationId"
        )),
        Some(id) => {
            let name = operation_extension
                .name
                .map_or_else(|| tool_name_slug(id), str::to_owned);
            if is_valid_tool_name(&name) {
                tool_name = Some(name);
            } else {
                problems.push(format!(
                    "{location} does not yield a valid tool name; set `{X_FERRUM_MCP}.name`"
                ));
            }
        }
    }
    let description = operation_extension
        .description
        .or_else(|| operation.get("description").and_then(Value::as_str))
        .or_else(|| operation.get("summary").and_then(Value::as_str));
    if description.is_none_or(|text| text.trim().is_empty()) {
        problems.push(format!(
            "{location} is exposed to agents but has no description; agents choose tools by it (set `{X_FERRUM_MCP}.description`, or document the handler)"
        ));
    }
    if let Some(problem) = request_body_problem(resolver, operation, location) {
        problems.push(problem);
    }
    Some(tool_name)
}

/// The parsed, enabled document-level extension.
#[derive(Default)]
struct DocumentExtension {
    endpoint_path: Option<String>,
    include_operations: BTreeSet<String>,
    include_tags: BTreeSet<String>,
    exclude_operations: BTreeSet<String>,
    exclude_tags: BTreeSet<String>,
}

impl DocumentExtension {
    /// Edge's selection (`mcp_operation_selected`): a per-operation `expose`
    /// is authoritative; otherwise `include` (by operationId or tag) selects,
    /// or only `GET` without one, and `exclude` then removes.
    fn selects(
        &self,
        operation: &Map<String, Value>,
        extension: &OperationExtension<'_>,
        method: &str,
    ) -> bool {
        if let Some(expose) = extension.expose {
            return expose;
        }
        let id = operation_id(operation);
        let tags: Vec<&str> = operation
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| tags.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let matches = |operations: &BTreeSet<String>, tag_set: &BTreeSet<String>| {
            id.is_some_and(|id| operations.contains(id))
                || tags.iter().any(|tag| tag_set.contains(*tag))
        };
        let included = if self.include_operations.is_empty() && self.include_tags.is_empty() {
            method == "get"
        } else {
            matches(&self.include_operations, &self.include_tags)
        };
        included && !matches(&self.exclude_operations, &self.exclude_tags)
    }
}

/// A parsed per-operation extension. Text is borrowed from the document.
#[derive(Default)]
struct OperationExtension<'a> {
    expose: Option<bool>,
    name: Option<&'a str>,
    description: Option<&'a str>,
    /// The operation's `annotations` object, when it is one.
    annotations: Option<&'a Map<String, Value>>,
}

impl OperationExtension<'_> {
    /// A boolean hint the operation sets.
    fn hint(&self, key: &str) -> Option<bool> {
        self.annotations?.get(key)?.as_bool()
    }
}

/// Hints may only state more risk than the method's default: a state-changing
/// operation must not claim `readOnlyHint: true`, and a `DELETE` must not
/// claim `destructiveHint: false`. An agent may skip asking its user before
/// calling a tool whose hints understate what it does.
fn understated_risk(
    method: &str,
    extension: &OperationExtension<'_>,
    location: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    let hint = |key: &str| extension.hint(key);
    if MUTATING_METHODS.contains(&method) && hint("readOnlyHint") == Some(true) {
        problems.push(format!(
            "{location} changes state but claims `readOnlyHint: true`; annotations may only state more risk than the method's default"
        ));
    }
    if method == "delete" && hint("destructiveHint") == Some(false) {
        problems.push(format!(
            "{location} is a DELETE but claims `destructiveHint: false`; annotations may only state more risk than the method's default"
        ));
    }
    problems
}

/// Every `(path, method, operation)` of the document, with Path Item `$ref`s
/// in the same document resolved. Paths that alias one Path Item yield the
/// same operation objects.
fn operations<'a>(
    resolver: &mut Resolver<'a>,
) -> Vec<(&'a str, &'static str, &'a Map<String, Value>)> {
    let mut found = Vec::new();
    let document = resolver.document;
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return found;
    };
    for (path, item) in paths {
        let Some(item) = resolver
            .resolve(item)
            .ok()
            .and_then(|resolved| resolved.value.as_object())
        else {
            continue;
        };
        for method in HTTP_METHODS {
            if let Some(operation) = item.get(*method).and_then(Value::as_object) {
                found.push((path.as_str(), *method, operation));
            }
        }
    }
    found
}

/// Distinct operations sharing one nonempty `operationId`. Selection and
/// the stamped `include` name operations by id alone, so a duplicate would
/// let one operation inherit another's exposure grant (#216); OpenAPI also
/// requires operationIds to be unique. The same operation object reached
/// through several aliased Path Items is one operation, not a duplicate.
fn duplicate_operation_id_problems(
    found: &[(&str, &'static str, &Map<String, Value>)],
    problems: &mut Problems,
) {
    let mut first: BTreeMap<&str, (*const Map<String, Value>, String)> = BTreeMap::new();
    // One problem per (operationId, duplicate operation) pair: the same
    // duplicate target reached through many aliased paths reports once,
    // and the id is excerpted like every other echoed text.
    let mut reported: HashSet<(*const Map<String, Value>, *const Map<String, Value>)> =
        HashSet::new();
    for &(path, method, operation) in found {
        let Some(id) = operation_id(operation) else {
            continue;
        };
        let key = std::ptr::from_ref(operation);
        match first.get(id) {
            None => {
                first.insert(id, (key, location(path, method)));
            }
            Some(&(seen, ref other)) if seen != key && reported.insert((seen, key)) => {
                let location = location(path, method);
                let other = other.clone();
                let id = excerpt_text(id).into_owned();
                problems.push(format!(
                    "{location} and {other} share operationId {id:?}; operationIds must be unique, because agent exposure is granted by operationId"
                ));
            }
            Some(_) => {}
        }
    }
}

/// Path Items whose `$ref` cannot be followed.
fn path_item_problems(resolver: &mut Resolver<'_>, problems: &mut Problems) {
    let document = resolver.document;
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return;
    };
    for (path, item) in paths {
        if let Err(problem) = resolver.resolve(item) {
            problems.push_with(|| format!("paths.{}: {problem}", excerpt_text(path)));
        }
    }
}

fn operation_id(operation: &Map<String, Value>) -> Option<&str> {
    operation
        .get("operationId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// The `expose` an operation declares: `x-ferrum-mcp: true`/`false` or the
/// object form's `expose`.
fn explicit_expose(operation: &Map<String, Value>) -> Option<bool> {
    match operation.get(X_FERRUM_MCP)? {
        Value::Bool(expose) => Some(*expose),
        Value::Object(object) => object.get("expose").and_then(Value::as_bool),
        _ => None,
    }
}

/// Where a local `$ref` chain ends.
#[derive(Clone, Copy)]
struct Resolved<'a> {
    /// The value the chain ends at (the value itself when it has no `$ref`).
    value: &'a Value,
    /// The JSON Pointer of the chain's last hop; `None` without a `$ref`.
    pointer: Option<&'a str>,
    /// `$ref` hops followed.
    hops: usize,
}

/// Follows local `$ref`s (`#/...` JSON Pointers), remembering the outcome for
/// every value with a `$ref` it passes. Many references to one shared chain
/// (or to one long `$ref` string) therefore cost one walk of it in total,
/// not one per reference.
struct Resolver<'a> {
    document: &'a Value,
    /// Keyed by the address of a value that has a `$ref`.
    memo: HashMap<*const Value, Result<Resolved<'a>, String>>,
    /// Whether a resolved request body offers a JSON media type, keyed by
    /// the body's address, so a body shared by many operations is scanned
    /// once.
    json_bodies: HashMap<*const Value, bool>,
}

impl<'a> Resolver<'a> {
    fn new(document: &'a Value) -> Self {
        Self {
            document,
            memo: HashMap::new(),
            json_bodies: HashMap::new(),
        }
    }

    /// Follows the `$ref` chain from `value`; a chain of [`MAX_REF_HOPS`] or
    /// more hops, or one that never ends, is a problem.
    fn resolve(&mut self, value: &'a Value) -> Result<Resolved<'a>, String> {
        // The values with a `$ref` passed on this walk, in order, and the
        // pointer each one names.
        let mut chain: Vec<(&'a Value, &'a str)> = Vec::new();
        let mut on_chain = HashSet::new();
        let mut current = value;
        let tail = loop {
            let Some(reference) = current.get("$ref") else {
                break Ok(Resolved {
                    value: current,
                    pointer: None,
                    hops: 0,
                });
            };
            let key = std::ptr::from_ref(current);
            if let Some(known) = self.memo.get(&key) {
                break known.clone();
            }
            if !on_chain.insert(key) {
                // A cycle: no chain through it ever ends.
                break Err(chain_too_long());
            }
            let Some(pointer) = reference.as_str().and_then(|r| r.strip_prefix('#')) else {
                break Err(format!(
                    "uses the non-local $ref {}, which this check cannot follow",
                    excerpt(reference)
                ));
            };
            chain.push((current, pointer));
            match self.document.pointer(pointer) {
                Some(next) => current = next,
                None => break Err(format!("$ref {} does not resolve", excerpt(reference))),
            }
        };
        // Record the outcome for every link of the walk, each measured from
        // that link, so a later walk reaching any of them stops there. The
        // tail is measured from where the walk stopped, one hop past the
        // last link.
        let last_pointer = chain.last().map(|(_, pointer)| *pointer);
        let length = chain.len();
        let mut outcome = tail;
        for (index, (link, _)) in chain.into_iter().enumerate().rev() {
            let result = match &outcome {
                Err(problem) => Err(problem.clone()),
                Ok(end) => {
                    let hops = length - index + end.hops;
                    // As before memoization: the walk gave up after taking
                    // its MAX_REF_HOPS-th hop.
                    if hops >= MAX_REF_HOPS {
                        Err(chain_too_long())
                    } else {
                        Ok(Resolved {
                            value: end.value,
                            pointer: end.pointer.or(last_pointer),
                            hops,
                        })
                    }
                }
            };
            self.memo.insert(std::ptr::from_ref(link), result.clone());
            if index == 0 {
                outcome = result;
            }
        }
        outcome
    }
}

fn chain_too_long() -> String {
    format!("has a $ref chain longer than {MAX_REF_HOPS} hops")
}

/// A `$ref` value for a problem message, shortened so that a huge reference
/// shared by many paths is not copied into every message.
fn excerpt(reference: &Value) -> String {
    match reference {
        Value::String(text) => {
            // Never walks more of the string than it echoes.
            let mut chars = text.chars();
            let mut head: String = chars.by_ref().take(MAX_REF_EXCERPT_CHARS).collect();
            if chars.next().is_some() {
                head.push('…');
            }
            Value::String(head).to_string()
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => reference.to_string(),
        Value::Array(_) => "(an array)".to_owned(),
        Value::Object(_) => "(an object)".to_owned(),
    }
}

/// The JSON media types the bridge sends (Edge `media_type_is_json`).
fn is_json_media_type(media_type: &str) -> bool {
    let essence = media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .trim()
        .as_bytes();
    let suffix = b"+json";
    essence.eq_ignore_ascii_case(b"application/json")
        || (essence.len() >= suffix.len()
            && essence[essence.len() - suffix.len()..].eq_ignore_ascii_case(suffix))
}

/// A request body the bridge cannot send. Edge refuses a required one at
/// import and silently drops an optional one; both are reported, because an
/// agent could not send either.
fn request_body_problem<'a>(
    resolver: &mut Resolver<'a>,
    operation: &'a Map<String, Value>,
    location: &str,
) -> Option<String> {
    let body = operation.get("requestBody")?;
    let body = match resolver.resolve(body) {
        Ok(resolved) => resolved.value,
        Err(problem) => return Some(format!("{location} request body: {problem}")),
    };
    let json = *resolver
        .json_bodies
        .entry(std::ptr::from_ref(body))
        .or_insert_with(|| {
            body.get("content")
                .and_then(Value::as_object)
                .is_some_and(|content| content.keys().any(|media| is_json_media_type(media)))
        });
    if json {
        return None;
    }
    Some(format!(
        "{location} takes a request body with no JSON media type; the Edge bridge sends JSON bodies only, so an agent could not call it"
    ))
}

/// Edge's `mcp_tool_name_slug`: every character outside `A-Za-z0-9.-`
/// becomes `_`, runs of `_` collapse, and leading and trailing `_` go.
fn tool_name_slug(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    for character in text.chars() {
        let mapped = if character.is_ascii_alphanumeric() || matches!(character, '.' | '-') {
            character
        } else {
            '_'
        };
        if mapped == '_' && slug.ends_with('_') {
            continue;
        }
        slug.push(mapped);
    }
    let slug = slug.trim_matches('_');
    slug.chars().take(MAX_TOOL_NAME_BYTES).collect()
}

/// 1-128 characters of `A-Za-z0-9_.-` (Edge `is_valid_bridge_tool_name`).
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// 1-64 characters of `A-Za-z0-9_-` (Edge's namespace rule).
pub(crate) fn is_valid_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace.len() <= MAX_NAMESPACE_BYTES
        && namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn unknown_keys(object: &Map<String, Value>, allowed: &[&str], at: &str, problems: &mut Problems) {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            problems.push(format!(
                "`{at}` has the unknown key {:?}; Edge accepts only {}",
                excerpt_text(key),
                allowed.join(", ")
            ));
        }
    }
}

fn string_set(
    object: &Map<String, Value>,
    key: &str,
    at: &str,
    problems: &mut Problems,
) -> BTreeSet<String> {
    match object.get(key) {
        None | Some(Value::Null) => BTreeSet::new(),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => strings(items),
        Some(_) => {
            problems.push(format!("`{at}.{key}` must be an array of strings"));
            BTreeSet::new()
        }
    }
}

fn strings(items: &[Value]) -> BTreeSet<String> {
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn selector(
    object: &Map<String, Value>,
    key: &str,
    problems: &mut Problems,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let at = format!("{X_FERRUM_MCP}.{key}");
    match object.get(key) {
        None | Some(Value::Null) => (BTreeSet::new(), BTreeSet::new()),
        Some(Value::Object(selector)) => {
            unknown_keys(selector, SELECTOR_KEYS, &at, problems);
            (
                string_set(selector, "operations", &at, problems),
                string_set(selector, "tags", &at, problems),
            )
        }
        Some(_) => {
            problems.push(format!(
                "`{at}` must be an object with operations and/or tags"
            ));
            (BTreeSet::new(), BTreeSet::new())
        }
    }
}

/// Parses the document-level extension (Edge `parse_x_ferrum_mcp_extension`).
/// `None` when it is absent, `null`, `false`, or `enabled: false`.
fn document_extension(value: Option<&Value>, problems: &mut Problems) -> Option<DocumentExtension> {
    let object = match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => return None,
        Some(Value::Bool(true)) => return Some(DocumentExtension::default()),
        Some(Value::Object(object)) => object,
        Some(_) => {
            problems.push(format!(
                "document-level `{X_FERRUM_MCP}` must be true, false, or an object"
            ));
            return None;
        }
    };
    unknown_keys(object, DOCUMENT_KEYS, X_FERRUM_MCP, problems);
    match object.get("enabled") {
        None | Some(Value::Null) | Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => return None,
        Some(_) => problems.push(format!("`{X_FERRUM_MCP}.enabled` must be a boolean")),
    }
    let mut endpoint_path = None;
    match object.get("endpoint") {
        None | Some(Value::Null) => {}
        Some(Value::Object(endpoint)) => {
            let at = format!("{X_FERRUM_MCP}.endpoint");
            unknown_keys(endpoint, ENDPOINT_KEYS, &at, problems);
            match endpoint.get("path") {
                None | Some(Value::Null) => {}
                Some(Value::String(path)) => endpoint_path = Some(path.clone()),
                Some(_) => problems.push(format!("`{at}.path` must be a string")),
            }
        }
        Some(_) => problems.push(format!("`{X_FERRUM_MCP}.endpoint` must be an object")),
    }
    match object.get("namespace") {
        None | Some(Value::Null) => {}
        Some(Value::String(namespace)) if is_valid_namespace(namespace) => {}
        Some(_) => problems.push(format!(
            "`{X_FERRUM_MCP}.namespace` must be 1-{MAX_NAMESPACE_BYTES} characters of A-Za-z0-9_-"
        )),
    }
    let (include_operations, include_tags) = selector(object, "include", problems);
    let (exclude_operations, exclude_tags) = selector(object, "exclude", problems);
    match object.get("limits") {
        None | Some(Value::Null) => {}
        Some(Value::Object(limits)) => {
            let at = format!("{X_FERRUM_MCP}.limits");
            unknown_keys(limits, LIMIT_KEYS, &at, problems);
            for (key, value) in limits {
                if value.as_u64().is_none_or(|limit| limit == 0) {
                    problems.push(format!(
                        "`{at}.{}` must be a positive integer",
                        excerpt_text(key)
                    ));
                }
            }
        }
        Some(_) => problems.push(format!("`{X_FERRUM_MCP}.limits` must be an object")),
    }
    match object.get("forward_request_headers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(names)) if names.iter().all(Value::is_string) => {}
        Some(_) => problems.push(format!(
            "`{X_FERRUM_MCP}.forward_request_headers` must be an array of header names"
        )),
    }
    Some(DocumentExtension {
        endpoint_path,
        include_operations,
        include_tags,
        exclude_operations,
        exclude_tags,
    })
}

fn optional_text<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    at: &str,
    problems: &mut Problems,
) -> Option<&'a str> {
    match object.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text),
        Some(_) => {
            problems.push(format!("`{at}.{key}` must be a string"));
            None
        }
    }
}

/// Parses a per-operation extension (Edge `parse_operation_mcp_extension`,
/// plus the annotation rules the generated plugin applies at load).
fn operation_extension<'a>(
    operation: &'a Map<String, Value>,
    location: &str,
    problems: &mut Problems,
) -> OperationExtension<'a> {
    let object = match operation.get(X_FERRUM_MCP) {
        None | Some(Value::Null) => return OperationExtension::default(),
        Some(Value::Bool(expose)) => {
            return OperationExtension {
                expose: Some(*expose),
                ..OperationExtension::default()
            };
        }
        Some(Value::Object(object)) => object,
        Some(_) => {
            problems.push(format!(
                "`{location}.{X_FERRUM_MCP}` must be a boolean or an object"
            ));
            return OperationExtension::default();
        }
    };
    let at = format!("{location}.{X_FERRUM_MCP}");
    unknown_keys(object, OPERATION_KEYS, &at, problems);
    let expose = match object.get("expose") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(expose)) => Some(*expose),
        Some(_) => {
            problems.push(format!("`{at}.expose` must be a boolean"));
            None
        }
    };
    let name = optional_text(object, "name", &at, problems);
    if let Some(name) = name
        && !is_valid_tool_name(name)
    {
        problems.push(format!(
            "`{at}.name` must be 1-{MAX_TOOL_NAME_BYTES} characters of A-Za-z0-9_.-"
        ));
    }
    let _title = optional_text(object, "title", &at, problems);
    let description = optional_text(object, "description", &at, problems);
    let mut hints = None;
    match object.get("annotations") {
        None | Some(Value::Null) => {}
        Some(Value::Object(annotations)) => {
            hints = Some(annotations);
            let at = format!("{at}.annotations");
            unknown_keys(annotations, ANNOTATION_KEYS, &at, problems);
            for (key, value) in annotations {
                let valid = if key == "title" {
                    value
                        .as_str()
                        .is_some_and(|title| title.len() <= MAX_TEXT_BYTES)
                } else {
                    value.is_boolean()
                };
                if !valid {
                    problems.push(format!(
                        "`{at}.{}` has the wrong type or is longer than {MAX_TEXT_BYTES} bytes",
                        excerpt_text(key)
                    ));
                }
            }
        }
        Some(_) => problems.push(format!("`{at}.annotations` must be an object")),
    }
    OperationExtension {
        expose,
        name: name.filter(|name| is_valid_tool_name(name)),
        description,
        annotations: hints,
    }
}

/// The MCP endpoint must sit under the public path (Edge: under the literal
/// `listen_path`) and must not overlap a selected operation, because it
/// reserves its whole subtree. Operation paths are those a client sees: the
/// public path followed by the Paths key.
fn endpoint_problems(
    extension: &DocumentExtension,
    public_path: &str,
    selected: &[(&str, String)],
    problems: &mut Problems,
) {
    let prefix = public_path.trim_end_matches('/');
    let endpoint = extension
        .endpoint_path
        .clone()
        .unwrap_or_else(|| format!("{prefix}/mcp"));
    let shown_endpoint = excerpt_text(&endpoint);
    let shown_public_path = excerpt_text(public_path);
    if !prefix.is_empty() && endpoint != prefix && !endpoint.starts_with(&format!("{prefix}/")) {
        problems.push(format!(
            "`{X_FERRUM_MCP}.endpoint.path` {shown_endpoint:?} is not under api.public_path {shown_public_path:?}"
        ));
    }
    // A client sees each operation at the public path followed by its Paths
    // key (the public path alone for `/`). The shared prefix is compared
    // once, and each path only against what is left of the endpoint, so no
    // per-path string is built.
    let scope = endpoint.trim_end_matches('/');
    let within_prefix = prefix == scope
        || prefix
            .strip_prefix(scope)
            .is_some_and(|rest| rest.starts_with('/'));
    let rest_of_scope = scope.strip_prefix(prefix);
    for (path, location) in selected {
        let suffix = if !prefix.is_empty() && *path == "/" {
            ""
        } else {
            path
        };
        let inside = within_prefix
            || rest_of_scope.is_some_and(|rest| {
                suffix == rest
                    || suffix
                        .strip_prefix(rest)
                        .is_some_and(|tail| tail.starts_with('/'))
            });
        if inside {
            problems.push(format!(
                "{location} is served at \"{}{}\", inside the MCP endpoint {shown_endpoint:?}; move the endpoint ([agents] endpoint_path)",
                excerpt_text(prefix),
                excerpt_text(suffix)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pointers spelled differently for one Path Item (`~2` is no escape, so
    /// `~02` reads back as the same key) yield that item once.
    #[test]
    fn referenced_path_items_are_distinct_targets() {
        let document = json!({
            "paths": {
                "/a": { "$ref": "#/x-item~2" },
                "/b": { "$ref": "#/x-item~02" },
                "/c": { "$ref": "#/x-item~2" }
            },
            "x-item~2": { "get": {} }
        });
        assert_eq!(referenced_path_items(&document), ["/x-item~2"]);
    }
}
