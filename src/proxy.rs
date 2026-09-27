use crate::{
    AppState, PER_KEY_CONCURRENCY, auth,
    error::{self, AppError, ErrorKind, Result},
    oauth,
    policy::{self, ToolMap},
    usage::{self, RequestGuard, Usage},
};
use axum::{
    Extension, Json,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::{io, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;

/// Largest request body a friend may send to the two POST routes.
pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Largest non-streaming upstream reply the router buffers.
pub(crate) const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Largest upstream error body read to learn its error type.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Largest server-sent event buffered from upstream before the stream counts as malformed.
pub(crate) const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Longest model identifier the router accepts and records.
pub(crate) const MAX_MODEL_ID_BYTES: usize = 200;
/// Longest upstream `request-id` the router records.
const MAX_UPSTREAM_REQUEST_ID_BYTES: usize = 200;
/// Stream chunks buffered between the upstream reader and the client.
const STREAM_CHANNEL_CAPACITY: usize = 4;
/// How long a client may stop reading a stream before the router abandons it.
pub(crate) const CLIENT_SEND_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound for a non-streaming `/v1/messages` call, including its reply body. It matches
/// the `x-stainless-timeout` the router announces upstream.
pub(crate) const MESSAGE_TIMEOUT: Duration = Duration::from_secs(600);
/// Upper bound for a `count_tokens` call, including its reply body.
pub(crate) const COUNT_TOKENS_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest a relayed stream may run. A stream still open after this ends with a
/// `timeout_error` event and is recorded as interrupted.
pub(crate) const MAX_STREAM_DURATION: Duration = Duration::from_secs(60 * 60);
/// How long the router tries to deliver the sanitized error event that ends a failed stream.
const STREAM_ERROR_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Seconds a client should wait before retrying when admission is saturated.
const BUSY_RETRY_AFTER_SECONDS: &str = "1";
/// Error message when Claude serves a model other than the one the router resolved.
const MODEL_MISMATCH: &str = "Claude answered with a different model than requested";
/// Error message when upstream ends a stream with an `error` event.
const STREAM_ERROR: &str = "Claude ended the stream with an error";
/// Client-requested betas reviewed for pass-through. The router's own compatibility betas,
/// [`oauth::BETA`], are always sent and may also be requested.
pub(crate) const CLIENT_BETAS: &[&str] = &[
    "prompt-caching-2024-07-31",
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
];

type Chunk = std::result::Result<Bytes, io::Error>;

/// Id of the router key that authenticated the request, set by [`authenticate`].
#[derive(Clone)]
pub(crate) struct KeyId(String);

/// Authenticates the router key from the headers alone, before any request body is read.
pub(crate) async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    match auth::api_key(request.headers(), &state).await {
        Ok(key) => {
            request.extensions_mut().insert(KeyId(key));
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

/// Parses a friend's JSON request body. The route's body limit has already been applied.
fn json_body(headers: &HeaderMap, body: &[u8]) -> Result<Value> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(AppError::bad("Expected an application/json request body"));
    }
    serde_json::from_slice(body).map_err(|_| AppError::bad("Invalid JSON body"))
}

pub(crate) async fn messages(
    State(state): State<Arc<AppState>>,
    Extension(KeyId(key)): Extension<KeyId>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let body = json_body(&headers, &body)?;
    forward(state, key, headers, body, false).await
}
pub(crate) async fn count_tokens(
    State(state): State<Arc<AppState>>,
    Extension(KeyId(key)): Extension<KeyId>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let body = json_body(&headers, &body)?;
    forward(state, key, headers, body, true).await
}

pub(crate) async fn models(
    State(state): State<Arc<AppState>>,
    Extension(KeyId(key)): Extension<KeyId>,
) -> Result<Json<Value>> {
    let rows = sqlx::query(
        "SELECT m.* FROM model m JOIN key_model_grant g ON m.id=g.model_id \
         WHERE g.key_id=? AND m.enabled=1 AND m.reviewed_at IS NOT NULL ORDER BY m.id",
    )
    .bind(key)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .iter()
        .filter(|row| !policy::blocked(row.get("id"), row.get("model_group")))
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "type": "model",
                "display_name": row.get::<String, _>("display_name"),
                "created_at": row.get::<Option<String>, _>("reviewed_at"),
            })
        })
        .collect();
    Ok(Json(json!({
        "data": data,
        "first_id": data.first().map(|v| &v["id"]),
        "last_id": data.last().map(|v| &v["id"]),
        "has_more": false,
    })))
}

