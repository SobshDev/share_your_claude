use super::*;
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

const MODEL: &str = "claude-sonnet-4-6";
#[derive(Default)]
struct Mock {
    requests: AtomicUsize,
    refreshes: AtomicUsize,
    disconnected: AtomicBool,
    captures: Mutex<Vec<(HeaderMap, Value)>>,
    refresh_error: AtomicBool,
}
async fn mock_message(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    mock.requests.fetch_add(1, Ordering::SeqCst);
    mock.captures.lock().await.push((headers, body.clone()));
    let content = body
        .pointer("/messages/0/content")
        .and_then(Value::as_str)
        .unwrap_or("hello");
    if content == "error" {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "20")],
            Json(json!({"error":{"message":"sensitive provider echo"}})),
        )
            .into_response();
    }
    if content == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"owner-secret"}})),
        )
            .into_response();
    }
    if content == "redirect" {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "http://127.0.0.1:9/should-not-follow")],
        )
            .into_response();
    }
    if body["stream"] == true {
        let prefix = format!(
            "event: message_start\r\ndata: {}\r\n\r\n",
            json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}}})
        );
        if content == "disconnect" {
            let (tx, rx) =
                tokio::sync::mpsc::channel::<std::result::Result<bytes::Bytes, std::io::Error>>(1);
            tokio::spawn(async move {
                let _ = tx.send(Ok(prefix.into())).await;
                tx.closed().await;
                mock.disconnected.store(true, Ordering::SeqCst);
            });
            return (
                [("content-type", "text/event-stream")],
                Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
            )
                .into_response();
        }
        let mut stream = prefix;
        stream.push_str("event: ping\ndata: {\"type\":\"ping\"}\n\n");
        stream.push_str("event: future_event\ndata: {\"type\":\"future_event\",\"value\":1}\n\n");
        if let Some(name) = body.pointer("/tools/0/name") {
            stream.push_str(&format!("event: content_block_start\ndata: {}\n\n",json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":name,"input":{}}})));
            stream.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n");
            stream.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        }
        if content != "no-final-usage" {
            stream.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n");
            stream.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n");
        }
        if content == "stream-error" {
            stream.push_str("event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"secret upstream text\"}}\n\n");
        } else if content != "truncated" {
            stream.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        }
        let chunks: Vec<_> = stream
            .as_bytes()
            .chunks(7)
            .map(|s| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(s)))
            .collect();
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
            .into_response();
    }
    let mut response = json!({"id":"msg_test","type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}});
    if content == "missing" {
        response.as_object_mut().unwrap().remove("usage");
    }
    if content == "wrong-model" {
        response["model"] = "claude-fable-5-1".into();
    }
    if let Some(name) = body.pointer("/tools/0/name") {
        response["content"] =
            json!([{"type":"tool_use","id":"tool_1","name":name,"input":{"name":"do not alter"}}]);
    }
    Json(response).into_response()
}
async fn mock_token(
    State(mock): State<Arc<Mock>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    mock.refreshes.fetch_add(1, Ordering::SeqCst);
    if mock.refresh_error.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant","secret":"must not leak"})),
        )
            .into_response();
    }
    assert!(body["grant_type"] == "refresh_token" || body["grant_type"] == "authorization_code");
    tokio::time::sleep(Duration::from_millis(30)).await;
    Json(json!({"access_token":"refreshed-access","refresh_token":"rotated-refresh","expires_in":3600})).into_response()
}

