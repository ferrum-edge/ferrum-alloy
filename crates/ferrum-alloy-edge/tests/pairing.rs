//! The pinned Edge release must be the same everywhere it appears, and CI must
//! test every release in the support window.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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

#[test]
fn ferrum_contracts_pin_and_local_adoption_match() {
    let root = repo_root();
    let pin: serde_json::Value = serde_json::from_str(&repo("contracts/ferrum-contracts/PIN"))
        .unwrap();
    assert_eq!(pin["tag"], "contracts-edge-0.9.8");
    assert_eq!(pin["commit"], "89ef3917ce6bba142dce50b84f2033d81eb429dd");

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
        pinned_files.insert(path.clone());
    }
    let vendored_root = root.join("contracts/ferrum-contracts");
    let mut vendored_files = BTreeSet::new();
    collect_files(&vendored_root, &vendored_root, &mut vendored_files);
    vendored_files.remove("PIN");
    let (missing, extra) = differences(&pinned_files, &vendored_files);
    assert!(missing.is_empty(), "vendored files missing from PIN: {missing:?}");
    assert!(extra.is_empty(), "vendored files absent from PIN: {extra:?}");

    let errors: serde_json::Value =
        serde_json::from_str(&repo("contracts/ferrum-contracts/vocabularies/gateway-errors.json"))
            .unwrap();
    let canonical_tokens: Vec<String> = errors["x_gateway_error_tokens"]
        .as_array()
        .unwrap()
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
    let canonical_token_set: BTreeSet<String> = canonical_tokens.into_iter().collect();
    let (missing, extra) = differences(&canonical_token_set, &edge_tokens);
    assert!(missing.is_empty(), "Edge contract tokens missing: {missing:?}");
    assert!(extra.is_empty(), "Edge contract tokens extra: {extra:?}");
    let (missing, extra) = differences(&canonical_token_set, &catalog_tokens);
    assert!(missing.is_empty(), "diagnostics catalog tokens missing: {missing:?}");
    assert!(extra.is_empty(), "diagnostics catalog tokens extra: {extra:?}");

    let canonical_meanings: std::collections::BTreeMap<&str, &str> =
        errors["x_gateway_error_tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["token"].as_str().unwrap(),
                    entry["meaning"].as_str().unwrap(),
                )
            })
            .collect();
    for (token, meaning) in ferrum_alloy_diagnostics::catalog::EDGE_GATEWAY_ERROR_TOKENS {
        assert_eq!(
            canonical_meanings.get(token),
            Some(meaning),
            "meaning differs for gateway error token {token}"
        );
    }

    let headers: serde_json::Value = serde_json::from_str(&repo(
        "contracts/ferrum-contracts/vocabularies/gateway-headers.json",
    ))
    .unwrap();
    let canonical_headers: BTreeSet<String> = headers["headers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| {
            matches!(
                entry["name"].as_str(),
                Some("X-Gateway-Error" | "X-Gateway-Upstream-Status")
            )
        })
        .map(|entry| entry["name"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    let local_headers = BTreeSet::from([
        ferrum_alloy_edge::contract::GATEWAY_ERROR.to_owned(),
        ferrum_alloy_edge::contract::GATEWAY_UPSTREAM_STATUS.to_owned(),
    ]);
    let (missing, extra) = differences(&canonical_headers, &local_headers);
    assert!(
        missing.is_empty(),
        "gateway headers missing locally: {missing:?}"
    );
    assert!(extra.is_empty(), "gateway headers extra locally: {extra:?}");

    let local_schema: serde_json::Value = serde_json::from_str(&repo(
        "contracts/diagnostics/diagnostic-report.v1.schema.json",
    ))
    .unwrap();
    let pinned_schema: serde_json::Value = serde_json::from_str(&repo(
        "contracts/ferrum-contracts/schemas/diagnostic-report/v1.schema.json",
    ))
    .unwrap();
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
    let fixtures: Vec<&String> = hashes
        .keys()
        .filter(|path| {
            path.starts_with("contracts/ferrum-contracts/fixtures/diagnostic-finding/valid/")
        })
        .collect();
    assert!(!fixtures.is_empty(), "PIN has no valid diagnostic-finding fixtures");
    for fixture in fixtures {
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(fixture.as_str())).unwrap(),
        )
        .unwrap();
        assert_finding_matches_schema(&value, finding_schema, &local_schema, fixture.as_str());
    }
}

