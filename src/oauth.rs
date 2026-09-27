//! OAuth wire constants and system compatibility are based on opencodex 2.49.0.
//! See `THIRD_PARTY_NOTICES.md`. The router exclusively owns its refresh token.
use crate::{
    AppState, auth, db,
    error::{AppError, Result},
};
use axum::{Json, extract::State, http::HeaderMap};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
// The OAuth client registers localhost; the equivalent loopback IP is not accepted.
pub(crate) const REDIRECT_URI: &str = "http://localhost:54545/callback";
pub(crate) const BETA: &str = "claude-code-20250219,oauth-2025-04-20";
pub(crate) const SYSTEM: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// Refresh when the access token expires within this many seconds.
const REFRESH_MARGIN_SECS: i64 = 300;
/// After a transient token endpoint failure, callers fail fast (or keep using the current,
/// unexpired token) for this many seconds instead of calling the endpoint again.
const REFRESH_BACKOFF_SECS: i64 = 30;
/// Attempts to save a refreshed credential before keeping it in memory instead.
const PERSIST_ATTEMPTS: u32 = 3;

/// In-process owner authentication state. Every lock on `AppState::oauth` is short and never
/// held across network or database I/O; refreshes are serialized by `refresh` instead.
pub(crate) struct OAuthState {
    pending: Option<Pending>,
    refresh: Arc<Mutex<Refresh>>,
    /// Admin sign-in throttling (kept here so it shares `AppState`'s existing lock).
    pub(crate) login: auth::LoginLimiter,
}
impl OAuthState {
    /// `trusted_proxy_hops` configures the admin login limiter; see [`auth::LoginLimiter`].
    pub(crate) fn new(trusted_proxy_hops: usize) -> Self {
        Self {
            pending: None,
            refresh: Arc::default(),
            login: auth::LoginLimiter::new(trusted_proxy_hops),
        }
    }
}
#[cfg(test)]
impl OAuthState {
    /// Ages the pending PKCE login past its expiry.
    pub(crate) fn expire_pending(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.expires = db::epoch() - 1;
        }
    }
    /// Ends the backoff after a transient refresh failure.
    pub(crate) async fn clear_refresh_backoff(&self) {
        self.refresh.lock().await.last_failure = None;
    }
}

/// Serialized refresh bookkeeping; held for the whole refresh so only one caller refreshes.
#[derive(Default)]
struct Refresh {
    /// Epoch second of the last transient token endpoint failure.
    last_failure: Option<i64>,
    /// A refreshed credential the database did not accept yet. The provider may already have
    /// rotated the stored refresh token, so this copy must be saved before refreshing again.
    unsaved: Option<Unsaved>,
}
struct Unsaved {
    /// Generation of the row these tokens replace.
    generation: i64,
    encrypted: Vec<u8>,
    expires: i64,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct Pending {
    state: String,
    verifier: String,
    session_hash: Vec<u8>,
    expires: i64,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) struct Tokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
}

pub(crate) fn encrypt(key: &[u8; 32], tokens: &Tokens) -> Result<Vec<u8>> {
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
pub(crate) fn decrypt(key: &[u8; 32], encrypted: &[u8]) -> Result<Tokens> {
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

pub(crate) async fn begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
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

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub(crate) struct Completion {
    redirect_url: String,
}

/// Validates the pasted redirect URL and returns its `code` and `state`.
fn redirect_params(redirect_url: &str) -> Result<(Zeroizing<String>, Zeroizing<String>)> {
    let url = url::Url::parse(redirect_url)
        .map_err(|_| AppError::bad("Paste the full redirect URL from your browser"))?;
    let result = (|| {
        let expected = url::Url::parse(REDIRECT_URI).expect("constant URL");
        if url.origin() != expected.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AppError::bad("Unexpected OAuth redirect URL"));
        }
        let unique = |name: &str| -> Result<Zeroizing<String>> {
            let mut values: Vec<_> = url
                .query_pairs()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| Zeroizing::new(v.into_owned()))
                .collect();
            match values.pop() {
                Some(value) if values.is_empty() && !value.is_empty() => Ok(value),
                _ => Err(AppError::bad(
                    "Redirect must contain one code and one state",
                )),
            }
        };
        Ok((unique("code")?, unique("state")?))
    })();
    // The parsed URL owns a copy of the authorization code.
    drop(Zeroizing::new(String::from(url)));
    result
}

