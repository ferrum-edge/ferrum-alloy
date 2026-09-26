//! The smallest Ferrum Alloy service: an ordinary Axum router plus
//! `AlloyApp`. Run it with `cargo run -p example-minimal` and try
//! `curl localhost:8080/hello`, `curl localhost:8080/readyz`, and
//! `curl localhost:9090/metrics`.

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
