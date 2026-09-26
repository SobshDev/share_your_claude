//! Proxying `/v1/messages`: streaming, cancellation, upstream errors, and served-model checks.
use super::*;
use crate::{oauth, proxy};

/// Feeds `input` to a fresh decoder in `chunk`-byte pieces and returns `(kind, data)` pairs.
fn decode(input: &[u8], chunk: usize) -> Vec<(String, Option<String>)> {
    let mut decoder = proxy::SseDecoder::default();
    let mut events = Vec::new();
    for piece in input.chunks(chunk) {
        decoder.push(piece).unwrap();
        while let Some(event) = decoder.next_event().unwrap() {
            events.push((event.kind, event.data));
        }
    }
    events
}

#[test]
fn sse_decoder_accepts_every_line_ending_split_anywhere() {
    let expected = vec![
        ("message_start".to_owned(), Some("{\"a\":1}".to_owned())),
        ("message".to_owned(), Some("line one\nline two".to_owned())),
        ("ping".to_owned(), None),
    ];
    for separator in ["\n", "\r\n", "\r"] {
        let input = [
            "event: message_start",
            "data: {\"a\":1}",
            "",
            ": comment lines are ignored",
            "data:line one",
            "data: line two",
            "",
            "event:ping",
            "",
            "",
        ]
        .join(separator);
        for chunk in [1, 2, 3, 7, input.len()] {
            assert_eq!(
                decode(input.as_bytes(), chunk),
                expected,
                "{separator:?} in {chunk}-byte chunks"
            );
        }
    }
    // Mixed endings, including `\r\n` followed by `\n` and a `\r` split from its `\n`.
    let mixed = b"event: a\r\ndata: 1\r\n\nevent: b\rdata: 2\r\r\nevent: c\ndata: 3\n\r";
    for chunk in [1, 2, 5, mixed.len()] {
        let kinds: Vec<_> = decode(mixed, chunk)
            .into_iter()
            .map(|(kind, data)| format!("{kind}={}", data.unwrap()))
            .collect();
        assert_eq!(kinds, ["a=1", "b=2", "c=3"], "{chunk}-byte chunks");
    }
}

#[test]
fn sse_decoder_is_incremental_and_bounded() {
    let mut decoder = proxy::SseDecoder::default();
    let data = "x".repeat(proxy::MAX_SSE_EVENT_BYTES - 64);
    let input = format!("event: big\ndata: {data}\n\n");
    for piece in input.as_bytes().chunks(1024) {
        decoder.push(piece).unwrap();
        if let Some(event) = decoder.next_event().unwrap() {
            assert_eq!(event.kind, "big");
            assert_eq!(event.data.as_deref().map(str::len), Some(data.len()));
        }
    }
    // Every byte is examined for a line ending about once, not once per chunk.
    assert!(decoder.scanned <= input.len() + 8, "{}", decoder.scanned);

    let mut decoder = proxy::SseDecoder::default();
    decoder
        .push(&vec![b'x'; proxy::MAX_SSE_EVENT_BYTES])
        .unwrap();
    assert!(decoder.next_event().unwrap().is_none());
    assert!(decoder.push(b"\n\n").is_err());
}

