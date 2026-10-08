//! One error envelope for every failure:
//! `{"error": {"type": "...", "message": "...", "details": {...}}}`
//!
//! Status rule: 400 invalid input, 401 auth, 402 payment declined, 404 not found
//! (including other tenants' resources), 405 wrong method, 409 state conflict or
//! request in progress, 413 body too large, 415 not JSON, 422 only for
//! idempotency-key reuse, 500 internal.

use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection, QueryRejection},
        FromRequest, FromRequestParts,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

#[derive(Debug)]
pub enum ApiError {
    Validation(String),
    Unauthorized,
    NotFound(&'static str),
    InvalidTransition {
        current_status: String,
        action: String,
    },
    Conflict {
        kind: &'static str,
        message: String,
    },
    IdempotencyKeyReused,
    MethodNotAllowed,
    UnsupportedMediaType(String),
    PayloadTooLarge,
    Internal(anyhow::Error),
}

impl ApiError {
    pub fn validation(msg: impl Into<String>) -> Self {
        ApiError::Validation(msg.into())
    }

    pub fn parts(&self) -> (StatusCode, &'static str, String, Value) {
        match self {
            ApiError::Validation(msg) => (
                StatusCode::BAD_REQUEST,
                "validation_error",
                msg.clone(),
                json!({}),
            ),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Missing or invalid API key".into(),
                json!({}),
            ),
            ApiError::NotFound(what) => (
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{what} not found"),
                json!({}),
            ),
            ApiError::InvalidTransition {
                current_status,
                action,
            } => (
                StatusCode::CONFLICT,
                "invalid_state_transition",
                format!("Cannot {action} an invoice in status '{current_status}'"),
                json!({ "current_status": current_status, "action": action }),
            ),
            ApiError::Conflict { kind, message } => {
                (StatusCode::CONFLICT, kind, message.clone(), json!({}))
            }
            ApiError::IdempotencyKeyReused => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "This Idempotency-Key was already used with a different request".into(),
                json!({}),
            ),
            ApiError::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "Method not allowed for this route".into(),
                json!({}),
            ),
            ApiError::UnsupportedMediaType(msg) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                msg.clone(),
                json!({}),
            ),
            ApiError::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "Request body is too large".into(),
                json!({}),
            ),
            ApiError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error".into(),
                json!({}),
            ),
        }
    }
}

pub fn error_body(kind: &str, message: &str, details: Value) -> Value {
    json!({ "error": { "type": kind, "message": message, "details": details } })
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(err) = &self {
            tracing::error!(error = ?err, "internal error");
        }
        let (status, kind, message, details) = self.parts();
        (status, axum::Json(error_body(kind, &message, details))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        ApiError::Internal(err.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}

impl From<JsonRejection> for ApiError {
    /// Keeps axum's 415 (not JSON) and 413 (too large). Everything else,
    /// including axum's default 422 for well-formed JSON of the wrong shape, is
    /// a 400: we reserve 422 for idempotency-key reuse.
    fn from(rejection: JsonRejection) -> Self {
        match rejection.status() {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                ApiError::UnsupportedMediaType(rejection.body_text())
            }
            StatusCode::PAYLOAD_TOO_LARGE => ApiError::PayloadTooLarge,
            _ => ApiError::Validation(rejection.body_text()),
        }
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        ApiError::Validation(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        ApiError::Validation(rejection.body_text())
    }
}

/// `Json` whose rejections use our error envelope.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

/// `Path` whose rejections (e.g. a malformed UUID) use our error envelope.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);

/// `Query` whose rejections use our error envelope.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

pub type ApiResult<T> = Result<T, ApiError>;
