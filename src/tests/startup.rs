//! Startup: database creation, recovery, and upgrades from older migration sets.
use super::*;

#[tokio::test]
async fn connect_creates_missing_database_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/data/router.sqlite");
    let pool = db::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    pool.close().await;
    assert!(path.is_file());
}
