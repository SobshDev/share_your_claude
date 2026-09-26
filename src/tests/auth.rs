//! Client key authentication, admin/client isolation, key revocation, and admin sign-in.
use super::*;

#[tokio::test]
async fn auth_conflicts_admin_isolation_and_revocation() {
    let h = Harness::new().await;
    let r = h
        .get(
            "/v1/models",
            &[
                ("x-api-key", h.key.as_str()),
                ("authorization", "Bearer different"),
            ],
        )
        .await;
    assert_eq!(r.status(), 401);
    let r = h
        .get("/admin/api/keys", &[("x-api-key", h.key.as_str())])
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        h.admin(
            &format!("/admin/api/keys/{}/models", h.key_id),
            "PUT",
            Some(json!({"models":[]}))
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        403
    );
    assert_eq!(
        h.admin(&format!("/admin/api/keys/{}", h.key_id), "DELETE", None)
            .await
            .status(),
        204
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        401
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn login_sets_cookie_and_throttles() {
    let h = Harness::new().await;
    let response = h.login(ADMIN_PASSWORD).await;
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    for _ in 0..4 {
        assert_eq!(h.login("wrong").await.status(), 401);
    }
    assert_eq!(h.login("wrong").await.status(), 429);
}

async fn last_used(h: &Harness) -> Option<String> {
    sqlx::query_scalar("SELECT last_used_at FROM api_key WHERE id=?")
        .bind(&h.key_id)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn set_last_used(h: &Harness, seconds_ago: i64) -> String {
    let value = (chrono::Utc::now() - chrono::Duration::seconds(seconds_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE api_key SET last_used_at=? WHERE id=?")
        .bind(&value)
        .bind(&h.key_id)
        .execute(&h.state.db)
        .await
        .unwrap();
    value
}

#[tokio::test]
async fn last_used_is_written_at_most_once_a_minute() {
    let h = Harness::new().await;
    async fn models(h: &Harness) -> StatusCode {
        h.get("/v1/models", &[("x-api-key", h.key.as_str())])
            .await
            .status()
    }
    assert_eq!(last_used(&h).await, None);
    assert_eq!(models(&h).await, 200);
    let first = last_used(&h).await.expect("first use is recorded");
    assert_eq!(models(&h).await, 200);
    assert_eq!(last_used(&h).await.as_ref(), Some(&first));
    let recent = set_last_used(&h, 30).await;
    assert_eq!(models(&h).await, 200);
    assert_eq!(last_used(&h).await, Some(recent));
    let old = set_last_used(&h, 120).await;
    assert_eq!(models(&h).await, 200);
    assert!(last_used(&h).await.unwrap() > old);
}

#[tokio::test]
async fn key_authentication_survives_a_failed_last_used_write() {
    let h = Harness::new().await;
    sqlx::query(
        "CREATE TRIGGER block_last_used BEFORE UPDATE OF last_used_at ON api_key \
         BEGIN SELECT RAISE(ABORT,'blocked'); END",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    let response = h.get("/v1/models", &[("x-api-key", h.key.as_str())]).await;
    assert_eq!(response.status(), 200);
    assert_eq!(last_used(&h).await, None);
}
