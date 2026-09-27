//! The pinned Edge release must be the same everywhere it appears, and CI must
//! test every release in the support window.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

fn repo(path: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path),
    )
    .unwrap()
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

/// Every line of `text` that pins an Edge image names one of `digests`.
fn assert_pins_only(file: &str, text: &str, digests: &[&str]) {
    for line in text
        .lines()
        .filter(|l| l.contains("ferrumedge/ferrum-edge@sha256:"))
    {
        assert!(
            digests.iter().any(|digest| line.contains(digest)),
            "{file} pins an Edge image outside the support window: {line}"
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
        r#"test("^ferrumedge/ferrum-edge@sha256:[0-9a-f]{64}$")"#,
        r#"test("^v[0-9]+\\.[0-9]+\\.[0-9]+$")"#,
        r#"test("^[0-9a-f]{40}$")"#,
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
