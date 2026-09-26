//! Fake Anthropic upstream serving `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`,
//! and the OAuth token endpoint.
//!
//! The text of the first user message selects a scenario: `error` (429), `unauthorized` (401),
//! `redirect` (307), `disconnect` (stream that waits for the client to hang up),
//! `no-final-usage`, `stream-error`, `truncated`, `missing` (no usage), and `wrong-model`
//! (serves `claude-fable-5-1`). Anything else succeeds.
use super::*;

#[derive(Default)]
pub(super) struct Mock {
    /// Calls to `/v1/messages`.
    pub(super) requests: AtomicUsize,
    /// Calls to the token endpoint (refreshes and code exchanges).
    pub(super) refreshes: AtomicUsize,
    /// Set once the router drops a `disconnect` stream.
    pub(super) disconnected: AtomicBool,
    /// Headers and body of every `/v1/messages` call.
    pub(super) captures: Mutex<Vec<(HeaderMap, Value)>>,
    /// Body of every token endpoint call.
    pub(super) token_captures: Mutex<Vec<Value>>,
    /// Makes the token endpoint answer `invalid_grant`.
    pub(super) refresh_error: AtomicBool,
}

/// Serves the fake upstream on an ephemeral loopback port.
pub(super) async fn spawn_upstream()
-> (Arc<Mock>, std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let mock = Arc::new(Mock::default());
    let service = Router::new()
        .route("/v1/messages", post(mock_message))
        .route(
            "/v1/messages/count_tokens",
            post(|| async { Json(json!({"input_tokens":123})) }),
        )
        .route("/v1/oauth/token", post(mock_token))
        .route(
            "/v1/models",
            get(|| async {
                Json(json!({"data":[{"id":MODEL,"display_name":"Claude Sonnet 4.6"},{"id":"claude-opus-5","display_name":"Claude Opus 5"},{"id":"claude-fable-5-1","display_name":"Claude Fable 5.1"}],"has_more":false}))
            }),
        )
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    (mock, address, server)
}

async fn mock_message(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.requests.fetch_add(1, Ordering::SeqCst);
    mock.captures.lock().await.push((headers, body.clone()));
    let content = body
        .pointer("/messages/0/content")
        .and_then(Value::as_str)
        .unwrap_or("hello");
    if content == "error" {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "20")],
            Json(json!({"error":{"message":"sensitive provider echo"}})),
        )
            .into_response();
    }
    if content == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"owner-secret"}})),
        )
            .into_response();
    }
    if content == "redirect" {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "http://127.0.0.1:9/should-not-follow")],
        )
            .into_response();
    }
    if body["stream"] == true {
        let prefix = format!(
            "event: message_start\r\ndata: {}\r\n\r\n",
            json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}}})
        );
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
        stream.push_str("event: ping\ndata: {\"type\":\"ping\"}\n\n");
        stream.push_str("event: future_event\ndata: {\"type\":\"future_event\",\"value\":1}\n\n");
        if let Some(name) = body.pointer("/tools/0/name") {
            stream.push_str(&format!("event: content_block_start\ndata: {}\n\n",json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":name,"input":{}}})));
            stream.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n");
            stream.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        }
        if content != "no-final-usage" {
            stream.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n");
            stream.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n");
        }
        if content == "stream-error" {
            stream.push_str("event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"secret upstream text\"}}\n\n");
        } else if content != "truncated" {
            stream.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        }
        let chunks: Vec<_> = stream
            .as_bytes()
            .chunks(7)
            .map(|s| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(s)))
            .collect();
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
            .into_response();
    }
    let mut response = json!({"id":"msg_test","type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}});
    if content == "missing" {
        response.as_object_mut().unwrap().remove("usage");
    }
    if content == "wrong-model" {
        response["model"] = "claude-fable-5-1".into();
    }
    if let Some(name) = body.pointer("/tools/0/name") {
        response["content"] =
            json!([{"type":"tool_use","id":"tool_1","name":name,"input":{"name":"do not alter"}}]);
    }
    Json(response).into_response()
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
