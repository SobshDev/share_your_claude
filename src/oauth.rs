//! OAuth wire constants and system compatibility are based on opencodex 2.49.0.
//! See THIRD_PARTY_NOTICES.md. The router exclusively owns its refresh token.
use crate::{
    AppState, auth, db,
    error::{AppError, Result},
};
use axum::{Json, extract::State, http::HeaderMap};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const REDIRECT_URI: &str = "http://127.0.0.1:54545/callback";
pub const BETA: &str = "claude-code-20250219,oauth-2025-04-20";
pub const SYSTEM: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

#[derive(Default)]
pub struct OAuthState {
    pending: Option<Pending>,
}
struct Pending {
    state: String,
    verifier: String,
    session_hash: Vec<u8>,
    expires: i64,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

pub fn encrypt(key: &[u8; 32], tokens: &Tokens) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0; 24];
    OsRng.fill_bytes(&mut nonce);
    let plaintext =
        zeroize::Zeroizing::new(serde_json::to_vec(tokens).map_err(|_| AppError::internal())?);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| AppError::internal())?;
    Ok([nonce.to_vec(), ciphertext].concat())
}
pub fn decrypt(key: &[u8; 32], encrypted: &[u8]) -> Result<Tokens> {
    if encrypted.len() < 40 {
        return Err(AppError::internal());
    }
    let cipher = XChaCha20Poly1305::new(key.into());
    let plaintext = zeroize::Zeroizing::new(
        cipher
            .decrypt(XNonce::from_slice(&encrypted[..24]), &encrypted[24..])
            .map_err(|_| AppError::internal())?,
    );
    serde_json::from_slice(&plaintext).map_err(|_| AppError::internal())
}

pub async fn begin(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Value>> {
    let pending = Pending {
        state: auth::random_secret(),
        verifier: auth::random_secret(),
        session_hash: auth::hash(&auth::session_token(&headers, &state)?),
        expires: db::epoch() + 600,
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
    let mut url = url::Url::parse("https://claude.ai/oauth/authorize").expect("constant URL");
    url.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "org:create_api_key user:profile user:inference"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &pending.state),
    ]);
    state.oauth.lock().await.pending = Some(pending);
    Ok(Json(json!({"authorize_url":url.as_str(),"expires_in":600})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    redirect_url: String,
}

pub async fn complete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Completion>,
) -> Result<Json<Value>> {
    let url = url::Url::parse(&input.redirect_url)
        .map_err(|_| AppError::bad("Paste the full redirect URL from your browser"))?;
    let expected = url::Url::parse(REDIRECT_URI).expect("constant URL");
    if url.origin() != expected.origin()
        || url.path() != expected.path()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::bad("Unexpected OAuth redirect URL"));
    }
    let query: Vec<_> = url.query_pairs().collect();
    let unique = |name: &str| -> Result<String> {
        let values: Vec<_> = query.iter().filter(|(k, _)| k == name).collect();
        if values.len() != 1 || values[0].1.is_empty() {
            return Err(AppError::bad(
                "Redirect must contain one code and one state",
            ));
        }
        Ok(values[0].1.to_string())
    };
    let code = unique("code")?;
    let returned_state = unique("state")?;
    let mut guard = state.oauth.lock().await;
    let pending = guard
        .pending
        .as_ref()
        .ok_or_else(|| AppError::bad("Start a new Claude connection first"))?;
    let session_hash = auth::hash(&auth::session_token(&headers, &state)?);
    if pending.expires < db::epoch()
        || pending
            .state
            .as_bytes()
            .ct_eq(returned_state.as_bytes())
            .unwrap_u8()
            != 1
        || pending.session_hash != session_hash
    {
        return Err(AppError::bad(
            "This connection expired or belongs to another session. Start again",
        ));
    }
    let pending = guard.pending.take().expect("checked pending");
    let (tokens, expires) = exchange(&state, json!({"grant_type":"authorization_code","client_id":CLIENT_ID,"code":code,"state":pending.state,"redirect_uri":REDIRECT_URI,"code_verifier":pending.verifier}), None).await?;
    let encrypted = encrypt(&state.config.encryption_key, &tokens)?;
    sqlx::query("INSERT INTO claude_credential VALUES(1,?,?,1,'connected') ON CONFLICT(id) DO UPDATE SET encrypted_tokens=excluded.encrypted_tokens,expires_at=excluded.expires_at,generation=generation+1,state='connected'")
        .bind(encrypted).bind(expires).execute(&state.db).await?;
    Ok(Json(json!({"state":"connected"})))
}

