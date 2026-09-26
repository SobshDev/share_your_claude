use crate::{
    AppState, auth,
    error::{AppError, Result},
    oauth,
    policy::{self, ToolMap},
    usage::{self, RequestGuard, Usage},
};
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::{io, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_stream::wrappers::ReceiverStream;

/// Largest request body a friend may send to the two POST routes.
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Largest non-streaming upstream reply the router buffers.
const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Largest server-sent event buffered from upstream before the stream counts as malformed.
pub const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Longest model identifier the router accepts and records.
pub const MAX_MODEL_ID_BYTES: usize = 200;
/// Longest upstream `request-id` the router records.
const MAX_UPSTREAM_REQUEST_ID_BYTES: usize = 200;
/// Stream chunks buffered between the upstream reader and the client.
const STREAM_CHANNEL_CAPACITY: usize = 4;
/// How long a client may stop reading a stream before the router abandons it.
const CLIENT_SEND_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the router tries to deliver the sanitized error event that ends a failed stream.
const STREAM_ERROR_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Sanitized event that replaces whatever went wrong in a stream.
const STREAM_INTERRUPTED: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"The stream was interrupted. Usage may be incomplete.\"}}\n\n";

type Chunk = std::result::Result<Bytes, io::Error>;

pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    forward(state, headers, body, false).await
}
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    forward(state, headers, body, true).await
}

pub async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Value>> {
    let key = auth::api_key(&headers, &state).await?;
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

pub async fn limited_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| AppError::upstream())?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(AppError::upstream());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A request that passed policy review and holds an admission permit.
struct Admitted {
    body: Value,
    model: String,
    mapping: ToolMap,
    /// Outbound `anthropic-beta` value, when the client asked for reviewed betas.
    betas: Option<HeaderValue>,
    permit: OwnedSemaphorePermit,
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
    headers: HeaderMap,
    body: Value,
    counting: bool,
) -> Result<Response> {
    let key = auth::api_key(&headers, &state).await?;
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
        Err(e) => return fail(&mut guard, "denied", e.0.as_u16(), true, e).await,
    };
    let access = match oauth::access(&state).await {
        Ok(v) => v,
        Err(e) => return fail(&mut guard, "upstream_error", e.0.as_u16(), true, e).await,
    };
    let streaming = !counting && admitted.body.get("stream") == Some(&Value::Bool(true));
    let response = match send_upstream(&state, endpoint, &access, &admitted, streaming).await {
        Ok(r) => r,
        Err(e) => return fail(&mut guard, "upstream_error", e.0.as_u16(), false, e).await,
    };
    record_upstream(&state, &id, &response).await?;
    if !response.status().is_success() {
        return handle_error_status(&state, &access, response, &mut guard).await;
    }
    if streaming {
        return open_stream(state, id, response, admitted, guard).await;
    }
    finish_json(&state, &id, response, admitted, counting, &mut guard).await
}

/// Applies the request policy and the beta allowlist, records the resolved model, and takes an
/// admission permit.
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
    let permit = state.admission.clone().try_acquire_owned().map_err(|_| {
        AppError(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "The router is busy. Try again shortly",
        )
    })?;
    Ok(Admitted {
        body,
        model,
        mapping,
        betas,
        permit,
    })
}

/// Caller-supplied betas may enable routing features outside the reviewed schema.
/// Clients can omit these headers; only the router's known compatibility betas go upstream.
fn reviewed_betas(headers: &HeaderMap) -> Result<Option<HeaderValue>> {
    let Some(betas) = headers.get("anthropic-beta") else {
        return Ok(None);
    };
    let allowed = [
        "claude-code-20250219",
        "oauth-2025-04-20",
        "prompt-caching-2024-07-31",
        "interleaved-thinking-2025-05-14",
        "fine-grained-tool-streaming-2025-05-14",
    ];
    let value = betas
        .to_str()
        .map_err(|_| AppError::bad("Invalid beta header"))?;
    if value.split(',').any(|b| !allowed.contains(&b.trim())) {
        return Err(AppError::bad("This beta feature has not been reviewed"));
    }
    HeaderValue::from_str(&format!("{},{value}", oauth::BETA))
        .map(Some)
        .map_err(|_| AppError::bad("Invalid beta header"))
}

