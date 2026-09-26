//! Test app construction, request and login helpers, and database fixtures.
use super::*;
use crate::{auth, oauth};

/// Password accepted by the harness's admin login.
pub(super) const ADMIN_PASSWORD: &str = "a-long-test-password";
/// Public origin configured for the harness; admin writes must send it.
pub(super) const ORIGIN: &str = "http://localhost:8080";
/// Message of every failed key or admin session check.
pub(super) const AUTH_REQUIRED: &str = "Authentication required";
/// Message when a key has no grant for the requested model, or the model does not exist.
pub(super) const NO_ACCESS: &str = "This key does not have access to that model";
/// Message when an admin write lacks the dashboard's origin.
pub(super) const WRONG_ORIGIN: &str = "This action must originate from the owner dashboard";
/// Message when an admin write lacks the session's CSRF token.
pub(super) const WRONG_CSRF: &str = "Your session changed. Reload the dashboard and try again";
/// Message when Claude could not be reached, sent an invalid reply, or could not refresh.
pub(super) const FAILED: &str = "Claude could not complete this request";
/// Message when the owner's Claude credential needs a new login.
pub(super) const RECONNECT: &str = "The owner must reconnect Claude in the dashboard";

/// Cookie pair and CSRF token of an admin session created through `/admin/api/login`.
pub(super) struct AdminSession {
    /// `router_session=<token>`, ready for a `cookie` header.
    pub(super) cookie: String,
    pub(super) csrf: String,
}

pub(super) struct Harness {
    pub(super) state: Arc<AppState>,
    pub(super) mock: Arc<Mock>,
    pub(super) router: Router,
    /// Secret of the key created for `person`.
    pub(super) key: String,
    pub(super) key_id: String,
    pub(super) person: String,
    session: tokio::sync::OnceCell<AdminSession>,
    _dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Harness {
    /// A fresh database with a connected Claude credential, `MODEL` enabled, and one person
    /// ("Alex") holding one key ("Laptop") granted `MODEL`, all pointed at a new fake upstream.
    pub(super) async fn new() -> Self {
        let (mock, address, server) = spawn_upstream().await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("test.sqlite").display());
        let pool = db::connect(&url).await.unwrap();
        let config = config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database_url: url,
            public_origin: ORIGIN.into(),
            password_hash: password_hash(),
            encryption_key: zeroize::Zeroizing::new([7; 32]),
            secure_cookie: false,
            trusted_proxy_hops: 0,
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
        sqlx::query(
            "INSERT INTO claude_credential(id,encrypted_tokens,expires_at,generation,state) VALUES(1,?,?,1,'connected')",
        )
        .bind(encrypted)
        .bind(db::epoch() + 3600)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model(id,display_name,model_group,enabled,reviewed_at) VALUES(?,?,?,1,?)",
        )
        .bind(MODEL)
        .bind("Sonnet")
        .bind("sonnet")
        .bind(db::now())
        .execute(&state.db)
        .await
        .unwrap();
        let person = add_person(&state, "Alex").await;
        let (key_id, key) = add_key(&state, &person, "Laptop").await;
        let router = app(state.clone());
        Self {
            state,
            mock,
            router,
            key,
            key_id,
            person,
            session: tokio::sync::OnceCell::new(),
            _dir: dir,
            server,
        }
    }

    /// Sends one request through the router with exactly the given headers.
    pub(super) async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> Response {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.router
            .clone()
            .oneshot(request.body(body.into()).unwrap())
            .await
            .unwrap()
    }

    pub(super) async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        self.send("GET", path, headers, Body::empty()).await
    }

    /// POSTs a JSON body to a proxy route, authenticated with `x-api-key`.
    pub(super) async fn request(&self, path: &str, key: &str, body: Value) -> Response {
        self.send(
            "POST",
            path,
            &[("content-type", "application/json"), ("x-api-key", key)],
            body.to_string(),
        )
        .await
    }

    /// One sign-in attempt through `/admin/api/login`. Every attempt, successful or not,
    /// counts toward the login rate limit.
    pub(super) async fn login(&self, password: &str) -> Response {
        self.send(
            "POST",
            "/admin/api/login",
            &[("origin", ORIGIN), ("content-type", "application/json")],
            json!({"password":password}).to_string(),
        )
        .await
    }

    /// The harness's admin session. The first call signs in through `/admin/api/login`;
    /// later calls reuse that session.
    pub(super) async fn session(&self) -> &AdminSession {
        self.session
            .get_or_init(|| async {
                let response = self.login(ADMIN_PASSWORD).await;
                assert_eq!(response.status(), StatusCode::OK);
                let cookie = response.headers()[header::SET_COOKIE]
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned();
                let csrf = json_body(response).await["csrf_token"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                AdminSession { cookie, csrf }
            })
            .await
    }

    /// Calls an admin route with the harness session cookie, origin, and CSRF token.
    pub(super) async fn admin(&self, path: &str, method: &str, body: Option<Value>) -> Response {
        let session = self.session().await;
        self.send(
            method,
            path,
            &[
                ("content-type", "application/json"),
                ("cookie", session.cookie.as_str()),
                ("origin", ORIGIN),
                ("x-csrf-token", session.csrf.as_str()),
            ],
            body.map(|b| b.to_string()).unwrap_or_default(),
        )
        .await
    }
}