pub(crate) async fn limited_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    read_body(response, limit)
        .await
        .map_err(|_| AppError::upstream())
}

/// Reads an upstream body up to `limit` bytes. A failure carries its log category.
async fn read_body(
    response: reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, &'static str> {
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| read_failure(&error))?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err("too_large");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Logs why a request to Claude failed, once per failed request. Only the router's request
/// id, the endpoint, a static category, and the upstream status are logged: never headers,
/// bodies, keys, tokens, or error text, which can contain URLs.
fn log_failure(id: &str, endpoint: &str, category: &'static str, status: Option<u16>) {
    tracing::warn!(
        request_id = id,
        endpoint,
        category,
        upstream_status = status,
        "upstream request failed"
    );
}

/// Category of a failure to send a request or receive response headers.
fn send_failure(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else {
        "request"
    }
}

/// Category of a failure while reading a response body.
fn read_failure(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else {
        "truncated"
    }
}

/// A request that passed policy review.
struct Admitted {
    body: Value,
    model: String,
    mapping: ToolMap,
    /// Outbound `anthropic-beta` value, when the client asked for reviewed betas.
    betas: Option<HeaderValue>,
}

/// Admission permits held until the upstream request, including any stream, ends.
struct Permits {
    _held: Vec<OwnedSemaphorePermit>,
}

/// Records the request's terminal outcome, then returns `error` to the client.
async fn fail<T>(
    guard: &mut RequestGuard,
    outcome: &str,
    status: u16,
    not_applicable: bool,
    error: AppError,
) -> Result<T> {
    guard.finish(outcome, Some(status), not_applicable).await?;
    Err(error)
}

async fn forward(
    state: Arc<AppState>,
    key: String,
    headers: HeaderMap,
    body: Value,
    counting: bool,
) -> Result<Response> {
    let endpoint = if counting {
        "/v1/messages/count_tokens"
    } else {
        "/v1/messages"
    };
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // Checked before `usage::start` so oversized identifiers are never stored.
    if requested.len() > MAX_MODEL_ID_BYTES {
        return Err(AppError::bad("Invalid model identifier"));
    }
    let id = usage::start(&state.db, &key, endpoint, &requested).await?;
    let mut guard = RequestGuard::new(state.db.clone(), id.clone());
    let admitted = match admit(&state, &key, &id, &headers, body, &requested, counting).await {
        Ok(v) => v,
        Err(e) => return fail(&mut guard, "denied", e.status().as_u16(), true, e).await,
    };
    let permits = match acquire(&state, &key, counting) {
        Ok(v) => v,
        Err(busy) => {
            guard.finish("denied", Some(429), true).await?;
            let mut response = busy.into_response();
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_static(BUSY_RETRY_AFTER_SECONDS),
            );
            return Ok(response);
        }
    };
    let mut access = match oauth::access(&state).await {
        Ok(v) => v,
        Err(e) => {
            log_failure(&id, endpoint, "credential", None);
            return fail(&mut guard, "upstream_error", e.status().as_u16(), true, e).await;
        }
    };
    let streaming = !counting && admitted.body.get("stream") == Some(&Value::Bool(true));
    let sent = send_upstream(&state, &id, endpoint, &access, &admitted, streaming).await;
    let mut response = match sent {
        Ok(r) => r,
        Err(e) => return fail(&mut guard, "upstream_error", e.status().as_u16(), false, e).await,
    };
    record_upstream(&state, &id, &response).await?;
    if response.status() == StatusCode::UNAUTHORIZED {
        (response, access) =
            match retry_unauthorized(&state, &id, endpoint, &access, &admitted, counting).await {
                Ok(v) => v,
                Err(e) => {
                    return fail(&mut guard, "upstream_error", e.status().as_u16(), true, e).await;
                }
            };
        record_upstream(&state, &id, &response).await?;
    }
    if !response.status().is_success() {
        log_failure(&id, endpoint, "status", Some(response.status().as_u16()));
        return handle_error_status(&state, &access, response, &mut guard).await;
    }
    if streaming {
        return open_stream(state, id, response, admitted, permits, guard).await;
    }
    finish_json(&state, &id, response, admitted, counting, &mut guard).await
}

