use crate::{
    AppState,
    error::{AppError, Result},
};
use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};
use std::sync::Arc;

#[derive(Deserialize, Default)]
pub struct UsageQuery {
    pub group_by: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub person_id: Option<String>,
    pub key_id: Option<String>,
    pub model: Option<String>,
    pub offset: Option<u32>,
}

struct Filter {
    from: String,
    to: String,
    query: UsageQuery,
}

impl Filter {
    fn new(query: UsageQuery) -> Result<Self> {
        let now = Utc::now();
        let timestamp = |value: Option<&str>, default: DateTime<Utc>| -> Result<String> {
            let date = match value {
                Some(v) => DateTime::parse_from_rfc3339(v)
                    .map_err(|_| AppError::bad("Use RFC3339 timestamps for from and to"))?
                    .with_timezone(&Utc),
                None => default,
            };
            // SQLite and the stored timestamps use four-digit UTC years.
            if !(1..=9999).contains(&chrono::Datelike::year(&date)) {
                return Err(AppError::bad("Choose dates between years 0001 and 9999"));
            }
            Ok(date.to_rfc3339_opts(SecondsFormat::Millis, true))
        };
        let from = timestamp(query.from.as_deref(), now - Duration::days(30))?;
        let to = timestamp(query.to.as_deref(), now)?;
        if from >= to {
            return Err(AppError::bad("from must precede to"));
        }
        Ok(Self { from, to, query })
    }

    fn bind<'a>(
        &'a self,
        sql: &'a str,
    ) -> sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>> {
        sqlx::query(sql)
            .bind(&self.from)
            .bind(&self.to)
            .bind(&self.query.person_id)
            .bind(&self.query.person_id)
            .bind(&self.query.key_id)
            .bind(&self.query.key_id)
            .bind(&self.query.model)
            .bind(&self.query.model)
    }
}

const SOURCE: &str =
    "FROM request_usage u JOIN api_key k ON k.id=u.key_id JOIN person p ON p.id=k.person_id
    WHERE u.endpoint='/v1/messages' AND u.started_at>=? AND u.started_at<?
    AND (? IS NULL OR p.id=?) AND (? IS NULL OR k.id=?)
    AND (? IS NULL OR COALESCE(u.resolved_model,u.requested_model)=?)";
const TOTAL: &str = "CASE WHEN u.raw_usage IS NOT NULL THEN COALESCE(u.input_tokens,0)+COALESCE(u.cache_read_tokens,0)+COALESCE(u.cache_write_tokens,0)+COALESCE(u.output_tokens,0) END";
const COUNTS: &[&str] = &[
    "requests",
    "completed_requests",
    "denied_requests",
    "errors",
    "in_progress_requests",
    "incomplete_requests",
    "complete_requests",
    "partial_requests",
    "unknown_requests",
    "active_users",
    "models_used",
    "duration_samples",
];
const TOKENS: &[&str] = &[
    "input_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "output_tokens",
    "observed_total_tokens",
];