pub(crate) async fn complete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Completion>,
) -> Result<Json<Value>> {
    let (code, returned_state) = redirect_params(&input.redirect_url)?;
    drop(input);
    let session_hash = auth::hash(&auth::session_token(&headers, &state)?);
    let pending = {
        let mut guard = state.oauth.lock().await;
        let pending = guard
            .pending
            .as_ref()
            .ok_or_else(|| AppError::bad("Start a new Claude connection first"))?;
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
        guard.pending.take().expect("checked pending")
    };
    let request = TokenRequest {
        grant_type: "authorization_code",
        code: Some(&code),
        state: Some(&pending.state),
        redirect_uri: Some(REDIRECT_URI),
        code_verifier: Some(&pending.verifier),
        ..TokenRequest::default()
    };
    let (tokens, expires) =
        exchange(&state, &request, None)
            .await
            .map_err(|error| match error {
                ExchangeError::Rejected => {
                    AppError::bad("Claude rejected this authorization code. Start a new connection")
                }
                ExchangeError::Failed => AppError::upstream(),
            })?;
    let encrypted = encrypt(&state.config.encryption_key, &tokens)?;
    sqlx::query("INSERT INTO claude_credential(id,encrypted_tokens,expires_at,generation,state) VALUES(1,?,?,1,'connected') ON CONFLICT(id) DO UPDATE SET encrypted_tokens=excluded.encrypted_tokens,expires_at=excluded.expires_at,generation=generation+1,state='connected'")
        .bind(encrypted).bind(expires).execute(&state.db).await?;
    Ok(Json(json!({"state":"connected"})))
}

/// Token endpoint request body. It borrows the secrets and is serialized into a buffer that
/// is wiped when the request body is dropped.
#[derive(Serialize)]
struct TokenRequest<'a> {
    grant_type: &'static str,
    client_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_uri: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<&'a str>,
}
impl Default for TokenRequest<'_> {
    fn default() -> Self {
        Self {
            grant_type: "",
            client_id: CLIENT_ID,
            code: None,
            state: None,
            redirect_uri: None,
            code_verifier: None,
            refresh_token: None,
        }
    }
}

enum ExchangeError {
    /// The token endpoint refused the grant (400, 401, or 403).
    Rejected,
    /// Network failure, another status, or an unusable reply.
    Failed,
}

async fn exchange(
    state: &AppState,
    request: &TokenRequest<'_>,
    refresh_fallback: Option<&str>,
) -> std::result::Result<(Tokens, i64), ExchangeError> {
    #[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
    struct Reply {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: i64,
    }
    let body = Zeroizing::new(serde_json::to_vec(request).map_err(|_| ExchangeError::Failed)?);
    let response = state
        .client
        .post(&state.token_endpoint)
        .timeout(Duration::from_secs(30))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(Bytes::from_owner(body))
        .send()
        .await
        .map_err(|_| ExchangeError::Failed)?;
    let status = response.status();
    if !status.is_success() {
        return Err(if matches!(status.as_u16(), 400 | 401 | 403) {
            ExchangeError::Rejected
        } else {
            ExchangeError::Failed
        });
    }
    let body = Zeroizing::new(
        crate::proxy::limited_body(response, 65536)
            .await
            .map_err(|_| ExchangeError::Failed)?,
    );
    let mut reply: Reply = serde_json::from_slice(&body).map_err(|_| ExchangeError::Failed)?;
    if reply.access_token.is_empty() || reply.expires_in <= 0 || reply.expires_in > 31_536_000 {
        return Err(ExchangeError::Failed);
    }
    let refresh_token = match reply.refresh_token.take().filter(|v| !v.is_empty()) {
        Some(token) => token,
        None => refresh_fallback.ok_or(ExchangeError::Failed)?.to_owned(),
    };
    Ok((
        Tokens {
            access_token: std::mem::take(&mut reply.access_token),
            refresh_token,
        },
        db::epoch() + reply.expires_in,
    ))
}

