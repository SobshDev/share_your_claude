pub mod admin;
pub mod analytics;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod oauth;
pub mod policy;
pub mod proxy;
pub mod usage;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, header},
    middleware,
    routing::{get, post},
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Semaphore};

/// Body limit for routes that do not declare their own.
const DEFAULT_BODY_LIMIT_BYTES: usize = 64 * 1024;
/// `/v1/messages` requests the router sends upstream at once, across all keys.
pub const GLOBAL_CONCURRENCY: usize = 8;
/// `/v1/messages` requests one key may have in flight, so one friend cannot hold every
/// global permit.
pub const PER_KEY_CONCURRENCY: usize = 3;
/// Token counts use their own small pool so they never wait behind long streams.
pub const COUNT_TOKENS_CONCURRENCY: usize = 4;

pub struct AppState {
    pub config: config::Config,
    pub db: sqlx::SqlitePool,
    pub client: reqwest::Client,
    pub oauth: Mutex<oauth::OAuthState>,
    pub admission: Arc<Semaphore>,
    pub count_admission: Arc<Semaphore>,
    /// Per-key admission, keyed by key id. Idle entries are pruned on use.
    pub key_admission: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
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
        Ok(Arc::new(Self {
            oauth: Mutex::new(oauth::OAuthState::new(config.trusted_proxy_hops)),
            config,
            db,
            client: config::http_client()?,
            admission: Arc::new(Semaphore::new(GLOBAL_CONCURRENCY)),
            count_admission: Arc::new(Semaphore::new(COUNT_TOKENS_CONCURRENCY)),
            key_admission: std::sync::Mutex::new(HashMap::new()),
            upstream: "https://api.anthropic.com".into(),
            token_endpoint: "https://api.anthropic.com/v1/oauth/token".into(),
            client_send_timeout: proxy::CLIENT_SEND_TIMEOUT,
            max_stream_duration: proxy::MAX_STREAM_DURATION,
        }))
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
        .layer(middleware::from_fn(|req: axum::extract::Request, next: middleware::Next| async move {
            let mut response = next.run(req).await;
            let h = response.headers_mut();
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
            h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
            h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"));
            response
        }))
        .with_state(state)
}

#[cfg(test)]
mod tests;
