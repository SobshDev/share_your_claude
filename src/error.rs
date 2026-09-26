use axum::{
    Json,
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, AppError>;

/// Every error type Anthropic documents. Only these strings are relayed from upstream.
const ERROR_TYPES: &[&str] = &[
    "invalid_request_error",
    "authentication_error",
    "billing_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "api_error",
    "timeout_error",
    "overloaded_error",
];

/// Anthropic's error type for an HTTP status.
pub fn error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        504 => "timeout_error",
        529 => "overloaded_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    }
}

/// Returns the router's copy of a documented error type, or `None` for anything else.
pub fn known_error_type(kind: &str) -> Option<&'static str> {
    ERROR_TYPES.iter().copied().find(|known| *known == kind)
}

/// Gives error responses that axum produces itself (unmatched methods, body limits, extractor
/// rejections) Anthropic's error envelope. Their plain-text detail is replaced, never relayed.
pub async fn envelope_rejections(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|v| v.as_bytes().starts_with(b"text/plain"));
    if !plain || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let message = match status.as_u16() {
        404 => "Not found",
        405 => "Method not allowed",
        413 => "Request body is too large",
        415 => "Expected an application/json request body",
        400 | 422 => "Invalid JSON request body",
        _ => "The router could not complete this operation",
    };
    let (mut parts, _) = response.into_parts();
    parts.headers.remove(header::CONTENT_TYPE);
    parts.headers.remove(header::CONTENT_LENGTH);
    let (envelope, body) = AppError(status, error_type(status), message)
        .into_response()
        .into_parts();
    parts.headers.extend(envelope.headers);
    Response::from_parts(parts, body)
}

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
    pub fn not_found(message: &'static str) -> Self {
        Self(StatusCode::NOT_FOUND, "not_found_error", message)
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
