//! Serves the OpenAPI documentation UI (feature `openapi-ui`) for a small
//! document, so CI can load it in a real browser under its
//! Content-Security-Policy (`ci/browser-smoke/`). `alloy.toml` puts the UI at
//! `/docs` on the management listener (127.0.0.1:19090), behind the token
//! from `FERRUM_ALLOY_MANAGEMENT_TOKEN`, and also on the application listener
//! (127.0.0.1:18080) with `openapi.public`. Never expose the UI publicly in
//! production.

use example_openapi_ui::{ApiDoc, router};
use ferrum_alloy::AlloyApp;
use utoipa::OpenApi;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    AlloyApp::new("openapi-ui-smoke")
        .version(env!("CARGO_PKG_VERSION"))
        .config_file(concat!(env!("CARGO_MANIFEST_DIR"), "/alloy.toml"))
        .router(router())
        .openapi(&ApiDoc::openapi())
        .run()
        .await?;

    Ok(())
}
