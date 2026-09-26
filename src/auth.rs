use crate::{
    AppState, db,
    error::{AppError, Result},
};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::{
    Extension, Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{OsRng, rand_core::RngCore};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use subtle::ConstantTimeEq;

/// `api_key.last_used_at` is refreshed at most once per this many seconds.
const LAST_USED_RESOLUTION_SECS: i64 = 60;
/// Lifetime of an admin session, in the database and in the cookie.
const SESSION_TTL_SECS: i64 = 43200;

pub fn random_secret() -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn hash(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(AppError::unauthorized());
    }
    value
        .map(|v| v.to_str().map_err(|_| AppError::unauthorized()))
        .transpose()
}

pub async fn api_key(headers: &HeaderMap, state: &AppState) -> Result<String> {
    let key = single_header(headers, "x-api-key")?;
    let bearer = match single_header(headers, "authorization")? {
        Some(value) => Some(
            value
                .strip_prefix("Bearer ")
                .ok_or_else(AppError::unauthorized)?,
        ),
        None => None,
    };
    if let (Some(a), Some(b)) = (key, bearer)
        && a.as_bytes().ct_eq(b.as_bytes()).unwrap_u8() != 1
    {
        return Err(AppError::unauthorized());
    }
    let secret = key.or(bearer).ok_or_else(AppError::unauthorized)?;
    if !secret.starts_with("sr_") || secret.len() != 46 {
        return Err(AppError::unauthorized());
    }
    let id: Option<String> =
        sqlx::query_scalar("SELECT id FROM api_key WHERE secret_hash=? AND revoked_at IS NULL")
            .bind(hash(secret))
            .fetch_optional(&state.db)
            .await?;
    let id = id.ok_or_else(AppError::unauthorized)?;
    // `last_used_at` is informational: write it at most once a minute per key, and never fail
    // an authenticated request because the write lost the race for SQLite's writer lock.
    let now = chrono::Utc::now();
    let cutoff = (now - chrono::Duration::seconds(LAST_USED_RESOLUTION_SECS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if sqlx::query(
        "UPDATE api_key SET last_used_at=? WHERE id=? AND (last_used_at IS NULL OR last_used_at<?)",
    )
    .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    .bind(&id)
    .bind(cutoff)
    .execute(&state.db)
    .await
    .is_err()
    {
        tracing::warn!("could not record key last use");
    }
    Ok(id)
}

pub fn origin(headers: &HeaderMap, state: &AppState) -> Result<()> {
    if single_header(headers, "origin")? != Some(state.config.public_origin.as_str()) {
        return Err(AppError::forbidden(
            "This action must originate from the owner dashboard",
        ));
    }
    Ok(())
}

pub fn cookie_name(state: &AppState) -> &'static str {
    if state.config.secure_cookie {
        "__Host-router_session"
    } else {
        "router_session"
    }
}

/// The `Set-Cookie` value that stores `value` as the admin session for `max_age` seconds.
/// An empty value with `max_age` 0 clears the cookie.
fn session_cookie(state: &AppState, value: &str, max_age: i64) -> String {
    let secure = if state.config.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}",
        cookie_name(state)
    )
}

pub fn session_token(headers: &HeaderMap, state: &AppState) -> Result<String> {
    let mut result = None;
    for value in headers.get_all(header::COOKIE) {
        for item in value
            .to_str()
            .map_err(|_| AppError::unauthorized())?
            .split(';')
        {
            if let Some((key, value)) = item.trim().split_once('=')
                && key == cookie_name(state)
            {
                if result.is_some() || value.len() != 43 {
                    return Err(AppError::unauthorized());
                }
                result = Some(value.to_owned());
            }
        }
    }
    result.ok_or_else(AppError::unauthorized)
}

/// Fingerprint of the configured owner password hash. Sessions are bound to it, so changing
/// `ADMIN_PASSWORD_HASH` signs out every existing session.
fn password_fingerprint(state: &AppState) -> Vec<u8> {
    hash(&state.config.password_hash)
}

pub async fn session(headers: &HeaderMap, state: &AppState) -> Result<String> {
    let token = session_token(headers, state)?;
    sqlx::query_scalar(
        "SELECT csrf_token FROM admin_session WHERE token_hash=? AND expires_at>? AND password_fp=?",
    )
    .bind(hash(&token))
    .bind(db::epoch())
    .bind(password_fingerprint(state))
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(AppError::unauthorized)
}

pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response> {
    let csrf = session(request.headers(), &state).await?;
    if !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        origin(request.headers(), &state)?;
        let supplied = single_header(request.headers(), "x-csrf-token")?.unwrap_or("");
        if csrf.as_bytes().ct_eq(supplied.as_bytes()).unwrap_u8() != 1 {
            return Err(AppError::forbidden(
                "Your session changed. Reload the dashboard and try again",
            ));
        }
    }
    Ok(next.run(request).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    password: String,
}

/// Failed sign-in throttling, kept per client address with a global backstop.
///
/// Each client may fail [`Self::CLIENT_FAILURES`] times per fixed [`Self::WINDOW_SECS`] window,
/// and all clients together [`Self::GLOBAL_FAILURES`] times, so a stranger cannot lock the
/// owner out and many addresses cannot guess quickly. An attempt reserves a failure before
/// the password is checked, which keeps concurrent guesses from overrunning the limit; a
/// successful sign-in releases the reservation and clears the caller's failures. At most
/// [`Self::MAX_CLIENTS`] addresses are tracked: expired windows are pruned first, then the
/// oldest window is evicted.
///
/// The client address is the socket peer, or, when `TRUSTED_PROXY_HOPS` is N > 0, the
/// N-th `X-Forwarded-For` entry from the right (the address the outermost trusted proxy saw).
/// IPv6 clients are grouped by /64. Requests without a usable address share one bucket.
pub struct LoginLimiter {
    pub(crate) trusted_proxy_hops: usize,
    clients: HashMap<Option<IpAddr>, Window>,
    global: Window,
}

