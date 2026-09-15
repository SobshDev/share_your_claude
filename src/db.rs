use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{str::FromStr, time::Duration};

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

pub async fn connect(url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    recover(&pool).await?;
    Ok(pool)
}

pub async fn recover(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE request_usage SET outcome='interrupted', finished_at=?, usage_state=CASE WHEN raw_usage IS NULL THEN 'unknown' ELSE 'partial' END WHERE outcome='in_progress'")
        .bind(now()).execute(pool).await?;
    sqlx::query("DELETE FROM admin_session WHERE expires_at <= ?")
        .bind(epoch())
        .execute(pool)
        .await?;
    Ok(())
}
