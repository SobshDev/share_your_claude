use anyhow::Context;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::PathBuf, str::FromStr, time::Duration};

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
    if let Some(parent) = database_file(url, &options)
        .as_deref()
        .and_then(std::path::Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create the database directory {}",
                parent.display()
            )
        })?;
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    recover(&pool).await?;
    Ok(pool)
}

/// The on-disk database file named by `url`, or `None` for an in-memory database.
fn database_file(url: &str, options: &SqliteConnectOptions) -> Option<PathBuf> {
    let filename = options.get_filename();
    let params = url.split_once('?').map_or("", |(_, params)| params);
    let in_memory = filename
        .to_str()
        .is_some_and(|name| name.starts_with("file:sqlx-in-memory-"))
        || url::form_urlencoded::parse(params.as_bytes())
            .any(|(key, value)| key == "mode" && value == "memory");
    (!in_memory).then(|| filename.to_path_buf())
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
