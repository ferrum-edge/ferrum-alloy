//! Orders API: ordinary Axum handlers over an ordinary `sqlx::PgPool`.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use ferrum_alloy::extract::{Json, Path, Query, ValidJson, Validate, ValidationErrors};
use ferrum_alloy::postgres;
use ferrum_alloy::{Problem, ProblemKind};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// Embedded migrations, applied only by `example-postgres-api migrate`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// OpenAPI metadata; paths are registered with the routes.
#[derive(OpenApi)]
#[openapi(info(title = "orders-api", version = "0.1.0"))]
pub struct ApiDoc;

/// A new order.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewOrder {
    /// Item name (1-200 characters).
    pub item: String,
    /// Quantity (positive).
    pub quantity: i32,
}

impl Validate for NewOrder {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::default();
        if self.item.trim().is_empty() || self.item.len() > 200 {
            errors.add("/item", "must be 1-200 characters");
        }
        if self.quantity <= 0 {
            errors.add("/quantity", "must be positive");
        }
        errors.into_result()
    }
}

/// A stored order.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Order {
    /// Identifier.
    pub id: i64,
    /// Item name.
    pub item: String,
    /// Quantity.
    pub quantity: i32,
}

/// Pagination.
#[derive(Debug, Deserialize, IntoParams)]
pub struct Page {
    /// Maximum rows (1-100, default 20).
    pub limit: Option<i64>,
}

fn not_found(id: i64) -> Problem {
    Problem::custom(
        "tag:example.com,2026:orders-api/problem/order-not-found",
        "Order not found",
        StatusCode::NOT_FOUND,
    )
    .with_detail(format!("no order with id {id}"))
}

fn database_error(error: &sqlx::Error) -> Problem {
    // Details stay in server logs; clients get a sanitized problem.
    tracing::error!(target: "orders_api", %error, "database operation failed");
    Problem::new(ProblemKind::Internal)
}

/// Create an order.
#[utoipa::path(post, path = "/orders", request_body = NewOrder,
    responses((status = 201, body = Order), (status = 422, description = "Invalid order")))]
async fn create(
    State(pool): State<PgPool>,
    ValidJson(order): ValidJson<NewOrder>,
) -> Result<(StatusCode, Json<Order>), Problem> {
    let mut connection = postgres::acquire(&pool)
        .await
        .map_err(|e| database_error(&e))?;
    let created = postgres::query("orders.insert", "INSERT", "INSERT orders")
        .run_result(
            sqlx::query_as::<_, Order>(
                "INSERT INTO orders (item, quantity) VALUES ($1, $2) RETURNING id, item, quantity",
            )
            .bind(&order.item)
            .bind(order.quantity)
            .fetch_one(&mut *connection),
        )
        .await
        .map_err(|e| database_error(&e))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Fetch an order.
#[utoipa::path(get, path = "/orders/{id}", params(("id" = i64, Path)),
    responses((status = 200, body = Order), (status = 404, description = "Not found")))]
async fn fetch(State(pool): State<PgPool>, Path(id): Path<i64>) -> Result<Json<Order>, Problem> {
    let order = postgres::query("orders.fetch", "SELECT", "SELECT orders by id")
        .run_result(
            sqlx::query_as::<_, Order>("SELECT id, item, quantity FROM orders WHERE id = $1")
                .bind(id)
                .fetch_optional(&pool),
        )
        .await
        .map_err(|e| database_error(&e))?;
    order.map(Json).ok_or_else(|| not_found(id))
}

/// List orders.
#[utoipa::path(get, path = "/orders", params(Page), responses((status = 200, body = [Order])))]
async fn list(
    State(pool): State<PgPool>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<Order>>, Problem> {
    let limit = page.limit.unwrap_or(20).clamp(1, 100);
    let orders = postgres::query("orders.list", "SELECT", "SELECT orders page")
        .run_result(
            sqlx::query_as::<_, Order>(
                "SELECT id, item, quantity FROM orders ORDER BY id LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&pool),
        )
        .await
        .map_err(|e| database_error(&e))?;
    Ok(Json(orders))
}

/// Delete an order.
#[utoipa::path(delete, path = "/orders/{id}", params(("id" = i64, Path)),
    responses((status = 204), (status = 404, description = "Not found")))]
async fn remove(State(pool): State<PgPool>, Path(id): Path<i64>) -> Result<StatusCode, Problem> {
    let mut tx = pool.begin().await.map_err(|e| database_error(&e))?;
    let deleted = postgres::query("orders.delete", "DELETE", "DELETE orders by id")
        .run_result(
            sqlx::query("DELETE FROM orders WHERE id = $1")
                .bind(id)
                .execute(&mut *tx),
        )
        .await
        .map_err(|e| database_error(&e))?;
    if deleted.rows_affected() == 0 {
        tx.rollback().await.map_err(|e| database_error(&e))?;
        return Err(not_found(id));
    }
    tx.commit().await.map_err(|e| database_error(&e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The router and its OpenAPI document, from one registration.
pub fn api(pool: PgPool) -> (Router, utoipa::openapi::OpenApi) {
    let (router, document) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(create, list))
        .routes(routes!(fetch, remove))
        .split_for_parts();
    (router.with_state(pool), document)
}