pub(crate) struct Access {
    pub(crate) tokens: Tokens,
    pub(crate) generation: i64,
}

/// The stored credential, decrypted.
struct Stored {
    tokens: Tokens,
    generation: i64,
    expires: i64,
}
impl From<Stored> for Access {
    fn from(stored: Stored) -> Self {
        Self {
            tokens: stored.tokens,
            generation: stored.generation,
        }
    }
}

async fn load(state: &AppState) -> Result<Stored> {
    let row = sqlx::query("SELECT * FROM claude_credential WHERE id=1")
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(AppError::reauth)?;
    if row.get::<String, _>("state") != "connected" {
        return Err(AppError::reauth());
    }
    let generation: i64 = row.get("generation");
    let Ok(tokens) = decrypt(
        &state.config.encryption_key,
        &row.get::<Vec<u8>, _>("encrypted_tokens"),
    ) else {
        // ENCRYPTION_KEY changed or the row is damaged. Only reconnecting can recover.
        tracing::error!("stored Claude credential cannot be decrypted; reconnect required");
        mark_reauth(state, generation).await?;
        return Err(AppError::reauth());
    };
    Ok(Stored {
        tokens,
        generation,
        expires: row.get("expires_at"),
    })
}

/// Replaces the credential of `generation`. Retries transient database errors; returns
/// `Ok(false)` when the credential was replaced by someone else in the meantime.
async fn persist(
    state: &AppState,
    generation: i64,
    encrypted: &[u8],
    expires: i64,
) -> std::result::Result<bool, sqlx::Error> {
    let mut attempt = 1;
    loop {
        let result = sqlx::query("UPDATE claude_credential SET encrypted_tokens=?,expires_at=?,generation=generation+1 WHERE id=1 AND generation=?")
            .bind(encrypted).bind(expires).bind(generation).execute(&state.db).await;
        match result {
            Ok(done) => return Ok(done.rows_affected() == 1),
            Err(error) if attempt >= PERSIST_ATTEMPTS => return Err(error),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt))).await;
                attempt += 1;
            }
        }
    }
}

/// Uses `stored` while its access token has not expired, otherwise fails as transient.
fn unexpired(stored: Stored) -> Result<Access> {
    if stored.expires > db::epoch() {
        Ok(stored.into())
    } else {
        Err(AppError::upstream())
    }
}

