//! Startup: database creation, recovery, and upgrades from older migration sets.
use super::*;
use std::path::Path;

fn url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn connect_creates_missing_database_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/data/router.sqlite");
    let pool = db::connect(&url(&path)).await.unwrap();
    pool.close().await;
    assert!(path.is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn new_database_and_wal_files_are_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data/router.sqlite");
    let pool = db::connect(&url(&path)).await.unwrap();
    sqlx::query("INSERT INTO person(id,name,created_at) VALUES('p','Alex','now')")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(mode(&path), 0o600);
    for suffix in ["-wal", "-shm"] {
        let sidecar = path.with_file_name(format!("router.sqlite{suffix}"));
        assert_eq!(mode(&sidecar), 0o600, "{}", sidecar.display());
    }
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    pool.close().await;
}

#[tokio::test]
async fn backup_writes_an_owner_only_copy_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let source = url(&dir.path().join("router.sqlite"));
    let pool = db::connect(&source).await.unwrap();
    sqlx::query("INSERT INTO person(id,name,created_at) VALUES('p','Alex','now')")
        .execute(&pool)
        .await
        .unwrap();

    let backup = dir.path().join("backup.sqlite");
    db::backup(&source, &backup).await.unwrap();
    #[cfg(unix)]
    assert_eq!(mode(&backup), 0o600);
    let copy = sqlx::SqlitePool::connect(&url(&backup)).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM person")
        .fetch_one(&copy)
        .await
        .unwrap();
    assert_eq!(name, "Alex");
    copy.close().await;

    let before = std::fs::read(&backup).unwrap();
    let error = db::backup(&source, &backup).await.unwrap_err();
    assert_eq!(error.to_string(), "Backup destination already exists");
    assert_eq!(std::fs::read(&backup).unwrap(), before);
    pool.close().await;
}

#[tokio::test]
async fn failed_backup_removes_the_file_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.sqlite");
    let missing = url(&dir.path().join("missing.sqlite"));
    assert!(db::backup(&missing, &backup).await.is_err());
    assert!(!backup.exists());
    assert!(!dir.path().join("missing.sqlite").exists());
}
