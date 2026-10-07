//! Owner Claude credential: token refresh, re-authentication, PKCE login, and encryption.
use super::*;
use crate::oauth;

/// Message when a pasted redirect's state does not match this session's pending connection.
const NOT_PENDING: &str = "This connection expired or belongs to another session. Start again";
/// Message when a pasted redirect is not the expected callback URL.
const UNEXPECTED_URL: &str = "Unexpected OAuth redirect URL";
/// Message when a pasted redirect lacks exactly one code or one state.
const ONE_CODE: &str = "Redirect must contain one code and one state";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let reply = assert_error(response, 503, "authentication_error", RECONNECT).await;
    assert!(!reply.contains("must not leak"));
    let state: String = sqlx::query_scalar("SELECT state FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(state, "needs_reauth");
    assert_error(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await,
        503,
        "authentication_error",
        RECONNECT,
    )
    .await;
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
    assert_error(
        h.admin("/admin/api/claude/complete", "POST", Some(unsupported))
            .await,
        400,
        "invalid_request_error",
        UNEXPECTED_URL,
    )
    .await;
    let wrong = json!({"redirect_url":format!("{}?code=abc&state=wrong",oauth::REDIRECT_URI)});
    assert_error(
        h.admin("/admin/api/claude/complete", "POST", Some(wrong))
            .await,
        400,
        "invalid_request_error",
        NOT_PENDING,
    )
    .await;
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
        use base64::Engine;
        use sha2::Digest;
        let captures = h.mock.token_captures.lock().await;
        assert_eq!(captures.len(), 1);
        let exchange = &captures[0];
        assert_eq!(exchange["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(exchange["code"], "abc");
        assert_eq!(exchange["state"], params["state"].as_ref());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            sha2::Sha256::digest(exchange["code_verifier"].as_str().unwrap().as_bytes()),
        );
        assert_eq!(challenge, params["code_challenge"]);
    }
    assert_error(
        h.admin("/admin/api/claude/complete", "POST", Some(good))
            .await,
        400,
        "invalid_request_error",
        "Start a new Claude connection first",
    )
    .await;
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
    // An upstream 401 refreshes the current generation once; messages are not replayed.
    assert_error(
        h.request("/v1/messages", &h.key, message("unauthorized", false))
            .await,
        503,
        "api_error",
        "The router renewed its Claude session. Retry the request",
    )
    .await;
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        200
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 2);
}

/// How the token endpoint of a [`TokenMock`] answers.
#[derive(Clone, Copy)]
enum Reply {
    /// New access token and rotated refresh token.
    Rotate,
    /// New access token without a refresh token.
    NoRefreshToken,
    /// `invalid_grant` (400).
    Rejected,
    /// 503.
    Unavailable,
    /// A reply with this `expires_in`.
    ExpiresIn(i64),
    /// Waits for one `release` permit, then rotates.
    Hold,
}

/// A token endpoint with selectable replies, plus a `/v1/models` that always answers 401.
struct TokenMock {
    reply: std::sync::Mutex<Reply>,
    calls: AtomicUsize,
    release: tokio::sync::Semaphore,
}