/// Applies the request policy and the beta allowlist and records the resolved model.
async fn admit(
    state: &AppState,
    key: &str,
    id: &str,
    headers: &HeaderMap,
    mut body: Value,
    requested: &str,
    counting: bool,
) -> Result<Admitted> {
    policy::validate(&body, counting)?;
    let model = policy::resolve(state, key, requested).await?;
    let betas = reviewed_betas(headers)?;
    body["model"] = model.clone().into();
    let mapping = ToolMap::prepare(&mut body)?;
    sqlx::query("UPDATE request_usage SET resolved_model=? WHERE id=?")
        .bind(&model)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Admitted {
        body,
        model,
        mapping,
        betas,
    })
}

/// Takes admission permits without waiting. `/v1/messages` takes the key's permit first and
/// then a global one; token counts use their own pool.
fn acquire(state: &AppState, key: &str, counting: bool) -> Result<Permits> {
    let busy = |_| {
        AppError::new(
            ErrorKind::Request,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "The router is busy. Try again shortly",
        )
    };
    if counting {
        let permit = state.count_admission.clone().try_acquire_owned();
        return Ok(Permits {
            _held: vec![permit.map_err(busy)?],
        });
    }
    let key_permit = {
        let mut keys = state
            .key_admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Acquired under the lock so pruning cannot drop a semaphore that is about to be used.
        keys.retain(|_, semaphore| semaphore.available_permits() < PER_KEY_CONCURRENCY);
        keys.entry(key.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(PER_KEY_CONCURRENCY)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                AppError::new(
                    ErrorKind::Request,
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "This key has too many requests in progress. Try again shortly",
                )
            })?
    };
    let global = state.admission.clone().try_acquire_owned().map_err(busy)?;
    Ok(Permits {
        _held: vec![key_permit, global],
    })
}

/// Caller-supplied betas may enable routing features outside the reviewed schema, so every
/// token across all `anthropic-beta` lines must be reviewed. Returns the outbound value: the
/// router's own betas followed by the client's, each once. `None` keeps the default.
fn reviewed_betas(headers: &HeaderMap) -> Result<Option<HeaderValue>> {
    let mut requested = Vec::new();
    for line in headers.get_all("anthropic-beta") {
        let line = line
            .to_str()
            .map_err(|_| AppError::bad("Invalid beta header"))?;
        requested.extend(line.split(',').map(str::trim).filter(|b| !b.is_empty()));
    }
    if requested.is_empty() {
        return Ok(None);
    }
    let mut betas: Vec<&str> = oauth::BETA.split(',').collect();
    for beta in requested {
        if !betas.contains(&beta) {
            if !CLIENT_BETAS.contains(&beta) {
                return Err(AppError::bad("This beta feature has not been reviewed"));
            }
            betas.push(beta);
        }
    }
    HeaderValue::from_str(&betas.join(","))
        .map(Some)
        .map_err(|_| AppError::bad("Invalid beta header"))
}

