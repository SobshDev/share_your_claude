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
/// A harness whose state was changed by `edit` before its router was built.
fn rebuilt(mut h: Harness, edit: impl FnOnce(&mut AppState)) -> Harness {
    // The router holds the only other references to the state; rebuild it around the change.
    h.router = Router::new();
    edit(Arc::get_mut(&mut h.state).unwrap());
    h.router = app(h.state.clone());
    h
}

/// Waits for the latest request to reach a terminal outcome and returns it with its status.
async fn settled(h: &Harness) -> (String, Option<i64>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row: (String, Option<i64>) = sqlx::query_as(
                "SELECT outcome,http_status FROM request_usage ORDER BY rowid DESC LIMIT 1",
            )
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
    .unwrap()
}

#[tokio::test]
async fn a_stream_past_the_duration_limit_ends_with_a_timeout_error() {
    let h = rebuilt(Harness::new().await, |state| {
        state.max_stream_duration = Duration::from_millis(100);
    });
    let response = h
        .request("/v1/messages", &h.key, message("disconnect", true))
        .await;
    assert_eq!(response.status(), 200);
    let body = tokio::time::timeout(Duration::from_secs(5), text_body(response))
        .await
        .unwrap();
    assert!(body.contains("event: message_start"), "{body}");
    assert!(body.contains("\"type\":\"timeout_error\""), "{body}");
    assert_eq!(settled(&h).await, ("interrupted".into(), Some(200)));
    // The router hung up on the upstream stream.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !h.mock.disconnected.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn readiness_fails_once_shutdown_begins() {
    let h = Harness::new().await;
    assert_eq!(h.get("/readyz", &[]).await.status(), 200);
    h.state.phase.send_replace(crate::Phase::Draining);
    let response = h.get("/readyz", &[]).await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        json_body(response).await["error"]["type"],
        "overloaded_error"
    );
    // Open requests still complete while draining.
    let response = h
        .request("/v1/messages", &h.key, message("hello", false))
        .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn stopping_interrupts_open_streams_and_keeps_their_usage() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("disconnect", true))
        .await;
    assert_eq!(response.status(), 200);
    let mut body = response.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("message_start"));
    h.state.phase.send_replace(crate::Phase::Stopping);
    let mut rest = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = body.next().await {
            rest.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await
    .unwrap();
    assert!(rest.contains("event: error"), "{rest}");
    assert_eq!(settled(&h).await, ("interrupted".into(), Some(200)));
    let (usage, input, finished): (String, Option<i64>, Option<String>) =
        sqlx::query_as("SELECT usage_state,input_tokens,finished_at FROM request_usage")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(usage, "partial");
    assert!(input.is_some());
    assert!(finished.is_some());
}
