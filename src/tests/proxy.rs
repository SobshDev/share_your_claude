//! Proxying `/v1/messages`: streaming, cancellation, upstream errors, and served-model checks.
use super::*;
use crate::oauth;

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