async fn send_upstream(
    state: &AppState,
    id: &str,
    endpoint: &str,
    access: &oauth::Access,
    admitted: &Admitted,
    streaming: bool,
) -> Result<reqwest::Response> {
    let mut outbound = oauth::upstream_headers(access)
        .inspect_err(|_| log_failure(id, endpoint, "credential", None))?;
    if let Some(betas) = &admitted.betas {
        outbound.insert("anthropic-beta", betas.clone());
    }
    let mut request = state
        .client
        .post(format!("{}{endpoint}", state.upstream))
        .headers(outbound)
        .header(
            "accept",
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .json(&admitted.body);
    // Streams are bounded by `MAX_STREAM_DURATION` while they are relayed.
    if !streaming {
        request = request.timeout(if endpoint == "/v1/messages" {
            MESSAGE_TIMEOUT
        } else {
            COUNT_TOKENS_TIMEOUT
        });
    }
    request.send().await.map_err(|error| {
        log_failure(id, endpoint, send_failure(&error), None);
        AppError::upstream()
    })
}

/// Records the upstream status and request id before the body is read.
async fn record_upstream(state: &AppState, id: &str, response: &reqwest::Response) -> Result<()> {
    let upstream_id = response
        .headers()
        .get("request-id")
        .and_then(|h| h.to_str().ok())
        .filter(|s| s.len() <= MAX_UPSTREAM_REQUEST_ID_BYTES);
    sqlx::query("UPDATE request_usage SET upstream_request_id=?,http_status=? WHERE id=?")
        .bind(upstream_id)
        .bind(i64::from(response.status().as_u16()))
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Handles an upstream 401 by refreshing the owner's token once. Token counts are idempotent
/// and are retried with the new token. Messages are never replayed, so their client gets a
/// retryable 503 and sends the request again itself.
async fn retry_unauthorized(
    state: &AppState,
    id: &str,
    endpoint: &str,
    access: &oauth::Access,
    admitted: &Admitted,
    counting: bool,
) -> Result<(reqwest::Response, oauth::Access)> {
    let fresh = oauth::force_refresh(state, access.generation)
        .await
        .inspect_err(|_| log_failure(id, endpoint, "refresh", Some(401)))?;
    if !counting {
        log_failure(id, endpoint, "unauthorized", Some(401));
        return Err(AppError::new(
            ErrorKind::Upstream,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "The router renewed its Claude session. Retry the request",
        ));
    }
    let response = send_upstream(state, id, endpoint, &fresh, admitted, false).await?;
    Ok((response, fresh))
}

/// Returns the `error.type` of an upstream error body, read up to a small bound. The body's
/// text is never logged or returned.
pub(crate) async fn upstream_error_type(response: reqwest::Response) -> Option<&'static str> {
    let body = limited_body(response, MAX_ERROR_BODY_BYTES).await.ok()?;
    let value: Value = serde_json::from_slice(&body).ok()?;
    error::known_error_type(value.pointer("/error/type")?.as_str()?)
}

async fn handle_error_status(
    state: &AppState,
    access: &oauth::Access,
    response: reqwest::Response,
    guard: &mut RequestGuard,
) -> Result<Response> {
    let status = response.status();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let credential_rejected = match status {
        // Still rejected with a token refreshed moments ago: only a new login can help, and
        // marking it stops every later request from refreshing again.
        StatusCode::UNAUTHORIZED => true,
        // Most 403s concern one request, such as a model the account cannot use.
        StatusCode::FORBIDDEN => {
            upstream_error_type(response).await == Some("authentication_error")
        }
        _ => false,
    };
    if credential_rejected {
        oauth::mark_reauth(state, access.generation).await?;
        return fail(
            guard,
            "upstream_error",
            status.as_u16(),
            true,
            AppError::reauth(),
        )
        .await;
    }
    guard
        .finish("upstream_error", Some(status.as_u16()), true)
        .await?;
    let code = if status.is_redirection() {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
    let mut result = AppError::new(
        ErrorKind::Upstream,
        code,
        error::error_type(code),
        "Claude rejected the request. Check the model, connection, or retry later",
    )
    .into_response();
    if let Some(retry) = retry_after {
        result.headers_mut().insert(header::RETRY_AFTER, retry);
    }
    Ok(result)
}

/// Starts relaying an upstream event stream. A spawned task holds the admission permits until
/// the stream ends, so the response is returned as soon as upstream headers arrive.
async fn open_stream(
    state: Arc<AppState>,
    id: String,
    response: reqwest::Response,
    admitted: Admitted,
    permits: Permits,
    mut guard: RequestGuard,
) -> Result<Response> {
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        let status = response.status().as_u16();
        log_failure(&id, "/v1/messages", "bad_content_type", Some(status));
        return fail(
            &mut guard,
            "upstream_error",
            502,
            false,
            AppError::upstream(),
        )
        .await;
    }
    let (tx, rx) = mpsc::channel::<Chunk>(STREAM_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let _permits = permits;
        let Admitted { model, mapping, .. } = admitted;
        let result = stream_response(&state, &id, &model, response, mapping, &tx, &mut guard).await;
        let (kind, outcome, status, category) = match result {
            Ok(()) => return,
            // The client is not reading, so no error event could reach it.
            Err(StreamError::ClientStalled) => {
                log_failure(&id, "/v1/messages", "client_stalled", Some(200));
                let _ = guard.finish("interrupted", Some(200), false).await;
                return;
            }
            Err(StreamError::TooLong) => ("timeout_error", "interrupted", 200, "stream_limit"),
            Err(StreamError::Shutdown) => ("api_error", "interrupted", 200, "shutdown"),
            Err(StreamError::Upstream(kind, category)) => (kind, "upstream_error", 200, category),
            Err(StreamError::Internal) => ("api_error", "interrupted", 500, "internal"),
        };
        log_failure(&id, "/v1/messages", category, Some(200));
        let event = interrupted_event(kind);
        let _ = tokio::time::timeout(STREAM_ERROR_SEND_TIMEOUT, tx.send(Ok(event))).await;
        let _ = guard.finish(outcome, Some(status), false).await;
    });
    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response())
}

