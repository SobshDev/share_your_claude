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
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

pub struct AppState {
    pub config: config::Config,
    pub db: sqlx::SqlitePool,
    pub client: reqwest::Client,
    pub oauth: Mutex<oauth::OAuthState>,
    pub login_attempts: Mutex<(i64, u32)>,
    pub admission: Arc<Semaphore>,
    // Only tests inside this crate can replace destinations. No environment/config overrides.
    pub(crate) upstream: String,
    pub(crate) token_endpoint: String,
}

impl AppState {
    pub fn new(config: config::Config, db: sqlx::SqlitePool) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            config,
            db,
            client: config::http_client()?,
            oauth: Mutex::new(oauth::OAuthState::default()),
            login_attempts: Mutex::new((0, 0)),
            admission: Arc::new(Semaphore::new(8)),
            upstream: "https://api.anthropic.com".into(),
            token_endpoint: "https://api.anthropic.com/v1/oauth/token".into(),
        }))
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(admin::ready))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .route("/v1/models", get(proxy::models))
        .merge(admin::routes(state.clone()))
        .fallback(|| async { error::AppError::not_found() })
        .layer(DefaultBodyLimit::max(proxy::MAX_REQUEST_BODY_BYTES))
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
