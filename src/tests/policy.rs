//! Model policy: reserved and ungranted models, request shapes, and tool name mapping.
use super::*;
use crate::policy;

/// A second router over the harness database whose Anthropic upstream is `service`.
/// The harness admin session stays valid on it because sessions live in the database.
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

/// Calls an admin route on `router` with the harness session.
async fn admin_on(
    h: &Harness,
    router: &Router,
    path: &str,
    method: &str,
    body: Option<Value>,
) -> Response {
    let session = h.session().await;
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("cookie", session.cookie.as_str())
        .header("origin", ORIGIN)
        .header("x-csrf-token", session.csrf.as_str())
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    router.clone().oneshot(request).await.unwrap()
}

#[test]
fn blocked_matches_every_fable_spelling_and_nothing_else() {
    for id in [
        "claude-fable-5-1",
        "claude-fable-5.1",
        "CLAUDE-FABLE-5-1",
        "claude-fable-5-1@20260915",
        "claude-fable-5-1-20260901",
        "claude_fable_5_1",
        "claude fable 5.1",
        "claude-fable5.1",
        "fable/5/1",
    ] {
        assert!(policy::blocked(id, ""), "{id} must be blocked");
    }
    for id in [
        "claude-sonnet-4-6",
        "claude-fable-5-10",
        "claude-fable-15-1",
        "claude-fable-5-2",
        "claude-fables-5-1",
        "claude-5-1-fable",
    ] {
        assert!(!policy::blocked(id, "claude"), "{id} must not be blocked");
    }
    // The group alone blocks a model, whatever its id.
    assert!(policy::blocked("claude-mystery-1", policy::FABLE_GROUP));
    assert!(policy::blocked("claude-mystery-1", "FABLE_5_1"));
    assert_eq!(
        policy::catalog_group("claude-mystery-1", "Claude Fable 5.1"),
        policy::FABLE_GROUP
    );
    assert_eq!(
        policy::catalog_group("claude-fable-5-10", "Claude Fable 5.10"),
        "claude"
    );
}

