//! Analytics reporting: scoping, totals, validation, and pagination.
use super::*;

#[tokio::test]
async fn analytics_scopes_totals_models_history_and_preserves_unknown_usage() {
    let h = Harness::new().await;
    let second = add_person(&h.state, "Sam").await;
    let (second_key, _) = add_key(&h.state, &second, "Laptop").await;
    let (rotated, _) = add_key(&h.state, &h.person, "New laptop").await;
    // Same key labels and a rotated/revoked key must retain their distinct attribution.
    for (
        index,
        key,
        endpoint,
        requested,
        resolved,
        outcome,
        measurement,
        tokens,
        started,
        finished,
    ) in [
        (
            0,
            h.key_id.as_str(),
            "/v1/messages",
            "shared-sonnet",
            Some(MODEL),
            "completed",
            "complete",
            Some(10),
            "2026-09-01T00:00:00.000Z",
            Some("2026-09-01T00:00:02.000Z"),
        ),
        (
            1,
            rotated.as_str(),
            "/v1/messages",
            MODEL,
            Some(MODEL),
            "interrupted",
            "partial",
            Some(5),
            "2026-09-02T00:00:00.000Z",
            Some("2026-09-02T00:00:04.000Z"),
        ),
        (
            2,
            second_key.as_str(),
            "/v1/messages",
            "claude-opus-5",
            Some("claude-opus-5"),
            "completed",
            "unknown",
            None,
            "2026-09-02T01:00:00.000Z",
            Some("2026-09-02T01:00:06.000Z"),
        ),
        (
            3,
            h.key_id.as_str(),
            "/v1/messages",
            "unresolved-model",
            None,
            "denied",
            "not_applicable",
            None,
            "2026-09-02T02:00:00.000Z",
            Some("2026-09-02T02:00:00.010Z"),
        ),
        (
            4,
            h.key_id.as_str(),
            "/v1/messages/count_tokens",
            MODEL,
            Some(MODEL),
            "completed",
            "complete",
            Some(999),
            "2026-09-02T03:00:00.000Z",
            Some("2026-09-02T03:00:01.000Z"),
        ),
        (
            5,
            h.key_id.as_str(),
            "/v1/messages",
            MODEL,
            Some(MODEL),
            "completed",
            "complete",
            Some(999),
            "2026-09-03T00:00:00.000Z",
            Some("2026-09-03T00:00:01.000Z"),
        ),
    ] {
        sqlx::query("INSERT INTO request_usage(id,key_id,endpoint,requested_model,resolved_model,started_at,finished_at,outcome,usage_state,input_tokens,output_tokens,raw_usage) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(format!("analytics-{index}")).bind(key).bind(endpoint).bind(requested).bind(resolved).bind(started).bind(finished).bind(outcome).bind(measurement).bind(tokens).bind(tokens).bind(tokens.map(|_| "{}"))
            .execute(&h.state.db).await.unwrap();
    }
    h.admin(&format!("/admin/api/keys/{}", h.key_id), "DELETE", None)
        .await;
    let range = "from=2026-09-01T00:00:00Z&to=2026-09-03T00:00:00Z";
    let report = json_body(
        h.admin(&format!("/admin/api/analytics?{range}"), "GET", None)
            .await,
    )
    .await;
    let total = &report["total"][0];
    assert_eq!(total["requests"], 4);
    assert_eq!(total["active_users"], 2);
    assert_eq!(total["models_used"], 2);
    assert_eq!(total["observed_total_tokens"], 30);
    assert_eq!(total["incomplete_requests"], 2);
    assert_eq!(total["denied_requests"], 1);
    assert_eq!(total["errors"], 1);
    assert_eq!(total["duration_samples"], 2);
    assert!((total["average_duration_ms"].as_f64().unwrap() - 4000.0).abs() < 1.0);
    assert_eq!(report["day"].as_array().unwrap().len(), 2);
    assert_eq!(report["key"].as_array().unwrap().len(), 3);
    assert_eq!(report["model"].as_array().unwrap().len(), 3);
    assert_eq!(report["requests"][0]["requested_model"], "unresolved-model");
    assert_eq!(report["requests"][3]["requested_model"], "shared-sonnet");
    for group in ["person", "key", "model", "day"] {
        assert_eq!(
            report[group]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["requests"].as_i64().unwrap())
                .sum::<i64>(),
            4
        );
    }
    let personal = json_body(
        h.admin(
            &format!(
                "/admin/api/analytics?{range}&person_id={}&model={MODEL}",
                h.person
            ),
            "GET",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(personal["total"][0]["requests"], 2);
    assert_eq!(personal["total"][0]["observed_total_tokens"], 30);
    assert_eq!(personal["model"][0]["id"], MODEL);
    assert!(
        personal["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["person_id"] == h.person)
    );
    let unknown = json_body(
        h.admin(
            &format!("/admin/api/analytics?{range}&person_id={second}"),
            "GET",
            None,
        )
        .await,
    )
    .await;
    assert!(unknown["total"][0]["observed_total_tokens"].is_null());
    assert_eq!(unknown["total"][0]["unknown_requests"], 1);
    let key = json_body(
        h.admin(
            &format!("/admin/api/analytics?{range}&key_id={rotated}"),
            "GET",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(key["total"][0]["requests"], 1);
    let legacy = json_body(
        h.admin(
            &format!("/admin/api/usage?{range}&model={MODEL}&group_by=model"),
            "GET",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(legacy["data"][0]["requests"], 2);
    assert!(report["requests"][0].get("raw_usage").is_none());
    assert!(report["requests"][0].get("secret_hash").is_none());
}

#[tokio::test]
async fn analytics_empty_ranges_validation_and_pagination() {
    let h = Harness::new().await;
    let anonymous = h.get("/admin/api/analytics", &[]).await;
    assert_eq!(anonymous.status(), 401);
    let empty = json_body(h.admin("/admin/api/analytics", "GET", None).await).await;
    assert_eq!(empty["total"][0]["requests"], 0);
    assert!(empty["total"][0]["observed_total_tokens"].is_null());
    assert!(empty["total"][0]["average_duration_ms"].is_null());
    assert_eq!(empty["has_more"], false);
    for query in [
        "from=invalid",
        "from=2026-09-02T00:00:00Z&to=2026-09-01T00:00:00Z",
        "offset=-1",
        "offset=4294967296",
    ] {
        assert_eq!(
            h.admin(&format!("/admin/api/analytics?{query}"), "GET", None)
                .await
                .status(),
            400
        );
    }
    assert_eq!(
        h.admin("/admin/api/usage?group_by=invalid", "GET", None)
            .await
            .status(),
        400
    );
    for index in 0..51 {
        sqlx::query("INSERT INTO request_usage(id,key_id,endpoint,requested_model,started_at,outcome) VALUES(?,?,'/v1/messages',?,'2026-09-02T00:00:00.000Z','in_progress')")
            .bind(format!("page-{index:03}")).bind(&h.key_id).bind(MODEL).execute(&h.state.db).await.unwrap();
    }
    let range = "from=2026-09-01T00:00:00Z&to=2026-09-03T00:00:00Z";
    let first = json_body(
        h.admin(&format!("/admin/api/analytics?{range}"), "GET", None)
            .await,
    )
    .await;
    let next = json_body(
        h.admin(
            &format!("/admin/api/analytics?{range}&offset=50"),
            "GET",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(first["total"][0]["requests"], 51);
    assert_eq!(first["total"][0]["in_progress_requests"], 51);
    assert_eq!(first["requests"].as_array().unwrap().len(), 50);
    assert_eq!(first["has_more"], true);
    assert_eq!(next["requests"].as_array().unwrap().len(), 1);
    assert_eq!(next["requests"][0]["id"], "page-000");
    assert_eq!(next["has_more"], false);
    assert!(
        !first["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == next["requests"][0]["id"])
    );
    let injection = json_body(
        h.admin(
            &format!("/admin/api/analytics?{range}&model=%27%20OR%201%3D1--"),
            "GET",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(injection["total"][0]["requests"], 0);
}
