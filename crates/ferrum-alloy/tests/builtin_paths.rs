//! Alloy's own paths never make composition panic: a path axum cannot route
//! literally fails validation, and two of Alloy's paths on one listener fail
//! `into_parts` with a configuration error, while the same path on two
//! different listeners is fine.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrum_alloy::config::{AlloyConfig, ConfigError};
use ferrum_alloy::{AlloyApp, AlloyError, AlloyParts, TelemetryInit};
use http_body_util::BodyExt;
use serde_json::Value;
use support::config;
use tower::ServiceExt;

async fn assert_method_not_allowed(response: axum::response::Response) {
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = response.headers().get("allow").unwrap();
    assert!(!allow.is_empty());
    assert_eq!(
        response.headers()["content-type"],
        "application/problem+json"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let problem: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        problem,
        serde_json::json!({
            "type": "tag:ferrumedge.com,2026:alloy/problem/method-not-allowed",
            "title": "Method not allowed",
            "status": 405,
            "detail": "The route does not support this method."
        })
    );
}

#[tokio::test]
async fn built_in_routes_return_problem_details_for_disallowed_methods() {
    let mut cfg = config();
    #[cfg(feature = "openapi")]
    {
        cfg.openapi.public = true;
        #[cfg(feature = "openapi-ui")]
        {
            cfg.openapi.ui = true;
        }
    }
    #[cfg(not(feature = "openapi"))]
    let app = AlloyApp::new("paths");
    #[cfg(feature = "openapi")]
    let app = AlloyApp::new("paths").openapi(&document());
    #[cfg(feature = "diagnostics")]
    let app = app.diagnostics_authorizer(
        |_: ferrum_alloy::diagnostics::DiagnosticsRequest| async {
            ferrum_alloy::diagnostics::DiagnosticsAccess::Deny
        },
    );
    let parts = app
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap();

    let mut application_paths = vec!["/livez", "/readyz"];
    #[cfg(feature = "openapi")]
    application_paths.push("/openapi.json");
    #[cfg(feature = "openapi-ui")]
    application_paths.extend([
        "/docs",
        "/docs/swagger-ui.css",
        "/docs/swagger-ui-bundle.js",
        "/docs/swagger-ui-bundle.js.LICENSE.txt",
        "/docs/swagger-initializer.js",
    ]);
    for path in application_paths {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        assert_method_not_allowed(parts.router.clone().oneshot(request).await.unwrap()).await;
    }

    let mut management_paths = vec!["/livez", "/readyz", "/health", "/metrics"];
    #[cfg(feature = "openapi")]
    management_paths.push("/openapi.json");
    #[cfg(feature = "openapi-ui")]
    management_paths.extend([
        "/docs",
        "/docs/swagger-ui.css",
        "/docs/swagger-ui-bundle.js",
        "/docs/swagger-ui-bundle.js.LICENSE.txt",
        "/docs/swagger-initializer.js",
    ]);
    #[cfg(feature = "diagnostics")]
    management_paths.push("/diagnostics/v1/requests/request-id");
    let management = parts.management_router.as_ref().unwrap();
    for path in management_paths {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        assert_method_not_allowed(management.clone().oneshot(request).await.unwrap()).await;
    }
}

/// Composes `app` with an empty router, so no application route can be
/// involved.
fn compose(app: AlloyApp, config: AlloyConfig) -> Result<AlloyParts, AlloyError> {
    app.router(Router::new())
        .config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
}

/// The configuration errors of a composition that must fail.
fn config_errors(result: Result<AlloyParts, AlloyError>) -> Vec<String> {
    match result {
        Err(AlloyError::Config(ConfigError::Invalid(errors))) => errors,
        Err(other) => panic!("expected a configuration error, got {other}"),
        Ok(_) => panic!("expected a configuration error, but composition succeeded"),
    }
}

#[test]
fn paths_axum_cannot_route_literally_fail_validation_and_composition() {
    type Set = fn(&mut AlloyConfig, &str);
    let settings: [(&str, Set); 3] = [
        ("health.liveness_path", |c, p| {
            c.health.liveness_path = p.into()
        }),
        ("health.readiness_path", |c, p| {
            c.health.readiness_path = p.into()
        }),
        ("openapi.path", |c, p| c.openapi.path = p.into()),
    ];
    for path in [
        "/:live",
        "/probes/:live",
        "/*live",
        "/probes/*",
        "/{live}",
        "/li ve",
        "livez",
        "",
    ] {
        for (name, set) in settings {
            let mut cfg = config();
            set(&mut cfg, path);
            let error = cfg
                .validate(ferrum_alloy::enabled_features())
                .unwrap_err()
                .to_string();
            assert!(error.contains(name), "{name} = {path:?}: {error}");

            let errors = config_errors(compose(AlloyApp::new("paths"), cfg));
            let literal = format!("{name} must be a literal path");
            assert!(
                errors.iter().any(|e| e.contains(&literal)),
                "{name} = {path:?}: {errors:?}"
            );
        }
    }
}

