use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use aionui_db::{
    CreateProviderParams, IProviderRepository, SqliteProviderRepository, UpdateProviderParams, init_database,
    init_database_memory, maybe_copy_legacy_database,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

fn legacy_fixture(path: &Path) -> rusqlite::Connection {
    let source = rusqlite::Connection::open(path).unwrap();
    source
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES ('checkpointed');
             PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO retained VALUES ('committed-in-wal');",
        )
        .unwrap();
    source
}

#[test]
fn legacy_snapshot_includes_committed_wal_without_closing_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_fixture(&dir.path().join("aionui.db"));
    let target = dir.path().join("aionui-backend.db");
    maybe_copy_legacy_database(&target).unwrap();
    let copy = rusqlite::Connection::open(&target).unwrap();
    let count: i64 = copy
        .query_row("SELECT count(*) FROM retained", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "the committed WAL row must be part of the independent snapshot"
    );
    let original_count: i64 = source
        .query_row("SELECT count(*) FROM retained", [], |row| row.get(0))
        .unwrap();
    assert_eq!(original_count, 2);
}

#[test]
fn invalid_legacy_database_does_not_publish_a_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("aionui.db");
    let target = dir.path().join("aionui-backend.db");
    std::fs::write(&source, b"not a database").unwrap();
    let error = maybe_copy_legacy_database(&target).expect_err("invalid source must not become the new database");
    assert!(error.to_string().contains("legacy"));
    assert!(!target.exists());
    assert_eq!(std::fs::read(&source).unwrap(), b"not a database");
}

#[test]
fn legacy_snapshot_archives_an_orphan_rollback_journal() {
    let dir = tempfile::tempdir().unwrap();
    let _source = legacy_fixture(&dir.path().join("aionui.db"));
    let target = dir.path().join("aionui-backend.db");
    let journal = dir.path().join("aionui-backend.db-journal");
    let old = dir.path().join("previous.db");
    let previous = rusqlite::Connection::open(&old).unwrap();
    previous
        .execute_batch("CREATE TABLE old(value TEXT); BEGIN IMMEDIATE; INSERT INTO old VALUES ('pending')")
        .unwrap();
    let saved_journal = std::fs::read(dir.path().join("previous.db-journal")).unwrap();
    assert!(!saved_journal.is_empty());
    std::fs::write(&journal, &saved_journal).unwrap();
    drop(previous);
    maybe_copy_legacy_database(&target).unwrap();
    assert!(
        !journal.exists(),
        "old rollback journals must not be attached to the new snapshot"
    );
    let archive = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir() && path.file_name().unwrap().to_string_lossy().contains(".backup."))
        .unwrap();
    assert_eq!(
        std::fs::read(archive.join("aionui-backend.db-journal")).unwrap(),
        saved_journal
    );
}

#[tokio::test]
async fn compiled_sqlite_includes_wal_reset_fix() {
    let db = init_database_memory().await.unwrap();
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let components: Vec<u32> = version.split('.').map(|part| part.parse().unwrap()).collect();
    assert!(
        components.as_slice() >= [3, 51, 3].as_slice(),
        "unfixed SQLite {version}"
    );
    db.close().await;
}

#[tokio::test]
async fn concurrent_provider_partial_updates_preserve_independent_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("provider.db");
    let db = init_database(&path).await.unwrap();
    let provider = SqliteProviderRepository::new(db.pool().clone())
        .create(CreateProviderParams {
            id: Some("parallel-provider"),
            user_id: "system_default_user",
            platform: "openai",
            name: "original",
            base_url: "https://example.invalid",
            api_key_encrypted: "synthetic",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .unwrap();
    db.close().await;

    // On the old SELECT-then-UPDATE implementation, release both SELECTs
    // together so both writers carry the same stale snapshot. The fixed
    // single UPDATE commits before releasing its connection.
    let enabled = Arc::new(AtomicBool::new(false));
    let releases = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let pool = SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .after_release({
            let enabled = enabled.clone();
            move |_, _| {
                let enabled = enabled.clone();
                let releases = releases.clone();
                let barrier = barrier.clone();
                Box::pin(async move {
                    if enabled.load(Ordering::SeqCst) && releases.fetch_add(1, Ordering::SeqCst) < 2 {
                        barrier.wait().await;
                    }
                    Ok(true)
                })
            }
        })
        .connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let repo = SqliteProviderRepository::new(pool.clone());
    enabled.store(true, Ordering::SeqCst);
    let updates = async {
        tokio::try_join!(
            repo.update(
                "system_default_user",
                &provider.id,
                UpdateProviderParams {
                    name: Some("renamed"),
                    ..Default::default()
                }
            ),
            repo.update(
                "system_default_user",
                &provider.id,
                UpdateProviderParams {
                    models: Some("[\"retained-model\"]"),
                    ..Default::default()
                }
            ),
        )
    };
    tokio::time::timeout(Duration::from_secs(5), updates)
        .await
        .unwrap()
        .unwrap();
    let updated = repo
        .find_by_id("system_default_user", &provider.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.models, "[\"retained-model\"]");
    pool.close().await;
}
