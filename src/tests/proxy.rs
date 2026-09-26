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
    let (headers, body) = &captures[0];
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
