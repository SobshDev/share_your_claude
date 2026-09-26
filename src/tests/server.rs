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
/// Collects formatted log output of the current thread.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}
impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[tokio::test]
async fn failures_log_one_sanitized_category_each() {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);
    let h = Harness::new().await;
    let cases = [
        ("wrong-model", false, "model_mismatch"),
        ("not-json", false, "malformed"),
        ("status-500", false, "status"),
        ("truncated", true, "truncated"),
        ("stream-error", true, "stream_error"),
    ];
    for (scenario, stream, _) in cases {
        let mut body = message(scenario, stream);
        body["system"] = json!("private-prompt-text");
        let response = h.request("/v1/messages", &h.key, body).await;
        text_body(response).await;
        settled(&h).await;
    }
    let failure = sqlx::query("INSERT INTO person(id,name,created_at) VALUES(?,?,?)")
        .bind(&h.person)
        .bind("Duplicate")
        .bind(db::now())
        .execute(&h.state.db)
        .await
        .unwrap_err();
    let _ = crate::error::AppError::from(failure);

    let text = logs.text();
    let failures: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("upstream request failed"))
        .collect();
    assert_eq!(failures.len(), cases.len(), "{text}");
    for ((scenario, _, category), line) in cases.iter().zip(&failures) {
        assert!(
            line.contains(&format!("category=\"{category}\"")),
            "{scenario}: {line}"
        );
        assert!(line.contains("request_id="), "{line}");
    }
    let database = text
        .lines()
        .find(|line| line.contains("database operation failed"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(database.contains("kind=\"unique_violation\""), "{database}");
    assert!(database.contains("at=src/tests/server.rs:"), "{database}");
    for secret in [
        h.key.as_str(),
        "owner-access-must-not-leak",
        "owner-refresh",
        "private-prompt-text",
        "INSERT",
        "Duplicate",
        "owner-secret",
    ] {
        assert!(!text.contains(secret), "logs contain {secret}: {text}");
    }
}
#[tokio::test]
async fn rejections_name_the_part_of_the_request_that_was_invalid() {
    let h = Harness::new().await;
    let response = h.admin("/admin/api/analytics?offset=-1", "GET", None).await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "Invalid query parameters");
    let session = h.session().await;
    let response = h
        .send(
            "POST",
            "/admin/api/people",
            &[
                ("content-type", "application/json"),
                ("cookie", session.cookie.as_str()),
                ("origin", ORIGIN),
                ("x-csrf-token", session.csrf.as_str()),
            ],
            "{",
        )
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        json_body(response).await["error"]["message"],
        "Invalid JSON request body"
    );
}
fn cache_control(response: &Response) -> &str {
    response.headers()["cache-control"].to_str().unwrap()
}

#[tokio::test]
async fn versioned_assets_are_cacheable_and_pages_are_not() {
    let h = Harness::new().await;
    let version = crate::admin::ASSET_VERSION.as_str();
    let login = h.get("/admin/login", &[]).await;
    assert_eq!(cache_control(&login), "no-store");
    let html = text_body(login).await;
    for asset in ["/assets/app.js", "/assets/app.css"] {
        assert!(html.contains(&format!("{asset}?v={version}\"")), "{html}");
        let versioned = h.get(&format!("{asset}?v={version}"), &[]).await;
        assert_eq!(versioned.status(), 200);
        assert_eq!(
            cache_control(&versioned),
            "public, max-age=31536000, immutable"
        );
        // The CSP and other defaults still apply to assets.
        assert_eq!(versioned.headers()["x-content-type-options"], "nosniff");
        for stale in [asset.to_owned(), format!("{asset}?v=old")] {
            assert_eq!(
                cache_control(&h.get(&stale, &[]).await),
                "no-cache",
                "{stale}"
            );
        }
    }
    let cookie = h.session().await.cookie.clone();
    let dashboard = h.get("/admin", &[("cookie", cookie.as_str())]).await;
    assert_eq!(cache_control(&dashboard), "no-store");
    assert!(
        text_body(dashboard)
            .await
            .contains(&format!("app.js?v={version}"))
    );
    let api = h.admin("/admin/api/me", "GET", None).await;
    assert_eq!(cache_control(&api), "no-store");
    assert!(!api.headers().contains_key("strict-transport-security"));
}

#[tokio::test]
async fn https_deployments_send_hsts() {
    let h = Harness::new().await;
    let (_, router) = h.variant(|state| state.config.secure_cookie = true);
    for path in ["/healthz", "/admin/login", "/assets/app.css"] {
        let response = send_to(&router, "GET", path, &[], Body::empty()).await;
        assert_eq!(
            response.headers()["strict-transport-security"],
            "max-age=31536000",
            "{path}"
        );
    }
}
