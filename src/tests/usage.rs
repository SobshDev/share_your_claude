//! Usage accounting: per-person totals, explicit unknown/partial usage, and recovery.
use super::*;
use crate::usage;

#[tokio::test]
async fn people_totals_survive_key_rotation_and_exclude_estimates() {
    let h = Harness::new().await;
    let second = add_person(&h.state, "Sam").await;
    let (_, key2) = add_key(&h.state, &second, "Workstation").await;
    for key in [&h.key, &key2] {
        assert_eq!(
            h.request("/v1/messages", key, message("hello", false))
                .await
                .status(),
            200
        );
    }
    let (_, rotated) = add_key(&h.state, &h.person, "New laptop").await;
    assert_eq!(
        h.request("/v1/messages", &rotated, message("hello", false))
            .await
            .status(),
        200
    );
    assert_eq!(
        h.request(
            "/v1/messages/count_tokens",
            &rotated,
            json!({"model":MODEL,"messages":[]})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        h.admin(&format!("/admin/api/keys/{}", h.key_id), "DELETE", None)
            .await
            .status(),
        204
    );
    let report = json_body(
        h.admin("/admin/api/usage?group_by=person", "GET", None)
            .await,
    )
    .await;
    let rows = report["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["label"], "Alex");
    assert_eq!(rows[0]["observed_total_tokens"], 134);
    assert_eq!(rows[0]["requests"], 2);
    assert_eq!(rows[1]["observed_total_tokens"], 67);
}

#[tokio::test]
async fn missing_error_and_interrupted_usage_remain_explicit() {
    let h = Harness::new().await;
    assert_eq!(
        h.request("/v1/messages", &h.key, message("missing", false))
            .await
            .status(),
        200
    );
    for scenario in ["truncated", "stream-error"] {
        let response = h
            .request("/v1/messages", &h.key, message(scenario, true))
            .await;
        let text = text_body(response).await;
        assert!(text.contains("stream was interrupted"));
        assert!(!text.contains("secret upstream text"));
    }
    let rows = sqlx::query("SELECT usage_state,outcome FROM request_usage ORDER BY started_at")
        .fetch_all(&h.state.db)
        .await
        .unwrap();
    assert_eq!(rows[0].get::<String, _>("usage_state"), "unknown");
    for row in &rows[1..] {
        assert_eq!(row.get::<String, _>("usage_state"), "partial");
        assert_eq!(row.get::<String, _>("outcome"), "upstream_error");
    }
    let id = usage::start(&h.state.db, &h.key_id, "/v1/messages", MODEL)
        .await
        .unwrap();
    db::recover(&h.state.db).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT outcome FROM request_usage WHERE id=?")
        .bind(id)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(state, "interrupted");
}

#[test]
fn usage_merges_snapshots_and_rejects_invalid_values() {
    let mut u = usage::Usage::default();
    assert_eq!(u.state(true), "unknown");
    u.merge(Some(&json!({"input_tokens":10,"output_tokens":0})));
    u.merge(Some(&json!({"output_tokens":3})));
    u.merge(Some(&json!({"output_tokens":7})));
    assert_eq!(u.get("output_tokens"), Some(7));
    assert_eq!(u.state(true), "complete");
    u.merge(Some(&json!({"output_tokens":-1})));
    assert_eq!(u.state(true), "partial");
}

#[tokio::test]
async fn missing_final_usage_never_appears_complete() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("no-final-usage", true))
        .await;
    response.into_body().collect().await.unwrap();
    let row = sqlx::query("SELECT outcome,usage_state FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("outcome"), "completed");
    assert_eq!(row.get::<String, _>("usage_state"), "partial");
}
