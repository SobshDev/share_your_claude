//! Fake Anthropic upstream serving `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`,
//! and the OAuth token endpoint. Replies follow Anthropic's wire format: error envelopes with an
//! `error.type`, complete stream event shapes, and a `request-id` header on every response.
//!
//! The text of the first user message selects a scenario: `error` (429), `unauthorized` (401),
//! `expired-token` (401 until the router sends a refreshed token, also for `count_tokens`),
//! `forbidden-auth` (403 `authentication_error`),
//! `redirect` (307), `status-NNN` (that status with an Anthropic error envelope),
//! `disconnect` (stream that waits for the client to hang up), `no-final-usage`,
//! `stream-error` (an `overloaded_error` event), `truncated`, `missing` (no usage), and
//! `wrong-model` (serves `claude-fable-5-1`). Anything else succeeds.
use super::*;
use axum::{
    extract::Query,
    http::{HeaderValue, Uri},
};
use std::collections::HashMap;

/// Request id the fake upstream sends with every response.
pub(super) const REQUEST_ID: &str = "req_011CMockUpstreamRequest";

/// One call to a fake API route.
pub(super) struct Capture {
    /// Path and query.
    pub(super) uri: String,
    pub(super) headers: HeaderMap,
    /// JSON body, or null for a GET.
    pub(super) body: Value,
}

#[derive(Default)]
pub(super) struct Mock {
    /// Calls to the API routes: messages, count_tokens, and models.
    pub(super) requests: AtomicUsize,
    /// Calls to the token endpoint (refreshes and code exchanges).
    pub(super) refreshes: AtomicUsize,
    /// Set once the router drops a `disconnect` stream.
    pub(super) disconnected: AtomicBool,
    /// Every API route call, in order.
    pub(super) captures: Mutex<Vec<Capture>>,
    /// Body of every token endpoint call.
    pub(super) token_captures: Mutex<Vec<Value>>,
    /// Makes the token endpoint answer `invalid_grant`.
    pub(super) refresh_error: AtomicBool,
    /// When nonzero, the largest models page served, whatever the client's `limit`.
    pub(super) models_page_size: AtomicUsize,
    /// Makes every models page report the same `last_id`, so pagination never advances.
    pub(super) models_repeat_cursor: AtomicBool,
}

impl Mock {
    async fn record(&self, uri: &Uri, headers: &HeaderMap, body: &Value) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.captures.lock().await.push(Capture {
            uri: uri.to_string(),
            headers: headers.clone(),
            body: body.clone(),
        });
    }
}

/// Serves the fake upstream on an ephemeral loopback port.
pub(super) async fn spawn_upstream()
-> (Arc<Mock>, std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let mock = Arc::new(Mock::default());
    let service = Router::new()
        .route("/v1/messages", post(mock_message))
        .route("/v1/messages/count_tokens", post(mock_count_tokens))
        .route("/v1/models", get(mock_models))
        .route("/v1/oauth/token", post(mock_token))
        .layer(middleware::map_response(|mut response: Response| async {
            let id = HeaderValue::from_static(REQUEST_ID);
            response.headers_mut().insert("request-id", id);
            response
        }))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    (mock, address, server)
}

/// An Anthropic error reply. Its message stands in for provider text the router must not relay.
fn error(status: u16, kind: &str, message: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({"type":"error","error":{"type":kind,"message":message}})),
    )
        .into_response()
}

/// The scenario selected by the first user message.
fn scenario(body: &Value) -> &str {
    body.pointer("/messages/0/content")
        .and_then(Value::as_str)
        .unwrap_or("hello")
}

/// The `expired-token` scenario: 401 until the router sends the token from a refresh.
fn token_rejection(headers: &HeaderMap, content: &str) -> Option<Response> {
    (content == "expired-token" && headers["authorization"] != "Bearer refreshed-access")
        .then(|| error(401, "authentication_error", "owner-secret"))
}

async fn mock_message(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.record(&uri, &headers, &body).await;
    let content = scenario(&body);
    if let Some(rejected) = token_rejection(&headers, content) {
        return rejected;
    }
    match content {
        "error" => {
            let mut response = error(429, "rate_limit_error", "sensitive provider echo");
            let retry = HeaderValue::from_static("20");
            response.headers_mut().insert("retry-after", retry);
            return response;
        }
        "unauthorized" => return error(401, "authentication_error", "owner-secret"),
        "forbidden-auth" => return error(403, "authentication_error", "owner-secret"),
        "redirect" => {
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", "http://127.0.0.1:9/should-not-follow")],
            )
                .into_response();
        }
        _ => (),
    }
    if let Some(status) = content.strip_prefix("status-") {
        let status: u16 = status.parse().unwrap();
        let kind = match status {
            400 => "invalid_request_error",
            403 => "permission_error",
            404 => "not_found_error",
            529 => "overloaded_error",
            _ => "api_error",
        };
        return error(status, kind, "secret upstream text");
    }
    if body["stream"] == true {
        return stream_reply(mock, content, &body);
    }
    let mut response = json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": [{"type":"text","text":"hello"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":20,"cache_creation_input_tokens":30},
    });
    if content == "missing" {
        response.as_object_mut().unwrap().remove("usage");
    }
    if content == "wrong-model" {
        response["model"] = "claude-fable-5-1".into();
    }
    if let Some(name) = body.pointer("/tools/0/name") {
        response["content"] =
            json!([{"type":"tool_use","id":"tool_1","name":name,"input":{"name":"do not alter"}}]);
        response["stop_reason"] = "tool_use".into();
    }
    Json(response).into_response()
}

