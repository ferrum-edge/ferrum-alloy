//! Extractors whose rejections are Problem Details.
//!
//! These wrap axum's extractors and keep their parsing behavior; only the
//! rejection changes. Raw `axum::Json`, `axum::extract::Path`, and friends
//! keep axum's own plain-text rejections when used directly.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::problem::{Problem, ProblemKind, sanitize_detail};

/// JSON body extractor and response with Problem Details rejections.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(json_problem(&rejection)),
        }
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

fn json_problem(rejection: &JsonRejection) -> Problem {
    let status = rejection.status();
    let kind = match rejection {
        JsonRejection::JsonSyntaxError(_) => ProblemKind::MalformedJson,
        JsonRejection::JsonDataError(_) => ProblemKind::InvalidBody,
        JsonRejection::MissingJsonContentType(_) => ProblemKind::UnsupportedMediaType,
        _ if status == http::StatusCode::PAYLOAD_TOO_LARGE => ProblemKind::PayloadTooLarge,
        _ => ProblemKind::MalformedJson,
    };
    let detail = match kind {
        ProblemKind::PayloadTooLarge => "The request body exceeds the configured limit.".to_owned(),
        ProblemKind::UnsupportedMediaType => "Expected Content-Type: application/json.".to_owned(),
        _ => sanitize_detail(&rejection.body_text()),
    };
    let mut problem = Problem::new(kind).with_detail(detail);
    if kind == ProblemKind::MalformedJson && status != http::StatusCode::BAD_REQUEST {
        // Unexpected axum rejection: keep axum's status rather than guessing.
        problem.status = status.as_u16();
    }
    problem
}

/// Path parameter extractor with Problem Details rejections.
#[derive(Debug, Clone, Copy, Default)]
pub struct Path<T>(pub T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(path_problem(&rejection)),
        }
    }
}

fn path_problem(rejection: &PathRejection) -> Problem {
    if rejection.status().is_server_error() {
        // A routing/wiring bug, not a client error; details stay in logs.
        tracing::error!(target: "ferrum_alloy::extract", rejection = %rejection.body_text(), "path extraction failed");
        return Problem::new(ProblemKind::Internal);
    }
    Problem::new(ProblemKind::InvalidPathParameters)
        .with_detail(sanitize_detail(&rejection.body_text()))
}

/// Query string extractor with Problem Details rejections.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T, S> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(query_problem(&rejection)),
        }
    }
}

fn query_problem(rejection: &QueryRejection) -> Problem {
    Problem::new(ProblemKind::InvalidQuery).with_detail(sanitize_detail(&rejection.body_text()))
}

/// A field-level validation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    /// JSON pointer or field name.
    pub field: String,
    /// What is wrong.
    pub message: String,
}

/// Validation errors returned by [`Validate`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<FieldError>);

impl ValidationErrors {
    /// Adds an error.
    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.0.push(FieldError {
            field: field.into(),
            message: message.into(),
        });
    }

    /// Returns `Err(self)` when any error was added.
    pub fn into_result(self) -> Result<(), Self> {
        if self.0.is_empty() { Ok(()) } else { Err(self) }
    }
}

/// Application-defined validation, run after deserialization.
pub trait Validate {
    /// Checks invariants the type system cannot express.
    fn validate(&self) -> Result<(), ValidationErrors>;
}

/// A JSON body that deserialized and passed [`Validate`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidJson<T>(pub T);

impl<T, S> FromRequest<S> for ValidJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(request, state).await?;
        match value.validate() {
            Ok(()) => Ok(Self(value)),
            Err(errors) => {
                let errors: Vec<serde_json::Value> = errors
                    .0
                    .iter()
                    .take(64)
                    .map(|e| {
                        serde_json::json!({
                            "field": sanitize_detail(&e.field),
                            "message": sanitize_detail(&e.message),
                        })
                    })
                    .collect();
                Err(Problem::new(ProblemKind::ValidationFailed)
                    .with_detail("One or more fields are invalid.")
                    .with_extension("errors", errors))
            }
        }
    }
}
