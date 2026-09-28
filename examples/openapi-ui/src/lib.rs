//! A small API and its OpenAPI document, for the documentation UI's browser
//! smoke test: two operations and one schema, so Swagger UI renders its
//! operation and model sections.

use axum::Router;
use axum::routing::get;
use ferrum_alloy::extract::{Json, Path};
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};

/// OpenAPI metadata and the documented operations.
#[derive(OpenApi)]
#[openapi(
    info(title = "Ferrum Alloy browser smoke", version = "0.1.0"),
    paths(list_orders, fetch_order),
    components(schemas(Order))
)]
pub struct ApiDoc;

/// An order.
#[derive(Serialize, ToSchema)]
struct Order {
    /// Identifier.
    id: i64,
    /// Item name.
    item: String,
}

fn order(id: i64) -> Order {
    Order {
        id,
        item: "widget".to_owned(),
    }
}

/// List orders.
#[utoipa::path(get, path = "/orders", responses((status = 200, body = [Order])))]
async fn list_orders() -> Json<Vec<Order>> {
    Json(vec![order(1)])
}

/// Fetch an order.
#[utoipa::path(get, path = "/orders/{id}", params(("id" = i64, Path)),
    responses((status = 200, body = Order)))]
async fn fetch_order(Path(id): Path<i64>) -> Json<Order> {
    Json(order(id))
}

/// The routes [`ApiDoc`] documents.
pub fn router() -> Router {
    Router::new()
        .route("/orders", get(list_orders))
        .route("/orders/{id}", get(fetch_order))
}
