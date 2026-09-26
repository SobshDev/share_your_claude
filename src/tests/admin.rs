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
        error_message(
            h.admin(
                "/admin/api/models/claude-fable-5-1",
                "PUT",
                Some(json!({"enabled":true}))
            )
            .await,
            403
        )
        .await,
        crate::policy::FABLE_RESERVED
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
    assert_eq!(error_message(r, 403).await, WRONG_ORIGIN);
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
        error_message(
            h.request("/v1/messages", &h.key, message("hello", false))
                .await,
            403
        )
        .await,
        NO_ACCESS
    );
}

/// Error message of an admin or proxy error response, after checking its status, its
/// envelope, and that its error type is the one Anthropic documents for the status.
async fn error_message(response: Response, status: u16) -> String {
    let kind = match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        _ => panic!("no error type listed for {status}"),
    };
    assert_error(response, status, kind, "").await
}

#[tokio::test]
async fn owner_workflow_through_the_admin_api() {
    let h = Harness::new().await;
    let sonnet = format!("/admin/api/models/{MODEL}");
    // A friend is created and renamed.
    let person = json_body(
        h.admin("/admin/api/people", "POST", Some(json!({"name":"Jordan"})))
            .await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let renamed = h
        .admin(
            &format!("/admin/api/people/{person}"),
            "PATCH",
            Some(json!({"name":"  Jordan R.  "})),
        )
        .await;
    assert_eq!(renamed.status(), 204);
    let people = json_body(h.admin("/admin/api/people", "GET", None).await).await;
    let jordan = people
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == person)
        .unwrap();
    assert_eq!(jordan["name"], "Jordan R.");
    // Opus is discovered and enabled; a new key receives both enabled models.
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
    let created = h
        .admin(
            "/admin/api/keys",
            "POST",
            Some(json!({"person_id":person,"label":"Desktop"})),
        )
        .await;
    assert_eq!(created.status(), 201);
    let created = json_body(created).await;
    let (key_id, secret) = (
        created["id"].as_str().unwrap().to_owned(),
        created["secret"].as_str().unwrap().to_owned(),
    );
    // An alias routes to its canonical model, which is what goes upstream.
    assert_eq!(
        h.admin(
            &format!("{sonnet}/aliases"),
            "POST",
            Some(json!({"alias":"team/sonnet_latest"})),
        )
        .await
        .status(),
        201
    );
    let mut body = message("hello", false);
    body["model"] = "team/sonnet_latest".into();
    assert_eq!(h.request("/v1/messages", &secret, body).await.status(), 200);
    assert_eq!(
        h.mock.captures.lock().await.last().unwrap().body["model"],
        MODEL
    );
    // The client configuration lists granted, enabled models and never the secret.
    let config = h
        .admin(&format!("/admin/api/keys/{key_id}/config"), "GET", None)
        .await;
    assert_eq!(config.status(), 200);
    let config = text_body(config).await;
    assert!(!config.contains(&secret));
    assert!(!config.contains("sr_"));
    let config: Value = serde_json::from_str(&config).unwrap();
    assert_eq!(
        config,
        json!({"providers":{"shared-claude":{
            "adapter":"anthropic",
            "baseUrl":ORIGIN,
            "authMode":"key",
            "apiKey":"${SHARED_CLAUDE_API_KEY}",
            "models":["claude-opus-5",MODEL],
        }}})
    );
    // Disabling a model blocks it everywhere at once but keeps the key's grant.
    assert_eq!(
        h.admin(&sonnet, "PUT", Some(json!({"enabled":false})))
            .await
            .status(),
        204
    );
    for model in [MODEL, "team/sonnet_latest"] {
        let mut body = message("hello", false);
        body["model"] = model.into();
        assert_eq!(
            error_message(h.request("/v1/messages", &secret, body).await, 403).await,
            "This key does not have access to that model"
        );
    }
    let bearer = format!("Bearer {secret}");
    let listed = json_body(
        h.get("/v1/models", &[("authorization", bearer.as_str())])
            .await,
    )
    .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], "claude-opus-5");
    let config = json_body(
        h.admin(&format!("/admin/api/keys/{key_id}/config"), "GET", None)
            .await,
    )
    .await;
    assert_eq!(
        config["providers"]["shared-claude"]["models"],
        json!(["claude-opus-5"])
    );
    let keys = json_body(h.admin("/admin/api/keys", "GET", None).await).await;
    let key = keys
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == key_id)
        .unwrap();
    assert_eq!(key["models"], json!(["claude-opus-5", MODEL]));
    assert_eq!(key["person_name"], "Jordan R.");
    // A disabled model can neither be granted nor aliased.
    assert_eq!(
        error_message(
            h.admin(
                &format!("/admin/api/keys/{key_id}/models"),
                "PUT",
                Some(json!({"models":[MODEL]})),
            )
            .await,
            400
        )
        .await,
        "Only reviewed, enabled models other than Fable 5.1 can be granted"
    );
    assert_eq!(
        error_message(
            h.admin(
                &format!("{sonnet}/aliases"),
                "POST",
                Some(json!({"alias":"other"}))
            )
            .await,
            400
        )
        .await,
        "Choose a reviewed and enabled model"
    );
    // Re-enabling restores access through the canonical id and the alias.
    assert_eq!(
        h.admin(&sonnet, "PUT", Some(json!({"enabled":true})))
            .await
            .status(),
        204
    );
    let mut body = message("hello", false);
    body["model"] = "team/sonnet_latest".into();
    assert_eq!(h.request("/v1/messages", &secret, body).await.status(), 200);
    // Revocation is idempotent and blocks the key.
    for _ in 0..2 {
        assert_eq!(
            h.admin(&format!("/admin/api/keys/{key_id}"), "DELETE", None)
                .await
                .status(),
            204
        );
    }
    assert_eq!(
        error_message(
            h.request("/v1/messages", &secret, message("hello", false))
                .await,
            401
        )
        .await,
        AUTH_REQUIRED
    );
    for (path, method, body) in [
        (
            format!("/admin/api/keys/{key_id}/models"),
            "PUT",
            Some(json!({"models":[]})),
        ),
        (format!("/admin/api/keys/{key_id}/config"), "GET", None),
    ] {
        assert_eq!(
            error_message(h.admin(&path, method, body).await, 400).await,
            "Choose an active key"
        );
    }
}

