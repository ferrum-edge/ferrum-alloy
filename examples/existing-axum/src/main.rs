//! An existing Axum application adopting only `ferrum-alloy-telemetry`.
//!
//! The application keeps its own Tokio runtime, tracing subscriber, state,
//! middleware, router construction, and server (`axum::serve`). Alloy adds
//! request ids, trace-context policy, route-template metrics, and response
//! lifecycle accounting.

use std::net::SocketAddr;

use example_existing_axum::app;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The application owns its subscriber; Alloy never installs one here.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    let (service, metrics) = app()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(
        listener,
        service.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    tracing::info!(in_flight = metrics.in_flight(), "stopped");
    Ok(())
}