#[derive(Clone, Copy, Default)]
struct Window {
    start: i64,
    failures: u32,
}
impl Window {
    fn current(&mut self, now: i64) -> &mut Self {
        if now - self.start >= LoginLimiter::WINDOW_SECS {
            *self = Self {
                start: now,
                failures: 0,
            };
        }
        self
    }
}

impl LoginLimiter {
    pub const WINDOW_SECS: i64 = 60;
    pub const CLIENT_FAILURES: u32 = 5;
    pub const GLOBAL_FAILURES: u32 = 30;
    pub const MAX_CLIENTS: usize = 4096;

    pub fn new(trusted_proxy_hops: usize) -> Self {
        Self {
            trusted_proxy_hops,
            clients: HashMap::new(),
            global: Window::default(),
        }
    }

    /// The rate-limit key for a request.
    pub fn client(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
        let address = if self.trusted_proxy_hops > 0 {
            let entries: Vec<&str> = headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .collect();
            entries
                .len()
                .checked_sub(self.trusted_proxy_hops)
                .and_then(|i| entries[i].parse().ok())
                .or_else(|| peer.map(|p| p.ip()))
        } else {
            peer.map(|p| p.ip())
        };
        address.map(|ip| match ip.to_canonical() {
            IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & (u128::MAX << 64)).into()),
            v4 => v4,
        })
    }

    /// Reserves one failed attempt for `client`, or returns false when it is throttled.
    pub fn begin(&mut self, client: Option<IpAddr>, now: i64) -> bool {
        if self.global.current(now).failures >= Self::GLOBAL_FAILURES
            || self
                .clients
                .get_mut(&client)
                .is_some_and(|w| w.current(now).failures >= Self::CLIENT_FAILURES)
        {
            return false;
        }
        if !self.clients.contains_key(&client) && self.clients.len() >= Self::MAX_CLIENTS {
            self.clients
                .retain(|_, w| now - w.start < Self::WINDOW_SECS);
            if self.clients.len() >= Self::MAX_CLIENTS
                && let Some(oldest) = self
                    .clients
                    .iter()
                    .min_by_key(|(_, w)| w.start)
                    .map(|(k, _)| *k)
            {
                self.clients.remove(&oldest);
            }
        }
        self.clients
            .entry(client)
            .or_default()
            .current(now)
            .failures += 1;
        self.global.failures += 1;
        true
    }

    /// Releases the attempt reserved by `begin` and clears the client's failures.
    pub fn succeeded(&mut self, client: Option<IpAddr>, now: i64) {
        self.clients.remove(&client);
        let global = self.global.current(now);
        global.failures = global.failures.saturating_sub(1);
    }

    #[cfg(test)]
    pub(crate) fn tracked_clients(&self) -> usize {
        self.clients.len()
    }
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> Result<Response> {
    origin(&headers, &state)?;
    if input.password.len() > 1024 {
        return Err(AppError::bad("Password is too long"));
    }
    let client = {
        let mut oauth = state.oauth.lock().await;
        let client = oauth.login.client(&headers, peer.map(|p| p.0.0));
        if !oauth.login.begin(client, db::epoch()) {
            return Err(AppError(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Too many sign-in attempts. Wait one minute",
            ));
        }
        client
    };
    let encoded = state.config.password_hash.clone();
    let password = zeroize::Zeroizing::new(input.password);
    let valid = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&encoded).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
    })
    .await
    .map_err(|_| AppError::internal())?;
    if !valid {
        return Err(AppError::unauthorized());
    }
    state
        .oauth
        .lock()
        .await
        .login
        .succeeded(client, db::epoch());
    let token = random_secret();
    let csrf = random_secret();
    let fingerprint = password_fingerprint(&state);
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM admin_session WHERE expires_at<=? OR password_fp<>?")
        .bind(db::epoch())
        .bind(&fingerprint)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO admin_session(token_hash,csrf_token,expires_at,password_fp) VALUES(?,?,?,?)",
    )
    .bind(hash(&token))
    .bind(&csrf)
    .bind(db::epoch() + SESSION_TTL_SECS)
    .bind(&fingerprint)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let cookie = session_cookie(&state, &token, SESSION_TTL_SECS);
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"csrf_token":csrf})),
    )
        .into_response())
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Response> {
    let token = session_token(&headers, &state)?;
    sqlx::query("DELETE FROM admin_session WHERE token_hash=?")
        .bind(hash(&token))
        .execute(&state.db)
        .await?;
    let cookie = session_cookie(&state, "", 0);
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    let csrf = session(&headers, &state).await?;
    let row = sqlx::query("SELECT state,expires_at FROM claude_credential WHERE id=1")
        .fetch_optional(&state.db)
        .await?;
    Ok(Json(
        json!({"csrf_token":csrf,"origin":state.config.public_origin,"claude":row.map(|r| json!({"state":r.get::<String,_>("state"),"expires_at":r.get::<i64,_>("expires_at")}))}),
    ))
}