/// Reads, validates, and relays a non-streaming upstream reply.
async fn finish_json(
    state: &AppState,
    id: &str,
    response: reqwest::Response,
    admitted: Admitted,
    counting: bool,
    guard: &mut RequestGuard,
) -> Result<Response> {
    let status = response.status();
    let Admitted { model, mapping, .. } = admitted;
    let endpoint = if counting {
        "/v1/messages/count_tokens"
    } else {
        "/v1/messages"
    };
    let malformed = || log_failure(id, endpoint, "malformed", Some(status.as_u16()));
    let data = match read_body(response, MAX_RESPONSE_BODY_BYTES).await {
        Ok(v) => v,
        Err(category) => {
            log_failure(id, endpoint, category, Some(status.as_u16()));
            return fail(guard, "upstream_error", 502, false, AppError::upstream()).await;
        }
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&data) else {
        malformed();
        return fail(guard, "upstream_error", 502, false, AppError::upstream()).await;
    };
    if counting {
        if !value
            .get("input_tokens")
            .and_then(Value::as_i64)
            .is_some_and(|n| n >= 0)
        {
            malformed();
            return fail(guard, "upstream_error", 502, true, AppError::upstream()).await;
        }
    } else {
        let mut usage = Usage::default();
        usage.merge(value.get("usage"));
        usage.checkpoint(&state.db, id, true).await?;
        if value.get("type").and_then(Value::as_str) != Some("message")
            || !value.get("content").is_some_and(Value::is_array)
        {
            malformed();
            return fail(guard, "upstream_error", 502, false, AppError::upstream()).await;
        }
        if let Err(error) = verify_response_model(state, id, &model, value.get("model")).await {
            let category = if error.kind() == ErrorKind::ModelMismatch {
                "model_mismatch"
            } else {
                "internal"
            };
            log_failure(id, endpoint, category, Some(status.as_u16()));
            return fail(guard, "upstream_error", 502, false, error).await;
        }
        mapping.restore(&mut value);
    }
    guard
        .finish("completed", Some(status.as_u16()), counting)
        .await?;
    Ok(Json(value).into_response())
}

/// Sanitized event that ends a failed stream. Only the error type is kept.
fn interrupted_event(kind: &str) -> Bytes {
    let payload = json!({"type":"error","error":{
        "type": kind,
        "message": "The stream was interrupted. Usage may be incomplete.",
    }});
    Bytes::from(format!("event: error\ndata: {payload}\n\n"))
}

async fn verify_response_model(
    state: &AppState,
    id: &str,
    expected: &str,
    value: Option<&Value>,
) -> Result<()> {
    if let Some(actual) = value.and_then(Value::as_str) {
        sqlx::query("UPDATE request_usage SET response_model=? WHERE id=?")
            .bind(actual)
            .bind(id)
            .execute(&state.db)
            .await?;
        if actual != expected {
            return Err(AppError::new(
                ErrorKind::ModelMismatch,
                StatusCode::BAD_GATEWAY,
                "api_error",
                MODEL_MISMATCH,
            ));
        }
    }
    Ok(())
}

/// Why a relayed stream ended before `message_stop`.
enum StreamError {
    /// Upstream sent malformed, truncated, or error data. Carries the error type to relay and
    /// the category to log.
    Upstream(&'static str, &'static str),
    /// The client stopped reading for longer than the send timeout.
    ClientStalled,
    /// The stream ran longer than [`MAX_STREAM_DURATION`].
    TooLong,
    /// The server is stopping after its drain period.
    Shutdown,
    /// The router itself failed, for example while writing a usage checkpoint.
    Internal,
}