impl TokenMock {
    async fn spawn(reply: Reply) -> (Arc<Self>, String) {
        let mock = Arc::new(Self {
            reply: std::sync::Mutex::new(reply),
            calls: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let service = Router::new()
            .route("/v1/oauth/token", post(token_reply))
            .route(
                "/v1/models",
                get(|| async { (StatusCode::UNAUTHORIZED, Json(json!({"error":{}}))) }),
            )
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        (mock, format!("http://{address}"))
    }

    fn set(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn wait_for_calls(&self, calls: usize) {
        while self.calls() < calls {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

async fn token_reply(State(mock): State<Arc<TokenMock>>, Json(body): Json<Value>) -> Response {
    mock.calls.fetch_add(1, Ordering::SeqCst);
    assert!(body["grant_type"] == "refresh_token" || body["grant_type"] == "authorization_code");
    let reply = *mock.reply.lock().unwrap();
    let success = |expires_in: i64| {
        Json(json!({"access_token":"refreshed-access","refresh_token":"rotated-refresh","expires_in":expires_in}))
            .into_response()
    };
    match reply {
        Reply::Rotate => success(3600),
        Reply::NoRefreshToken => {
            Json(json!({"access_token":"refreshed-access","expires_in":3600})).into_response()
        }
        Reply::Rejected => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        )
            .into_response(),
        Reply::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Reply::ExpiresIn(expires_in) => success(expires_in),
        Reply::Hold => {
            mock.release.acquire().await.unwrap().forget();
            success(3600)
        }
    }
}

/// A router over the harness database whose token endpoint is `mock`.
async fn with_token_mock(h: &Harness, reply: Reply) -> (Arc<TokenMock>, Arc<AppState>, Router) {
    let (mock, base) = TokenMock::spawn(reply).await;
    let (state, router) =
        h.variant(|state| state.token_endpoint = format!("{base}/v1/oauth/token"));
    (mock, state, router)
}

async fn ask(router: &Router, key: &str) -> Response {
    send_to(
        router,
        "POST",
        "/v1/messages",
        &[("content-type", "application/json"), ("x-api-key", key)],
        message("hello", false).to_string(),
    )
    .await
}

async fn expire_at(h: &Harness, expires_at: i64) {
    sqlx::query("UPDATE claude_credential SET expires_at=?")
        .bind(expires_at)
        .execute(&h.state.db)
        .await
        .unwrap();
}

/// The stored credential: state, generation, and tokens decrypted with the harness key.
async fn credential(h: &Harness) -> (String, i64, oauth::Tokens) {
    let row = sqlx::query("SELECT state,generation,encrypted_tokens FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    let tokens = oauth::decrypt(
        &h.state.config.encryption_key,
        &row.get::<Vec<u8>, _>("encrypted_tokens"),
    )
    .unwrap();
    (row.get("state"), row.get("generation"), tokens)
}

/// The bearer token of the latest `/v1/messages` call the fake upstream received.
async fn last_bearer(h: &Harness) -> String {
    let captures = h.mock.captures.lock().await;
    captures.last().unwrap().headers["authorization"]
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_refresh_failure_stays_connected_and_backs_off() {
    let h = Harness::new().await;
    let (mock, state, router) = with_token_mock(&h, Reply::Unavailable).await;
    expire_at(&h, 0).await;
    // Three concurrent requests on one key.
    let (a, b, c) = tokio::join!(
        ask(&router, &h.key),
        ask(&router, &h.key),
        ask(&router, &h.key)
    );
    for response in [a, b, c] {
        assert_error(response, 502, "api_error", FAILED).await;
    }
    assert_eq!(mock.calls(), 1, "one token call within the backoff window");
    let (status, generation, tokens) = credential(&h).await;
    assert_eq!((status.as_str(), generation), ("connected", 1));
    assert_eq!(tokens.refresh_token, "owner-refresh");
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);

    state.oauth.lock().await.clear_refresh_backoff().await;
    mock.set(Reply::Rotate);
    assert_eq!(ask(&router, &h.key).await.status(), 200);
    assert_eq!(mock.calls(), 2);
    assert_eq!(credential(&h).await.1, 2);
}

#[tokio::test]
async fn transient_refresh_failure_keeps_using_an_unexpired_token() {
    let h = Harness::new().await;
    let (mock, _, router) = with_token_mock(&h, Reply::Unavailable).await;
    expire_at(&h, db::epoch() + 100).await;
    for _ in 0..2 {
        assert_eq!(ask(&router, &h.key).await.status(), 200);
        assert_eq!(last_bearer(&h).await, "Bearer owner-access-must-not-leak");
    }
    assert_eq!(mock.calls(), 1);
    assert_eq!(credential(&h).await.0, "connected");
}

#[tokio::test]
async fn refresh_without_a_new_refresh_token_keeps_the_previous_one() {
    let h = Harness::new().await;
    let (mock, _, router) = with_token_mock(&h, Reply::NoRefreshToken).await;
    expire_at(&h, 0).await;
    assert_eq!(ask(&router, &h.key).await.status(), 200);
    assert_eq!(mock.calls(), 1);
    let (status, generation, tokens) = credential(&h).await;
    assert_eq!((status.as_str(), generation), ("connected", 2));
    assert_eq!(tokens.access_token, "refreshed-access");
    assert_eq!(tokens.refresh_token, "owner-refresh");
}

#[tokio::test]
async fn refresh_rejects_an_invalid_expires_in() {
    let h = Harness::new().await;
    expire_at(&h, 0).await;
    for expires_in in [0, -60, 31_536_001] {
        let (mock, _, router) = with_token_mock(&h, Reply::ExpiresIn(expires_in)).await;
        assert_error(ask(&router, &h.key).await, 502, "api_error", FAILED).await;
        assert_eq!(mock.calls(), 1);
        let (status, generation, tokens) = credential(&h).await;
        assert_eq!((status.as_str(), generation), ("connected", 1));
        assert_eq!(tokens.access_token, "owner-access-must-not-leak");
    }
}

#[tokio::test]
async fn refresh_does_not_overwrite_a_credential_replaced_meanwhile() {
    let h = Harness::new().await;
    let (mock, _, router) = with_token_mock(&h, Reply::Hold).await;
    expire_at(&h, 0).await;
    let request = tokio::spawn({
        let router = router.clone();
        let key = h.key.clone();
        async move { ask(&router, &key).await.status() }
    });
    mock.wait_for_calls(1).await;
    // The owner reconnects while the refresh is in flight.
    let replacement = oauth::encrypt(
        &h.state.config.encryption_key,
        &oauth::Tokens {
            access_token: "reconnected-access".into(),
            refresh_token: "reconnected-refresh".into(),
        },
    )
    .unwrap();
    sqlx::query(
        "UPDATE claude_credential SET encrypted_tokens=?,expires_at=?,generation=generation+1",
    )
    .bind(replacement)
    .bind(db::epoch() + 3600)
    .execute(&h.state.db)
    .await
    .unwrap();
    mock.release.add_permits(1);
    assert_eq!(request.await.unwrap(), 200);
    assert_eq!(last_bearer(&h).await, "Bearer reconnected-access");
    let (status, generation, tokens) = credential(&h).await;
    assert_eq!((status.as_str(), generation), ("connected", 2));
    assert_eq!(tokens.refresh_token, "reconnected-refresh");
}

#[tokio::test]
async fn refreshed_credential_survives_a_failed_save() {
    let h = Harness::new().await;
    let (mock, _, router) = with_token_mock(&h, Reply::Rotate).await;
    expire_at(&h, 0).await;
    sqlx::query(
        "CREATE TRIGGER block_refresh BEFORE UPDATE OF encrypted_tokens ON claude_credential \
         BEGIN SELECT RAISE(ABORT,'blocked'); END",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    assert_eq!(ask(&router, &h.key).await.status(), 200);
    assert_eq!(last_bearer(&h).await, "Bearer refreshed-access");
    let (status, generation, tokens) = credential(&h).await;
    assert_eq!((status.as_str(), generation), ("connected", 1));
    assert_eq!(tokens.refresh_token, "owner-refresh");

    sqlx::query("DROP TRIGGER block_refresh")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(ask(&router, &h.key).await.status(), 200);
    assert_eq!(last_bearer(&h).await, "Bearer refreshed-access");
    assert_eq!(
        mock.calls(),
        1,
        "the rotated token is saved, not refreshed again"
    );
    let (_, generation, tokens) = credential(&h).await;
    assert_eq!(generation, 2);
    assert_eq!(tokens.refresh_token, "rotated-refresh");
}

#[tokio::test]
async fn unexpired_token_does_not_wait_for_a_refresh() {
    let h = Harness::new().await;
    let (mock, _, router) = with_token_mock(&h, Reply::Hold).await;
    expire_at(&h, 0).await;
    let refreshing = tokio::spawn({
        let router = router.clone();
        let key = h.key.clone();
        async move { ask(&router, &key).await.status() }
    });
    mock.wait_for_calls(1).await;
    expire_at(&h, db::epoch() + 3600).await;
    let response = tokio::time::timeout(Duration::from_secs(5), ask(&router, &h.key))
        .await
        .expect("a valid token is served without waiting for the refresh");
    assert_eq!(response.status(), 200);
    assert!(!refreshing.is_finished());
    mock.release.add_permits(1);
    assert_eq!(refreshing.await.unwrap(), 200);
}

/// Starts a Claude connection as `session` and returns the matching redirect URL.
async fn start_connection(router: &Router, session: &AdminSession, code: &str) -> Value {
    let started =
        json_body(admin_as(router, session, "POST", "/admin/api/claude/login", None).await).await;
    let url = url::Url::parse(started["authorize_url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    json!({"redirect_url":format!("{}?code={code}&state={state}", oauth::REDIRECT_URI)})
}

#[tokio::test]
async fn rejected_authorization_code_keeps_the_credential() {
    let h = Harness::new().await;
    let (mock, _, router) = with_token_mock(&h, Reply::Rejected).await;
    let before: (Vec<u8>, i64) =
        sqlx::query_as("SELECT encrypted_tokens,generation FROM claude_credential")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    let session = sign_in(&router).await;
    let redirect = start_connection(&router, &session, "bad-code").await;
    let response = admin_as(
        &router,
        &session,
        "POST",
        "/admin/api/claude/complete",
        Some(redirect),
    )
    .await;
    assert_error(
        response,
        400,
        "invalid_request_error",
        "Claude rejected this authorization code. Start a new connection",
    )
    .await;
    assert_eq!(mock.calls(), 1);
    let after: (Vec<u8>, i64) =
        sqlx::query_as("SELECT encrypted_tokens,generation FROM claude_credential")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert_eq!(credential(&h).await.0, "connected");
}

#[tokio::test]
async fn catalog_refresh_unauthorized_requires_reconnect() {
    let h = Harness::new().await;
    let (_, base) = TokenMock::spawn(Reply::Rotate).await;
    let (_, router) = h.variant(|state| state.upstream = base);
    let session = sign_in(&router).await;
    let response = admin_as(&router, &session, "POST", "/admin/api/models/refresh", None).await;
    assert_eq!(response.status(), 503);
    assert_eq!(credential(&h).await.0, "needs_reauth");
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

#[tokio::test]
async fn unreadable_credential_requires_reconnect_and_reconnecting_restores_it() {
    let logs = Logs::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish(),
    );
    let h = Harness::new().await;
    let stored: Vec<u8> = sqlx::query_scalar("SELECT encrypted_tokens FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    // The credential was stored under the harness key; this router runs with another one.
    let (mock, base) = TokenMock::spawn(Reply::Rotate).await;
    let (state, router) = h.variant(|state| {
        state.config.encryption_key = zeroize::Zeroizing::new([8; 32]);
        state.token_endpoint = format!("{base}/v1/oauth/token");
    });
    let response = ask(&router, &h.key).await;
    assert_error(response, 503, "authentication_error", RECONNECT).await;
    let session = sign_in(&router).await;
    let me = json_body(admin_as(&router, &session, "GET", "/admin/api/me", None).await).await;
    assert_eq!(me["claude"]["state"], "needs_reauth");
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);

    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("stored Claude credential cannot be decrypted; reconnect required"));
    for secret in ["owner-access-must-not-leak", "owner-refresh"] {
        assert!(!output.contains(secret));
    }
    for encoding in [
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &stored),
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &stored),
    ] {
        assert!(!output.contains(&encoding[..16]));
    }

    let redirect = start_connection(&router, &session, "abc").await;
    assert_eq!(
        admin_as(
            &router,
            &session,
            "POST",
            "/admin/api/claude/complete",
            Some(redirect)
        )
        .await
        .status(),
        200
    );
    assert_eq!(mock.calls(), 1);
    assert_eq!(ask(&router, &h.key).await.status(), 200);
    assert_eq!(last_bearer(&h).await, "Bearer refreshed-access");
    let encrypted: Vec<u8> = sqlx::query_scalar("SELECT encrypted_tokens FROM claude_credential")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(oauth::decrypt(&state.config.encryption_key, &encrypted).is_ok());
}

#[tokio::test]
async fn oauth_state_is_bound_to_the_session_that_started_it() {
    let h = Harness::new().await;
    let owner = h.session().await;
    let other = sign_in(&h.router).await;
    let redirect = start_connection(&h.router, owner, "abc").await;
    let response = admin_as(
        &h.router,
        &other,
        "POST",
        "/admin/api/claude/complete",
        Some(redirect.clone()),
    )
    .await;
    assert_error(response, 400, "invalid_request_error", NOT_PENDING).await;
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 0);
    // The rejected attempt did not consume the pending connection.
    assert_eq!(
        h.admin("/admin/api/claude/complete", "POST", Some(redirect))
            .await
            .status(),
        200
    );
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn malformed_or_expired_oauth_redirects_are_rejected() {
    let h = Harness::new().await;
    let redirect = start_connection(&h.router, h.session().await, "abc").await;
    let good = redirect["redirect_url"].as_str().unwrap().to_owned();
    let state = good.split("state=").nth(1).unwrap().to_owned();
    for (url, message) in [
        (
            format!("{}?code=abc&code=def&state={state}", oauth::REDIRECT_URI),
            ONE_CODE,
        ),
        (format!("{}?code=abc&state=", oauth::REDIRECT_URI), ONE_CODE),
        (
            format!("{}?code=&state={state}", oauth::REDIRECT_URI),
            ONE_CODE,
        ),
        (format!("{}?state={state}", oauth::REDIRECT_URI), ONE_CODE),
        (
            format!(
                "{}?code=abc&state={state}&state={state}",
                oauth::REDIRECT_URI
            ),
            ONE_CODE,
        ),
        (format!("{good}#fragment"), UNEXPECTED_URL),
        (
            format!("http://user@localhost:54545/callback?code=abc&state={state}"),
            UNEXPECTED_URL,
        ),
        (
            "not a url".to_owned(),
            "Paste the full redirect URL from your browser",
        ),
    ] {
        let response = h
            .admin(
                "/admin/api/claude/complete",
                "POST",
                Some(json!({"redirect_url":url})),
            )
            .await;
        assert_error(response, 400, "invalid_request_error", message).await;
    }
    h.state.oauth.lock().await.expire_pending();
    let response = h
        .admin("/admin/api/claude/complete", "POST", Some(redirect))
        .await;
    assert_error(response, 400, "invalid_request_error", NOT_PENDING).await;
    assert_eq!(h.mock.refreshes.load(Ordering::SeqCst), 0);
}
