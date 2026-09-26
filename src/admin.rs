use crate::{
    AppState, auth, db,
    error::{AppError, Result},
    oauth, policy, proxy,
};
use askama::Template;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, patch, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::{collections::HashMap, sync::Arc};

/// Upper bound for each upstream catalog page request, including its body.
pub const CATALOG_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let protected = Router::new()
        .route("/admin/api/me", get(auth::me))
        .route("/admin/api/logout", post(auth::logout))
        .route("/admin/api/people", get(people).post(create_person))
        .route("/admin/api/people/{id}", patch(rename_person))
        .route("/admin/api/keys", get(keys).post(create_key))
        .route("/admin/api/keys/{id}", delete(revoke_key))
        .route("/admin/api/keys/{id}/models", put(set_grants))
        .route("/admin/api/keys/{id}/config", get(client_config))
        .route("/admin/api/models", get(models))
        .route("/admin/api/models/refresh", post(refresh_models))
        .route("/admin/api/models/{id}", put(review_model))
        .route("/admin/api/models/{id}/aliases", post(add_alias))
        .route("/admin/api/claude/login", post(oauth::begin))
        .route("/admin/api/claude/complete", post(oauth::complete))
        .route("/admin/api/usage", get(crate::analytics::usage_report))
        .route("/admin/api/analytics", get(crate::analytics::report))
        .route_layer(middleware::from_fn_with_state(state, auth::require_admin));
    Router::new()
        .merge(protected)
        .route("/", get(|| async { Redirect::to("/admin") }))
        .route("/admin", get(dashboard))
        .route("/admin/login", get(login_page))
        .route("/admin/api/login", post(auth::login))
        .route(
            "/assets/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/app.js"),
                )
            }),
        )
        .route(
            "/assets/common.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/common.js"),
                )
            }),
        )
        .route(
            "/assets/analytics.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/analytics.js"),
                )
            }),
        )
        .route(
            "/assets/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../static/app.css"),
                )
            }),
        )
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct Dashboard;
#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage;

