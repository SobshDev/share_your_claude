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

#[test]
fn null_usage_counters_are_unreported_not_invalid() {
    let mut u = usage::Usage::default();
    u.merge(Some(
        &json!({"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":4}),
    ));
    u.merge(Some(&json!({
        "input_tokens":null,
        "output_tokens":7,
        "cache_read_input_tokens":null,
        "cache_creation_input_tokens":null
    })));
    assert_eq!(u.get("input_tokens"), Some(10));
    assert_eq!(u.get("cache_read_input_tokens"), Some(4));
    assert_eq!(u.get("cache_creation_input_tokens"), None);
    assert_eq!(u.state(true), "complete");
    for invalid in [json!({"output_tokens":"8"}), json!({"output_tokens":1.5})] {
        let mut u = usage::Usage::default();
        u.merge(Some(&json!({"input_tokens":10,"output_tokens":0})));
        u.merge(Some(&invalid));
        assert_eq!(u.state(true), "partial");
    }
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

#[tokio::test]
async fn cancelling_before_upstream_headers_records_an_interrupted_request() {
    let h = Harness::new().await;
    let mut request = Box::pin(h.request("/v1/messages", &h.key, message("slow", false)));
    tokio::select! {
        _ = &mut request => panic!("the slow scenario answered"),
        _ = async {
            while h.mock.requests.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => {}
    }
    // Dropping the in-flight request drops its RequestGuard, which finalizes the row.
    drop(request);
    let row = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row: (String, Option<i64>, String) =
                sqlx::query_as("SELECT outcome,http_status,usage_state FROM request_usage")
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
    assert_eq!(row, ("interrupted".into(), None, "unknown".into()));
}

#[tokio::test]
async fn complete_usage_fills_missing_cache_counters_and_counts_are_not_applicable() {
    let h = Harness::new().await;
    for (path, body) in [
        ("/v1/messages", message("no-cache", false)),
        (
            "/v1/messages/count_tokens",
            json!({"model":MODEL,"messages":[]}),
        ),
    ] {
        assert_eq!(h.request(path, &h.key, body).await.status(), 200);
    }
    type Row = (String, String, String, Option<i64>, Option<i64>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT endpoint,outcome,usage_state,cache_read_tokens,cache_write_tokens \
         FROM request_usage ORDER BY endpoint",
    )
    .fetch_all(&h.state.db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (
                "/v1/messages".into(),
                "completed".into(),
                "complete".into(),
                Some(0),
                Some(0)
            ),
            (
                "/v1/messages/count_tokens".into(),
                "completed".into(),
                "not_applicable".into(),
                None,
                None
            ),
        ]
    );
}
