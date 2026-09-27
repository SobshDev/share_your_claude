//! Model policy: ungranted models, Fable 5.1 as an ordinary model, request shapes, and tool name mapping.
use super::*;
use crate::policy;

/// A second router over the harness database whose Anthropic upstream is `service`.
async fn with_upstream(h: &Harness, service: Router) -> (Router, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let config = config::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url: h.state.config.database_url.clone(),
        public_origin: ORIGIN.into(),
        password_hash: password_hash(),
        encryption_key: zeroize::Zeroizing::new(*h.state.config.encryption_key),
        secure_cookie: false,
        trusted_proxy_hops: 0,
    };
    let state = AppState::with_endpoints(
        config,
        h.state.db.clone(),
        format!("http://{address}"),
        TOKEN_ENDPOINT.into(),
    )
    .unwrap();
    (app(Arc::new(state)), server)
}

#[tokio::test]
async fn ungranted_models_and_routing_fields_are_refused_before_upstream() {
    let h = Harness::new().await;
    for model in ["claude-fable-5-1", "claude-fable-5-1-20260901", "unknown"] {
        let mut body = message("hello", false);
        body["model"] = model.into();
        let response = h.request("/v1/messages", &h.key, body).await;
        assert_error(response, 403, "permission_error", NO_ACCESS).await;
    }
    let mut body = message("hello", false);
    body["fallback"] = json!({"model":"claude-fable-5-1"});
    assert_error(
        h.request("/v1/messages", &h.key, body).await,
        400,
        "invalid_request_error",
        "Unsupported request field",
    )
    .await;
    let mut body = message("hello", false);
    body["tools"] =
        json!([{"type":"advisor_20260901","name":"advisor","model":"claude-fable-5-1"}]);
    assert_error(
        h.request("/v1/messages", &h.key, body).await,
        400,
        "invalid_request_error",
        "Unsupported tool field",
    )
    .await;
    let bearer = format!("Bearer {}", h.key);
    let r = h
        .get("/v1/models", &[("authorization", bearer.as_str())])
        .await;
    let data = json_body(r).await;
    assert_eq!(data["data"].as_array().unwrap().len(), 1);
    assert_eq!(data["data"][0]["id"], MODEL);
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
    let r = h
        .request("/v1/messages/batches", &h.key, message("hello", false))
        .await;
    assert_error(r, 404, "not_found_error", "Not found").await;
}

#[test]
fn tool_mapping_preserves_distinct_prefixed_names_and_payloads() {
    let mut body = message("hello", false);
    body["tools"] = json!([{"name":"search","input_schema":{"type":"object"}},{"name":"custom_search","input_schema":{"type":"object"}}]);
    let map = policy::ToolMap::prepare(&mut body).unwrap();
    assert_ne!(body["tools"][0]["name"], body["tools"][1]["name"]);
    let mut response = json!({"content":[{"type":"tool_use","name":body["tools"][1]["name"],"input":{"type":"tool_use","name":"unchanged"}}]});
    map.restore(&mut response);
    assert_eq!(response["content"][0]["name"], "custom_search");
    assert_eq!(response["content"][0]["input"]["name"], "unchanged");
}

#[test]
fn builtin_tools_keep_their_names_through_history() {
    let mut body = message("hello", false);
    body["tools"] = json!([{"name":"str_replace_editor","type":"text_editor_20250124"}]);
    body["messages"] = json!([{"role":"assistant","content":[{"type":"tool_use","name":"str_replace_editor","id":"t1","input":{"command":"view"}}]}]);
    policy::ToolMap::prepare(&mut body).unwrap();
    assert_eq!(body["tools"][0]["name"], "str_replace_editor");
    assert_eq!(
        body["messages"][0]["content"][0]["name"],
        "str_replace_editor"
    );
}