fn assert_finding_matches_schema(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root_schema: &serde_json::Value,
    path: &str,
) {
    if let Some(reference) = schema["$ref"].as_str() {
        let target = reference
            .strip_prefix("#/")
            .unwrap()
            .split('/')
            .fold(root_schema, |current, key| &current[key]);
        assert_finding_matches_schema(value, target, root_schema, path);
    }
    if let Some(expected_types) = schema.get("type") {
        let types: Vec<&str> = expected_types
            .as_str()
            .map(|value| vec![value])
            .or_else(|| {
                expected_types.as_array().map(|values| {
                    values.iter().map(|value| value.as_str().unwrap()).collect()
                })
            })
            .unwrap();
        assert!(
            types.iter().any(|expected| match *expected {
                "array" => value.is_array(),
                "boolean" => value.is_boolean(),
                "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                "null" => value.is_null(),
                "number" => value.is_number(),
                "object" => value.is_object(),
                "string" => value.is_string(),
                unknown => panic!("unsupported schema type {unknown} in {path}"),
            }),
            "{path} does not match schema type {types:?}"
        );
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        assert!(
            variants
                .iter()
                .any(|variant| schema_matches(value, variant, root_schema, path)),
            "{path} does not match any schema alternative"
        );
    }
    if let Some(allowed) = schema["enum"].as_array() {
        assert!(
            allowed.contains(value),
            "{path} value {value} is not in {allowed:?}"
        );
    }
    if let (Some(minimum), Some(actual)) = (schema["minimum"].as_i64(), value.as_i64()) {
        assert!(actual >= minimum, "{path} is below minimum {minimum}");
    }
    if let Some(pattern) = schema["pattern"].as_str() {
        let text = value.as_str().unwrap();
        let valid = match pattern {
            "^[A-Za-z0-9._:-]{1,64}$" => {
                (1..=64).contains(&text.len())
                    && text
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
            }
            unsupported => panic!("unsupported schema pattern {unsupported} in {path}"),
        };
        assert!(valid, "{path} does not match schema pattern {pattern}");
    }
    if let Some(required) = schema["required"].as_array() {
        for field in required {
            let field = field.as_str().unwrap();
            assert!(
                value.get(field).is_some(),
                "{path} is missing required field {field}"
            );
        }
    }
    if let Some(properties) = schema["properties"].as_object() {
        for (field, property_schema) in properties {
            if let Some(property) = value.get(field) {
                assert_finding_matches_schema(
                    property,
                    property_schema,
                    root_schema,
                    &format!("{path}/{field}"),
                );
            }
        }
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (index, item) in values.iter().enumerate() {
            assert_finding_matches_schema(item, items, root_schema, &format!("{path}/{index}"));
        }
    }
}

fn schema_matches(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root_schema: &serde_json::Value,
    path: &str,
) -> bool {
    if let Some(variants) = schema["anyOf"].as_array() {
        return variants
            .iter()
            .any(|variant| schema_matches(value, variant, root_schema, path));
    }
    if let Some(reference) = schema["$ref"].as_str() {
        let Some(pointer) = reference.strip_prefix("#/") else {
            return false;
        };
        let target = pointer
            .split('/')
            .fold(root_schema, |current, key| &current[key]);
        return schema_matches(value, target, root_schema, path);
    }
    if let Some(allowed) = schema["enum"].as_array() {
        return allowed.contains(value);
    }
    if let Some(expected_types) = schema.get("type") {
        let types: Vec<&str> = expected_types
            .as_str()
            .map(|value| vec![value])
            .or_else(|| {
                expected_types.as_array().map(|values| {
                    values.iter().map(|value| value.as_str().unwrap()).collect()
                })
            })
            .unwrap();
        return types.iter().any(|expected_type| match *expected_type {
            "array" => value.is_array(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "null" => value.is_null(),
            "number" => value.is_number(),
            "object" => value.is_object(),
            "string" => value.is_string(),
            _ => false,
        });
    }
    false
}
