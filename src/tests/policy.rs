//! Model policy: reserved and ungranted models, request shapes, and tool name mapping.
use super::*;
use crate::policy;

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