/// One server-sent event named after its payload's `type`.
fn event(payload: Value) -> String {
    format!(
        "event: {}\ndata: {payload}\n\n",
        payload["type"].as_str().unwrap()
    )
}

fn stream_reply(mock: Arc<Mock>, content: &str, body: &Value) -> Response {
    let start = json!({"type":"message_start","message":{
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": [],
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":20,"cache_creation_input_tokens":30},
    }});
    // CRLF framing on the first event exercises the router's SSE line-ending handling.
    let prefix = format!("event: message_start\r\ndata: {start}\r\n\r\n");
    if content == "disconnect" {
        let (tx, rx) =
            tokio::sync::mpsc::channel::<std::result::Result<bytes::Bytes, std::io::Error>>(1);
        tokio::spawn(async move {
            let _ = tx.send(Ok(prefix.into())).await;
            tx.closed().await;
            mock.disconnected.store(true, Ordering::SeqCst);
        });
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
        )
            .into_response();
    }
    let mut stream = prefix;
    stream.push_str(&event(json!({"type":"ping"})));
    stream.push_str(&event(json!({"type":"future_event","value":1})));
    let tool = body.pointer("/tools/0/name");
    let (block, delta, stop_reason) = match tool {
        Some(name) => (
            json!({"type":"tool_use","id":"tool_1","name":name,"input":{}}),
            json!({"type":"input_json_delta","partial_json":"{}"}),
            "tool_use",
        ),
        None => (
            json!({"type":"text","text":""}),
            json!({"type":"text_delta","text":"hello"}),
            "end_turn",
        ),
    };
    stream.push_str(&event(
        json!({"type":"content_block_start","index":0,"content_block":block}),
    ));
    stream.push_str(&event(
        json!({"type":"content_block_delta","index":0,"delta":delta}),
    ));
    stream.push_str(&event(json!({"type":"content_block_stop","index":0})));
    if content != "no-final-usage" {
        // Output counts are cumulative: the router must keep the last one, not add them up.
        stream.push_str(&event(json!({"type":"message_delta",
            "delta":{"stop_reason":null,"stop_sequence":null},"usage":{"output_tokens":3}})));
        stream.push_str(&event(json!({"type":"message_delta",
            "delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":7}})));
    }
    if content == "stream-error" {
        stream.push_str(&event(json!({"type":"error",
            "error":{"type":"overloaded_error","message":"secret upstream text"}})));
    } else if content != "truncated" {
        stream.push_str(&event(json!({"type":"message_stop"})));
    }
    let chunks: Vec<_> = stream
        .as_bytes()
        .chunks(7)
        .map(|s| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(s)))
        .collect();
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(futures_util::stream::iter(chunks)),
    )
        .into_response()
}

async fn mock_count_tokens(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.record(&uri, &headers, &body).await;
    if let Some(rejected) = token_rejection(&headers, scenario(&body)) {
        return rejected;
    }
    Json(json!({"input_tokens":123})).into_response()
}

/// Lists three models, one page at a time, honouring `limit` and `after_id`.
async fn mock_models(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    mock.record(&uri, &headers, &Value::Null).await;
    let models = [
        (MODEL, "Claude Sonnet 4.6"),
        ("claude-opus-5", "Claude Opus 5"),
        ("claude-fable-5-1", "Claude Fable 5.1"),
    ];
    let limit = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let page_size = match mock.models_page_size.load(Ordering::SeqCst) {
        0 => limit,
        size => size.min(limit),
    };
    let start = query.get("after_id").map_or(0, |after| {
        models
            .iter()
            .position(|(id, _)| id == after)
            .map_or(models.len(), |i| i + 1)
    });
    let page: Vec<Value> = models[start..]
        .iter()
        .take(page_size)
        .map(|(id, name)| {
            json!({"type":"model","id":id,"display_name":name,"created_at":"2026-01-01T00:00:00Z"})
        })
        .collect();
    let mut has_more = start + page.len() < models.len();
    let mut last_id = page.last().map(|m| m["id"].clone());
    if mock.models_repeat_cursor.load(Ordering::SeqCst) {
        has_more = true;
        last_id = Some(models[0].0.into());
    }
    Json(json!({
        "data": page,
        "first_id": page.first().map(|m| &m["id"]),
        "last_id": last_id,
        "has_more": has_more,
    }))
    .into_response()
}

async fn mock_token(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> Response {
    mock.refreshes.fetch_add(1, Ordering::SeqCst);
    mock.token_captures.lock().await.push(body.clone());
    if mock.refresh_error.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant","secret":"must not leak"})),
        )
            .into_response();
    }
    assert!(body["grant_type"] == "refresh_token" || body["grant_type"] == "authorization_code");
    tokio::time::sleep(Duration::from_millis(30)).await;
    Json(json!({"access_token":"refreshed-access","refresh_token":"rotated-refresh","expires_in":3600})).into_response()
}
