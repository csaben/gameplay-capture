use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// JSON error `{"error": code, "error_description": msg}` (OAuth style).
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }
    pub fn bad_request(code: &'static str, m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, m)
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid_token", m)
    }
    pub fn forbidden(code: &'static str, m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", m)
    }
    pub fn conflict(code: &'static str, m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, m)
    }
    pub fn internal(m: impl std::fmt::Display) -> Self {
        tracing::error!(error = %m, "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code, "error_description": self.message }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::internal(format!("db: {e}"))
    }
}

impl From<object_store::Error> for ApiError {
    fn from(e: object_store::Error) -> Self {
        ApiError::new(StatusCode::BAD_GATEWAY, "storage_error", format!("{e}"))
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