#[tokio::test]
async fn catalog_refresh_files_every_fable_spelling_under_the_blocked_group() {
    let h = Harness::new().await;
    // A dated variant already stored under the wrong group, enabled and granted.
    sqlx::query("INSERT INTO model(id,display_name,model_group,enabled,reviewed_at) VALUES('claude-fable-5-1-20260901','Dated','claude',1,?)")
        .bind(db::now())
        .execute(&h.state.db)
        .await
        .unwrap();
    let fable = [
        "claude-fable-5.1",
        "CLAUDE-FABLE-5-1",
        "claude-fable-5-1@20260915",
        "claude-fable-5-1-20260901",
        "claude-mystery-1",
    ];
    let catalog = json!({"has_more":false,"data":[
        {"id":"claude-fable-5.1","display_name":"Fable"},
        {"id":"CLAUDE-FABLE-5-1","display_name":"Fable"},
        {"id":"claude-fable-5-1@20260915","display_name":"Fable"},
        {"id":"claude-fable-5-1-20260901","display_name":"Fable"},
        {"id":"claude-mystery-1","display_name":"Claude Fable 5.1"},
        {"id":"claude-fable-5-10","display_name":"Claude Fable 5.10"},
    ]});
    let (router, server) = with_upstream(
        &h,
        Router::new().route("/v1/models", get(move || async move { Json(catalog) })),
    )
    .await;
    let refreshed = admin_on(&h, &router, "/admin/api/models/refresh", "POST", None).await;
    assert_eq!(refreshed.status(), 200);
    server.abort();
    for id in fable {
        let row = sqlx::query("SELECT model_group,enabled FROM model WHERE id=?")
            .bind(id)
            .fetch_one(&h.state.db)
            .await
            .unwrap();
        assert_eq!(
            row.get::<String, _>("model_group"),
            policy::FABLE_GROUP,
            "{id}"
        );
        assert!(!row.get::<bool, _>("enabled"), "{id}");
        let review = h
            .admin(
                &format!("/admin/api/models/{id}"),
                "PUT",
                Some(json!({"enabled":true})),
            )
            .await;
        assert_error(review, 403, "permission_error", policy::FABLE_RESERVED).await;
        // Even a forced database grant stays unreachable.
        sqlx::query("UPDATE model SET enabled=1,reviewed_at=? WHERE id=?")
            .bind(db::now())
            .bind(id)
            .execute(&h.state.db)
            .await
            .unwrap();
        force_grant(&h, id).await;
        let mut body = message("hello", false);
        body["model"] = id.into();
        let r = h.request("/v1/messages", &h.key, body).await;
        assert_error(r, 403, "permission_error", policy::FABLE_RESERVED).await;
    }
    let near_miss: String =
        sqlx::query_scalar("SELECT model_group FROM model WHERE id='claude-fable-5-10'")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(near_miss, "claude");
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn policy_blocks_before_upstream_and_models_are_filtered() {
    let h = Harness::new().await;
    // Even an erroneous database grant cannot enable the reserved model.
    sqlx::query("UPDATE model SET enabled=1,reviewed_at=? WHERE id='claude-fable-5-1'")
        .bind(db::now())
        .execute(&h.state.db)
        .await
        .unwrap();
    force_grant(&h, "claude-fable-5-1").await;
    sqlx::query(
        "INSERT INTO model_alias(alias,model_id) VALUES('fable-latest','claude-fable-5-1')",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    for (model, denial) in [
        ("claude-fable-5-1", policy::FABLE_RESERVED),
        ("fable-latest", policy::FABLE_RESERVED),
        ("claude-fable-5-1-20260901", NO_ACCESS),
        ("unknown", NO_ACCESS),
    ] {
        let mut body = message("hello", false);
        body["model"] = model.into();
        let response = h.request("/v1/messages", &h.key, body).await;
        assert_error(response, 403, "permission_error", denial).await;
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

/// Enabled, reviewed, granted Fable rows, one per `blocked` branch, each with an alias:
/// the exact id, a dated id filed under an ordinary group, and an ordinary id in the Fable
/// group. Returns every name a friend could send.
/// Grants a model even if it is Fable, by dropping the guard triggers from migration 0005 to
/// simulate an older or tampered database. The router must still refuse the model.
async fn force_grant(h: &Harness, model: &str) {
    sqlx::query("DROP TRIGGER IF EXISTS key_model_grant_no_fable_insert")
        .execute(&h.state.db)
        .await
        .unwrap();
    grant(&h.state, &h.key_id, model).await;
}

async fn force_fable_rows(h: &Harness) -> Vec<&'static str> {
    for (id, group) in [
        ("claude-fable-5-1", policy::FABLE_GROUP),
        ("claude-fable-5-1-20260901", "claude"),
        ("claude-reserved-1", policy::FABLE_GROUP),
    ] {
        sqlx::query("INSERT INTO model(id,display_name,model_group,enabled,reviewed_at) VALUES(?,?,?,1,?) ON CONFLICT(id) DO UPDATE SET model_group=excluded.model_group,enabled=1,reviewed_at=excluded.reviewed_at")
            .bind(id)
            .bind(id)
            .bind(group)
            .bind(db::now())
            .execute(&h.state.db)
            .await
            .unwrap();
        force_grant(h, id).await;
        sqlx::query("INSERT INTO model_alias(alias,model_id) VALUES(?,?)")
            .bind(format!("{id}-alias"))
            .bind(id)
            .execute(&h.state.db)
            .await
            .unwrap();
    }
    vec![
        "claude-fable-5-1",
        "claude-fable-5-1-alias",
        "claude-fable-5-1-20260901",
        "claude-fable-5-1-20260901-alias",
        "claude-reserved-1",
        "claude-reserved-1-alias",
    ]
}

#[tokio::test]
async fn fable_is_denied_on_every_friend_endpoint_before_upstream() {
    let h = Harness::new().await;
    let names = force_fable_rows(&h).await;
    for endpoint in ["/v1/messages", "/v1/messages/count_tokens"] {
        for name in &names {
            let mut body = message("hello", false);
            body["model"] = (*name).into();
            let r = h.request(endpoint, &h.key, body).await;
            assert_error(r, 403, "permission_error", policy::FABLE_RESERVED).await;
        }
    }
    // Every attempt was recorded as denied and none reached the upstream.
    let outcomes: Vec<(String, Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT outcome,http_status,resolved_model FROM request_usage")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(outcomes.len(), names.len() * 2);
    assert!(
        outcomes
            .iter()
            .all(|row| *row == ("denied".to_owned(), Some(403), None))
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
    let bearer = format!("Bearer {}", h.key);
    let listed = json_body(
        h.get("/v1/models", &[("authorization", bearer.as_str())])
            .await,
    )
    .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], MODEL);
}

#[tokio::test]
async fn admin_mutations_never_grant_alias_or_configure_fable() {
    let h = Harness::new().await;
    force_fable_rows(&h).await;
    for id in [
        "claude-fable-5-1",
        "claude-fable-5-1-20260901",
        "claude-reserved-1",
    ] {
        let review = h
            .admin(
                &format!("/admin/api/models/{id}"),
                "PUT",
                Some(json!({"enabled":true})),
            )
            .await;
        assert_error(review, 403, "permission_error", policy::FABLE_RESERVED).await;
        let grants = h
            .admin(
                &format!("/admin/api/keys/{}/models", h.key_id),
                "PUT",
                Some(json!({"models":[MODEL,id]})),
            )
            .await;
        assert_error(
            grants,
            400,
            "invalid_request_error",
            "Only reviewed, enabled models other than Fable 5.1 can be granted",
        )
        .await;
        let alias = h
            .admin(
                &format!("/admin/api/models/{id}/aliases"),
                "POST",
                Some(json!({"alias":"innocent-name"})),
            )
            .await;
        assert_error(
            alias,
            400,
            "invalid_request_error",
            "Choose a reviewed and enabled model",
        )
        .await;
    }
    for alias in ["claude-fable-5-1", "fable_5_1", "my/fable-5-1-latest"] {
        let r = h
            .admin(
                &format!("/admin/api/models/{MODEL}/aliases"),
                "POST",
                Some(json!({"alias":alias})),
            )
            .await;
        assert_error(r, 400, "invalid_request_error", "Invalid or reserved alias").await;
    }
    // A new key receives every enabled model except the Fable rows.
    let created = json_body(
        h.admin(
            "/admin/api/keys",
            "POST",
            Some(json!({"person_id":h.person,"label":"Fresh"})),
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
    assert_eq!(fresh["models"], json!([MODEL]));
    // The client config omits Fable even though the database grants it to this key.
    let config = json_body(
        h.admin(&format!("/admin/api/keys/{}/config", h.key_id), "GET", None)
            .await,
    )
    .await;
    assert_eq!(
        config["providers"]["shared-claude"]["models"],
        json!([MODEL])
    );
}

/// Upstream that streams a successful reply whose serving model is Fable, either in
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
async fn streamed_reply_from_fable_is_rejected_and_recorded() {
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