struct Harness {
    state: Arc<AppState>,
    mock: Arc<Mock>,
    router: Router,
    _dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
    key: String,
    key_id: String,
    person: String,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Harness {
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let service=Router::new().route("/v1/messages",post(mock_message)).route("/v1/messages/count_tokens",post(||async{Json(json!({"input_tokens":123}))}))
            .route("/v1/oauth/token",post(mock_token))
            .route("/v1/models",get(||async{Json(json!({"data":[{"id":MODEL,"display_name":"Claude Sonnet 4.6"},{"id":"claude-opus-5","display_name":"Claude Opus 5"},{"id":"claude-fable-5-1","display_name":"Claude Fable 5.1"}],"has_more":false}))})).with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("test.sqlite").display());
        let pool = db::connect(&url).await.unwrap();
        let config = config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database_url: url,
            public_origin: "http://localhost:8080".into(),
            password_hash: password_hash(),
            encryption_key: zeroize::Zeroizing::new([7; 32]),
            secure_cookie: false,
        };
        let mut state = AppState::new(config, pool).unwrap();
        {
            let inner = Arc::get_mut(&mut state).unwrap();
            inner.upstream = format!("http://{address}");
            inner.token_endpoint = format!("http://{address}/v1/oauth/token");
        }
        let encrypted = oauth::encrypt(
            &state.config.encryption_key,
            &oauth::Tokens {
                access_token: "owner-access-must-not-leak".into(),
                refresh_token: "owner-refresh".into(),
            },
        )
        .unwrap();
        sqlx::query("INSERT INTO claude_credential VALUES(1,?,?,1,'connected')")
            .bind(encrypted)
            .bind(db::epoch() + 3600)
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO model VALUES(?,?,?,1,?)")
            .bind(MODEL)
            .bind("Sonnet")
            .bind("sonnet")
            .bind(db::now())
            .execute(&state.db)
            .await
            .unwrap();
        let person = db::id();
        sqlx::query("INSERT INTO person VALUES(?,?,?)")
            .bind(&person)
            .bind("Alex")
            .bind(db::now())
            .execute(&state.db)
            .await
            .unwrap();
        let (key_id, key) = add_key(&state, &person, "Laptop").await;
        let router = app(state.clone());
        Self {
            state,
            mock,
            router,
            _dir: dir,
            server,
            key,
            key_id,
            person,
        }
    }
    async fn request(&self, path: &str, key: &str, body: Value) -> axum::response::Response {
        self.router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-api-key", key)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    async fn admin(
        &self,
        path: &str,
        method: &str,
        body: Option<Value>,
    ) -> axum::response::Response {
        let token = "s".repeat(43);
        sqlx::query("INSERT OR REPLACE INTO admin_session VALUES(?,?,?)")
            .bind(auth::hash(&token))
            .bind("csrf-test")
            .bind(db::epoch() + 3600)
            .execute(&self.state.db)
            .await
            .unwrap();
        self.router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .method(method)
                    .header("content-type", "application/json")
                    .header("cookie", format!("router_session={token}"))
                    .header("origin", "http://localhost:8080")
                    .header("x-csrf-token", "csrf-test")
                    .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}
