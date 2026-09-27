//! The OpenAPI documentation UI (feature `openapi-ui`): the same access
//! policy as the OpenAPI document, strict response headers, a page that loads
//! nothing from another origin, and embedded assets that match the vendored
//! files' manifest.

#![cfg(feature = "openapi-ui")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::routing::get;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::AlloyConfig;
use support::{Reply, TOKEN, config, fetch, fetch_with, start};

/// The page and every asset beneath it, at the default `openapi.ui_path`.
const UI_PATHS: [&str; 4] = [
    "/docs",
    "/docs/swagger-ui.css",
    "/docs/swagger-ui-bundle.js",
    "/docs/swagger-initializer.js",
];

fn document() -> ferrum_alloy::utoipa::openapi::OpenApi {
    ferrum_alloy::utoipa::openapi::OpenApiBuilder::new()
        .info(
            ferrum_alloy::utoipa::openapi::InfoBuilder::new()
                .title("docs-ui")
                .version("1")
                .build(),
        )
        .build()
}

fn app() -> AlloyApp {
    let router = Router::new().route("/orders", get(|| async { "orders" }));
    AlloyApp::new("docs-ui").router(router).openapi(&document())
}

fn ui_config() -> AlloyConfig {
    let mut cfg = config();
    cfg.openapi.ui = true;
    cfg
}

async fn with_token(url: &str) -> Reply {
    let bearer = format!("Bearer {TOKEN}");
    fetch_with(url, &[("authorization", &bearer)]).await
}

