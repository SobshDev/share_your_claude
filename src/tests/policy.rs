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
    };
    let mut state = AppState::new(config, h.state.db.clone()).unwrap();
    Arc::get_mut(&mut state).unwrap().upstream = format!("http://{address}");
    (app(state), server)
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
        assert_eq!(review.status(), 403, "{id}");
        // Even a forced database grant stays unreachable.
        sqlx::query("UPDATE model SET enabled=1,reviewed_at=? WHERE id=?")
            .bind(db::now())
            .bind(id)
            .execute(&h.state.db)
            .await
            .unwrap();
        grant(&h.state, &h.key_id, id).await;
        let mut body = message("hello", false);
        body["model"] = id.into();
        let r = h.request("/v1/messages", &h.key, body).await;
        assert_eq!(r.status(), 403, "{id}");
        assert_eq!(
            json_body(r).await["error"]["message"],
            policy::FABLE_RESERVED,
            "{id}"
        );
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
    grant(&h.state, &h.key_id, "claude-fable-5-1").await;
    sqlx::query(
        "INSERT INTO model_alias(alias,model_id) VALUES('fable-latest','claude-fable-5-1')",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    for model in [
        "claude-fable-5-1",
        "fable-latest",
        "claude-fable-5-1-20260901",
        "unknown",
    ] {
        let mut body = message("hello", false);
        body["model"] = model.into();
        assert_eq!(h.request("/v1/messages", &h.key, body).await.status(), 403);
    }
    let mut body = message("hello", false);
    body["fallback"] = json!({"model":"claude-fable-5-1"});
    assert_eq!(h.request("/v1/messages", &h.key, body).await.status(), 400);
    let mut body = message("hello", false);
    body["tools"] =
        json!([{"type":"advisor_20260901","name":"advisor","model":"claude-fable-5-1"}]);
    assert_eq!(h.request("/v1/messages", &h.key, body).await.status(), 400);
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
    assert_eq!(r.status(), 404);
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
