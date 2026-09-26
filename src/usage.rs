use crate::{db, error::Result};
use serde_json::{Map, Value};
use sqlx::SqlitePool;

#[derive(Default, Debug)]
pub struct Usage {
    values: Map<String, Value>,
    invalid: bool,
}

impl Usage {
    pub fn merge(&mut self, value: Option<&Value>) -> bool {
        let Some(value) = value else { return false };
        let Some(object) = value.as_object() else {
            self.invalid = true;
            return true;
        };
        // Persist an allowlist of accounting fields, never arbitrary upstream payloads.
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            // Anthropic declares several delta counters nullable; null means "not reported".
            if let Some(value) = object.get(key).filter(|v| !v.is_null()) {
                if let Some(n) = value.as_i64().filter(|n| *n >= 0) {
                    // A cumulative counter moving backwards makes final accounting uncertain.
                    if self
                        .values
                        .get(key)
                        .and_then(Value::as_i64)
                        .is_some_and(|old| n < old)
                    {
                        self.invalid = true;
                    }
                    self.values.insert(key.into(), n.into());
                } else {
                    self.invalid = true;
                }
            }
        }
        true
    }
    pub fn get(&self, field: &str) -> Option<i64> {
        self.values.get(field).and_then(Value::as_i64)
    }
    pub fn state(&self, completed: bool) -> &'static str {
        if self.values.is_empty() {
            "unknown"
        } else if completed
            && !self.invalid
            && self.get("input_tokens").is_some()
            && self.get("output_tokens").is_some()
        {
            "complete"
        } else {
            "partial"
        }
    }
    pub async fn checkpoint(&self, pool: &SqlitePool, id: &str, completed: bool) -> Result<()> {
        let status = self.state(completed);
        let cache_read = self
            .get("cache_read_input_tokens")
            .or_else(|| (status == "complete").then_some(0));
        let cache_write = self
            .get("cache_creation_input_tokens")
            .or_else(|| (status == "complete").then_some(0));
        let raw = (!self.values.is_empty()).then(|| Value::Object(self.values.clone()).to_string());
        sqlx::query("UPDATE request_usage SET usage_state=?,input_tokens=?,cache_read_tokens=?,cache_write_tokens=?,output_tokens=?,raw_usage=? WHERE id=? AND outcome='in_progress'")
            .bind(status).bind(self.get("input_tokens")).bind(cache_read).bind(cache_write).bind(self.get("output_tokens"))
            .bind(raw).bind(id).execute(pool).await?;
        Ok(())
    }
}

pub async fn start(pool: &SqlitePool, key_id: &str, endpoint: &str, model: &str) -> Result<String> {
    let id = db::id();
    sqlx::query("INSERT INTO request_usage(id,key_id,endpoint,requested_model,started_at,outcome) VALUES(?,?,?,?,?,'in_progress')")
        .bind(&id).bind(key_id).bind(endpoint).bind(model).bind(db::now()).execute(pool).await?;
    Ok(id)
}

pub async fn finish(
    pool: &SqlitePool,
    id: &str,
    outcome: &str,
    status: Option<u16>,
    not_applicable: bool,
) -> Result<()> {
    sqlx::query("UPDATE request_usage SET outcome=?,http_status=COALESCE(?,http_status),finished_at=?,usage_state=CASE WHEN ? THEN 'not_applicable' WHEN ? != 'completed' AND usage_state='complete' THEN 'partial' ELSE usage_state END WHERE id=? AND outcome='in_progress'")
        .bind(outcome).bind(status.map(i64::from)).bind(db::now()).bind(not_applicable).bind(outcome).bind(id).execute(pool).await?;
    Ok(())
}

/// Cancellation before response headers, or an unexpectedly dropped worker, still gets a terminal record.
pub struct RequestGuard {
    pool: SqlitePool,
    id: String,
    armed: bool,
}
impl RequestGuard {
    pub fn new(pool: SqlitePool, id: String) -> Self {
        Self {
            pool,
            id,
            armed: true,
        }
    }
    pub async fn finish(
        &mut self,
        outcome: &str,
        status: Option<u16>,
        not_applicable: bool,
    ) -> Result<()> {
        finish(&self.pool, &self.id, outcome, status, not_applicable).await?;
        self.armed = false;
        Ok(())
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed {
            let pool = self.pool.clone();
            let id = self.id.clone();
            tokio::spawn(async move {
                if finish(&pool, &id, "interrupted", None, false)
                    .await
                    .is_err()
                {
                    tracing::error!(
                        "failed to finalize interrupted request; startup recovery will repair it"
                    );
                }
            });
        }
    }
}