#[tokio::test]
async fn ui_requires_the_management_token_and_stays_off_the_public_listener() {
    let server = start(app(), ui_config()).await;
    for path in UI_PATHS {
        let url = server.management_url(path);
        let anonymous = fetch(&url).await;
        assert_eq!(anonymous.status, 401, "{path}");
        assert_eq!(anonymous.headers["www-authenticate"], "Bearer", "{path}");
        let wrong = fetch_with(&url, &[("authorization", "Bearer not-the-token")]).await;
        assert_eq!(wrong.status, 401, "{path}");
        assert_eq!(with_token(&url).await.status, 200, "{path}");
        assert_eq!(
            fetch(&server.url(path)).await.status,
            404,
            "{path} is not on the public listener"
        );
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ui_is_public_only_on_explicit_opt_in() {
    let mut cfg = ui_config();
    cfg.openapi.public = true;
    let server = start(app(), cfg).await;
    for path in UI_PATHS {
        assert_eq!(fetch(&server.url(path)).await.status, 200, "{path}");
        assert_eq!(
            fetch(&server.management_url(path)).await.status,
            401,
            "{path} still needs the token on the management listener"
        );
    }
    let page = fetch(&server.url("/docs")).await.text();
    assert!(page.contains(r#"data-document="/openapi.json""#), "{page}");
    assert_eq!(fetch(&server.url("/openapi.json")).await.status, 200);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ui_is_off_unless_configured_and_needs_a_document() {
    let mut cfg = config();
    cfg.openapi.public = true;
    let server = start(app(), cfg).await;
    let docs = server.management_url("/docs");
    assert_eq!(with_token(&docs).await.status, 404);
    assert_eq!(fetch(&server.url("/docs")).await.status, 404);
    server.shutdown().await.unwrap();

    let router = Router::new().route("/orders", get(|| async { "orders" }));
    let server = start(AlloyApp::new("docs-ui").router(router), ui_config()).await;
    let docs = server.management_url("/docs");
    assert_eq!(
        with_token(&docs).await.status,
        404,
        "no registered document, no UI"
    );
    server.shutdown().await.unwrap();

    let mut cfg = ui_config();
    cfg.openapi.serve = false;
    let server = start(app(), cfg).await;
    let docs = server.management_url("/docs");
    assert_eq!(with_token(&docs).await.status, 404);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ui_responses_are_locked_down() {
    let server = start(app(), ui_config()).await;
    let content_types = [
        "text/html; charset=utf-8",
        "text/css; charset=utf-8",
        "text/javascript; charset=utf-8",
        "text/javascript; charset=utf-8",
    ];
    for (path, content_type) in UI_PATHS.into_iter().zip(content_types) {
        let reply = with_token(&server.management_url(path)).await;
        assert_eq!(reply.status, 200, "{path}");
        let header = |name: &str| {
            reply.headers[name]
                .to_str()
                .unwrap_or_else(|_| panic!("{path}: {name}"))
                .to_owned()
        };
        assert_eq!(header("content-type"), content_type, "{path}");
        assert_eq!(header("cache-control"), "no-store", "{path}");
        assert_eq!(header("x-content-type-options"), "nosniff", "{path}");
        assert_eq!(header("referrer-policy"), "no-referrer", "{path}");
        assert_eq!(header("x-frame-options"), "DENY", "{path}");
        assert_eq!(header("cross-origin-resource-policy"), "same-origin");
        let policy = header("content-security-policy");
        let directives: Vec<&str> = policy.split(';').map(str::trim).collect();
        for directive in [
            "default-src 'none'",
            "script-src 'self'",
            "style-src 'self'",
            "img-src 'self' data:",
            "connect-src 'self'",
            "base-uri 'none'",
            "form-action 'none'",
            "frame-ancestors 'none'",
        ] {
            assert!(directives.contains(&directive), "{path}: {policy}");
        }
        assert!(!policy.contains("unsafe"), "{path}: {policy}");
    }

    let page = with_token(&server.management_url("/docs")).await.text();
    assert!(page.contains(r#"data-document="/openapi.json""#), "{page}");
    for absent in ["http:", "https:", "//", "style="] {
        assert!(!page.contains(absent), "{absent} in {page}");
    }
    let scripts: Vec<&str> = page.split("<script").skip(1).collect();
    assert_eq!(scripts.len(), 2, "{page}");
    for script in scripts {
        assert!(script.starts_with(r#" src="/docs/swagger-"#), "{page}");
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ui_follows_the_configured_paths() {
    let mut cfg = ui_config();
    cfg.openapi.ui_path = "/api/docs".into();
    cfg.openapi.path = "/api/spec.json".into();
    let server = start(app(), cfg).await;
    let page = with_token(&server.management_url("/api/docs")).await;
    assert_eq!(page.status, 200);
    let page = page.text();
    assert!(page.contains(r#"data-document="/api/spec.json""#), "{page}");
    assert!(
        page.contains(r#"src="/api/docs/swagger-ui-bundle.js""#),
        "{page}"
    );
    let asset = server.management_url("/api/docs/swagger-initializer.js");
    assert_eq!(with_token(&asset).await.status, 200);
    let document = with_token(&server.management_url("/api/spec.json")).await;
    assert_eq!(document.json()["info"]["title"], "docs-ui");
    let docs = server.management_url("/docs");
    assert_eq!(with_token(&docs).await.status, 404);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_initializer_loads_only_the_same_origin_document() {
    let server = start(app(), ui_config()).await;
    let url = server.management_url("/docs/swagger-initializer.js");
    let script = with_token(&url).await.text();
    for required in [
        r#"getAttribute("data-document")"#,
        "documentUrl.origin !== window.location.origin",
        "queryConfigEnabled: false",
        "validatorUrl: null",
        "supportedSubmitMethods: []",
    ] {
        assert!(script.contains(required), "{required} in {script}");
    }
    server.shutdown().await.unwrap();
}

fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/swagger-ui/5.33.0")
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `SHA256SUMS`: file name to lowercase hex SHA-256.
fn manifest() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(assets_dir().join("SHA256SUMS")).unwrap();
    text.lines()
        .map(|line| {
            let (hash, name) = line.split_once("  ").expect("`<sha256>  <name>`");
            assert_eq!(hash.len(), 64, "{line}");
            (name.to_owned(), hash.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn openapi_ui_assets_match_the_manifest() {
    let manifest = manifest();
    let listed: Vec<&str> = manifest.keys().map(String::as_str).collect();
    assert_eq!(
        listed,
        [
            "LICENSE",
            "NOTICE",
            "swagger-ui-bundle.js",
            "swagger-ui-bundle.js.LICENSE.txt",
            "swagger-ui.css",
        ],
        "the license, notices, and every embedded file are pinned"
    );
    let mut present: Vec<String> = std::fs::read_dir(assets_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "SHA256SUMS" && name != "README.md")
        .collect();
    present.sort();
    assert_eq!(present, listed, "every vendored file is in the manifest");
    for (name, hash) in &manifest {
        let bytes = std::fs::read(assets_dir().join(name)).unwrap();
        assert_eq!(&sha256_hex(&bytes), hash, "{name} on disk");
    }

    // The served bytes are the embedded ones.
    let server = start(app(), ui_config()).await;
    for name in ["swagger-ui.css", "swagger-ui-bundle.js"] {
        let reply = with_token(&server.management_url(&format!("/docs/{name}"))).await;
        assert_eq!(reply.status, 200, "{name}");
        assert_eq!(&sha256_hex(&reply.body), &manifest[name], "{name} served");
    }
    server.shutdown().await.unwrap();
}
