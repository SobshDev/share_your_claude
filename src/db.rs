use anyhow::Context;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    fs::{DirBuilder, OpenOptions},
    io,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

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
    if let Some(path) = database_file(url, &options) {
        create_private_file(&path)?;
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

/// Creates the database file and any missing parent directories before SQLite opens it, so
/// they are private to the owner (0600 file, 0700 directories) regardless of the umask.
/// SQLite gives the `-wal` and `-shm` files the same mode as the database file.
/// Existing files and directories keep their current permissions.
fn create_private_file(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(parent).with_context(|| {
            format!(
                "could not create the database directory {}",
                parent.display()
            )
        })?;
    }
    match private_options().open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("could not create the database file {}", path.display())),
    }
}

/// Options that create a new file readable and writable only by its owner.
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Writes a consistent copy of the database at `url` to a new file at `destination`.
///
/// The destination is created empty with owner-only permissions before any data is written,
/// so the backup is never readable by other users. An existing destination is never
/// overwritten, and a failed backup removes the file it created.
pub async fn backup(url: &str, destination: &Path) -> anyhow::Result<()> {
    let target = destination
        .to_str()
        .context("The backup path must be valid UTF-8")?;
    match private_options().open(destination) {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            anyhow::bail!("Backup destination already exists")
        }
        Err(error) => return Err(error).context("Could not create the backup file"),
    }
    let result = async {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str(url)?)
            .await?;
        // SQLite accepts an existing empty file as the VACUUM INTO target.
        let vacuum = sqlx::query("VACUUM INTO ?")
            .bind(target)
            .execute(&pool)
            .await;
        pool.close().await;
        vacuum?;
        anyhow::Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
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