async fn dashboard(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth::session(&headers, &state).await.is_err() {
        return Redirect::to("/admin/login").into_response();
    }
    match Dashboard.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => page_error(),
    }
}
async fn login_page() -> Response {
    match LoginPage.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => page_error(),
    }
}
/// A minimal HTML 500 page for browser routes, which should not show a JSON error.
fn page_error() -> Response {
    tracing::error!("dashboard template failed to render");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\" />\
             <title>Shared Router</title></head><body><h1>Something went wrong</h1>\
             <p>The dashboard could not be displayed. Reload the page, or check the router \
             logs if this keeps happening.</p></body></html>",
        ),
    )
        .into_response()
}
pub async fn ready(State(state): State<Arc<AppState>>) -> Result<&'static str> {
    // Load balancers stop routing here once shutdown has begun.
    if state.phase() != crate::Phase::Serving {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "The router is shutting down",
        ));
    }
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok("ready")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonInput {
    name: String,
}
fn valid_label(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 100 || value.chars().any(char::is_control) {
        return Err(AppError::bad("Enter a name between 1 and 100 characters"));
    }
    Ok(value.to_owned())
}
/// Rejects keys that do not exist or were revoked.
async fn require_active_key<'e>(db: impl sqlx::SqliteExecutor<'e>, id: &str) -> Result<()> {
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_key WHERE id=? AND revoked_at IS NULL")
            .bind(id)
            .fetch_one(db)
            .await?;
    if active == 0 {
        return Err(AppError::bad("Choose an active key"));
    }
    Ok(())
}
async fn people(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM person ORDER BY created_at")
        .fetch_all(&state.db)
        .await?;
    Ok(Json(json!(rows.iter().map(|r|json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"created_at":r.get::<String,_>("created_at")})).collect::<Vec<_>>())))
}
async fn create_person(
    State(state): State<Arc<AppState>>,
    Json(input): Json<PersonInput>,
) -> Result<(StatusCode, Json<Value>)> {
    let name = valid_label(&input.name)?;
    let id = db::id();
    sqlx::query("INSERT INTO person(id,name,created_at) VALUES(?,?,?)")
        .bind(&id)
        .bind(&name)
        .bind(db::now())
        .execute(&state.db)
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"id":id,"name":name}))))
}
async fn rename_person(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<PersonInput>,
) -> Result<StatusCode> {
    let name = valid_label(&input.name)?;
    let n = sqlx::query("UPDATE person SET name=? WHERE id=?")
        .bind(name)
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("Friend not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn keys(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let rows=sqlx::query("SELECT k.*,p.name FROM api_key k JOIN person p ON p.id=k.person_id ORDER BY k.created_at DESC").fetch_all(&state.db).await?;
    let mut grants: HashMap<String, Vec<String>> = HashMap::new();
    for (key, model) in sqlx::query_as::<_, (String, String)>(
        "SELECT key_id,model_id FROM key_model_grant ORDER BY key_id,model_id",
    )
    .fetch_all(&state.db)
    .await?
    {
        grants.entry(key).or_default().push(model);
    }
    let result: Vec<Value> = rows.iter().map(|r| {
        let id: String = r.get("id");
        let models = grants.remove(&id).unwrap_or_default();
        json!({"id":id,"person_id":r.get::<String,_>("person_id"),"person_name":r.get::<String,_>("name"),"label":r.get::<String,_>("label"),"prefix":r.get::<String,_>("prefix"),"created_at":r.get::<String,_>("created_at"),"last_used_at":r.get::<Option<String>,_>("last_used_at"),"revoked_at":r.get::<Option<String>,_>("revoked_at"),"models":models})
    }).collect();
    Ok(Json(json!(result)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyInput {
    person_id: String,
    label: String,
}
async fn create_key(
    State(state): State<Arc<AppState>>,
    Json(input): Json<KeyInput>,
) -> Result<(StatusCode, Json<Value>)> {
    let label = valid_label(&input.label)?;
    let id = db::id();
    // Wiped on drop; the one-time response below is the only copy that leaves this handler.
    let secret = zeroize::Zeroizing::new(format!("sr_{}", auth::random_secret()));
    let prefix = &secret[..11];
    let mut tx = state.db.begin().await?;
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM person WHERE id=?")
        .bind(&input.person_id)
        .fetch_one(&mut *tx)
        .await?
        == 0
    {
        return Err(AppError::bad("Choose an existing friend"));
    }
    sqlx::query(
        "INSERT INTO api_key(id,person_id,label,prefix,secret_hash,created_at) VALUES(?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(&input.person_id)
    .bind(label)
    .bind(prefix)
    .bind(auth::hash(&secret))
    .bind(db::now())
    .execute(&mut *tx)
    .await?;
    let enabled =
        sqlx::query("SELECT id,model_group FROM model WHERE enabled=1 AND reviewed_at IS NOT NULL")
            .fetch_all(&mut *tx)
            .await?;
    for row in &enabled {
        let model: &str = row.get("id");
        if policy::blocked(model, row.get("model_group")) {
            continue;
        }
        sqlx::query("INSERT INTO key_model_grant(key_id,model_id) VALUES(?,?)")
            .bind(&id)
            .bind(model)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id":id,"secret":secret.as_str(),"prefix":prefix})),
    ))
}
async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let n = sqlx::query("UPDATE api_key SET revoked_at=COALESCE(revoked_at,?) WHERE id=?")
        .bind(db::now())
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("Key not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grants {
    models: Vec<String>,
}
async fn set_grants(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<Grants>,
) -> Result<StatusCode> {
    if input.models.len() > 200 {
        return Err(AppError::bad("Too many models"));
    }
    let mut tx = state.db.begin().await?;
    require_active_key(&mut *tx, &id).await?;
    for model in &input.models {
        let group: Option<String> = sqlx::query_scalar(
            "SELECT model_group FROM model WHERE id=? AND enabled=1 AND reviewed_at IS NOT NULL",
        )
        .bind(model)
        .fetch_optional(&mut *tx)
        .await?;
        if group.is_none() || policy::blocked(model, group.as_deref().unwrap_or("")) {
            return Err(AppError::bad(
                "Only reviewed, enabled models other than Fable 5.1 can be granted",
            ));
        }
    }
    // The form lists only enabled models, so only their grants are replaced. Grants for
    // disabled models stay and take effect again when the model is re-enabled.
    sqlx::query("DELETE FROM key_model_grant WHERE key_id=? AND model_id IN (SELECT id FROM model WHERE enabled=1 AND reviewed_at IS NOT NULL)")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    for model in input.models {
        sqlx::query("INSERT OR IGNORE INTO key_model_grant(key_id,model_id) VALUES(?,?)")
            .bind(&id)
            .bind(model)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn client_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    require_active_key(&state.db, &id).await?;
    let rows=sqlx::query("SELECT m.id,m.model_group FROM model m JOIN key_model_grant g ON m.id=g.model_id WHERE g.key_id=? AND m.enabled=1 AND m.reviewed_at IS NOT NULL ORDER BY m.id").bind(&id).fetch_all(&state.db).await?;
    let models: Vec<String> = rows
        .iter()
        .filter(|r| !policy::blocked(r.get("id"), r.get("model_group")))
        .map(|r| r.get("id"))
        .collect();
    Ok(Json(
        json!({"providers":{"shared-claude":{"adapter":"anthropic","baseUrl":state.config.public_origin,"authMode":"key","apiKey":"${SHARED_CLAUDE_API_KEY}","models":models}}}),
    ))
}

async fn models(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM model ORDER BY display_name")
        .fetch_all(&state.db)
        .await?;
    Ok(Json(json!(rows.iter().map(|r|json!({"id":r.get::<String,_>("id"),"display_name":r.get::<String,_>("display_name"),"enabled":r.get::<bool,_>("enabled"),"reviewed_at":r.get::<Option<String>,_>("reviewed_at"),"blocked":policy::blocked(r.get("id"),r.get("model_group"))})).collect::<Vec<_>>())))
}
async fn refresh_models(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let mut access = oauth::access(&state).await?;
    let mut refreshed = false;
    let mut after: Option<String> = None;
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut done = false;
    for _ in 0..20 {
        let response = loop {
            let response = models_page(&state, &access, after.as_deref()).await?;
            match response.status() {
                // Listing models is idempotent, so the page is fetched again with new tokens.
                StatusCode::UNAUTHORIZED if !refreshed => {
                    access = oauth::force_refresh(&state, access.generation).await?;
                    refreshed = true;
                }
                StatusCode::UNAUTHORIZED => return reject_credential(&state, &access).await,
                StatusCode::FORBIDDEN => {
                    // Most 403s concern permissions, not the owner's token.
                    if proxy::upstream_error_type(response).await == Some("authentication_error") {
                        return reject_credential(&state, &access).await;
                    }
                    return Err(AppError::forbidden("Claude did not allow listing models"));
                }
                _ => break response,
            }
        };
        if !response.status().is_success() {
            return Err(AppError::upstream());
        }
        let value: Value =
            serde_json::from_slice(&proxy::limited_body(response, 2 * 1024 * 1024).await?)
                .map_err(|_| AppError::upstream())?;
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(AppError::upstream)?;
        for item in data {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(AppError::upstream)?;
            // Variant spellings (`.`, `_`, `@`, uppercase) are kept so that policy can file
            // them under the blocked group instead of silently dropping them.
            if id.len() > 150
                || !id
                    .get(..7)
                    .is_some_and(|p| p.eq_ignore_ascii_case("claude-"))
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-._@".contains(&c))
            {
                continue;
            }
            let name = item
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(id);
            if name.len() > 200 {
                continue;
            }
            if seen.insert(id.to_owned()) {
                candidates.push((id.to_owned(), name.to_owned()));
            }
        }
        if value.get("has_more") != Some(&Value::Bool(true)) {
            done = true;
            break;
        }
        let cursor = value
            .get("last_id")
            .and_then(Value::as_str)
            .ok_or_else(AppError::upstream)?;
        if after.as_deref() == Some(cursor) {
            return Err(AppError::upstream());
        }
        after = Some(cursor.to_owned());
    }
    if !done {
        return Err(AppError::upstream());
    }
    let mut tx = state.db.begin().await?;
    for (id, name) in &candidates {
        let group = policy::catalog_group(id, name);
        // An existing row keeps its reviewed group unless policy now blocks the model; then it
        // moves to the blocked group and is disabled.
        let sql = if group == policy::FABLE_GROUP {
            "INSERT INTO model(id,display_name,model_group) VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name,model_group=excluded.model_group,enabled=0"
        } else {
            "INSERT INTO model(id,display_name,model_group) VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name"
        };
        sqlx::query(sql)
            .bind(id)
            .bind(name)
            .bind(group)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(
        json!({"discovered":candidates.len(),"message":"Review new models before granting access"}),
    ))
}

/// Requests one page of the upstream model catalog.
async fn models_page(
    state: &AppState,
    access: &oauth::Access,
    after: Option<&str>,
) -> Result<reqwest::Response> {
    let mut request = state
        .client
        .get(format!("{}/v1/models", state.upstream))
        .timeout(CATALOG_REQUEST_TIMEOUT)
        .headers(oauth::upstream_headers(access)?)
        .query(&[("limit", "100")]);
    if let Some(cursor) = after {
        request = request.query(&[("after_id", cursor)]);
    }
    request.send().await.map_err(|_| AppError::upstream())
}

/// Upstream rejected tokens that were just refreshed, or reported an authentication error:
/// only a new login can help.
async fn reject_credential<T>(state: &AppState, access: &oauth::Access) -> Result<T> {
    oauth::mark_reauth(state, access.generation).await?;
    Err(AppError::reauth())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    enabled: bool,
}
async fn review_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<Review>,
) -> Result<StatusCode> {
    let group: Option<String> = sqlx::query_scalar("SELECT model_group FROM model WHERE id=?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?;
    let group =
        group.ok_or_else(|| AppError::bad("Refresh the catalog to discover this model first"))?;
    if input.enabled && policy::blocked(&id, &group) {
        return Err(AppError::forbidden(policy::FABLE_RESERVED));
    }
    sqlx::query("UPDATE model SET enabled=?,reviewed_at=? WHERE id=?")
        .bind(input.enabled)
        .bind(db::now())
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Alias {
    alias: String,
}
async fn add_alias(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<Alias>,
) -> Result<StatusCode> {
    let alias = input.alias;
    if alias.is_empty()
        || alias.len() > 150
        || !alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_/".contains(&b))
        || policy::blocked(&alias, "")
    {
        return Err(AppError::bad("Invalid or reserved alias"));
    }
    let mut tx = state.db.begin().await?;
    let group: Option<String> = sqlx::query_scalar(
        "SELECT model_group FROM model WHERE id=? AND reviewed_at IS NOT NULL AND enabled=1",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?;
    if group.is_none() || policy::blocked(&id, group.as_deref().unwrap_or("")) {
        return Err(AppError::bad("Choose a reviewed and enabled model"));
    }
    let occupied:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM model WHERE id=?)+(SELECT count(*) FROM model_alias WHERE alias=?)").bind(&alias).bind(&alias).fetch_one(&mut *tx).await?;
    if occupied != 0 {
        return Err(AppError::bad("That alias is already in use"));
    }
    sqlx::query("INSERT INTO model_alias(alias,model_id) VALUES(?,?)")
        .bind(alias)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::CREATED)
}