#[tokio::test]
async fn admin_api_rejects_invalid_aliases_grants_and_unknown_ids() {
    let h = Harness::new().await;
    let aliases = format!("/admin/api/models/{MODEL}/aliases");
    assert_eq!(
        h.admin(&aliases, "POST", Some(json!({"alias":"shared-sonnet"})))
            .await
            .status(),
        201
    );
    let long = "a".repeat(151);
    for (alias, message) in [
        ("", "Invalid or reserved alias"),
        ("has space", "Invalid or reserved alias"),
        ("dotted.alias", "Invalid or reserved alias"),
        (long.as_str(), "Invalid or reserved alias"),
        // Aliases cannot replace canonical ids, including the model's own id.
        (MODEL, "That alias is already in use"),
        ("claude-fable-5-1", "Invalid or reserved alias"),
        ("shared-sonnet", "That alias is already in use"),
    ] {
        assert_eq!(
            error_message(
                h.admin(&aliases, "POST", Some(json!({"alias":alias})))
                    .await,
                400
            )
            .await,
            message,
            "{alias}"
        );
    }
    assert_eq!(
        error_message(
            h.admin(
                "/admin/api/models/claude-unknown/aliases",
                "POST",
                Some(json!({"alias":"unknown-alias"})),
            )
            .await,
            400
        )
        .await,
        "Choose a reviewed and enabled model"
    );
    let grants = format!("/admin/api/keys/{}/models", h.key_id);
    // claude-opus-5 has not been discovered in this test, so it is unknown too.
    for models in [json!(["claude-unknown"]), json!([MODEL, "claude-opus-5"])] {
        assert_eq!(
            error_message(
                h.admin(&grants, "PUT", Some(json!({"models":models})))
                    .await,
                400
            )
            .await,
            "Only reviewed, enabled models other than Fable 5.1 can be granted"
        );
    }
    assert_eq!(
        error_message(
            h.admin(&grants, "PUT", Some(json!({"models":vec![MODEL; 201]})))
                .await,
            400
        )
        .await,
        "Too many models"
    );
    // Rejected edits leave the existing grant in place.
    let keys = json_body(h.admin("/admin/api/keys", "GET", None).await).await;
    assert_eq!(keys[0]["models"], json!([MODEL]));
    for (path, method, body, status, message) in [
        (
            "/admin/api/people/unknown".to_owned(),
            "PATCH",
            Some(json!({"name":"Nobody"})),
            404,
            "Friend not found",
        ),
        (
            "/admin/api/keys/unknown".to_owned(),
            "DELETE",
            None,
            404,
            "Key not found",
        ),
        (
            "/admin/api/keys/unknown/models".to_owned(),
            "PUT",
            Some(json!({"models":[]})),
            400,
            "Choose an active key",
        ),
        (
            "/admin/api/keys/unknown/config".to_owned(),
            "GET",
            None,
            400,
            "Choose an active key",
        ),
        (
            "/admin/api/keys".to_owned(),
            "POST",
            Some(json!({"person_id":"unknown","label":"Laptop"})),
            400,
            "Choose an existing friend",
        ),
        (
            "/admin/api/models/claude-unknown".to_owned(),
            "PUT",
            Some(json!({"enabled":true})),
            400,
            "Refresh the catalog to discover this model first",
        ),
        (
            format!("/admin/api/people/{}", h.person),
            "PATCH",
            Some(json!({"name":"   "})),
            400,
            "Enter a name between 1 and 100 characters",
        ),
    ] {
        assert_eq!(
            error_message(h.admin(&path, method, body).await, status).await,
            message,
            "{method} {path}"
        );
    }
}
