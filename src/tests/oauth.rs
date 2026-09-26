//! Owner Claude credential: token refresh, re-authentication, PKCE login, and encryption.
use super::*;
use crate::oauth;

#[tokio::test]
async fn concurrent_requests_refresh_once_and_persist_rotation() {
    let h = Harness::new().await;
    sqlx::query("UPDATE claude_credential SET expires_at=0")
        .execute(&h.state.db)
        .await
        .unwrap();
    let (a, b, c) = tokio::join!(
        h.request("/v1/messages", &h.key, message("hello", false)),
        h.request("/v1/messages", &h.key, message("hello", false)),
        h.request("/v1/messages", &h.key, message("hello", false))
    );
    assert_eq!(
        (a.status(), b.status(), c.status()),
        (StatusCode::OK, StatusCode::OK, StatusCode::OK)
    );
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
    let encrypted: Vec<u8> = sqlx::query_scalar("SELECT encrypted_tokens FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&encrypted).contains("rotated-refresh"));
    assert_eq!(
        oauth::decrypt(&h.state.config.encryption_key, &encrypted)
            .unwrap()
            .refresh_token,
        "rotated-refresh"
    );
}

#[tokio::test]
async fn refresh_failure_requires_reconnect_without_secret_leaks() {
    let h = Harness::new().await;
    h.mock.refresh_error.store(true, Ordering::SeqCst);
    sqlx::query("UPDATE claude_credential SET expires_at=0")
        .execute(&h.state.db)
        .await
        .unwrap();
    let response = h
        .request("/v1/messages", &h.key, message("hello", false))
        .await;
    assert_eq!(response.status(), 503);
    let text = json_body(response).await.to_string();
    assert!(!text.contains("must not leak"));
    let state: String = sqlx::query_scalar("SELECT state FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(state, "needs_reauth");
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        503
    );
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn oauth_state_is_session_bound_and_consumed_once() {
    let h = Harness::new().await;
    let started = json_body(h.admin("/admin/api/claude/login", "POST", None).await).await;
    let url = url::Url::parse(started["authorize_url"].as_str().unwrap()).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["redirect_uri"], "http://localhost:54545/callback");
    let unsupported = json!({"redirect_url":format!(
        "http://127.0.0.1:54545/callback?code=abc&state={}", params["state"]
    )});
    assert_eq!(
        h.admin("/admin/api/claude/complete", "POST", Some(unsupported))
            .await
            .status(),
        400
    );
    let wrong = json!({"redirect_url":format!("{}?code=abc&state=wrong",oauth::REDIRECT_URI)});
    assert_eq!(
        h.admin("/admin/api/claude/complete", "POST", Some(wrong))
            .await
            .status(),
        400
    );
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 0);
    let good =
        json!({"redirect_url":format!("{}?code=abc&state={}",oauth::REDIRECT_URI,params["state"])});
    assert_eq!(
        h.admin("/admin/api/claude/complete", "POST", Some(good.clone()))
            .await
            .status(),
        200
    );
    {
        let captures = h.mock.token_captures.lock().await;
        assert_eq!(captures.len(), 1);
        let exchange = &captures[0];
        assert_eq!(exchange["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(exchange["code"], "abc");
        assert_eq!(exchange["state"], params["state"].as_ref());
        use base64::Engine;
        use sha2::Digest;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            sha2::Sha256::digest(exchange["code_verifier"].as_str().unwrap().as_bytes()),
        );
        assert_eq!(challenge, params["code_challenge"]);
    }
    assert_eq!(
        h.admin("/admin/api/claude/complete", "POST", Some(good))
            .await
            .status(),
        400
    );
}

#[test]
fn credential_cipher_rejects_tampering_and_wrong_keys() {
    let tokens = oauth::Tokens {
        access_token: "a".into(),
        refresh_token: "b".into(),
    };
    let mut encrypted = oauth::encrypt(&[7; 32], &tokens).unwrap();
    assert!(oauth::decrypt(&[8; 32], &encrypted).is_err());
    encrypted[25] ^= 1;
    assert!(oauth::decrypt(&[7; 32], &encrypted).is_err());
}

#[tokio::test]
async fn stale_auth_failures_do_not_invalidate_a_new_login() {
    let h = Harness::new().await;
    sqlx::query("UPDATE claude_credential SET generation=2")
        .execute(&h.state.db)
        .await
        .unwrap();
    oauth::mark_reauth(&h.state, 1).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(state, "connected");
    assert_eq!(
        h.request("/v1/messages", &h.key, message("unauthorized", false))
            .await
            .status(),
        401
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        503
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 1);
}
