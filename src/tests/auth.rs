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
