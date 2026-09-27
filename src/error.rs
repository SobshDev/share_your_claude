use axum::{
    Json,
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub(crate) type Result<T> = std::result::Result<T, AppError>;

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
pub(crate) fn error_type(status: StatusCode) -> &'static str {
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
pub(crate) fn known_error_type(kind: &str) -> Option<&'static str> {
    ERROR_TYPES.iter().copied().find(|known| *known == kind)
}

/// Gives error responses that axum produces itself (unmatched methods, body limits, extractor
/// rejections) Anthropic's error envelope. Their plain-text detail is replaced, never relayed.
pub(crate) async fn envelope_rejections(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|v| v.as_bytes().starts_with(b"text/plain"));
    if !plain || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let message = match status.as_u16() {
        404 => "Not found",
        405 => "Method not allowed",
        413 => "Request body is too large",
        415 => "Expected an application/json request body",
        400 | 422 => rejection_message(body).await,
        _ => "The router could not complete this operation",
    };
    parts.headers.remove(header::CONTENT_TYPE);
    parts.headers.remove(header::CONTENT_LENGTH);
    let (envelope, body) = AppError::new(ErrorKind::Request, status, error_type(status), message)
        .into_response()
        .into_parts();
    parts.headers.extend(envelope.headers);
    Response::from_parts(parts, body)
}

/// A client-facing message for an axum 400 or 422 rejection. The body is axum's own text,
/// read only to tell query, JSON, and other rejections apart; it is never relayed.
async fn rejection_message(body: axum::body::Body) -> &'static str {
    let text = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
    if text.starts_with(b"Failed to deserialize query string") {
        "Invalid query parameters"
    } else if text.starts_with(b"Failed to parse the request body as JSON")
        || text.starts_with(b"Failed to deserialize the JSON body")
    {
        "Invalid JSON request body"
    } else {
        "Invalid request"
    }
}

/// What failed. Callers branch on this instead of on the status code or message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// The request was refused: invalid, unauthenticated, forbidden, over a limit, or sent
    /// while the router shuts down.
    Request,
    /// The router itself failed, for example on a database error.
    Internal,
    /// Claude was unreachable, refused the request, or sent a reply the router cannot use.
    Upstream,
    /// Claude answered with a different model than the one requested.
    ModelMismatch,
    /// Claude ended a stream with an error event.
    StreamError,
    /// The stored Claude credential is unusable until the owner reconnects.
    Reauth,
}

/// An error response in Anthropic's envelope: `{"type":"error","error":{"type","message"}}`.
#[derive(Debug)]
pub(crate) struct AppError {
    kind: ErrorKind,
    status: StatusCode,
    /// Anthropic error type sent as `error.type`.
    error_type: &'static str,
    message: &'static str,
}

impl AppError {
    pub(crate) fn new(
        kind: ErrorKind,
        status: StatusCode,
        error_type: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            kind,
            status,
            error_type,
            message,
        }
    }
    pub(crate) fn bad(message: &'static str) -> Self {
        Self::new(
            ErrorKind::Request,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message,
        )
    }
    pub(crate) fn unauthorized() -> Self {
        Self::new(
            ErrorKind::Request,
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Authentication required",
        )
    }
    pub(crate) fn forbidden(message: &'static str) -> Self {
        Self::new(
            ErrorKind::Request,
            StatusCode::FORBIDDEN,
            "permission_error",
            message,
        )
    }
    pub(crate) fn internal() -> Self {
        Self::new(
            ErrorKind::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "The router could not complete this operation",
        )
    }
    pub(crate) fn upstream() -> Self {
        Self::new(
            ErrorKind::Upstream,
            StatusCode::BAD_GATEWAY,
            "api_error",
            "Claude could not complete this request; try again later",
        )
    }
    pub(crate) fn reauth() -> Self {
        Self::new(
            ErrorKind::Reauth,
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_error",
            "The owner must reconnect Claude in the dashboard",
        )
    }
    pub(crate) fn not_found(message: &'static str) -> Self {
        Self::new(
            ErrorKind::Request,
            StatusCode::NOT_FOUND,
            "not_found_error",
            message,
        )
    }
    pub(crate) fn kind(&self) -> ErrorKind {
        self.kind
    }
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }
    pub(crate) fn error_type(&self) -> &'static str {
        self.error_type
    }
    #[cfg(test)]
    pub(crate) fn message(&self) -> &'static str {
        self.message
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"type":"error","error":{"type":self.error_type,"message":self.message}})),
        )
            .into_response()
    }
}
impl From<sqlx::Error> for AppError {
    #[track_caller]
    fn from(error: sqlx::Error) -> Self {
        log_database_error(&error);
        Self::internal()
    }
}

/// Logs a sanitized category of a database failure and the source location of the caller
/// (the `?` that converted it). Database messages can contain values, so only the error kind,
/// SQLite's result code, and the I/O error kind are logged.
#[track_caller]
pub(crate) fn log_database_error(error: &sqlx::Error) {
    use sqlx::error::ErrorKind;
    let at = std::panic::Location::caller();
    let (kind, code) = match error {
        sqlx::Error::Database(error) => {
            let kind = match error.kind() {
                ErrorKind::UniqueViolation => "unique_violation",
                ErrorKind::ForeignKeyViolation => "foreign_key_violation",
                ErrorKind::NotNullViolation => "not_null_violation",
                ErrorKind::CheckViolation => "check_violation",
                _ => "database",
            };
            (kind, error.code().map(std::borrow::Cow::into_owned))
        }
        sqlx::Error::PoolTimedOut => ("pool_timed_out", None),
        sqlx::Error::PoolClosed => ("pool_closed", None),
        sqlx::Error::Io(error) => ("io", Some(format!("{:?}", error.kind()))),
        sqlx::Error::RowNotFound => ("row_not_found", None),
        sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnIndexOutOfBounds { .. }
        | sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::Decode(_)
        | sqlx::Error::TypeNotFound { .. } => ("decode", None),
        sqlx::Error::Protocol(_) => ("protocol", None),
        sqlx::Error::WorkerCrashed => ("worker_crashed", None),
        sqlx::Error::Migrate(_) => ("migrate", None),
        _ => ("other", None),
    };
    tracing::error!(kind, code = code.as_deref(), at = %at, "database operation failed");
}