impl From<AppError> for StreamError {
    fn from(error: AppError) -> Self {
        // Database failures surface as internal errors; everything else came from upstream.
        let category = match error.kind() {
            ErrorKind::Internal => return Self::Internal,
            ErrorKind::ModelMismatch => "model_mismatch",
            ErrorKind::StreamError => "stream_error",
            ErrorKind::Request | ErrorKind::Upstream | ErrorKind::Reauth => "malformed",
        };
        Self::Upstream(error.error_type(), category)
    }
}

async fn stream_response(
    state: &AppState,
    id: &str,
    model: &str,
    response: reqwest::Response,
    mapping: ToolMap,
    tx: &mpsc::Sender<Chunk>,
    guard: &mut RequestGuard,
) -> std::result::Result<(), StreamError> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut progress = StreamProgress::default();
    let deadline = tokio::time::sleep(state.max_stream_duration);
    tokio::pin!(deadline);
    let mut phase = state.phase.subscribe();
    loop {
        let chunk = tokio::select! {
            _ = tx.closed() => { guard.finish("interrupted",Some(200),false).await?; return Ok(()); }
            () = &mut deadline => return Err(StreamError::TooLong),
            true = async { phase.wait_for(|p| *p == crate::Phase::Stopping).await.is_ok() } => {
                // Keep the counts seen so far before the stream is recorded as interrupted.
                progress.usage.checkpoint(&state.db, id, false).await?;
                return Err(StreamError::Shutdown);
            }
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            return Err(StreamError::Upstream("api_error", "truncated"));
        };
        let chunk =
            chunk.map_err(|error| StreamError::Upstream("api_error", read_failure(&error)))?;
        decoder.push(&chunk)?;
        while let Some(event) = decoder.next_event()? {
            let (kind, bytes) = progress
                .observe(state, id, model, event, &mapping, guard)
                .await?;
            if tokio::time::timeout(state.client_send_timeout, tx.send(Ok(bytes)))
                .await
                .is_err()
            {
                return Err(StreamError::ClientStalled);
            }
            if tx.is_closed() {
                guard.finish("interrupted", Some(200), false).await?;
                return Ok(());
            }
            if kind == "message_stop" {
                return Ok(());
            }
        }
    }
}

/// Protocol and accounting state of one upstream event stream.
#[derive(Default)]
struct StreamProgress {
    usage: Usage,
    started: bool,
    output_updated: bool,
}

impl StreamProgress {
    /// Validates one upstream event, records its usage, and returns its kind and the bytes to
    /// relay to the client.
    async fn observe(
        &mut self,
        state: &AppState,
        id: &str,
        model: &str,
        event: Event,
        mapping: &ToolMap,
        guard: &mut RequestGuard,
    ) -> Result<(String, Bytes)> {
        let mut payload = event
            .data
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let kind = payload
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            .unwrap_or(&event.kind)
            .to_owned();
        if event.kind != "message" && event.kind != kind {
            return Err(AppError::upstream());
        }
        if [
            "message_start",
            "message_delta",
            "message_stop",
            "content_block_start",
            "error",
        ]
        .contains(&kind.as_str())
            && payload.is_none()
        {
            return Err(AppError::upstream());
        }
        if let Some(value) = payload.as_mut() {
            self.apply(state, id, model, &kind, value, guard).await?;
            mapping.restore(value);
        }
        let bytes = match payload {
            Some(value) => Bytes::from(format!("event: {}\ndata: {}\n\n", event.kind, value)),
            None => Bytes::from(event.raw),
        };
        Ok((kind, bytes))
    }