async fn send_upstream(
    state: &AppState,
    endpoint: &str,
    access: &oauth::Access,
    admitted: &Admitted,
    streaming: bool,
) -> Result<reqwest::Response> {
    let mut outbound = oauth::upstream_headers(access)?;
    if let Some(betas) = &admitted.betas {
        outbound.insert("anthropic-beta", betas.clone());
    }
    state
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
        .json(&admitted.body)
        .send()
        .await
        .map_err(|_| AppError::upstream())
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

async fn handle_error_status(
    state: &AppState,
    access: &oauth::Access,
    response: reqwest::Response,
    guard: &mut RequestGuard,
) -> Result<Response> {
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        oauth::mark_reauth(state, access.generation).await?;
    }
    guard
        .finish("upstream_error", Some(status.as_u16()), true)
        .await?;
    let code = if status.is_redirection() {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
    let mut result = AppError(
        code,
        if code == StatusCode::TOO_MANY_REQUESTS {
            "rate_limit_error"
        } else {
            "api_error"
        },
        "Claude rejected the request. Check the model, connection, or retry later",
    )
    .into_response();
    if let Some(retry) = response.headers().get("retry-after") {
        result.headers_mut().insert("retry-after", retry.clone());
    }
    Ok(result)
}

/// Starts relaying an upstream event stream. A spawned task holds the admission permit until
/// the stream ends, so the response is returned as soon as upstream headers arrive.
async fn open_stream(
    state: Arc<AppState>,
    id: String,
    response: reqwest::Response,
    admitted: Admitted,
    mut guard: RequestGuard,
) -> Result<Response> {
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
    {
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
        let Admitted {
            model,
            mapping,
            permit: _permit,
            ..
        } = admitted;
        let result = stream_response(&state, &id, &model, response, mapping, &tx, &mut guard).await;
        if result.is_err() {
            let error = Bytes::from_static(STREAM_INTERRUPTED);
            let _ = tokio::time::timeout(STREAM_ERROR_SEND_TIMEOUT, tx.send(Ok(error))).await;
            let _ = guard.finish("upstream_error", Some(200), false).await;
        }
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
    let Admitted {
        model,
        mapping,
        permit: _permit,
        ..
    } = admitted;
    let data = match limited_body(response, MAX_RESPONSE_BODY_BYTES).await {
        Ok(v) => v,
        Err(e) => return fail(guard, "upstream_error", 502, false, e).await,
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&data) else {
        return fail(guard, "upstream_error", 502, false, AppError::upstream()).await;
    };
    if counting {
        if !value
            .get("input_tokens")
            .and_then(Value::as_i64)
            .is_some_and(|n| n >= 0)
        {
            return fail(guard, "upstream_error", 502, true, AppError::upstream()).await;
        }
    } else {
        let mut usage = Usage::default();
        usage.merge(value.get("usage"));
        usage.checkpoint(&state.db, id, true).await?;
        if value.get("type").and_then(Value::as_str) != Some("message")
            || !value.get("content").is_some_and(Value::is_array)
        {
            return fail(guard, "upstream_error", 502, false, AppError::upstream()).await;
        }
        if let Err(error) = verify_response_model(state, id, &model, value.get("model")).await {
            return fail(guard, "upstream_error", 502, false, error).await;
        }
        mapping.restore(&mut value);
    }
    guard
        .finish("completed", Some(status.as_u16()), counting)
        .await?;
    Ok(Json(value).into_response())
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
            return Err(AppError::upstream());
        }
    }
    Ok(())
}

async fn stream_response(
    state: &AppState,
    id: &str,
    model: &str,
    response: reqwest::Response,
    mapping: ToolMap,
    tx: &mpsc::Sender<Chunk>,
    guard: &mut RequestGuard,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut progress = StreamProgress::default();
    loop {
        let chunk = tokio::select! {
            _ = tx.closed() => { guard.finish("interrupted",Some(200),false).await?; return Ok(()); }
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            return Err(AppError::upstream());
        };
        decoder.push(&chunk.map_err(|_| AppError::upstream())?)?;
        while let Some(event) = decoder.next_event()? {
            let (kind, bytes) = progress
                .observe(state, id, model, event, &mapping, guard)
                .await?;
            if tokio::time::timeout(CLIENT_SEND_TIMEOUT, tx.send(Ok(bytes)))
                .await
                .is_err()
            {
                return Err(AppError::upstream());
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
            "error" => return Err(AppError::upstream()),
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
pub struct SseDecoder {
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
pub struct Event {
    pub(crate) kind: String,
    pub(crate) data: Option<String>,
    pub(crate) raw: String,
}
impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        if self.buffer.len() + chunk.len() > MAX_SSE_EVENT_BYTES {
            return Err(AppError::upstream());
        }
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }
    pub fn next_event(&mut self) -> Result<Option<Event>> {
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