async fn exchange(
    state: &AppState,
    body: Value,
    refresh_fallback: Option<&str>,
) -> Result<(Tokens, i64)> {
    let response = state
        .client
        .post(&state.token_endpoint)
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(|_| AppError::upstream())?;
    let status = response.status();
    if !status.is_success() {
        return Err(if matches!(status.as_u16(), 400 | 401 | 403) {
            AppError::reauth()
        } else {
            AppError::upstream()
        });
    }
    #[derive(Deserialize)]
    struct Reply {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: i64,
    }
    let reply: Reply = serde_json::from_slice(&crate::proxy::limited_body(response, 65536).await?)
        .map_err(|_| AppError::upstream())?;
    let refresh = reply
        .refresh_token
        .filter(|v| !v.is_empty())
        .or_else(|| refresh_fallback.map(str::to_owned))
        .ok_or_else(AppError::upstream)?;
    if reply.access_token.is_empty() || reply.expires_in <= 0 || reply.expires_in > 31_536_000 {
        return Err(AppError::upstream());
    }
    Ok((
        Tokens {
            access_token: reply.access_token,
            refresh_token: refresh,
        },
        db::epoch() + reply.expires_in,
    ))
}

pub struct Access {
    pub tokens: Tokens,
    pub generation: i64,
}

pub async fn access(state: &AppState) -> Result<Access> {
    // One service replica. The lock serializes login and refresh and re-reads the latest generation.
    let _guard = state.oauth.lock().await;
    let row = sqlx::query("SELECT * FROM claude_credential WHERE id=1")
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(AppError::reauth)?;
    if row.get::<String, _>("state") != "connected" {
        return Err(AppError::reauth());
    }
    let generation: i64 = row.get("generation");
    let tokens = decrypt(
        &state.config.encryption_key,
        &row.get::<Vec<u8>, _>("encrypted_tokens"),
    )?;
    if row.get::<i64, _>("expires_at") > db::epoch() + 300 {
        return Ok(Access { tokens, generation });
    }
    let result = exchange(state, json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":tokens.refresh_token}), Some(&tokens.refresh_token)).await;
    let (tokens, expires) = match result {
        Ok(value) => value,
        Err(error) => {
            if error.0 == axum::http::StatusCode::SERVICE_UNAVAILABLE {
                mark_reauth(state, generation).await?;
            }
            return Err(error);
        }
    };
    let encrypted = encrypt(&state.config.encryption_key, &tokens)?;
    let changed = sqlx::query("UPDATE claude_credential SET encrypted_tokens=?,expires_at=?,generation=generation+1 WHERE id=1 AND generation=?")
        .bind(encrypted).bind(expires).bind(generation).execute(&state.db).await?.rows_affected();
    if changed != 1 {
        return Err(AppError::reauth());
    }
    Ok(Access {
        tokens,
        generation: generation + 1,
    })
}

pub async fn mark_reauth(state: &AppState, generation: i64) -> Result<()> {
    sqlx::query("UPDATE claude_credential SET state='needs_reauth' WHERE id=1 AND generation=?")
        .bind(generation)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub fn upstream_headers(access: &Access) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut h = HeaderMap::new();
    for (k, v) in [
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", BETA),
        ("content-type", "application/json"),
        ("user-agent", "@anthropic-ai/sdk/0.74.0"),
        ("x-app", "cli"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-lang", "js"),
        ("x-stainless-timeout", "600"),
        ("x-stainless-package-version", "0.74.0"),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    // Token values must be valid headers; malformed upstream tokens are not sent.
    let mut value = HeaderValue::from_str(&format!("Bearer {}", access.tokens.access_token))
        .map_err(|_| AppError::reauth())?;
    value.set_sensitive(true);
    h.insert("authorization", value);
    let mut session: [u8; 16] = Sha256::digest(format!(
        "claude-code-session:{}",
        access.tokens.access_token
    ))[..16]
        .try_into()
        .expect("digest length");
    session[6] = (session[6] & 0x0f) | 0x40;
    session[8] = (session[8] & 0x3f) | 0x80;
    h.insert(
        "x-claude-code-session-id",
        HeaderValue::from_str(&uuid::Uuid::from_bytes(session).to_string()).expect("UUID header"),
    );
    h.insert(
        "x-client-request-id",
        HeaderValue::from_str(&db::id()).expect("UUID header"),
    );
    Ok(h)
}