pub(crate) async fn access(state: &AppState) -> Result<Access> {
    // One service replica. A credential that is not due for refresh is served without locking.
    let stored = load(state).await?;
    if stored.expires > db::epoch() + REFRESH_MARGIN_SECS {
        return Ok(stored.into());
    }
    let refresh = state.oauth.lock().await.refresh.clone();
    let mut refresh = refresh.lock().await;
    // Another caller may have refreshed or reconnected while this one waited.
    let stored = load(state).await?;
    if let Some(unsaved) = refresh.unsaved.take()
        && unsaved.generation == stored.generation
    {
        let tokens = decrypt(&state.config.encryption_key, &unsaved.encrypted)?;
        return match persist(
            state,
            unsaved.generation,
            &unsaved.encrypted,
            unsaved.expires,
        )
        .await
        {
            Ok(true) => Ok(Access {
                tokens,
                generation: unsaved.generation + 1,
            }),
            Ok(false) => Ok(load(state).await?.into()),
            Err(_) => {
                tracing::error!("refreshed Claude credential still cannot be saved");
                let generation = unsaved.generation;
                refresh.unsaved = Some(unsaved);
                Ok(Access { tokens, generation })
            }
        };
    }
    if stored.expires > db::epoch() + REFRESH_MARGIN_SECS {
        return Ok(stored.into());
    }
    if refresh
        .last_failure
        .is_some_and(|at| db::epoch() - at < REFRESH_BACKOFF_SECS)
    {
        return unexpired(stored);
    }
    let request = TokenRequest {
        grant_type: "refresh_token",
        refresh_token: Some(&stored.tokens.refresh_token),
        ..TokenRequest::default()
    };
    let (tokens, expires) =
        match exchange(state, &request, Some(&stored.tokens.refresh_token)).await {
            Ok(value) => value,
            Err(ExchangeError::Rejected) => {
                mark_reauth(state, stored.generation).await?;
                return Err(AppError::reauth());
            }
            Err(ExchangeError::Failed) => {
                refresh.last_failure = Some(db::epoch());
                return unexpired(stored);
            }
        };
    refresh.last_failure = None;
    let encrypted = encrypt(&state.config.encryption_key, &tokens)?;
    match persist(state, stored.generation, &encrypted, expires).await {
        Ok(true) => Ok(Access {
            tokens,
            generation: stored.generation + 1,
        }),
        // The owner reconnected during the refresh; the new credential wins.
        Ok(false) => Ok(load(state).await?.into()),
        Err(_) => {
            // The provider may have rotated the refresh token already, so keep the new
            // credential in memory and save it on the next call.
            tracing::error!("refreshed Claude credential could not be saved; retrying on next use");
            refresh.unsaved = Some(Unsaved {
                generation: stored.generation,
                encrypted,
                expires,
            });
            Ok(Access {
                tokens,
                generation: stored.generation,
            })
        }
    }
}

/// Refreshes the owner's tokens after upstream rejected the access token of `generation`.
/// When another request already replaced that generation, returns the newer tokens without
/// refreshing again. The credential is marked for reconnection only when the refresh itself
/// is rejected.
pub(crate) async fn force_refresh(state: &AppState, generation: i64) -> Result<Access> {
    let refresh = state.oauth.lock().await.refresh.clone();
    let mut refresh = refresh.lock().await;
    let stored = load(state).await?;
    if stored.generation != generation {
        return Ok(stored.into());
    }
    let request = TokenRequest {
        grant_type: "refresh_token",
        refresh_token: Some(&stored.tokens.refresh_token),
        ..TokenRequest::default()
    };
    let (tokens, expires) =
        match exchange(state, &request, Some(&stored.tokens.refresh_token)).await {
            Ok(value) => value,
            Err(ExchangeError::Rejected) => {
                mark_reauth(state, generation).await?;
                return Err(AppError::reauth());
            }
            Err(ExchangeError::Failed) => {
                refresh.last_failure = Some(db::epoch());
                return Err(AppError::upstream());
            }
        };
    refresh.last_failure = None;
    let encrypted = encrypt(&state.config.encryption_key, &tokens)?;
    match persist(state, generation, &encrypted, expires).await {
        Ok(true) => Ok(Access {
            tokens,
            generation: generation + 1,
        }),
        Ok(false) => Ok(load(state).await?.into()),
        Err(_) => {
            tracing::error!("refreshed Claude credential could not be saved; retrying on next use");
            refresh.unsaved = Some(Unsaved {
                generation,
                encrypted,
                expires,
            });
            Ok(Access { tokens, generation })
        }
    }
}

pub(crate) async fn mark_reauth(state: &AppState, generation: i64) -> Result<()> {
    sqlx::query("UPDATE claude_credential SET state='needs_reauth' WHERE id=1 AND generation=?")
        .bind(generation)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub(crate) fn upstream_headers(access: &Access) -> Result<reqwest::header::HeaderMap> {
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
    let bearer = Zeroizing::new(format!("Bearer {}", access.tokens.access_token));
    let mut value = HeaderValue::from_str(&bearer).map_err(|_| AppError::reauth())?;
    value.set_sensitive(true);
    h.insert("authorization", value);
    let mut session: [u8; 16] = Sha256::new()
        .chain_update("claude-code-session:")
        .chain_update(&access.tokens.access_token)
        .finalize()[..16]
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