async fn aggregate(
    conn: &mut SqliteConnection,
    filter: &Filter,
    group: &str,
) -> Result<Vec<Value>> {
    let (expr, label) = match group {
        "total" => ("'total'", "'All users'"),
        "person" => ("p.id", "p.name"),
        "key" => ("k.id", "p.name || ' / ' || k.label"),
        "model" => (
            "COALESCE(u.resolved_model,u.requested_model)",
            "COALESCE(u.resolved_model,u.requested_model)",
        ),
        "day" => ("substr(u.started_at,1,10)", "substr(u.started_at,1,10)"),
        _ => return Err(AppError::bad("group_by must be person, key, model, or day")),
    };
    let grouping = if group == "total" {
        String::new()
    } else {
        format!("GROUP BY {expr} ORDER BY {label}, {expr}")
    };
    // Only fixed expressions are interpolated. Every user filter is bound.
    let sql = format!("SELECT {expr} AS id,{label} AS label,COUNT(*) AS requests,
        COUNT(CASE WHEN u.outcome='completed' THEN 1 END) AS completed_requests,
        COUNT(CASE WHEN u.outcome='denied' THEN 1 END) AS denied_requests,
        COUNT(CASE WHEN u.outcome IN ('upstream_error','interrupted') THEN 1 END) AS errors,
        COUNT(CASE WHEN u.outcome='in_progress' THEN 1 END) AS in_progress_requests,
        COUNT(CASE WHEN u.usage_state IN ('partial','unknown') THEN 1 END) AS incomplete_requests,
        COUNT(CASE WHEN u.usage_state='complete' THEN 1 END) AS complete_requests,
        COUNT(CASE WHEN u.usage_state='partial' THEN 1 END) AS partial_requests,
        COUNT(CASE WHEN u.usage_state='unknown' THEN 1 END) AS unknown_requests,
        COUNT(DISTINCT p.id) AS active_users,
        COUNT(DISTINCT CASE WHEN u.resolved_model IS NOT NULL AND u.outcome!='denied' THEN u.resolved_model END) AS models_used,
        SUM(u.input_tokens) AS input_tokens,SUM(u.cache_read_tokens) AS cache_read_tokens,
        SUM(u.cache_write_tokens) AS cache_write_tokens,SUM(u.output_tokens) AS output_tokens,
        SUM({TOTAL}) AS observed_total_tokens,
        AVG(CASE WHEN u.outcome='completed' THEN MAX(0,(julianday(u.finished_at)-julianday(u.started_at))*86400000) END) AS average_duration_ms,
        COUNT(CASE WHEN u.outcome='completed' AND u.finished_at IS NOT NULL THEN 1 END) AS duration_samples,
        MAX(u.started_at) AS last_used_at {SOURCE} {grouping}");
    let rows = filter.bind(&sql).fetch_all(conn).await?;
    Ok(rows.iter().map(|row| {
        let mut value = json!({"id":row.get::<String,_>("id"), "label":row.get::<String,_>("label"),
            "average_duration_ms":row.get::<Option<f64>,_>("average_duration_ms"),
            "last_used_at":row.get::<Option<String>,_>("last_used_at")});
        for field in COUNTS { value[*field] = json!(row.get::<i64,_>(*field)); }
        for field in TOKENS { value[*field] = json!(row.get::<Option<i64>,_>(*field)); }
        value
    }).collect())
}

pub async fn usage_report(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>> {
    let filter = Filter::new(query)?;
    let group = filter.query.group_by.as_deref().unwrap_or("person");
    if !["person", "key", "model", "day"].contains(&group) {
        return Err(AppError::bad("group_by must be person, key, model, or day"));
    }
    let mut conn = state.db.acquire().await?;
    let data = aggregate(&mut conn, &filter, group).await?;
    Ok(Json(
        json!({"group_by":group,"from":filter.from,"to":filter.to,"data":data}),
    ))
}

pub async fn report(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>> {
    let filter = Filter::new(query)?;
    // One read snapshot keeps totals, charts and request history consistent during traffic.
    let mut tx = state.db.begin().await?;
    let mut result = json!({"from":filter.from,"to":filter.to});
    for group in ["total", "person", "model", "day", "key"] {
        result[group] = json!(aggregate(&mut tx, &filter, group).await?);
    }
    let sql = format!("SELECT u.id,p.id AS person_id,p.name AS person_name,k.label AS key_label,
        u.requested_model,u.resolved_model,u.response_model,u.started_at,u.finished_at,
        u.outcome,u.http_status,u.usage_state,u.input_tokens,u.cache_read_tokens,u.cache_write_tokens,u.output_tokens,
        {TOTAL} AS observed_total_tokens {SOURCE} ORDER BY u.started_at DESC,u.id DESC LIMIT 51 OFFSET ?");
    let offset = filter.query.offset.unwrap_or(0);
    let rows = filter
        .bind(&sql)
        .bind(i64::from(offset))
        .fetch_all(&mut *tx)
        .await?;
    result["has_more"] = json!(rows.len() > 50);
    result["offset"] = json!(offset);
    result["requests"] = json!(
        rows.iter()
            .take(50)
            .map(|row| {
                let mut value = json!({});
                for field in [
                    "id",
                    "person_id",
                    "person_name",
                    "key_label",
                    "requested_model",
                    "resolved_model",
                    "response_model",
                    "started_at",
                    "finished_at",
                    "outcome",
                    "usage_state",
                ] {
                    value[field] = json!(row.get::<Option<String>, _>(field));
                }
                for field in TOKENS.iter().copied().chain(["http_status"]) {
                    value[field] = json!(row.get::<Option<i64>, _>(field));
                }
                value
            })
            .collect::<Vec<_>>()
    );
    tx.commit().await?;
    Ok(Json(result))
}
