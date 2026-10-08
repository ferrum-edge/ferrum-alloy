//! The pinned Edge release must be the same everywhere it appears, and CI must
//! test every release in the support window.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ferrum_alloy_diagnostics::Finding;

fn repo(path: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path),
    )
    .unwrap()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collect_files(root: &Path, current: &Path, files: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(current).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else {
            files.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

fn differences(
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    let missing = expected.difference(actual).cloned().collect();
    let extra = actual.difference(expected).cloned().collect();
    (missing, extra)
}

fn compatibility() -> serde_json::Value {
    serde_json::from_str(&repo("docs/compatibility.json")).unwrap()
}

fn image_digest(image: &str) -> &str {
    image.split_once('@').unwrap().1
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `vMAJOR.MINOR.PATCH`, the only Edge release tag shape CI accepts.
fn is_release_tag(text: &str) -> bool {
    let Some(version) = text.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    parts.len() == 3 && parts.into_iter().all(numeric)
}

/// Mirrors the shape check in ci.yml's `edge-support` job.
fn assert_entry_shape(edge: &serde_json::Value) {
    let release = edge["release"].as_str().unwrap();
    assert!(is_release_tag(release), "release {release}");
    let commit = edge["source_commit"].as_str().unwrap();
    assert!(is_lower_hex(commit, 40), "source_commit {commit}");
    let image = edge["image"].as_str().unwrap();
    let digest = image
        .strip_prefix("ferrumedge/ferrum-edge@sha256:")
        .unwrap_or_else(|| panic!("image {image} is not a ferrumedge/ferrum-edge digest"));
    assert!(is_lower_hex(digest, 64), "image {image}");
}

/// Every Edge image pin in `text` (the digest prefix followed by exactly 64
/// lowercase hex digits, so not a regex that describes one) names one of
/// `digests`.
fn assert_pins_only(file: &str, text: &str, digests: &[&str]) {
    const PIN: &str = "ferrumedge/ferrum-edge@sha256:";
    for (at, _) in text.match_indices(PIN) {
        let rest = &text[at + PIN.len()..];
        let hex_len = rest
            .bytes()
            .take_while(|&b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            .count();
        if hex_len != 64 {
            continue;
        }
        let pin = &rest[..64];
        assert!(
            digests
                .iter()
                .any(|digest| digest.strip_prefix("sha256:") == Some(pin)),
            "{file} pins an Edge image outside the support window: sha256:{pin}"
        );
    }
}

/// One row of the supported-release table in `docs/compatibility.md`.
fn docs_row(edge: &serde_json::Value) -> String {
    let release = edge["release"].as_str().unwrap();
    let commit = edge["source_commit"].as_str().unwrap();
    let image = edge["image"].as_str().unwrap();
    format!("| {release} | `{commit}` | `{image}` |")
}

#[test]
fn edge_pin_is_consistent() {
    let pairing = compatibility();
    let image = pairing["edge"]["image"].as_str().unwrap();
    let digest = image_digest(image);
    assert_eq!(
        pairing["edge"]["release"],
        ferrum_alloy_edge::contract::EDGE_RELEASE
    );
    assert_eq!(
        pairing["edge"]["source_commit"],
        ferrum_alloy_edge::contract::EDGE_SOURCE_COMMIT
    );
    for file in [
        "examples/edge-observability/Dockerfile",
        "docs/compatibility.md",
    ] {
        let text = repo(file);
        assert!(text.contains(digest), "{file} does not pin {digest}");
    }
    // The demo defaults to the contract baseline; CI overrides it per release.
    let dockerfile = repo("examples/edge-observability/Dockerfile");
    assert_pins_only("Dockerfile", &dockerfile, &[digest]);
    let collector = pairing["collector"]["image"].as_str().unwrap();
    assert!(dockerfile.contains(collector));
}

#[test]
fn ci_tests_every_supported_edge_release() {
    let pairing = compatibility();
    let tested = pairing["edge_support"]["tested"].as_array().unwrap();
    assert_eq!(tested.len(), 2, "latest release plus the previous one");
    for edge in tested {
        assert_entry_shape(edge);
    }
    for key in ["release", "source_commit", "image"] {
        assert_eq!(tested[0][key], pairing["edge"][key], "{key}");
    }
    assert_ne!(tested[0]["release"], tested[1]["release"]);

    // CI reads its Edge matrix from compatibility.json, so the scheduled bump
    // (`.github/workflows/edge-bump.yml`) never has to edit a workflow.
    let ci = repo(".github/workflows/ci.yml");
    assert!(ci.contains("jq -ce '.edge_support.tested"));
    for pattern in [
        r#"test("^ferrumedge/ferrum-edge@sha256:[0-9a-f]{64}\\z")"#,
        r#"test("^v[0-9]+\\.[0-9]+\\.[0-9]+\\z")"#,
        r#"test("^[0-9a-f]{40}\\z")"#,
    ] {
        assert!(ci.contains(pattern), "ci.yml edge-support lacks {pattern}");
    }
    let matrix = "edge: ${{ fromJSON(needs.edge-support.outputs.tested) }}";
    assert_eq!(ci.matches(matrix).count(), 2, "edge-e2e and edge-config");
    assert_pins_only("ci.yml", &ci, &[]);

    let digests: Vec<&str> = tested
        .iter()
        .map(|edge| image_digest(edge["image"].as_str().unwrap()))
        .collect();

    let docs = repo("docs/compatibility.md");
    for edge in tested {
        let row = docs_row(edge);
        assert!(docs.contains(&row), "compatibility.md lacks {row}");
    }
    assert_pins_only("compatibility.md", &docs, &digests);
}

#[test]
fn gateway_error_tokens_match_the_diagnostics_catalog() {
    let catalog: Vec<&str> = ferrum_alloy_diagnostics::catalog::EDGE_GATEWAY_ERROR_TOKENS
        .iter()
        .map(|(token, _)| *token)
        .collect();
    assert_eq!(catalog, ferrum_alloy_edge::contract::GATEWAY_ERROR_TOKENS);
}

const PIN: &str = "contracts/ferrum-contracts/PIN";
const LOCAL_SCHEMA: &str = "contracts/diagnostics/diagnostic-report.v1.schema.json";
const FINDING_FIXTURES: &str = "contracts/ferrum-contracts/fixtures/diagnostic-finding/valid/";

/// The release-specific `X-Gateway-Error` meanings pinned in
/// `contracts-edge-0.9.14`, unchanged from `contracts-edge-0.9.8`. They are
/// recorded here only to detect drift and are never rendered: rule
/// `alloy.r007` renders the version-neutral explanations in
/// `catalog::EDGE_GATEWAY_ERROR_TOKENS`, which must never narrow a token's
/// meaning because a header names no Edge version. A pin bump that changes a
/// meaning fails `gateway_error_tokens_match_the_pinned_vocabulary` until the
/// explanations are re-reviewed and this table is updated.
const PINNED_GATEWAY_ERROR_MEANINGS: &[(&str, &str)] = &[
    (
        "connection_failure",
        "Pre-wire connect, DNS or TLS failure: the gateway could not set up a connection to the backend. Also the token for every ErrorClass whose request_reached_wire is false.",
    ),
    (
        "backend_timeout",
        "A backend held the request (it accepted the connection and was sent the request) but did not answer in time. Never used for a timeout no backend held.",
    ),
    (
        "backend_error",
        "The backend returned a 5xx, or a post-wire 5xx had no more specific token. Also the metric label for an unclassified backend 5xx. Never used for a response that did not reach a backend.",
    ),
    (
        "circuit_breaker_open",
        "The circuit breaker for the backend was open; the request never reached a backend.",
    ),
    (
        "overload",
        "Gateway resource refusal: overload or drain reject_new_requests (503), or response-transformer output above the configured response ceiling (502).",
    ),
    ("config_stale", "Data-plane stale-config fence."),
    (
        "concurrency_limit",
        "adaptive_concurrency admission refused the request.",
    ),
    (
        "request_timeout",
        "A matched route rule's total request deadline (mesh_route_dispatch request_timeout_ms, Gateway API timeouts.request) expired before any backend held the request: during the client upload, a gateway-local phase, admission, or retry backoff. New in v0.9.8 (#5762).",
    ),
];

/// JSON Schema keywords `schema_errors` implements. `assert_supported_schema`
/// fails on any other keyword, so no constraint is silently ignored.
const SUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "$ref",
    "anyOf",
    "enum",
    "items",
    "minimum",
    "pattern",
    "properties",
    "required",
    "type",
];

/// Annotation keywords, which never affect validation.
const ANNOTATION_SCHEMA_KEYWORDS: &[&str] = &["$comment", "description", "title"];

/// JSON Schema types `has_type` implements.
const SCHEMA_TYPES: &[&str] = &[
    "array", "boolean", "integer", "null", "number", "object", "string",
];

/// The only `pattern` the Finding schema uses; `matches_pattern` implements it.
const OBSERVATION_ID_PATTERN: &str = "^[A-Za-z0-9._:-]{1,64}$";

fn repo_json(path: &str) -> serde_json::Value {
    serde_json::from_str(&repo(path)).unwrap()
}

fn pinned_json(path: &str) -> serde_json::Value {
    repo_json(&format!("contracts/ferrum-contracts/{path}"))
}

/// The pinned valid Finding fixtures, by repository path.
fn finding_fixtures() -> Vec<String> {
    let pin = repo_json(PIN);
    let fixtures: Vec<String> = pin["files"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|path| path.starts_with(FINDING_FIXTURES))
        .cloned()
        .collect();
    assert!(!fixtures.is_empty(), "PIN has no valid Finding fixtures");
    fixtures
}

#[test]
fn ferrum_contracts_pin_matches_the_vendored_files() {
    let root = repo_root();
    let pin = repo_json(PIN);
    assert_eq!(pin["tag"], "contracts-edge-0.9.14");
    assert_eq!(pin["commit"], "ddbdd845733b7046c4393ac951011dafb774db33");

    let hashes = pin["files"].as_object().unwrap();
    let mut pinned_files = BTreeSet::new();
    for (path, expected_hash) in hashes {
        let full_path = root.join(path);
        let bytes = std::fs::read(&full_path).unwrap();
        let actual_hash = ring::digest::digest(&ring::digest::SHA256, &bytes);
        let actual_hash = actual_hash
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            expected_hash.as_str().unwrap(),
            actual_hash,
            "pinned contract file changed: {path}"
        );
        pinned_files.insert(
            path.strip_prefix("contracts/ferrum-contracts/")
                .unwrap()
                .to_owned(),
        );
    }
    let vendored_root = root.join("contracts/ferrum-contracts");
    let mut vendored_files = BTreeSet::new();
    collect_files(&vendored_root, &vendored_root, &mut vendored_files);
    vendored_files.remove("PIN");
    let (missing, extra) = differences(&pinned_files, &vendored_files);
    assert!(
        missing.is_empty(),
        "vendored files missing from PIN: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "vendored files absent from PIN: {extra:?}"
    );
}

#[test]
fn edge_diagnostic_ref_pattern_matches_the_pinned_schema() {
    let schema = pinned_json("schemas/diagnostic-ref/v1.schema.json");
    assert_eq!(
        ferrum_alloy_diagnostics::catalog::EDGE_DIAGNOSTIC_REF_PATTERN,
        schema["properties"]["ref"]["pattern"].as_str().unwrap()
    );
}

#[test]
fn gateway_error_tokens_match_the_pinned_vocabulary() {
    let errors = pinned_json("vocabularies/gateway-errors.json");
    assert_eq!(
        errors["edge_release"],
        ferrum_alloy_edge::contract::EDGE_RELEASE
    );
    let entries = errors["x_gateway_error_tokens"].as_array().unwrap();
    let canonical_tokens: BTreeSet<String> = entries
        .iter()
        .map(|entry| entry["token"].as_str().unwrap().to_owned())
        .collect();
    let edge_tokens: BTreeSet<String> = ferrum_alloy_edge::contract::GATEWAY_ERROR_TOKENS
        .iter()
        .map(|token| (*token).to_owned())
        .collect();
    let catalog_tokens: BTreeSet<String> =
        ferrum_alloy_diagnostics::catalog::EDGE_GATEWAY_ERROR_TOKENS
            .iter()
            .map(|(token, _)| (*token).to_owned())
            .collect();
    let (missing, extra) = differences(&canonical_tokens, &edge_tokens);
    assert!(
        missing.is_empty(),
        "Edge contract tokens missing: {missing:?}"
    );
    assert!(extra.is_empty(), "Edge contract tokens extra: {extra:?}");
    let (missing, extra) = differences(&canonical_tokens, &catalog_tokens);
    assert!(
        missing.is_empty(),
        "diagnostics catalog tokens missing: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "diagnostics catalog tokens extra: {extra:?}"
    );

    // Compared with the recorded pinned meanings, never with the rendered
    // catalog explanations, which stay version-neutral.
    let canonical_meanings: BTreeMap<&str, &str> = entries
        .iter()
        .map(|entry| {
            (
                entry["token"].as_str().unwrap(),
                entry["meaning"].as_str().unwrap(),
            )
        })
        .collect();
    let recorded_meanings: BTreeMap<&str, &str> =
        PINNED_GATEWAY_ERROR_MEANINGS.iter().copied().collect();
    assert!(
        canonical_meanings == recorded_meanings,
        "pinned X-Gateway-Error meanings changed: re-review catalog::EDGE_GATEWAY_ERROR_TOKENS (never narrow a meaning), then update PINNED_GATEWAY_ERROR_MEANINGS to {canonical_meanings:#?}"
    );
}

#[test]
fn released_gateway_diagnostic_headers_match_the_pinned_vocabulary() {
    let headers = pinned_json("vocabularies/gateway-headers.json");
    assert_eq!(
        headers["edge_release"],
        ferrum_alloy_edge::contract::EDGE_RELEASE
    );
    let canonical_headers: BTreeSet<String> = headers["headers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| {
            entry["role"] == "gateway_diagnostic" && entry["availability"] != "unreleased"
        })
        .map(|entry| entry["name"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    let local_headers = BTreeSet::from([
        ferrum_alloy_edge::contract::GATEWAY_ERROR.to_owned(),
        ferrum_alloy_edge::contract::GATEWAY_UPSTREAM_STATUS.to_owned(),
        ferrum_alloy_edge::contract::DIAGNOSTIC_REF.to_owned(),
    ]);
    let (missing, extra) = differences(&canonical_headers, &local_headers);
    assert!(
        missing.is_empty(),
        "released gateway diagnostic headers missing locally: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "local gateway diagnostic headers not released in the pin: {extra:?}"
    );
}

#[test]
fn diagnostic_reference_grammar_matches_the_pinned_vocabulary() {
    use ferrum_alloy_diagnostics::catalog;
    let headers = pinned_json("vocabularies/gateway-headers.json");
    let entry = headers["headers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "X-Ferrum-Diagnostic-Ref")
        .unwrap();
    assert_eq!(
        entry["values"]["pattern"].as_str().unwrap(),
        catalog::EDGE_DIAGNOSTIC_REF_PATTERN,
        "the pinned reference grammar changed: update catalog::is_edge_diagnostic_ref"
    );
    assert_eq!(
        catalog::EDGE_DIAGNOSTIC_REF_HEADER,
        ferrum_alloy_edge::contract::DIAGNOSTIC_REF
    );
}

#[test]
fn pinned_diagnostic_schema_and_finding_fixtures_match_alloy() {
    let root = repo_root();
    let local_schema = repo_json(LOCAL_SCHEMA);
    let pinned_schema = pinned_json("schemas/diagnostic-report/v1.schema.json");
    let pin = repo_json(PIN);
    let local_contract = &local_schema["x-contract"];
    let pinned_contract = &pinned_schema["x-contract"];
    for contract in [local_contract, pinned_contract] {
        assert_eq!(contract["status"], "implemented");
        assert_eq!(contract["owner"], "ferrum-edge/ferrum-alloy");
        assert!(
            contract["shared_status"]
                .as_str()
                .unwrap()
                .starts_with("EXISTING shared v1")
        );
    }
    assert_eq!(local_contract["availability"], "unreleased");
    assert_eq!(local_contract["contracts_tag"], pin["tag"]);
    assert_eq!(local_contract["contracts_commit"], pin["commit"]);
    let release = &pinned_contract["coordinated_release"];
    // The unchanged canonical report records the original shared v1 freeze,
    // separately from the later tag whose bytes this consumer adopts.
    assert_eq!(release["contracts_tag"], "contracts-edge-0.9.11");
    assert_eq!(
        local_contract["qualified_owner_commit"],
        release["qualified_owner_commit"]
    );
    assert_eq!(
        release["qualified_owner_commit"],
        "81cbb410d34ff5fba1f3d54cfd2e7ebccaed397e"
    );
    assert_eq!(
        pinned_contract["provenance"][0]["commit"],
        release["qualified_owner_commit"]
    );
    assert_eq!(
        pinned_contract["provenance"][0]["availability"],
        "unreleased"
    );
    // Keep every description in parity, including historical PROPOSED text.
    // The tag's x-contract metadata records the accepted shared v1 freeze;
    // its pending-publication wording is historical released source text.
    let without_contract_metadata = |mut schema: serde_json::Value| {
        schema.as_object_mut().unwrap().remove("$id");
        schema.as_object_mut().unwrap().remove("x-contract");
        schema
    };
    assert_eq!(
        without_contract_metadata(local_schema.clone()),
        without_contract_metadata(pinned_schema),
        "Alloy diagnostic-report schema differs from the pinned contract outside $id and x-contract"
    );

    let finding_schema = &local_schema["$defs"]["Finding"];
    assert_supported_schema(
        finding_schema,
        &local_schema,
        "#/$defs/Finding",
        &mut BTreeSet::new(),
    );
    for fixture in finding_fixtures() {
        let bytes = std::fs::read(root.join(&fixture)).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if let Err(error) = schema_errors(&value, finding_schema, &local_schema, &fixture) {
            panic!("{fixture} does not match Alloy's Finding schema: {error}");
        }
        let finding: Finding = serde_json::from_value(value)
            .unwrap_or_else(|error| panic!("{fixture} is not a Finding: {error}"));
        let unrecognized = unrecognized_finding_values(&finding);
        assert!(
            unrecognized.is_empty(),
            "{fixture} uses values Alloy does not recognize: {unrecognized:?}"
        );
        assert!(
            !finding.does_not_prove.is_empty(),
            "{fixture} lists nothing under does_not_prove"
        );
    }
}

#[test]
fn fixture_schema_validator_rejects_invalid_values_and_unknown_keywords() {
    let root_schema = repo_json(LOCAL_SCHEMA);
    let finding_schema = &root_schema["$defs"]["Finding"];
    let fixture = finding_fixtures()
        .into_iter()
        .find(|path| path.ends_with("ferrum-token-connection-failure.json"))
        .unwrap();
    let mut valid = repo_json(&fixture);
    valid["supporting_observations"] = serde_json::json!(["obs-1"]);
    let result = schema_errors(&valid, finding_schema, &root_schema, "valid");
    assert!(result.is_ok(), "{result:?}");

    for (pointer, replacement) in [
        ("/scope", serde_json::json!(7)),
        ("/rule_version", serde_json::json!("1")),
        ("/does_not_prove/0", serde_json::json!(7)),
        ("/evidence/0/source", serde_json::json!(false)),
        ("/evidence/0/attempt", serde_json::json!(-1)),
        ("/remediation/0/owner", serde_json::json!(null)),
        ("/supporting_observations/0", serde_json::json!("bad id")),
    ] {
        let invalid = replaced(&valid, pointer, replacement);
        let result = schema_errors(&invalid, finding_schema, &root_schema, pointer);
        assert!(result.is_err(), "an invalid {pointer} was accepted");
    }
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("does_not_prove");
    let result = schema_errors(&missing, finding_schema, &root_schema, "missing");
    assert!(result.is_err(), "a missing required field was accepted");

    for keyword in [
        "additionalProperties",
        "allOf",
        "const",
        "format",
        "minLength",
        "oneOf",
    ] {
        let mut schema = finding_schema.clone();
        schema["properties"]["code"][keyword] = serde_json::json!(true);
        let result = std::panic::catch_unwind(|| {
            assert_supported_schema(&schema, &root_schema, "probe", &mut BTreeSet::new());
        });
        assert!(result.is_err(), "keyword {keyword} was ignored");
    }
}

fn replaced(
    value: &serde_json::Value,
    pointer: &str,
    replacement: serde_json::Value,
) -> serde_json::Value {
    let mut value = value.clone();
    *value.pointer_mut(pointer).unwrap() = replacement;
    value
}

/// Values the shared fixture uses that this Alloy version would preserve as
/// unrecognized instead of treating as the known value they name.
fn unrecognized_finding_values(finding: &Finding) -> Vec<String> {
    let mut values = Vec::new();
    if finding.scope.is_unrecognized() {
        values.push(format!("scope {}", finding.scope));
    }
    if finding.confidence.is_unrecognized() {
        values.push(format!("confidence {}", finding.confidence));
    }
    if finding.severity.is_unrecognized() {
        values.push(format!("severity {}", finding.severity));
    }
    if finding.owner.is_unrecognized() {
        values.push(format!("owner {}", finding.owner));
    }
    for evidence in &finding.evidence {
        if evidence.source.is_unrecognized() {
            values.push(format!("evidence source {}", evidence.source));
        }
    }
    for remediation in &finding.remediation {
        if remediation.owner.is_unrecognized() {
            values.push(format!("remediation owner {}", remediation.owner));
        }
    }
    values
}

/// Fails the test when `schema`, or any schema it reaches through `$ref`,
/// `anyOf`, `properties`, or `items`, uses a keyword, type, or pattern that
/// `schema_errors` does not implement.
fn assert_supported_schema(
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
    visited: &mut BTreeSet<String>,
) {
    let keywords = schema
        .as_object()
        .unwrap_or_else(|| panic!("schema at {path} is not an object"));
    for (keyword, argument) in keywords {
        let keyword = keyword.as_str();
        assert!(
            SUPPORTED_SCHEMA_KEYWORDS.contains(&keyword)
                || ANNOTATION_SCHEMA_KEYWORDS.contains(&keyword),
            "unsupported schema keyword {keyword} at {path}"
        );
        match keyword {
            "$ref" => {
                let reference = argument.as_str().unwrap();
                if visited.insert(reference.to_owned()) {
                    let target = resolve_ref(reference, root);
                    assert_supported_schema(target, root, reference, visited);
                }
            }
            "anyOf" => {
                for (index, variant) in argument.as_array().unwrap().iter().enumerate() {
                    let variant_path = format!("{path}/anyOf/{index}");
                    assert_supported_schema(variant, root, &variant_path, visited);
                }
            }
            "items" => {
                let items_path = format!("{path}/items");
                assert_supported_schema(argument, root, &items_path, visited);
            }
            "properties" => {
                for (field, property) in argument.as_object().unwrap() {
                    let property_path = format!("{path}/properties/{field}");
                    assert_supported_schema(property, root, &property_path, visited);
                }
            }
            "type" => {
                for name in schema_types(argument) {
                    assert!(
                        SCHEMA_TYPES.contains(&name),
                        "unsupported schema type {name} at {path}"
                    );
                }
            }
            "pattern" => {
                let pattern = argument.as_str().unwrap();
                assert!(
                    pattern == OBSERVATION_ID_PATTERN,
                    "unsupported schema pattern {pattern} at {path}"
                );
            }
            "enum" => assert!(argument.is_array(), "enum at {path} is not an array"),
            "minimum" => assert!(argument.is_number(), "minimum at {path} is not a number"),
            "required" => assert!(argument.is_array(), "required at {path} is not an array"),
            _ => {}
        }
    }
}

fn resolve_ref<'a>(reference: &str, root: &'a serde_json::Value) -> &'a serde_json::Value {
    let pointer = reference
        .strip_prefix('#')
        .unwrap_or_else(|| panic!("unsupported non-local $ref {reference}"));
    root.pointer(pointer)
        .unwrap_or_else(|| panic!("unresolved $ref {reference}"))
}

fn schema_types(argument: &serde_json::Value) -> Vec<&str> {
    if let Some(name) = argument.as_str() {
        return vec![name];
    }
    argument
        .as_array()
        .unwrap_or_else(|| panic!("invalid schema type {argument}"))
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect()
}

fn has_type(value: &serde_json::Value, name: &str) -> bool {
    match name {
        "array" => value.is_array(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "object" => value.is_object(),
        "string" => value.is_string(),
        unsupported => panic!("unsupported schema type {unsupported}"),
    }
}

fn matches_pattern(pattern: &str, text: &str) -> bool {
    assert!(
        pattern == OBSERVATION_ID_PATTERN,
        "unsupported schema pattern {pattern}"
    );
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

/// Validates `value` against the JSON Schema subset `assert_supported_schema`
/// admits. `$ref` targets and every `anyOf` alternative are validated with
/// this full validator, including their nested keywords.
fn schema_errors(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    if let Some(reference) = schema["$ref"].as_str() {
        schema_errors(value, resolve_ref(reference, root), root, path)?;
    }
    if let Some(argument) = schema.get("type") {
        let types = schema_types(argument);
        if !types.iter().any(|name| has_type(value, name)) {
            return Err(format!("{path}: {value} is not of type {types:?}"));
        }
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        let failures: Vec<String> = variants
            .iter()
            .filter_map(|variant| schema_errors(value, variant, root, path).err())
            .collect();
        if failures.len() == variants.len() {
            return Err(format!("{path}: no anyOf match: {failures:?}"));
        }
    }
    let allowed = schema["enum"].as_array();
    if allowed.is_some_and(|values| !values.contains(value)) {
        return Err(format!("{path}: {value} is not an allowed value"));
    }
    let below_minimum = match (schema["minimum"].as_f64(), value.as_f64()) {
        (Some(minimum), Some(actual)) => actual < minimum,
        _ => false,
    };
    if below_minimum {
        return Err(format!("{path}: {value} is below the minimum"));
    }
    let pattern_mismatch = match (schema["pattern"].as_str(), value.as_str()) {
        (Some(pattern), Some(text)) => !matches_pattern(pattern, text),
        _ => false,
    };
    if pattern_mismatch {
        return Err(format!("{path}: {value} does not match its pattern"));
    }
    if let Some(object) = value.as_object() {
        for field in schema["required"].as_array().into_iter().flatten() {
            let field = field.as_str().unwrap();
            if !object.contains_key(field) {
                return Err(format!("{path}: missing required field {field}"));
            }
        }
        for (field, property) in schema["properties"].as_object().into_iter().flatten() {
            if let Some(member) = object.get(field) {
                schema_errors(member, property, root, &format!("{path}/{field}"))?;
            }
        }
    }
    if let (Some(values), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (index, item) in values.iter().enumerate() {
            schema_errors(item, item_schema, root, &format!("{path}/{index}"))?;
        }
    }
    Ok(())
}
