use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub struct AppError(pub StatusCode, pub &'static str, pub &'static str);

impl AppError {
    pub fn bad(message: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }
    pub fn unauthorized() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Authentication required",
        )
    }
    pub fn forbidden(message: &'static str) -> Self {
        Self(StatusCode::FORBIDDEN, "permission_error", message)
    }
    pub fn internal() -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "The router could not complete this operation",
        )
    }
    pub fn upstream() -> Self {
        Self(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "Claude could not complete this request; try again later",
        )
    }
    pub fn reauth() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_error",
            "The owner must reconnect Claude in the dashboard",
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"type":"error","error":{"type":self.1,"message":self.2}})),
        )
            .into_response()
    }
}
impl From<sqlx::Error> for AppError {
    fn from(_: sqlx::Error) -> Self {
        // Database errors can contain values. Never log them or return them verbatim.
        tracing::error!("database operation failed");
        Self::internal()
    }
}