#[tokio::test]
async fn stream_usage_is_cumulative_and_tool_names_round_trip() {
    let h = Harness::new().await;
    let mut body = message("hello", true);
    body["tools"] = json!([{"name":"custom_search","input_schema":{"type":"object"}}]);
    let response = h.request("/v1/messages", &h.key, body).await;
    assert_eq!(response.status(), 200);
    let text = text_body(response).await;
    assert!(text.contains("\"name\":\"custom_search\""));
    assert!(text.contains("future_event"));
    assert!(text.contains("message_stop"));
    let row = sqlx::query("SELECT * FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("outcome"), "completed");
    assert_eq!(row.get::<String, _>("usage_state"), "complete");
    assert_eq!(row.get::<i64, _>("input_tokens"), 10);
    assert_eq!(row.get::<i64, _>("output_tokens"), 7);
    assert_eq!(row.get::<i64, _>("cache_read_tokens"), 20);
    assert_eq!(row.get::<i64, _>("cache_write_tokens"), 30);
    let captures = h.mock.captures.lock().await;
    let Capture { headers, body, .. } = &captures[0];
    assert_eq!(
        headers["authorization"],
        "Bearer owner-access-must-not-leak"
    );
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(body["system"][0]["text"], oauth::SYSTEM);
    assert!(
        body["tools"][0]["name"]
            .as_str()
            .unwrap()
            .starts_with("custom_")
    );
    assert!(!text.contains("owner-access"));
}

#[tokio::test]
async fn disconnect_cancels_upstream_and_preserves_checkpoint() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("disconnect", true))
        .await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.is_some());
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.mock.disconnected.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let row = sqlx::query("SELECT usage_state,outcome,input_tokens FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("usage_state"), "partial");
    assert_eq!(row.get::<String, _>("outcome"), "interrupted");
    assert_eq!(row.get::<i64, _>("input_tokens"), 10);
}

#[tokio::test]
async fn upstream_errors_are_sanitized_and_never_replayed() {
    let h = Harness::new().await;
    let r = h
        .request("/v1/messages", &h.key, message("error", false))
        .await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "20");
    assert!(
        !json_body(r)
            .await
            .to_string()
            .contains("sensitive provider echo")
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("redirect", false))
            .await
            .status(),
        502
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unexpected_serving_model_is_recorded_and_response_rejected() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("wrong-model", false))
        .await;
    assert_eq!(response.status(), 502);
    let row = sqlx::query("SELECT outcome,response_model,usage_state FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("outcome"), "upstream_error");
    assert_eq!(row.get::<String, _>("response_model"), "claude-fable-5-1");
    assert_eq!(row.get::<String, _>("usage_state"), "partial");
}

#[tokio::test]
#[ignore = "requires Bun and OPENCODEX_SOURCE pointing at an installed opencodex package"]
async fn opencodex_adapter_smoke() {
    let source = std::env::var("OPENCODEX_SOURCE").expect("set OPENCODEX_SOURCE");
    let h = Harness::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = h.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let output = tokio::process::Command::new("bun")
        .arg("scripts/opencodex-smoke.ts")
        .env("OPENCODEX_SOURCE", source)
        .env("SMOKE_ROUTER_URL", url)
        .env("SMOKE_ROUTER_KEY", &h.key)
        .output()
        .await
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);
}

/// Sends `message("hello", false)` to `/v1/messages` with extra request headers.
async fn with_headers(h: &Harness, extra: &[(&str, &str)]) -> Response {
    let mut headers = vec![
        ("content-type", "application/json"),
        ("x-api-key", h.key.as_str()),
    ];
    headers.extend_from_slice(extra);
    h.send(
        "POST",
        "/v1/messages",
        &headers,
        message("hello", false).to_string(),
    )
    .await
}

#[tokio::test]
async fn reviewed_betas_are_normalized_and_sent_once() {
    let h = Harness::new().await;
    for extra in [
        &[("anthropic-beta", "prompt-caching-2024-07-31,")][..],
        &[
            (
                "anthropic-beta",
                " oauth-2025-04-20 , prompt-caching-2024-07-31",
            ),
            ("anthropic-beta", "prompt-caching-2024-07-31,,"),
        ][..],
    ] {
        assert_eq!(with_headers(&h, extra).await.status(), 200);
    }
    // An empty header adds nothing, so the router's own betas go upstream unchanged.
    assert_eq!(
        with_headers(&h, &[("anthropic-beta", "")]).await.status(),
        200
    );
    let captures = h.mock.captures.lock().await;
    let sent: Vec<Vec<_>> = captures
        .iter()
        .map(|c| {
            c.headers
                .get_all("anthropic-beta")
                .iter()
                .cloned()
                .collect()
        })
        .collect();
    let merged = format!("{},prompt-caching-2024-07-31", oauth::BETA);
    assert_eq!(
        sent,
        [
            vec![merged.as_str()],
            vec![merged.as_str()],
            vec![oauth::BETA]
        ]
    );
}

