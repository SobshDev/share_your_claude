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

/// A database with one person and one key, for tests that need `request_usage` rows.
async fn database_with_key(dir: &Path) -> (sqlx::SqlitePool, String) {
    let pool = db::connect(&url(&dir.join("router.sqlite"))).await.unwrap();
    sqlx::query("INSERT INTO person(id,name,created_at) VALUES('p','Alex','now')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_key(id,person_id,label,prefix,secret_hash,created_at) VALUES('k','p','Laptop','sr_test',x'00','now')")
        .execute(&pool)
        .await
        .unwrap();
    (pool, "k".into())
}

#[tokio::test]
async fn shutdown_records_open_requests_with_their_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (pool, key) = database_with_key(dir.path()).await;
    let counted = crate::usage::start(&pool, &key, "/v1/messages", MODEL)
        .await
        .unwrap();
    let mut checkpoint = crate::usage::Usage::default();
    checkpoint.merge(Some(&json!({"input_tokens":12,"output_tokens":3})));
    checkpoint.checkpoint(&pool, &counted, false).await.unwrap();
    let uncounted = crate::usage::start(&pool, &key, "/v1/messages", MODEL)
        .await
        .unwrap();
    let finished = crate::usage::start(&pool, &key, "/v1/messages", MODEL)
        .await
        .unwrap();
    crate::usage::finish(&pool, &finished, "completed", Some(200), false)
        .await
        .unwrap();

    let before = db::now();
    assert_eq!(db::interrupt_in_flight(&pool).await.unwrap(), 2);
    let after = db::now();
    for (id, state, input, output) in [
        (&counted, "partial", Some(12), Some(3)),
        (&uncounted, "unknown", None, None),
    ] {
        let row = sqlx::query("SELECT * FROM request_usage WHERE id=?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("outcome"), "interrupted");
        assert_eq!(row.get::<String, _>("usage_state"), state);
        assert_eq!(row.get::<Option<i64>, _>("input_tokens"), input);
        assert_eq!(row.get::<Option<i64>, _>("output_tokens"), output);
        let finished_at = row.get::<String, _>("finished_at");
        assert!(
            before <= finished_at && finished_at <= after,
            "{finished_at}"
        );
    }
    let outcome: String = sqlx::query_scalar("SELECT outcome FROM request_usage WHERE id=?")
        .bind(&finished)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outcome, "completed");
    pool.close().await;
}
