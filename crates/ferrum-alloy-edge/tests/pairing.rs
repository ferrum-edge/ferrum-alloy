//! The pinned Edge release must be the same everywhere it appears.

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

#[test]
fn edge_pin_is_consistent() {
    let pairing: serde_json::Value =
        serde_json::from_str(&repo("docs/compatibility.json")).unwrap();
    let image = pairing["edge"]["image"].as_str().unwrap();
    let digest = image.split_once('@').unwrap().1;
    assert_eq!(
        pairing["edge"]["release"],
        ferrum_alloy_edge::contract::EDGE_RELEASE
    );
    assert_eq!(
        pairing["edge"]["source_commit"],
        ferrum_alloy_edge::contract::EDGE_SOURCE_COMMIT
    );
    for file in [
        ".github/workflows/ci.yml",
        "examples/edge-observability/Dockerfile",
        "docs/compatibility.md",
    ] {
        let text = repo(file);
        assert!(text.contains(digest), "{file} does not pin {digest}");
        for line in text
            .lines()
            .filter(|l| l.contains("ferrumedge/ferrum-edge@sha256:"))
        {
            assert!(
                line.contains(digest),
                "{file} pins a different Edge image: {line}"
            );
        }
    }
    let collector = pairing["collector"]["image"].as_str().unwrap();
    assert!(repo("examples/edge-observability/Dockerfile").contains(collector));
}
