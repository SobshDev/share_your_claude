pub(crate) mod admin;
pub(crate) mod analytics;
pub(crate) mod auth;
pub mod config;
pub mod db;
pub(crate) mod error;
pub(crate) mod oauth;
pub(crate) mod policy;
pub(crate) mod proxy;
pub(crate) mod usage;

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Semaphore, watch};

/// Body limit for routes that do not declare their own.
const DEFAULT_BODY_LIMIT_BYTES: usize = 64 * 1024;
/// Anthropic API base URL that friend requests are relayed to.
const UPSTREAM: &str = "https://api.anthropic.com";
/// Anthropic OAuth token endpoint used for code exchange and refresh.
const TOKEN_ENDPOINT: &str = "https://api.anthropic.com/v1/oauth/token";
/// `/v1/messages` requests the router sends upstream at once, across all keys.
pub(crate) const GLOBAL_CONCURRENCY: usize = 8;
/// `/v1/messages` requests one key may have in flight, so one friend cannot hold every
/// global permit.
pub(crate) const PER_KEY_CONCURRENCY: usize = 3;
/// Token counts use their own small pool so they never wait behind long streams.
pub(crate) const COUNT_TOKENS_CONCURRENCY: usize = 4;

/// Where the server is in its lifecycle. `main` advances it on a shutdown signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Accepting and serving requests.
    Serving,
    /// A shutdown signal arrived: `/readyz` reports 503 while open requests finish.
    Draining,
    /// The drain period is over: open streams record their outcome as interrupted and end.
    Stopping,
}

pub struct AppState {
    pub(crate) config: config::Config,
    pub(crate) db: sqlx::SqlitePool,
    pub(crate) client: reqwest::Client,
    pub(crate) oauth: Mutex<oauth::OAuthState>,
    pub(crate) admission: Arc<Semaphore>,
    pub(crate) count_admission: Arc<Semaphore>,
    /// Per-key admission, keyed by key id. Idle entries are pruned on use.
    pub(crate) key_admission: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Lifecycle phase. Subscribe to wait for a change; `main` sends the transitions.
    pub(crate) phase: watch::Sender<Phase>,
    // Only tests inside this crate can replace destinations. No environment/config overrides.
    pub(crate) upstream: String,
    pub(crate) token_endpoint: String,
    /// How long a client may stop reading a stream; tests shorten it.
    pub(crate) client_send_timeout: std::time::Duration,
    /// Longest a relayed stream may run; tests shorten it.
    pub(crate) max_stream_duration: std::time::Duration,
}

impl AppState {
    pub fn new(config: config::Config, db: sqlx::SqlitePool) -> anyhow::Result<Arc<Self>> {
        Self::with_endpoints(config, db, UPSTREAM.into(), TOKEN_ENDPOINT.into()).map(Arc::new)
    }

    /// A state that is not shared yet and sends API and token requests to the given URLs.
    /// Only the in-crate tests choose other destinations, and they may adjust other fields
    /// before sharing the state.
    fn with_endpoints(
        config: config::Config,
        db: sqlx::SqlitePool,
        upstream: String,
        token_endpoint: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            oauth: Mutex::new(oauth::OAuthState::new(config.trusted_proxy_hops)),
            config,
            db,
            client: config::http_client()?,
            admission: Arc::new(Semaphore::new(GLOBAL_CONCURRENCY)),
            count_admission: Arc::new(Semaphore::new(COUNT_TOKENS_CONCURRENCY)),
            key_admission: std::sync::Mutex::new(HashMap::new()),
            phase: watch::Sender::new(Phase::Serving),
            upstream,
            token_endpoint,
            client_send_timeout: proxy::CLIENT_SEND_TIMEOUT,
            max_stream_duration: proxy::MAX_STREAM_DURATION,
        })
    }

    /// The current lifecycle phase.
    pub(crate) fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    /// A receiver that observes every lifecycle phase change.
    pub fn subscribe_phase(&self) -> watch::Receiver<Phase> {
        self.phase.subscribe()
    }

    /// Advances the lifecycle phase. Only `main` calls this, on shutdown.
    pub fn set_phase(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    // Friend routes authenticate from headers before any body is buffered or parsed.
    let friend_api = Router::new()
        .route(
            "/v1/messages",
            post(proxy::messages).layer(DefaultBodyLimit::max(proxy::MAX_REQUEST_BODY_BYTES)),
        )
        .route(
            "/v1/messages/count_tokens",
            post(proxy::count_tokens).layer(DefaultBodyLimit::max(proxy::MAX_REQUEST_BODY_BYTES)),
        )
        .route("/v1/models", get(proxy::models))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            proxy::authenticate,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(admin::ready))
        .merge(friend_api)
        .merge(admin::routes(state.clone()))
        .fallback(|| async { error::AppError::not_found("Not found") })
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_LIMIT_BYTES))
        .layer(middleware::from_fn(error::envelope_rejections))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state)
}

/// Adds the router's default response headers. A handler may set its own value first, as the
/// versioned assets do for `Cache-Control`. HSTS is sent only when `PUBLIC_ORIGIN` is HTTPS.
async fn security_headers(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    let mut default = |name: header::HeaderName, value: &'static str| {
        h.entry(name).or_insert(HeaderValue::from_static(value));
    };
    default(header::CACHE_CONTROL, "no-store");
    default(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    default(header::REFERRER_POLICY, "no-referrer");
    default(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    if state.config.secure_cookie {
        default(header::STRICT_TRANSPORT_SECURITY, "max-age=31536000");
    }
    response
}

#[cfg(test)]
mod tests;
