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

/// Creates a database file at `path` with only the migrations up to `version` applied, as an
/// older release would have left it. Returns a pool so the caller can add data for that schema.
async fn database_at(path: &Path, version: i64) -> sqlx::SqlitePool {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let older = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let number: i64 = name.split('_').next().unwrap().parse().unwrap();
        if number <= version {
            std::fs::copy(entry.path(), older.path().join(&name)).unwrap();
        }
    }
    let migrator = sqlx::migrate::Migrator::new(older.path()).await.unwrap();
    assert_eq!(migrator.iter().last().unwrap().version, version);
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    migrator.run(&pool).await.unwrap();
    pool
}

/// Startup against a database from the first release: every later migration applies, data is
/// kept, requests left open by a crash are recovered, and expired sessions are removed.
/// Later migrations extend this test by asserting their own schema changes below.
#[tokio::test]
async fn startup_upgrades_an_initial_database_and_recovers_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let old = database_at(&path, 1).await;
    for sql in [
        "INSERT INTO person(id,name,created_at) VALUES('p','Alex','2026-01-01T00:00:00.000Z')",
        "INSERT INTO api_key(id,person_id,label,prefix,secret_hash,created_at) VALUES('k','p','Laptop','sr_test',x'00','2026-01-01T00:00:00.000Z')",
        "INSERT INTO request_usage(id,key_id,endpoint,requested_model,started_at,outcome,usage_state,input_tokens,output_tokens,raw_usage) VALUES('counted','k','/v1/messages','claude-sonnet-4-6','2026-01-01T00:00:01.000Z','in_progress','complete',12,3,'{\"input_tokens\":12,\"output_tokens\":3}')",
        "INSERT INTO request_usage(id,key_id,endpoint,requested_model,started_at,outcome) VALUES('uncounted','k','/v1/messages','claude-sonnet-4-6','2026-01-01T00:00:02.000Z','in_progress')",
        "INSERT INTO request_usage(id,key_id,endpoint,requested_model,started_at,finished_at,outcome,usage_state) VALUES('done','k','/v1/messages','claude-sonnet-4-6','2026-01-01T00:00:03.000Z','2026-01-01T00:00:04.000Z','completed','unknown')",
        "INSERT INTO model(id,display_name,model_group,enabled,reviewed_at) VALUES('claude-sonnet-4-6','Sonnet','claude',1,'now')",
        "INSERT INTO key_model_grant(key_id,model_id) VALUES('k','claude-sonnet-4-6'),('k','claude-fable-5-1')",
    ] {
        sqlx::query(sql).execute(&old).await.unwrap();
    }
    for (hash, expires_at) in [
        (&b"expired"[..], db::epoch() - 1),
        (b"current", db::epoch() + 3600),
    ] {
        sqlx::query(
            "INSERT INTO admin_session(token_hash,csrf_token,expires_at) VALUES(?,'csrf',?)",
        )
        .bind(hash)
        .bind(expires_at)
        .execute(&old)
        .await
        .unwrap();
    }
    old.close().await;

    let pool = db::connect(&url(&path)).await.unwrap();

    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    let embedded: Vec<i64> = sqlx::migrate!().iter().map(|m| m.version).collect();
    assert_eq!(applied, embedded);
    let index = |name: &'static str| {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?",
        )
        .bind(name)
        .fetch_one(&pool)
    };
    // 0002
    assert_eq!(index("usage_endpoint_time").await.unwrap(), 1);
    // 0004
    assert_eq!(index("usage_model_time").await.unwrap(), 0);
    assert_eq!(index("usage_endpoint_model_time").await.unwrap(), 1);
    // 0005: an erroneous Fable grant from an older release is removed; other grants stay.
    let grants: Vec<String> =
        sqlx::query_scalar("SELECT model_id FROM key_model_grant WHERE key_id='k'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(grants, ["claude-sonnet-4-6"]);

    let name: String = sqlx::query_scalar("SELECT name FROM person WHERE id='p'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Alex");
    let rows = sqlx::query("SELECT * FROM request_usage ORDER BY started_at")
        .fetch_all(&pool)
        .await
        .unwrap();
    let summary: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("id"),
                row.get::<String, _>("outcome"),
                row.get::<String, _>("usage_state"),
                row.get::<Option<i64>, _>("input_tokens"),
                row.get::<Option<String>, _>("finished_at"),
            )
        })
        .collect();
    let row = |id: &str, outcome: &str, state: &str, input: Option<i64>, finished: Option<&str>| {
        (
            id.to_owned(),
            outcome.to_owned(),
            state.to_owned(),
            input,
            finished.map(str::to_owned),
        )
    };
    assert_eq!(
        summary,
        [
            // The stop time of a crashed process is unknown, so recovery leaves it empty.
            row("counted", "interrupted", "partial", Some(12), None),
            row("uncounted", "interrupted", "unknown", None, None),
            row(
                "done",
                "completed",
                "unknown",
                None,
                Some("2026-01-01T00:00:04.000Z")
            ),
        ]
    );
    let sessions: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM admin_session")
        .fetch_all(&pool)
        .await
        .unwrap();
    // Migration 0003 binds sessions to the password hash and clears every older session,
    // so the unexpired session from 0001 is gone too.
    assert!(sessions.is_empty());
    pool.close().await;
}