    async fn apply(
        &mut self,
        state: &AppState,
        id: &str,
        model: &str,
        kind: &str,
        value: &Value,
        guard: &mut RequestGuard,
    ) -> Result<()> {
        match kind {
            "message_start" => {
                if self.started {
                    return Err(AppError::upstream());
                }
                self.started = true;
                let message = value.get("message");
                self.usage.merge(message.and_then(|v| v.get("usage")));
                self.usage.checkpoint(&state.db, id, false).await?;
                verify_response_model(state, id, model, message.and_then(|v| v.get("model")))
                    .await?;
            }
            "message_delta" => {
                if !self.started {
                    return Err(AppError::upstream());
                }
                self.output_updated |= value.pointer("/usage/output_tokens").is_some();
                self.usage.merge(value.get("usage"));
                self.usage.checkpoint(&state.db, id, false).await?;
                let delta_model = value.get("delta").and_then(|v| v.get("model"));
                verify_response_model(state, id, model, delta_model).await?;
            }
            "error" => {
                // Relay a documented error type, such as a retryable overload, never the text.
                let kind = value
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .and_then(error::known_error_type)
                    .unwrap_or("api_error");
                return Err(AppError::new(
                    ErrorKind::StreamError,
                    StatusCode::BAD_GATEWAY,
                    kind,
                    STREAM_ERROR,
                ));
            }
            "message_stop" => {
                if !self.started {
                    return Err(AppError::upstream());
                }
                self.usage
                    .checkpoint(&state.db, id, self.output_updated)
                    .await?;
                guard.finish("completed", Some(200), false).await?;
            }
            "content_block_start"
                if value.pointer("/content_block/type").and_then(Value::as_str)
                    == Some("fallback") =>
            {
                return Err(AppError::upstream());
            }
            _ => (),
        }
        Ok(())
    }
}

/// Incremental server-sent events decoder. Lines may end in `\n`, `\r\n`, or a bare `\r`, and a
/// blank line ends an event. Each buffered byte is examined for a line ending once.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    /// First buffered byte not yet examined for a line ending.
    scan_from: usize,
    /// Start of the line that contains `scan_from`.
    line_start: usize,
    /// The last line ended in `\r`, so a `\n` at `scan_from` belongs to that line ending.
    after_cr: bool,
    #[cfg(test)]
    pub(crate) scanned: usize,
}
pub(crate) struct Event {
    pub(crate) kind: String,
    pub(crate) data: Option<String>,
    pub(crate) raw: String,
}
impl SseDecoder {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<()> {
        if self.buffer.len() + chunk.len() > MAX_SSE_EVENT_BYTES {
            return Err(AppError::upstream());
        }
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }
    pub(crate) fn next_event(&mut self) -> Result<Option<Event>> {
        loop {
            if self.after_cr {
                match self.buffer.get(self.scan_from) {
                    None => return Ok(None),
                    Some(b'\n') if self.scan_from == 0 => {
                        self.buffer.remove(0);
                    }
                    Some(b'\n') => {
                        self.scan_from += 1;
                        self.line_start = self.scan_from;
                    }
                    Some(_) => (),
                }
                self.after_cr = false;
            }
            let Some(offset) = self.buffer[self.scan_from..]
                .iter()
                .position(|b| matches!(b, b'\n' | b'\r'))
            else {
                #[cfg(test)]
                {
                    self.scanned += self.buffer.len() - self.scan_from;
                }
                self.scan_from = self.buffer.len();
                return Ok(None);
            };
            let index = self.scan_from + offset;
            #[cfg(test)]
            {
                self.scanned += offset + 1;
            }
            let ending = match (self.buffer[index], self.buffer.get(index + 1)) {
                (b'\r', Some(b'\n')) => 2,
                (b'\r', None) => {
                    // The matching `\n`, if any, has not arrived yet.
                    self.after_cr = true;
                    1
                }
                _ => 1,
            };
            let blank = index == self.line_start;
            self.scan_from = index + ending;
            self.line_start = self.scan_from;
            if !blank {
                continue;
            }
            let end = self.scan_from;
            self.scan_from = 0;
            self.line_start = 0;
            let raw = String::from_utf8(self.buffer.drain(..end).collect())
                .map_err(|_| AppError::upstream())?;
            if end == ending {
                // A blank line with no fields before it dispatches nothing.
                continue;
            }
            return Ok(Some(Event::parse(raw)));
        }
    }
}
impl Event {
    fn parse(raw: String) -> Self {
        let mut kind = String::new();
        let mut data = Vec::new();
        for line in raw.split(['\r', '\n']) {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "event" => value.clone_into(&mut kind),
                "data" => data.push(value),
                _ => (),
            }
        }
        if kind.is_empty() {
            kind.push_str("message");
        }
        let data = (!data.is_empty()).then(|| data.join("\n"));
        Self { kind, data, raw }
    }
}