/// Argon2 hash of `ADMIN_PASSWORD`, computed once per test binary.
pub(super) fn password_hash() -> String {
    static HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        use argon2::PasswordHasher;
        argon2::Argon2::default()
            .hash_password(
                ADMIN_PASSWORD.as_bytes(),
                &argon2::password_hash::SaltString::encode_b64(b"test-salt-16-byte").unwrap(),
            )
            .unwrap()
            .to_string()
    });
    HASH.clone()
}

/// Inserts a person and returns their id.
pub(super) async fn add_person(state: &AppState, name: &str) -> String {
    let id = db::id();
    sqlx::query("INSERT INTO person(id,name,created_at) VALUES(?,?,?)")
        .bind(&id)
        .bind(name)
        .bind(db::now())
        .execute(&state.db)
        .await
        .unwrap();
    id
}

/// Inserts a key for `person`, grants it `MODEL`, and returns `(key_id, secret)`.
pub(super) async fn add_key(state: &AppState, person: &str, label: &str) -> (String, String) {
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
    grant(state, &id, MODEL).await;
    (id, secret)
}

/// Writes a key grant directly to the database, bypassing admin validation.
pub(super) async fn grant(state: &AppState, key_id: &str, model: &str) {
    sqlx::query("INSERT INTO key_model_grant(key_id,model_id) VALUES(?,?)")
        .bind(key_id)
        .bind(model)
        .execute(&state.db)
        .await
        .unwrap();
}

/// A `/v1/messages` body for `MODEL` with one user message; `content` picks the mock scenario.
pub(super) fn message(content: &str, stream: bool) -> Value {
    json!({"model":MODEL,"max_tokens":128,"stream":stream,"messages":[{"role":"user","content":content}]})
}

pub(super) async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

pub(super) async fn text_body(response: Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// Checks that `response` is Anthropic's error envelope,
/// `{"type":"error","error":{"type":kind,"message":...}}`, with `status` and a message
/// containing `message`, and returns the full message. The message tells apart denials that
/// share a status and type, such as a failed origin check and a failed CSRF check.
pub(super) async fn assert_error(
    response: Response,
    status: u16,
    kind: &str,
    message: &str,
) -> String {
    let actual_status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let text = text_body(response).await;
    assert_eq!(
        actual_status, status,
        "expected {kind} {message:?}, got {text}"
    );
    assert_eq!(
        content_type.as_ref().and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "{text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("not JSON: {text}"));
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], kind, "{body}");
    let actual = body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("no error message: {body}"));
    assert!(actual.contains(message), "expected {message:?} in {body}");
    actual.to_owned()
}

/// Checks the headers the router adds to every response, whatever route produced it.
pub(super) fn assert_security_headers(response: &Response) {
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap()),
        Some("no-store")
    );
    assert_security_headers_except_caching(response);
}

/// Like `assert_security_headers`, for responses such as static assets that set their own
/// caching policy.
pub(super) fn assert_security_headers_except_caching(response: &Response) {
    let headers = response.headers();
    let value = |name: &str| {
        headers
            .get(name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .to_str()
            .unwrap()
    };
    assert_eq!(value("x-content-type-options"), "nosniff");
    assert_eq!(value("referrer-policy"), "no-referrer");
    let csp = value("content-security-policy");
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
    ] {
        assert!(csp.contains(directive), "{csp}");
    }
}

impl Harness {
    /// A second app over this harness's database and fake upstream, with a copy of the
    /// harness configuration that `edit` may change (cookie mode, password hash, encryption
    /// key, upstream or token endpoint). The harness's own router is unaffected.
    pub(super) fn variant(&self, edit: impl FnOnce(&mut AppState)) -> (Arc<AppState>, Router) {
        let current = &self.state.config;
        let config = config::Config {
            bind: current.bind,
            database_url: current.database_url.clone(),
            public_origin: current.public_origin.clone(),
            password_hash: current.password_hash.clone(),
            encryption_key: zeroize::Zeroizing::new(*current.encryption_key),
            secure_cookie: current.secure_cookie,
            trusted_proxy_hops: current.trusted_proxy_hops,
        };
        let mut state = AppState::new(config, self.state.db.clone()).unwrap();
        {
            let inner = Arc::get_mut(&mut state).unwrap();
            inner.upstream = self.state.upstream.clone();
            inner.token_endpoint = self.state.token_endpoint.clone();
            edit(inner);
        }
        let router = app(state.clone());
        (state, router)
    }
}

/// Sends one request to `router` with exactly the given headers.
pub(super) async fn send_to(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    router
        .clone()
        .oneshot(request.body(body.into()).unwrap())
        .await
        .unwrap()
}

/// Signs in to `router` with `ADMIN_PASSWORD` and returns a new session.
pub(super) async fn sign_in(router: &Router) -> AdminSession {
    let response = send_to(
        router,
        "POST",
        "/admin/api/login",
        &[("origin", ORIGIN), ("content-type", "application/json")],
        json!({"password":ADMIN_PASSWORD}).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = json_body(response).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    AdminSession { cookie, csrf }
}

/// Calls an admin route on `router` as `session`, with the right origin and CSRF token.
pub(super) async fn admin_as(
    router: &Router,
    session: &AdminSession,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Response {
    send_to(
        router,
        method,
        path,
        &[
            ("content-type", "application/json"),
            ("cookie", session.cookie.as_str()),
            ("origin", ORIGIN),
            ("x-csrf-token", session.csrf.as_str()),
        ],
        body.map(|b| b.to_string()).unwrap_or_default(),
    )
    .await
}