#[tokio::test]
async fn model_filtered_reports_can_use_the_model_index() {
    let dir = tempfile::tempdir().unwrap();
    let (pool, _) = database_with_key(dir.path()).await;
    let plan: Vec<String> = sqlx::query("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM request_usage u WHERE u.endpoint='/v1/messages' AND COALESCE(u.resolved_model,u.requested_model)=? AND u.started_at>=? AND u.started_at<?")
        .bind(MODEL)
        .bind("2026-01-01")
        .bind("2026-02-01")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get("detail"))
        .collect();
    assert!(
        plan.iter()
            .any(|step| step.contains("usage_endpoint_model_time")),
        "{plan:?}"
    );
    pool.close().await;
}

#[tokio::test]
async fn database_rejects_key_labels_outside_the_length_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (pool, key) = database_with_key(dir.path()).await;
    let insert = |id: &'static str, label: String| {
        sqlx::query("INSERT INTO api_key(id,person_id,label,prefix,secret_hash,created_at) VALUES(?,'p',?,'sr_test',?,'now')")
            .bind(id)
            .bind(label)
            .bind(id.as_bytes())
            .execute(&pool)
    };
    assert!(insert("empty", String::new()).await.is_err());
    assert!(insert("long", "x".repeat(101)).await.is_err());
    insert("limit", "é".repeat(100)).await.unwrap();
    let rename = |label: String| {
        sqlx::query("UPDATE api_key SET label=? WHERE id=?")
            .bind(label)
            .bind(&key)
            .execute(&pool)
    };
    assert!(rename("x".repeat(101)).await.is_err());
    rename("Desktop".into()).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn database_refuses_to_grant_fable() {
    let dir = tempfile::tempdir().unwrap();
    let (pool, key) = database_with_key(dir.path()).await;
    for (id, group) in [
        ("claude-fable-5-1-20260901", "claude"),
        ("CLAUDE-FABLE-5-1", "claude"),
        ("renamed-fable", "fable-5.1"),
        ("claude-sonnet-4-6", "claude"),
    ] {
        sqlx::query("INSERT INTO model(id,display_name,model_group,enabled,reviewed_at) VALUES(?,?,?,1,'now')")
            .bind(id)
            .bind(id)
            .bind(group)
            .execute(&pool)
            .await
            .unwrap();
    }
    let grant = |model: &'static str| {
        sqlx::query("INSERT INTO key_model_grant(key_id,model_id) VALUES(?,?)")
            .bind(&key)
            .bind(model)
            .execute(&pool)
    };
    for model in [
        "claude-fable-5-1",
        "claude-fable-5-1-20260901",
        "CLAUDE-FABLE-5-1",
        "renamed-fable",
    ] {
        let error = grant(model).await.unwrap_err();
        assert!(
            error.to_string().contains("Fable 5.1 cannot be granted"),
            "{model}: {error}"
        );
    }
    grant("claude-sonnet-4-6").await.unwrap();
    let moved =
        sqlx::query("UPDATE key_model_grant SET model_id='claude-fable-5-1' WHERE key_id=?")
            .bind(&key)
            .execute(&pool)
            .await;
    assert!(moved.is_err());

    // Regrouping a granted model as Fable revokes its grants.
    sqlx::query("UPDATE model SET model_group='fable-5.1' WHERE id='claude-sonnet-4-6'")
        .execute(&pool)
        .await
        .unwrap();
    let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM key_model_grant")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
    pool.close().await;
}
