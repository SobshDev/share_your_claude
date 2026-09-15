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
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::{io, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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
    let rows = sqlx::query("SELECT m.* FROM model m JOIN key_model_grant g ON m.id=g.model_id WHERE g.key_id=? AND m.enabled=1 AND m.reviewed_at IS NOT NULL ORDER BY m.id")
        .bind(key).fetch_all(&state.db).await?;
    let data: Vec<Value> = rows.iter().filter(|r| !policy::blocked(r.get("id"),r.get("model_group")))
        .map(|r|json!({"id":r.get::<String,_>("id"),"type":"model","display_name":r.get::<String,_>("display_name"),"created_at":r.get::<Option<String>,_>("reviewed_at")})).collect();
    Ok(Json(
        json!({"first_id":data.first().map(|v|&v["id"]),"last_id":data.last().map(|v|&v["id"]),"has_more":false,"data":data}),
    ))
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

async fn forward(
    state: Arc<AppState>,
    headers: HeaderMap,
    mut body: Value,
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
    if requested.len() > 200 {
        return Err(AppError::bad("Invalid model identifier"));
    }
    let id = usage::start(&state.db, &key, endpoint, &requested).await?;
    let mut guard = RequestGuard::new(state.db.clone(), id.clone());
    let permitted = async {
        policy::validate(&body, counting)?;
        let model = policy::resolve(&state, &key, &requested).await?;
        // Caller-supplied betas may enable routing features outside the reviewed schema.
        // Clients can omit these headers; only the router's known compatibility betas go upstream.
        if let Some(betas) = headers.get("anthropic-beta") {
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
        }
        body["model"] = model.clone().into();
        let mapping = ToolMap::prepare(&mut body)?;
        Ok::<_, AppError>((model, mapping))
    }
    .await;
    let (model, mapping) = match permitted {
        Ok(v) => v,
        Err(e) => {
            guard.finish("denied", Some(e.0.as_u16()), true).await?;
            return Err(e);
        }
    };
    sqlx::query("UPDATE request_usage SET resolved_model=? WHERE id=?")
        .bind(&model)
        .bind(&id)
        .execute(&state.db)
        .await?;
    let permit = match state.admission.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            guard.finish("denied", Some(429), true).await?;
            return Err(AppError(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "The router is busy. Try again shortly",
            ));
        }
    };
    let access = match oauth::access(&state).await {
        Ok(v) => v,
        Err(e) => {
            guard
                .finish("upstream_error", Some(e.0.as_u16()), true)
                .await?;
            return Err(e);
        }
    };
    let streaming = !counting && body.get("stream") == Some(&Value::Bool(true));
    let mut outbound = oauth::upstream_headers(&access)?;
    if let Some(betas) = headers.get("anthropic-beta") {
        let betas = format!("{},{}", oauth::BETA, betas.to_str().unwrap_or_default());
        outbound.insert(
            "anthropic-beta",
            betas
                .parse()
                .map_err(|_| AppError::bad("Invalid beta header"))?,
        );
    }
    let response = match state
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
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            guard.finish("upstream_error", Some(502), false).await?;
            return Err(AppError::upstream());
        }
    };
    let status = response.status();
    let upstream_id = response
        .headers()
        .get("request-id")
        .and_then(|h| h.to_str().ok())
        .filter(|s| s.len() <= 200)
        .map(str::to_owned);
    sqlx::query("UPDATE request_usage SET upstream_request_id=?,http_status=? WHERE id=?")
        .bind(upstream_id)
        .bind(i64::from(status.as_u16()))
        .bind(&id)
        .execute(&state.db)
        .await?;
    if !status.is_success() {
        if matches!(status.as_u16(), 401 | 403) {
            oauth::mark_reauth(&state, access.generation).await?;
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
        return Ok(result);
    }
    if streaming {
        if !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            guard.finish("upstream_error", Some(502), false).await?;
            return Err(AppError::upstream());
        }
        let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, io::Error>>(4);
        tokio::spawn(async move {
            let _permit = permit;
            let result =
                stream_response(&state, &id, &model, response, mapping, &tx, &mut guard).await;
            if result.is_err() {
                let error = Bytes::from_static(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"The stream was interrupted. Usage may be incomplete.\"}}\n\n");
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), tx.send(Ok(error)))
                    .await;
                let _ = guard.finish("upstream_error", Some(200), false).await;
            }
        });
        return Ok((
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::HeaderName::from_static("x-accel-buffering"), "no"),
            ],
            Body::from_stream(ReceiverStream::new(rx)),
        )
            .into_response());
    }
    let data = match limited_body(response, 32 * 1024 * 1024).await {
        Ok(v) => v,
        Err(e) => {
            guard.finish("upstream_error", Some(502), false).await?;
            return Err(e);
        }
    };
    let mut value: Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(_) => {
            guard.finish("upstream_error", Some(502), false).await?;
            return Err(AppError::upstream());
        }
    };
    if counting
        && !value
            .get("input_tokens")
            .and_then(Value::as_i64)
            .is_some_and(|n| n >= 0)
    {
        guard.finish("upstream_error", Some(502), true).await?;
        return Err(AppError::upstream());
    }
    if !counting {
        let mut usage = Usage::default();
        usage.merge(value.get("usage"));
        usage.checkpoint(&state.db, &id, true).await?;
        if value.get("type").and_then(Value::as_str) != Some("message")
            || !value.get("content").is_some_and(Value::is_array)
        {
            guard.finish("upstream_error", Some(502), false).await?;
            return Err(AppError::upstream());
        }
        if let Err(error) = verify_response_model(&state, &id, &model, value.get("model")).await {
            guard.finish("upstream_error", Some(502), false).await?;
            return Err(error);
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
    tx: &mpsc::Sender<std::result::Result<Bytes, io::Error>>,
    guard: &mut RequestGuard,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut usage = Usage::default();
    let mut started = false;
    let mut output_updated = false;
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
                match kind.as_str() {
                    "message_start" => {
                        if started {
                            return Err(AppError::upstream());
                        }
                        started = true;
                        usage.merge(value.get("message").and_then(|v| v.get("usage")));
                        usage.checkpoint(&state.db, id, false).await?;
                        verify_response_model(
                            state,
                            id,
                            model,
                            value.get("message").and_then(|v| v.get("model")),
                        )
                        .await?;
                    }
                    "message_delta" => {
                        if !started {
                            return Err(AppError::upstream());
                        }
                        output_updated |= value.pointer("/usage/output_tokens").is_some();
                        usage.merge(value.get("usage"));
                        usage.checkpoint(&state.db, id, false).await?;
                        verify_response_model(
                            state,
                            id,
                            model,
                            value.get("delta").and_then(|v| v.get("model")),
                        )
                        .await?;
                    }
                    "error" => return Err(AppError::upstream()),
                    "message_stop" => {
                        if !started {
                            return Err(AppError::upstream());
                        }
                        usage.checkpoint(&state.db, id, output_updated).await?;
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
                mapping.restore(value);
            }
            let bytes = match payload {
                Some(value) => Bytes::from(format!("event: {}\ndata: {}\n\n", event.kind, value)),
                None => Bytes::from(event.raw),
            };
            if tokio::time::timeout(std::time::Duration::from_secs(120), tx.send(Ok(bytes)))
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

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}
pub struct Event {
    kind: String,
    data: Option<String>,
    raw: String,
}
impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        if self.buffer.len() + chunk.len() > 4 * 1024 * 1024 {
            return Err(AppError::upstream());
        }
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        let boundary = self
            .buffer
            .windows(2)
            .position(|b| b == b"\n\n")
            .map(|i| (i, 2));
        let crlf = self
            .buffer
            .windows(4)
            .position(|b| b == b"\r\n\r\n")
            .map(|i| (i, 4));
        let end = match (boundary, crlf) {
            (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
            (a, b) => a.or(b),
        };
        let Some((index, len)) = end else {
            return Ok(None);
        };
        let raw = String::from_utf8(self.buffer.drain(..index + len).collect())
            .map_err(|_| AppError::upstream())?;
        let mut kind = "message".to_owned();
        let mut data = Vec::new();
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                kind = v.strip_prefix(' ').unwrap_or(v).into();
            }
            if let Some(v) = line.strip_prefix("data:") {
                data.push(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        let data = (!data.is_empty()).then(|| data.join("\n"));
        Ok(Some(Event { kind, data, raw }))
    }
}