#[tokio::test]
async fn fable_is_reviewed_granted_and_aliased_like_any_model() {
    let h = Harness::new().await;
    let fable = "claude-fable-5-1";
    let enabled = h
        .admin(
            &format!("/admin/api/models/{fable}"),
            "PUT",
            Some(json!({"enabled":true})),
        )
        .await;
    assert_eq!(enabled.status(), 204);
    let granted = h
        .admin(
            &format!("/admin/api/keys/{}/models", h.key_id),
            "PUT",
            Some(json!({"models":[MODEL, fable]})),
        )
        .await;
    assert_eq!(granted.status(), 204);
    let alias = h
        .admin(
            &format!("/admin/api/models/{fable}/aliases"),
            "POST",
            Some(json!({"alias":"fable-latest"})),
        )
        .await;
    assert_eq!(alias.status(), 201);
    for requested in [fable, "fable-latest"] {
        assert_eq!(
            policy::resolve(&h.state, &h.key_id, requested)
                .await
                .unwrap(),
            fable
        );
    }
    let bearer = format!("Bearer {}", h.key);
    let listed = json_body(
        h.get("/v1/models", &[("authorization", bearer.as_str())])
            .await,
    )
    .await;
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [fable, MODEL]);
    let config = json_body(
        h.admin(&format!("/admin/api/keys/{}/config", h.key_id), "GET", None)
            .await,
    )
    .await;
    assert_eq!(
        config["providers"]["shared-claude"]["models"],
        json!([fable, MODEL])
    );
    // A key created with an explicit model list receives exactly that list.
    let created = json_body(
        h.admin(
            "/admin/api/keys",
            "POST",
            Some(json!({"person_id":h.person,"label":"Fable only","models":[fable]})),
        )
        .await,
    )
    .await;
    let keys = json_body(h.admin("/admin/api/keys", "GET", None).await).await;
    let fresh = keys
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == created["id"])
        .unwrap();
    assert_eq!(fresh["models"], json!([fable]));
    // Disabling the model removes it from friends again.
    let disabled = h
        .admin(
            &format!("/admin/api/models/{fable}"),
            "PUT",
            Some(json!({"enabled":false})),
        )
        .await;
    assert_eq!(disabled.status(), 204);
    let mut body = message("hello", false);
    body["model"] = fable.into();
    let r = h.request("/v1/messages", &h.key, body).await;
    assert_error(r, 403, "permission_error", NO_ACCESS).await;
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

/// Upstream that streams a successful reply served by a model other than the granted one, either in
/// `message_start` (content `start`) or only in the `message_delta` (anything else).
async fn fable_stream(State(calls): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> Response {
    calls.fetch_add(1, Ordering::SeqCst);
    let fable_at_start = body.pointer("/messages/0/content") == Some(&json!("start"));
    let start_model = if fable_at_start {
        "claude-fable-5-1"
    } else {
        MODEL
    };
    let events = [
        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":start_model,"content":[],"usage":{"input_tokens":4,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from fable"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","model":"claude-fable-5-1"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ];
    let stream: String = events.into_iter().map(event).collect();
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn streamed_reply_from_another_model_is_rejected_and_recorded() {
    let h = Harness::new().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (router, server) = with_upstream(
        &h,
        Router::new()
            .route("/v1/messages", post(fable_stream))
            .with_state(calls.clone()),
    )
    .await;
    for scenario in ["start", "delta"] {
        sqlx::query("DELETE FROM request_usage")
            .execute(&h.state.db)
            .await
            .unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", h.key.as_str())
            .body(Body::from(message(scenario, true).to_string()))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200, "{scenario}");
        let text = text_body(response).await;
        assert!(text.contains("event: error"), "{scenario}: {text}");
        assert!(!text.contains("message_stop"), "{scenario}: {text}");
        if scenario == "start" {
            assert!(!text.contains("from fable"), "{text}");
        }
        let row = sqlx::query("SELECT outcome,response_model FROM request_usage")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
        assert_eq!(
            row.get::<String, _>("outcome"),
            "upstream_error",
            "{scenario}"
        );
        assert_eq!(
            row.get::<Option<String>, _>("response_model").as_deref(),
            Some("claude-fable-5-1"),
            "{scenario}"
        );
    }
    // One upstream call per request: a rejected stream is never replayed.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}
