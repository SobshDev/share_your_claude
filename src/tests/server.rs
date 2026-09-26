//! Server lane: catalog refresh credential handling, shutdown, timeouts, logging, and
//! static asset caching.
use super::*;

/// Serves `service` on an ephemeral loopback port and returns its base URL.
async fn serve(service: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    format!("http://{address}")
}

/// A `/v1/models` upstream that answers every request with `reply`, given the bearer token.
async fn models_upstream(reply: fn(&str) -> Response) -> String {
    let service = Router::new().route(
        "/v1/models",
        axum::routing::get(move |headers: HeaderMap| async move {
            reply(headers["authorization"].to_str().unwrap())
        }),
    );
    serve(service).await
}

fn upstream_error(status: StatusCode, kind: &str) -> Response {
    let body = json!({"type":"error","error":{"type":kind,"message":"provider-secret"}});
    (status, Json(body)).into_response()
}

/// State and generation of the stored Claude credential.
async fn credential(h: &Harness) -> (String, i64) {
    sqlx::query_as("SELECT state,generation FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn refresh_catalog(h: &Harness, upstream: String) -> Response {
    let (_, router) = h.variant(|state| state.upstream = upstream);
    let session = sign_in(&router).await;
    admin_as(&router, &session, "POST", "/admin/api/models/refresh", None).await
}

#[tokio::test]
async fn catalog_refresh_renews_a_rejected_token_once_and_lists_again() {
    let h = Harness::new().await;
    let upstream = models_upstream(|bearer| {
        if bearer == "Bearer refreshed-access" {
            Json(json!({"data":[{"id":"claude-new-1","display_name":"New"}],"has_more":false}))
                .into_response()
        } else {
            upstream_error(StatusCode::UNAUTHORIZED, "authentication_error")
        }
    })
    .await;
    let response = refresh_catalog(&h, upstream).await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["discovered"], 1);
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(credential(&h).await, ("connected".into(), 2));
}

#[tokio::test]
async fn catalog_refresh_permission_error_keeps_claude_connected() {
    let h = Harness::new().await;
    let upstream =
        models_upstream(|_| upstream_error(StatusCode::FORBIDDEN, "permission_error")).await;
    let response = refresh_catalog(&h, upstream).await;
    assert_eq!(response.status(), 403);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "permission_error");
    assert_eq!(
        body["error"]["message"],
        "Claude did not allow listing models"
    );
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(credential(&h).await, ("connected".into(), 1));
}

#[tokio::test]
async fn catalog_refresh_authentication_403_requires_reconnect() {
    let h = Harness::new().await;
    let upstream =
        models_upstream(|_| upstream_error(StatusCode::FORBIDDEN, "authentication_error")).await;
    let response = refresh_catalog(&h, upstream).await;
    assert_eq!(response.status(), 503);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(!body.to_string().contains("provider-secret"));
    assert_eq!(credential(&h).await.0, "needs_reauth");
}
