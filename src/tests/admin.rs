//! Admin API: model catalog review, key management, and CSRF protection.
use super::*;

#[tokio::test]
async fn catalog_review_key_creation_and_csrf() {
    let h = Harness::new().await;
    assert_eq!(
        h.admin("/admin/api/models/refresh", "POST", None)
            .await
            .status(),
        200
    );
    assert_eq!(
        h.admin(
            "/admin/api/models/claude-opus-5",
            "PUT",
            Some(json!({"enabled":true}))
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        h.admin(
            "/admin/api/models/claude-fable-5-1",
            "PUT",
            Some(json!({"enabled":true}))
        )
        .await
        .status(),
        403
    );
    let created = json_body(
        h.admin(
            "/admin/api/keys",
            "POST",
            Some(json!({"person_id":h.person,"label":"New key"})),
        )
        .await,
    )
    .await;
    assert_eq!(created["secret"].as_str().unwrap().len(), 46);
    let list = json_body(h.admin("/admin/api/keys", "GET", None).await).await;
    assert!(
        !list
            .to_string()
            .contains(created["secret"].as_str().unwrap())
    );
    let grants: Vec<String> =
        sqlx::query_scalar("SELECT model_id FROM key_model_grant WHERE key_id=? ORDER BY model_id")
            .bind(created["id"].as_str().unwrap())
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(grants.len(), 2);
    assert!(!grants.contains(&"claude-fable-5-1".into()));
    // A valid session cookie without origin and CSRF token must not allow writes.
    let r = h
        .send(
            "POST",
            "/admin/api/people",
            &[
                ("cookie", h.session().await.cookie.as_str()),
                ("content-type", "application/json"),
            ],
            "{\"name\":\"Attacker\"}",
        )
        .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn editing_access_keeps_grants_for_disabled_models() {
    let h = Harness::new().await;
    let opus = "/admin/api/models/claude-opus-5";
    let grants = format!("/admin/api/keys/{}/models", h.key_id);
    let bearer = format!("Bearer {}", h.key);
    assert_eq!(
        h.admin("/admin/api/models/refresh", "POST", None)
            .await
            .status(),
        200
    );
    assert_eq!(
        h.admin(opus, "PUT", Some(json!({"enabled":true})))
            .await
            .status(),
        204
    );
    let r = h
        .admin(
            &grants,
            "PUT",
            Some(json!({"models":[MODEL,"claude-opus-5"]})),
        )
        .await;
    assert_eq!(r.status(), 204);
    // Disable Opus, then save a change to another model the way the form does: it submits
    // only enabled models, so Opus is absent from the payload.
    assert_eq!(
        h.admin(opus, "PUT", Some(json!({"enabled":false})))
            .await
            .status(),
        204
    );
    assert_eq!(
        h.admin(&grants, "PUT", Some(json!({"models":[]})))
            .await
            .status(),
        204
    );
    let listed = json_body(
        h.get("/v1/models", &[("authorization", bearer.as_str())])
            .await,
    )
    .await;
    assert_eq!(listed["data"], json!([]));
    assert_eq!(
        h.admin(opus, "PUT", Some(json!({"enabled":true})))
            .await
            .status(),
        204
    );
    let listed = json_body(
        h.get("/v1/models", &[("authorization", bearer.as_str())])
            .await,
    )
    .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], "claude-opus-5");
    let mut body = message("hello", false);
    body["model"] = "claude-opus-5".into();
    // count_tokens: the mock always reports MODEL as the serving model of a message.
    assert_eq!(
        h.request("/v1/messages/count_tokens", &h.key, body)
            .await
            .status(),
        200
    );
    // The enabled model that was unchecked stays removed.
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        403
    );
}