#[test]
fn a_colon_inside_a_segment_is_a_literal_path() {
    // Only a segment that starts with ':' is refused.
    let mut cfg = config();
    cfg.health.liveness_path = "/probes/live:z".into();
    drop(compose(AlloyApp::new("paths"), cfg).unwrap());
}

#[test]
fn health_paths_may_reuse_management_paths_on_the_other_listener() {
    // The application listener serves these; the management listener serves
    // its own `/metrics` and `/health`.
    let mut cfg = config();
    cfg.health.liveness_path = "/metrics".into();
    cfg.health.readiness_path = "/health".into();
    let parts = compose(AlloyApp::new("paths"), cfg).unwrap();
    assert!(parts.management_router.is_some());
    drop(parts);
}

#[cfg(feature = "openapi")]
fn document() -> ferrum_alloy::utoipa::openapi::OpenApi {
    ferrum_alloy::utoipa::openapi::OpenApiBuilder::new()
        .info(
            ferrum_alloy::utoipa::openapi::InfoBuilder::new()
                .title("paths")
                .version("1")
                .build(),
        )
        .build()
}

#[cfg(feature = "openapi")]
#[test]
fn builtin_paths_served_twice_on_one_listener_are_configuration_errors() {
    let app = || AlloyApp::new("paths").openapi(&document());
    /// What the case is, how it changes the configuration, and the errors.
    type Case = (&'static str, fn(&mut AlloyConfig), &'static [&'static str]);
    let cases: [Case; 5] = [
        (
            "a public document at the liveness path",
            |c| {
                c.openapi.public = true;
                c.openapi.path = "/livez".into();
            },
            &[
                "openapi.path /livez is also served by health.liveness_path on the application listener; give each a path of its own",
                "openapi.path /livez is also served by a built-in management route on the management listener; give each a path of its own",
            ],
        ),
        (
            "a public document at the readiness path, management disabled",
            |c| {
                c.openapi.public = true;
                c.openapi.path = "/readyz".into();
                c.management.enabled = false;
            },
            &[
                "openapi.path /readyz is also served by health.readiness_path on the application listener; give each a path of its own",
            ],
        ),
        (
            "the document at the management metrics path",
            |c| c.openapi.path = "/metrics".into(),
            &[
                "openapi.path /metrics is also served by a built-in management route on the management listener; give each a path of its own",
            ],
        ),
        (
            "the document at the management health path",
            |c| c.openapi.path = "/health".into(),
            &[
                "openapi.path /health is also served by a built-in management route on the management listener; give each a path of its own",
            ],
        ),
        (
            "the document at the management liveness path, health off the application listener",
            |c| {
                c.health.app_endpoints = false;
                c.openapi.path = "/livez".into();
            },
            &[
                "openapi.path /livez is also served by a built-in management route on the management listener; give each a path of its own",
            ],
        ),
    ];
    for (case, mutate, expected) in cases {
        let mut cfg = config();
        mutate(&mut cfg);
        cfg.validate(ferrum_alloy::enabled_features())
            .unwrap_or_else(|e| panic!("{case}: validation accepts it: {e}"));
        let errors = config_errors(compose(app(), cfg));
        assert_eq!(errors, *expected, "{case}");
    }
}

#[cfg(feature = "openapi")]
#[test]
fn the_same_paths_compose_when_nothing_else_serves_them() {
    // Without a registered document, `openapi.path` is served nowhere.
    let mut cfg = config();
    cfg.openapi.public = true;
    cfg.openapi.path = "/metrics".into();
    drop(compose(AlloyApp::new("paths"), cfg).unwrap());

    // With `openapi.serve = false`, it is served nowhere either.
    let mut cfg = config();
    cfg.openapi.serve = false;
    cfg.openapi.path = "/metrics".into();
    drop(compose(AlloyApp::new("paths").openapi(&document()), cfg).unwrap());
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn a_public_document_may_take_a_path_the_other_listener_uses() {
    // Health is served on the management listener only, which is off, so
    // the public document may take `/livez` on the application listener.
    let mut cfg = config();
    cfg.openapi.public = true;
    cfg.openapi.path = "/livez".into();
    cfg.health.app_endpoints = false;
    cfg.management.enabled = false;
    let parts = compose(AlloyApp::new("paths").openapi(&document()), cfg).unwrap();
    let request = Request::get("/livez").body(Body::empty()).unwrap();
    let response = parts.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
}
