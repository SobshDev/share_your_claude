use crate::{
    AppState, db,
    error::{AppError, Result},
};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::{
    Json,
    extract::{Request, State},
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
use std::sync::Arc;
use subtle::ConstantTimeEq;

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
    sqlx::query("UPDATE api_key SET last_used_at=? WHERE id=?")
        .bind(db::now())
        .bind(&id)
        .execute(&state.db)
        .await?;
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

pub async fn session(headers: &HeaderMap, state: &AppState) -> Result<String> {
    let token = session_token(headers, state)?;
    sqlx::query_scalar("SELECT csrf_token FROM admin_session WHERE token_hash=? AND expires_at>?")
        .bind(hash(&token))
        .bind(db::epoch())
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

pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> Result<Response> {
    origin(&headers, &state)?;
    if input.password.len() > 1024 {
        return Err(AppError::bad("Password is too long"));
    }
    {
        let mut limit = state.login_attempts.lock().await;
        if db::epoch() - limit.0 >= 60 {
            *limit = (db::epoch(), 0);
        }
        if limit.1 >= 5 {
            return Err(AppError(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Too many sign-in attempts. Wait one minute",
            ));
        }
        limit.1 += 1;
    }
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
    let token = random_secret();
    let csrf = random_secret();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM admin_session WHERE expires_at<=?")
        .bind(db::epoch())
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO admin_session VALUES(?,?,?)")
        .bind(hash(&token))
        .bind(&csrf)
        .bind(db::epoch() + 43200)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let secure = if state.config.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200{secure}",
        cookie_name(&state)
    );
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
    let secure = if state.config.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}",
        cookie_name(&state)
    );
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