#[tokio::test]
async fn unreviewed_betas_are_rejected_before_upstream() {
    let h = Harness::new().await;
    for extra in [
        &[("anthropic-beta", "context-management-2025-06-27")][..],
        &[
            ("anthropic-beta", "prompt-caching-2024-07-31"),
            ("anthropic-beta", "files-api-2025-04-14"),
        ][..],
    ] {
        let response = with_headers(&h, extra).await;
        assert_eq!(response.status(), 400);
        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
    let outcomes: Vec<(String, i64)> =
        sqlx::query_as("SELECT outcome,http_status FROM request_usage")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(outcomes, [("denied".into(), 400), ("denied".into(), 400)]);
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn upstream_statuses_keep_their_anthropic_error_type() {
    let h = Harness::new().await;
    for (status, kind) in [
        (400, "invalid_request_error"),
        (404, "not_found_error"),
        (529, "overloaded_error"),
        (500, "api_error"),
    ] {
        let response = h
            .request(
                "/v1/messages",
                &h.key,
                message(&format!("status-{status}"), false),
            )
            .await;
        assert_eq!(response.status(), status);
        let body = json_body(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], kind, "status {status}");
        assert!(!body.to_string().contains("secret upstream text"));
    }
}

#[tokio::test]
async fn stream_error_events_keep_only_their_type() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("stream-error", true))
        .await;
    let text = text_body(response).await;
    let last = text.trim_end().rsplit("\n\n").next().unwrap();
    assert!(last.starts_with("event: error\n"), "{last}");
    let data: Value = serde_json::from_str(last.split_once("data: ").unwrap().1).unwrap();
    assert_eq!(data["error"]["type"], "overloaded_error");
    assert!(!text.contains("secret upstream text"));
}

#[tokio::test]
async fn router_generated_errors_use_the_anthropic_envelope() {
    let h = Harness::new().await;
    let auth = [("x-api-key", h.key.as_str())];
    let oversized = vec![b' '; proxy::MAX_REQUEST_BODY_BYTES + 1];
    for (response, status, kind) in [
        (h.get("/v1/unknown", &auth).await, 404, "not_found_error"),
        (
            h.request("/v1/messages/batches", &h.key, message("hello", false))
                .await,
            404,
            "not_found_error",
        ),
        (
            h.get("/v1/messages", &auth).await,
            405,
            "invalid_request_error",
        ),
        (
            h.send(
                "POST",
                "/v1/messages",
                &[("content-type", "application/json"), auth[0]],
                oversized,
            )
            .await,
            413,
            "request_too_large",
        ),
    ] {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["content-type"], "application/json");
        let body = json_body(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], kind, "status {status}");
    }
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn keys_are_checked_before_the_request_body_is_read() {
    let h = Harness::new().await;
    for headers in [
        &[("content-type", "application/json")][..],
        &[
            ("content-type", "application/json"),
            ("x-api-key", "sr_wrong"),
        ][..],
    ] {
        // A body that never finishes: reading it would hang the request.
        let pending = Body::from_stream(futures_util::stream::pending::<
            std::result::Result<bytes::Bytes, std::io::Error>,
        >());
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            h.send("POST", "/v1/messages", headers, pending),
        )
        .await
        .expect("rejected without reading the body");
        assert_eq!(response.status(), 401);
        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "authentication_error");
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn malformed_bodies_and_content_types_are_invalid_requests() {
    let h = Harness::new().await;
    let valid = message("hello", false).to_string();
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        for (content_type, body) in [
            (Some("application/json"), "{not json".to_owned()),
            (Some("text/plain"), valid.clone()),
            (None, valid.clone()),
        ] {
            let mut headers = vec![("x-api-key", h.key.as_str())];
            headers.extend(content_type.map(|v| ("content-type", v)));
            let response = h.send("POST", path, &headers, body).await;
            assert_eq!(response.status(), 400, "{path} {content_type:?}");
            let body = json_body(response).await;
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], "invalid_request_error");
        }
    }
    let charset = h
        .send(
            "POST",
            "/v1/messages",
            &[
                ("x-api-key", h.key.as_str()),
                ("content-type", "application/json; charset=utf-8"),
            ],
            valid,
        )
        .await;
    assert_eq!(charset.status(), 200);
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 1);
}

