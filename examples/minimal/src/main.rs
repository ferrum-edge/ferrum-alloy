//! The smallest Ferrum Alloy service: an ordinary Axum router plus
//! `AlloyApp`. Run it with `cargo run -p example-minimal` and try
//! `curl localhost:8080/hello` and `curl localhost:8080/readyz`. The
//! management listener's `/livez` and `/readyz` are token-free probes. Its
//! detailed `/health` and `/metrics` routes require a configured bearer token
//! even on loopback. Set `FERRUM_ALLOY_MANAGEMENT_TOKEN_FILE` to a file holding
//! a secret of at least 32 characters before requesting them; protect the file
//! from other users (for example, mode `0600` on Unix). Do not put the token in
//! source code, a URL, or an unprotected config file. Browsers do not add bearer
//! tokens automatically; access the management UI through a local proxy or a
//! carefully scoped header-injecting extension backed by a protected secret.

use axum::{Router, routing::get};
use ferrum_alloy::AlloyApp;

async fn hello() -> &'static str {
    "Hello from Ferrum Alloy"
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new().route("/hello", get(hello));

    AlloyApp::new("hello-api").router(router).run().await?;

    Ok(())
}
