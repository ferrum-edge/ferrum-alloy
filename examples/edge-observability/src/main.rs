//! An Alloy service meant to run behind Ferrum Edge.
//!
//! Configuration comes from `FERRUM_ALLOY_CONFIG` (see `alloy.toml`): TLS
//! with client-certificate verification, `gateway_required`, the gateway's
//! SPIFFE identity as the only trusted peer, and OTLP export.
//!
//! `/items/{id}` performs an explicitly instrumented dependency call. The
//! dependency is **simulated** with a fixed delay so the end-to-end test can
//! check trace structure and timing boundaries deterministically; it is not a
//! database.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::edge::GatewayContext;
use ferrum_alloy::extract::{Json, Path};
use ferrum_alloy::telemetry::operation::{Operation, OperationKind};
use serde::Serialize;

/// Simulated dependency latency.
const DEPENDENCY_DELAY: Duration = Duration::from_millis(120);

#[derive(Serialize)]
struct Item {
    id: u32,
    in_stock: u32,
    consumer: Option<String>,
}

async fn get_item(Path(id): Path<u32>, gateway: Option<GatewayContext>) -> Json<Item> {
    let in_stock = Operation::new("inventory.lookup")
        .kind(OperationKind::Client)
        .run(async move {
            tokio::time::sleep(DEPENDENCY_DELAY).await;
            id % 7
        })
        .await;
    Json(Item {
        id,
        in_stock,
        consumer: gateway.and_then(|g| g.consumer_username),
    })
}

async fn events() -> Response {
    let (mut sender, body) = http_body_util::channel::Channel::<Bytes, Infallible>::new(2);
    tokio::spawn(async move {
        for i in 0..5 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            if sender
                .send_data(Bytes::from(format!("event: tick\ndata: {i}\n\n")))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(Body::new(body))
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new()
        .route("/hello", get(|| async { "hello from behind Ferrum Edge" }))
        .route("/items/{id}", get(get_item))
        .route("/events", get(events));
    AlloyApp::new("edge-demo-api")
        .version(env!("CARGO_PKG_VERSION"))
        .router(router)
        .run()
        .await?;
    Ok(())
}