/// Opens a `disconnect` stream on `key`; it holds its admission permits until the body drops.
async fn hold_stream(h: &Harness, key: &str) -> Body {
    let response = h
        .request("/v1/messages", key, message("disconnect", true))
        .await;
    assert_eq!(response.status(), 200);
    response.into_body()
}

async fn assert_busy(response: Response) {
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "1");
    let body = json_body(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

fn count_body() -> Value {
    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]})
}

#[tokio::test]
async fn one_key_cannot_take_every_admission_permit() {
    let h = Harness::new().await;
    let sam = add_person(&h.state, "Sam").await;
    let (_, other) = add_key(&h.state, &sam, "Desktop").await;
    let mut held = Vec::new();
    for _ in 0..PER_KEY_CONCURRENCY {
        held.push(hold_stream(&h, &h.key).await);
    }
    assert_busy(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await,
    )
    .await;
    let counted = h
        .request("/v1/messages/count_tokens", &h.key, count_body())
        .await;
    assert_eq!(counted.status(), 200);
    let response = h
        .request("/v1/messages", &other, message("hello", false))
        .await;
    assert_eq!(response.status(), 200);
    drop(held);
    // The stream tasks release their permits once they notice the disconnects.
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.state.admission.available_permits() < GLOBAL_CONCURRENCY {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let response = h
        .request("/v1/messages", &h.key, message("hello", false))
        .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn saturated_admission_is_denied_before_upstream() {
    let h = Harness::new().await;
    let messages = h
        .state
        .admission
        .clone()
        .try_acquire_many_owned(GLOBAL_CONCURRENCY as u32)
        .unwrap();
    let counts = h
        .state
        .count_admission
        .clone()
        .try_acquire_many_owned(COUNT_TOKENS_CONCURRENCY as u32)
        .unwrap();
    assert_busy(
        h.request("/v1/messages", &h.key, message("hello", true))
            .await,
    )
    .await;
    assert_busy(
        h.request("/v1/messages/count_tokens", &h.key, count_body())
            .await,
    )
    .await;
    drop((messages, counts));
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT endpoint,outcome,http_status FROM request_usage ORDER BY endpoint")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            ("/v1/messages".into(), "denied".into(), 429),
            ("/v1/messages/count_tokens".into(), "denied".into(), 429),
        ]
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_client_that_stops_reading_is_interrupted_not_an_upstream_error() {
    let mut h = Harness::new().await;
    // The router holds the only other references to the state; rebuild it around the change.
    h.router = Router::new();
    Arc::get_mut(&mut h.state).unwrap().client_send_timeout = Duration::from_millis(50);
    h.router = app(h.state.clone());
    let response = h
        .request("/v1/messages", &h.key, message("hello", true))
        .await;
    assert_eq!(response.status(), 200);
    // The body is never polled, so the relay channel fills and a send times out.
    let row = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row: (String, String) =
                sqlx::query_as("SELECT outcome,usage_state FROM request_usage")
                    .fetch_one(&h.state.db)
                    .await
                    .unwrap();
            if row.0 != "in_progress" {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(row, ("interrupted".into(), "partial".into()));
    drop(response);
}

async fn credential(h: &Harness) -> (String, i64) {
    sqlx::query_as("SELECT state,generation FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_upstream_401_refreshes_once_and_never_replays_messages() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("expired-token", false))
        .await;
    assert_eq!(response.status(), 503);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "api_error");
    assert!(!body.to_string().contains("owner-secret"));
    // The message was sent once; the refresh left the credential connected.
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 1);
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(credential(&h).await, ("connected".into(), 2));
    let response = h
        .request("/v1/messages", &h.key, message("expired-token", false))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_upstream_401_on_count_tokens_is_retried_after_refresh() {
    let h = Harness::new().await;
    let mut body = count_body();
    body["messages"][0]["content"] = "expired-token".into();
    let response = h.request("/v1/messages/count_tokens", &h.key, body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["input_tokens"], 123);
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(credential(&h).await, ("connected".into(), 2));
}

#[tokio::test]
async fn a_rejected_refresh_after_a_401_requires_reconnect() {
    let h = Harness::new().await;
    h.mock.refresh_error.store(true, Ordering::SeqCst);
    let response = h
        .request("/v1/messages", &h.key, message("expired-token", false))
        .await;
    assert_eq!(response.status(), 503);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(!body.to_string().contains("owner-secret"));
    assert!(!body.to_string().contains("must not leak"));
    assert_eq!(credential(&h).await, ("needs_reauth".into(), 1));
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn upstream_403s_disconnect_only_for_authentication_errors() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("status-403", false))
        .await;
    assert_eq!(response.status(), 403);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "permission_error");
    assert!(!body.to_string().contains("secret upstream text"));
    assert_eq!(credential(&h).await, ("connected".into(), 1));
    let response = h
        .request("/v1/messages", &h.key, message("hello", false))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);

    let response = h
        .request("/v1/messages", &h.key, message("forbidden-auth", false))
        .await;
    assert_eq!(response.status(), 503);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(!body.to_string().contains("owner-secret"));
    assert_eq!(credential(&h).await, ("needs_reauth".into(), 1));
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn outbound_requests_carry_the_compatibility_headers() {
    let h = Harness::new().await;
    let sent = [
        ("/v1/messages", message("hello", false), "application/json"),
        ("/v1/messages", message("hello", true), "text/event-stream"),
        (
            "/v1/messages/count_tokens",
            count_body(),
            "application/json",
        ),
    ];
    for (path, body, _) in &sent {
        let response = h.request(path, &h.key, body.clone()).await;
        assert_eq!(response.status(), 200);
        text_body(response).await;
    }
    let captures = h.mock.captures.lock().await;
    assert_eq!(captures.len(), sent.len());
    for (capture, (path, _, accept)) in captures.iter().zip(&sent) {
        assert_eq!(capture.uri, *path);
        let headers = &capture.headers;
        for (name, value) in [
            ("authorization", "Bearer owner-access-must-not-leak"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", oauth::BETA),
            ("content-type", "application/json"),
            ("accept", accept),
            ("x-app", "cli"),
            ("user-agent", "@anthropic-ai/sdk/0.74.0"),
            ("x-stainless-retry-count", "0"),
        ] {
            assert_eq!(
                headers.get_all(name).iter().collect::<Vec<_>>(),
                [value],
                "{name}"
            );
        }
        for name in ["x-claude-code-session-id", "x-client-request-id"] {
            assert!(headers.contains_key(name), "{name}");
        }
        assert!(!headers.contains_key("x-api-key"));
        assert!(!headers.contains_key("cookie"));
    }
    let ids: Vec<Option<String>> =
        sqlx::query_scalar("SELECT upstream_request_id FROM request_usage")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(ids, vec![Some(REQUEST_ID.to_owned()); sent.len()]);
}

#[tokio::test]
async fn catalog_refresh_follows_pages_and_rejects_a_repeated_cursor() {
    let h = Harness::new().await;
    h.mock.models_page_size.store(1, Ordering::SeqCst);
    let response = h.admin("/admin/api/models/refresh", "POST", None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["discovered"], 3);
    let pages: Vec<String> = h
        .mock
        .captures
        .lock()
        .await
        .iter()
        .map(|c| c.uri.clone())
        .collect();
    assert_eq!(
        pages,
        [
            "/v1/models?limit=100".to_owned(),
            format!("/v1/models?limit=100&after_id={MODEL}"),
            "/v1/models?limit=100&after_id=claude-opus-5".to_owned(),
        ]
    );
    h.mock.models_repeat_cursor.store(true, Ordering::SeqCst);
    let response = h.admin("/admin/api/models/refresh", "POST", None).await;
    assert_eq!(response.status(), 502);
    assert_eq!(json_body(response).await["error"]["type"], "api_error");
}

/// Outcome, HTTP status, and usage state of the most recent request.
async fn last_row(h: &Harness) -> (String, Option<i64>, String) {
    sqlx::query_as(
        "SELECT outcome,http_status,usage_state FROM request_usage ORDER BY rowid DESC LIMIT 1",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap()
}

#[tokio::test]
async fn malformed_upstream_streams_end_with_a_sanitized_error() {
    let h = Harness::new().await;
    for (scenario, usage_state) in [
        ("stream-duplicate-start", "partial"),
        ("stream-delta-first", "unknown"),
        ("stream-stop-first", "unknown"),
        ("stream-mismatch", "partial"),
        ("stream-no-payload", "partial"),
        ("stream-fallback", "partial"),
        ("stream-reset", "partial"),
        ("stream-oversized", "partial"),
        ("stream-bad-utf8", "partial"),
        ("wrong-model", "partial"),
        ("truncated", "partial"),
    ] {
        let response = h
            .request("/v1/messages", &h.key, message(scenario, true))
            .await;
        assert_eq!(response.status(), 200, "{scenario}");
        let text = text_body(response).await;
        let last = text.trim_end().rsplit("\n\n").next().unwrap();
        let data: Value = serde_json::from_str(last.split_once("data: ").unwrap().1).unwrap();
        assert_eq!(data["error"]["type"], "api_error", "{scenario}");
        assert!(last.contains("stream was interrupted"), "{scenario}");
        assert!(!text.contains("fallback") && !text.contains("claude-fable-5-1"));
        let expected = ("upstream_error".into(), Some(200), usage_state.into());
        assert_eq!(last_row(&h).await, expected, "{scenario}");
    }
}

#[tokio::test]
async fn invalid_upstream_replies_are_bad_gateways() {
    let h = Harness::new().await;
    let mut bad_count = count_body();
    bad_count["messages"][0]["content"] = "bad-count".into();
    for (path, body, usage_state) in [
        ("/v1/messages", message("not-json", false), "unknown"),
        ("/v1/messages", message("not-message", false), "unknown"),
        ("/v1/messages", message("oversized-reply", false), "unknown"),
        ("/v1/messages", message("stream-not-sse", true), "unknown"),
        ("/v1/messages/count_tokens", bad_count, "not_applicable"),
    ] {
        let label = body.to_string();
        let response = h.request(path, &h.key, body).await;
        assert_eq!(response.status(), 502, "{label}");
        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "api_error");
        let expected = ("upstream_error".into(), Some(502), usage_state.into());
        assert_eq!(last_row(&h).await, expected, "{label}");
    }
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_bad_gateway() {
    let mut h = Harness::new().await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    // The router holds the only other references to the state; rebuild it around the change.
    h.router = Router::new();
    Arc::get_mut(&mut h.state).unwrap().upstream = format!("http://{closed}");
    h.router = app(h.state.clone());
    let response = h
        .request("/v1/messages", &h.key, message("hello", false))
        .await;
    assert_eq!(response.status(), 502);
    assert_eq!(json_body(response).await["error"]["type"], "api_error");
    let expected = ("upstream_error".into(), Some(502), "unknown".into());
    assert_eq!(last_row(&h).await, expected);
}
