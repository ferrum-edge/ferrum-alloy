//! The document the browser smoke test expects Swagger UI to render
//! (`ci/browser-smoke/tests/openapi-ui.spec.ts`).

#![allow(clippy::expect_used)]

use example_openapi_ui::ApiDoc;
use utoipa::OpenApi;

#[test]
fn the_document_has_the_title_and_operations_the_browser_test_expects() {
    let document = ApiDoc::openapi();
    assert_eq!(document.info.title, "Ferrum Alloy browser smoke");
    let mut paths: Vec<&str> = document.paths.paths.keys().map(String::as_str).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["/orders", "/orders/{id}"]);
    let operations: usize = document
        .paths
        .paths
        .values()
        .map(|item| usize::from(item.get.is_some()))
        .sum();
    assert_eq!(operations, 2);
    let components = document.components.expect("components");
    assert!(components.schemas.contains_key("Order"));
}