fn password_hash() -> String {
    static HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        use argon2::PasswordHasher;
        argon2::Argon2::default()
            .hash_password(
                b"a-long-test-password",
                &argon2::password_hash::SaltString::encode_b64(b"test-salt-16-byte").unwrap(),
            )
            .unwrap()
            .to_string()
    });
    HASH.clone()
}
async fn add_key(state: &AppState, person: &str, label: &str) -> (String, String) {
    let secret = format!("sr_{}", auth::random_secret());
    let id = db::id();
    sqlx::query(
        "INSERT INTO api_key(id,person_id,label,prefix,secret_hash,created_at) VALUES(?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(person)
    .bind(label)
    .bind(&secret[..11])
    .bind(auth::hash(&secret))
    .bind(db::now())
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO key_model_grant VALUES(?,?)")
        .bind(&id)
        .bind(MODEL)
        .execute(&state.db)
        .await
        .unwrap();
    (id, secret)
}
fn message(content: &str, stream: bool) -> Value {
    json!({"model":MODEL,"max_tokens":128,"stream":stream,"messages":[{"role":"user","content":content}]})
}
async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn policy_blocks_before_upstream_and_models_are_filtered() {
    let h = Harness::new().await;
    // Even an erroneous database grant cannot enable the reserved model.
    sqlx::query("UPDATE model SET enabled=1,reviewed_at=? WHERE id='claude-fable-5-1'")
        .bind(db::now())
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO key_model_grant VALUES(?,'claude-fable-5-1')")
        .bind(&h.key_id)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO model_alias VALUES('fable-latest','claude-fable-5-1')")
        .execute(&h.state.db)
        .await
        .unwrap();
    for model in [
        "claude-fable-5-1",
        "fable-latest",
        "claude-fable-5-1-20260901",
        "unknown",
    ] {
        let mut body = message("hello", false);
        body["model"] = model.into();
        assert_eq!(h.request("/v1/messages", &h.key, body).await.status(), 403);
    }
    let mut body = message("hello", false);
    body["fallback"] = json!({"model":"claude-fable-5-1"});
    assert_eq!(h.request("/v1/messages", &h.key, body).await.status(), 400);
    let mut body = message("hello", false);
    body["tools"] =
        json!([{"type":"advisor_20260901","name":"advisor","model":"claude-fable-5-1"}]);
    assert_eq!(h.request("/v1/messages", &h.key, body).await.status(), 400);
    let r = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", format!("Bearer {}", h.key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let data = json_body(r).await;
    assert_eq!(data["data"].as_array().unwrap().len(), 1);
    assert_eq!(data["data"][0]["id"], MODEL);
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
    let r = h
        .request("/v1/messages/batches", &h.key, message("hello", false))
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn auth_conflicts_admin_isolation_and_revocation() {
    let h = Harness::new().await;
    let r = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("x-api-key", &h.key)
                .header("authorization", "Bearer different")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/api/keys")
                .header("x-api-key", &h.key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        h.admin(
            &format!("keys/{}/models", h.key_id).replacen("keys/", "/admin/api/keys/", 1),
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
async fn stream_usage_is_cumulative_and_tool_names_round_trip() {
    let h = Harness::new().await;
    let mut body = message("hello", true);
    body["tools"] = json!([{"name":"custom_search","input_schema":{"type":"object"}}]);
    let response = h.request("/v1/messages", &h.key, body).await;
    assert_eq!(response.status(), 200);
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("\"name\":\"custom_search\""));
    assert!(text.contains("future_event"));
    assert!(text.contains("message_stop"));
    let row = sqlx::query("SELECT * FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("outcome"), "completed");
    assert_eq!(row.get::<String, _>("usage_state"), "complete");
    assert_eq!(row.get::<i64, _>("input_tokens"), 10);
    assert_eq!(row.get::<i64, _>("output_tokens"), 7);
    assert_eq!(row.get::<i64, _>("cache_read_tokens"), 20);
    assert_eq!(row.get::<i64, _>("cache_write_tokens"), 30);
    let captures = h.mock.captures.lock().await;
    let (headers, body) = &captures[0];
    assert_eq!(
        headers["authorization"],
        "Bearer owner-access-must-not-leak"
    );
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(body["system"][0]["text"], oauth::SYSTEM);
    assert!(
        body["tools"][0]["name"]
            .as_str()
            .unwrap()
            .starts_with("custom_")
    );
    assert!(!text.contains("owner-access"));
}

#[tokio::test]
async fn people_totals_survive_key_rotation_and_exclude_estimates() {
    let h = Harness::new().await;
    let second = db::id();
    sqlx::query("INSERT INTO person VALUES(?,?,?)")
        .bind(&second)
        .bind("Sam")
        .bind(db::now())
        .execute(&h.state.db)
        .await
        .unwrap();
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
        let text = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
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

#[tokio::test]
async fn disconnect_cancels_upstream_and_preserves_checkpoint() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("disconnect", true))
        .await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.is_some());
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.mock.disconnected.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let row = sqlx::query("SELECT usage_state,outcome,input_tokens FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("usage_state"), "partial");
    assert_eq!(row.get::<String, _>("outcome"), "interrupted");
    assert_eq!(row.get::<i64, _>("input_tokens"), 10);
}

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
async fn upstream_errors_are_sanitized_and_never_replayed() {
    let h = Harness::new().await;
    let r = h
        .request("/v1/messages", &h.key, message("error", false))
        .await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "20");
    assert!(
        !json_body(r)
            .await
            .to_string()
            .contains("sensitive provider echo")
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("redirect", false))
            .await
            .status(),
        502
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);
}

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
    let r = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/api/people")
                .method("POST")
                .header("cookie", format!("router_session={}", "s".repeat(43)))
                .header("content-type", "application/json")
                .body(Body::from("{\"name\":\"Attacker\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn oauth_state_is_session_bound_and_consumed_once() {
    let h = Harness::new().await;
    let started = json_body(h.admin("/admin/api/claude/login", "POST", None).await).await;
    let url = url::Url::parse(started["authorize_url"].as_str().unwrap()).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["code_challenge_method"], "S256");
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
    assert_eq!(
        h.admin("/admin/api/claude/complete", "POST", Some(good))
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn login_sets_cookie_and_throttles() {
    let h = Harness::new().await;
    let login = |password: &str| {
        Request::builder()
            .uri("/admin/api/login")
            .method("POST")
            .header("origin", "http://localhost:8080")
            .header("content-type", "application/json")
            .body(Body::from(json!({"password":password}).to_string()))
            .unwrap()
    };
    let response = h
        .router
        .clone()
        .oneshot(login("a-long-test-password"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    for _ in 0..4 {
        assert_eq!(
            h.router
                .clone()
                .oneshot(login("wrong"))
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        h.router
            .clone()
            .oneshot(login("wrong"))
            .await
            .unwrap()
            .status(),
        429
    );
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
fn tool_mapping_preserves_distinct_prefixed_names_and_payloads() {
    let mut body = message("hello", false);
    body["tools"] = json!([{"name":"search","input_schema":{"type":"object"}},{"name":"custom_search","input_schema":{"type":"object"}}]);
    let map = policy::ToolMap::prepare(&mut body).unwrap();
    assert_ne!(body["tools"][0]["name"], body["tools"][1]["name"]);
    let mut response = json!({"content":[{"type":"tool_use","name":body["tools"][1]["name"],"input":{"type":"tool_use","name":"unchanged"}}]});
    map.restore(&mut response);
    assert_eq!(response["content"][0]["name"], "custom_search");
    assert_eq!(response["content"][0]["input"]["name"], "unchanged");
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

#[test]
fn builtin_tools_keep_their_names_through_history() {
    let mut body = message("hello", false);
    body["tools"] = json!([{"name":"str_replace_editor","type":"text_editor_20250124"}]);
    body["messages"] = json!([{"role":"assistant","content":[{"type":"tool_use","name":"str_replace_editor","id":"t1","input":{"command":"view"}}]}]);
    policy::ToolMap::prepare(&mut body).unwrap();
    assert_eq!(body["tools"][0]["name"], "str_replace_editor");
    assert_eq!(
        body["messages"][0]["content"][0]["name"],
        "str_replace_editor"
    );
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
async fn unexpected_serving_model_is_recorded_and_response_rejected() {
    let h = Harness::new().await;
    let response = h
        .request("/v1/messages", &h.key, message("wrong-model", false))
        .await;
    assert_eq!(response.status(), 502);
    let row = sqlx::query("SELECT outcome,response_model,usage_state FROM request_usage")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("outcome"), "upstream_error");
    assert_eq!(row.get::<String, _>("response_model"), "claude-fable-5-1");
    assert_eq!(row.get::<String, _>("usage_state"), "partial");
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

#[tokio::test]
#[ignore = "requires Bun and OPENCODEX_SOURCE pointing at an installed opencodex package"]
async fn opencodex_adapter_smoke() {
    let source = std::env::var("OPENCODEX_SOURCE").expect("set OPENCODEX_SOURCE");
    let h = Harness::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = h.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let output = tokio::process::Command::new("bun")
        .arg("scripts/opencodex-smoke.ts")
        .env("OPENCODEX_SOURCE", source)
        .env("SMOKE_ROUTER_URL", url)
        .env("SMOKE_ROUTER_KEY", &h.key)
        .output()
        .await
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);
}
