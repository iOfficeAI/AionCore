use aionui_db::{
    DatabaseInitOptions, init_database, init_database_memory, init_database_with_options, maybe_copy_legacy_database,
};
use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

// -- T1.1 Initialization --

#[tokio::test]
async fn init_creates_users_table() {
    let db = init_database_memory().await.unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert!(
        count.0 >= 1,
        "users table should exist and have at least the system user"
    );
}

// -- T1.2 Pragma configuration --

#[tokio::test]
async fn pragma_foreign_keys_enabled() {
    let db = init_database_memory().await.unwrap();

    let row: (i64,) = sqlx::query_as("PRAGMA foreign_keys")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.0, 1, "foreign_keys should be ON");
}

#[tokio::test]
async fn pragma_busy_timeout() {
    let db = init_database_memory().await.unwrap();

    let row: (i64,) = sqlx::query_as("PRAGMA busy_timeout")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.0, 5000, "busy_timeout should be 5000ms");
}

#[tokio::test]
async fn pragma_journal_mode_wal_on_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database(&dir.path().join("test.db")).await.unwrap();

    let row: (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(
        row.0.to_lowercase(),
        "wal",
        "journal_mode should be WAL for file-backed DB"
    );
    db.close().await;
}

// -- T1.3 Idempotent re-initialization --

#[tokio::test]
async fn idempotent_reinit_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // First init + insert test data
    let db = init_database(&path).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u1', 'alice', 'hash123', 1000, 1000)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    // Second init — data should persist
    let db = init_database(&path).await.unwrap();
    let row = sqlx::query("SELECT username FROM users WHERE id = 'u1'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("username"), "alice");
    db.close().await;
}

// -- T1.4 Migrations --

#[tokio::test]
async fn migrations_applied() {
    let db = init_database_memory().await.unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert!(count.0 >= 1, "at least one migration should be applied");
}

// -- T1.5 System default user --

#[tokio::test]
async fn system_default_user_exists() {
    let db = init_database_memory().await.unwrap();

    let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE id = 'system_default_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("id"), "system_default_user");
    assert_eq!(row.get::<String, _>("username"), "admin");
    assert_eq!(
        row.get::<String, _>("password_hash"),
        "",
        "system user should have empty password hash"
    );
}

#[tokio::test]
async fn system_user_has_valid_timestamps() {
    let before = aionui_common::now_ms();
    let db = init_database_memory().await.unwrap();
    let after = aionui_common::now_ms();

    let row = sqlx::query("SELECT created_at, updated_at FROM users WHERE id = 'system_default_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    let created = row.get::<i64, _>("created_at");
    let updated = row.get::<i64, _>("updated_at");
    assert!(
        created >= before && created <= after,
        "created_at should be within test window"
    );
    assert!(
        updated >= before && updated <= after,
        "updated_at should be within test window"
    );
}

// -- Schema validation --

#[tokio::test]
async fn users_table_accepts_all_columns() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users \
         (id, username, email, password_hash, avatar_path, jwt_secret, created_at, updated_at, last_login) \
         VALUES ('u1', 'testuser', 'test@example.com', 'hash', '/avatar.png', 'secret', 1000, 2000, 3000)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let row = sqlx::query("SELECT * FROM users WHERE id = 'u1'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("email"), "test@example.com");
    assert_eq!(
        row.get::<Option<String>, _>("avatar_path"),
        Some("/avatar.png".to_string())
    );
    assert_eq!(row.get::<Option<String>, _>("jwt_secret"), Some("secret".to_string()));
    assert_eq!(row.get::<Option<i64>, _>("last_login"), Some(3000));
}

#[tokio::test]
async fn username_unique_constraint() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u1', 'duplicate', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u2', 'duplicate', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "duplicate username should violate unique constraint");
}

#[tokio::test]
async fn email_unique_constraint() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) \
         VALUES ('u1', 'user1', 'same@example.com', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) \
         VALUES ('u2', 'user2', 'same@example.com', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "duplicate email should violate unique constraint");
}

// -- Corruption recovery --

#[tokio::test]
async fn corruption_recovery_creates_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Write invalid content to simulate corruption
    std::fs::write(&path, b"not a valid sqlite database").unwrap();

    let db = init_database_with_options(
        &path,
        DatabaseInitOptions {
            recover_corrupted_database: true,
        },
    )
    .await
    .unwrap();

    // Recovered database should work
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(count.0 >= 1, "recovered DB should have system user");

    // Backup file should exist
    let has_backup = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("backup"));
    assert!(has_backup, "backup of corrupted file should exist");

    db.close().await;
}

#[tokio::test]
async fn corruption_without_recovery_authorization_preserves_original_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let original = b"not a valid sqlite database";
    std::fs::write(&path, original).unwrap();

    let result = init_database(&path).await;

    assert!(result.is_err(), "unconfirmed startup must not rebuild corrupted DB");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let has_backup = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("backup"));
    assert!(!has_backup, "unconfirmed startup must not create a backup file");
}

// -- Directory creation --

#[tokio::test]
async fn creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("nested").join("test.db");

    let db = init_database(&path).await.unwrap();
    assert!(path.exists(), "database file should be created in nested directory");
    db.close().await;
}

// -- Legacy database copy --

#[test]
fn copy_legacy_noop_when_no_legacy_db() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");

    maybe_copy_legacy_database(&target).unwrap();
    assert!(!target.exists(), "target should not be created when no legacy db");
}

#[test]
fn copy_legacy_noop_when_target_exists() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    std::fs::write(&legacy, b"legacy data").unwrap();
    std::fs::write(&target, b"existing target").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"existing target", "existing target must not be overwritten");
}

#[test]
fn copy_legacy_copies_when_target_missing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    std::fs::write(&legacy, b"legacy database content").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    assert!(target.exists(), "target should be created");
    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"legacy database content", "content should match legacy");

    let legacy_content = std::fs::read(&legacy).unwrap();
    assert_eq!(
        legacy_content, b"legacy database content",
        "legacy must not be modified"
    );
}

#[test]
fn copy_legacy_removes_wal_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    std::fs::write(&legacy, b"legacy data").unwrap();
    std::fs::write(target.with_extension("db-wal"), b"wal").unwrap();
    std::fs::write(target.with_extension("db-shm"), b"shm").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    assert!(
        !target.with_extension("db-wal").exists(),
        "WAL sidecar should be removed"
    );
    assert!(
        !target.with_extension("db-shm").exists(),
        "SHM sidecar should be removed"
    );
}

#[test]
fn copy_legacy_overwrites_leftover_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");
    let tmp = target.with_extension("db.tmp");

    std::fs::write(&legacy, b"real data").unwrap();
    std::fs::write(&tmp, b"leftover from crash").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    assert!(target.exists(), "target should be created");
    assert!(!tmp.exists(), "tmp file should be cleaned up via rename");
    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"real data");
}

#[tokio::test]
async fn copy_legacy_then_init_database_works() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    let legacy_db = init_database(&legacy).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('test_user', 'alice', 'hash', 1000, 1000)",
    )
    .execute(legacy_db.pool())
    .await
    .unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(legacy_db.pool())
        .await
        .unwrap();
    legacy_db.close().await;

    maybe_copy_legacy_database(&target).unwrap();

    let db = init_database(&target).await.unwrap();

    let row = sqlx::query("SELECT username FROM users WHERE id = 'test_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("username"), "alice");

    let legacy_db2 = init_database(&legacy).await.unwrap();
    let row2 = sqlx::query("SELECT username FROM users WHERE id = 'test_user'")
        .fetch_one(legacy_db2.pool())
        .await
        .unwrap();
    assert_eq!(row2.get::<String, _>("username"), "alice");

    db.close().await;
    legacy_db2.close().await;
}

// -- Concurrent migrator regression (ELECTRON-1KK) --
//
// Repro for the Sentry secondary symptom: two processes opening the same
// SQLite DB on first start (e.g. Electron auto-update spawning the new
// version while the old one is still finalising shutdown, or
// `aioncore doctor` racing the server) both decide to apply the same
// migration version. sqlx-sqlite's lock()/unlock() are no-ops, so without
// the advisory file lock and retry-on-UNIQUE the slower process used to
// blow up with `UNIQUE constraint failed: _sqlx_migrations.version`.
//
// We use OS threads (not tokio::spawn) so each migrator runs on its own
// runtime — this matches the real "two processes" topology more closely
// than cooperative tasks would, and avoids the `&SqlitePool: Send` lifetime
// gymnastics that block tokio::spawn on this future.
#[test]
fn concurrent_init_database_does_not_panic_on_unique_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move { init_database(&p).await })
        }));
    }

    // Every thread must succeed — none should bubble up the UNIQUE-constraint
    // error from `_sqlx_migrations`.
    let mut errors = Vec::new();
    for h in handles {
        match h.join().expect("thread panicked") {
            Ok(_db) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    assert!(
        errors.is_empty(),
        "all parallel migrators should succeed, got errors: {errors:?}"
    );

    // All migrators converged on the same baseline schema with no duplicate
    // `_sqlx_migrations` rows.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = init_database(&path).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(count.0 >= 1, "at least one migration should be recorded");

        let dup: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM (SELECT version FROM _sqlx_migrations GROUP BY version HAVING COUNT(*) > 1)",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(dup.0, 0, "no duplicate versions should ever exist in _sqlx_migrations");
        db.close().await;
    });

    // Lock file is created next to the DB and is harmless to leave behind.
    let lock = path.with_file_name("aionui-backend.db.migrate.lock");
    assert!(lock.exists(), "advisory lock file should be present after migrate");
}

#[tokio::test]
async fn legacy_roadmap_migration_ranges_are_reconciled_without_losing_topic_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let current_migration_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");

    for (case_name, legacy_start, current_start, current_end, legacy_shift) in [
        ("original-partial", 17_i64, 28_i64, 29_i64, 11_i64),
        ("original-full", 17_i64, 28_i64, 45_i64, 11_i64),
        ("intermediate-full", 26_i64, 28_i64, 45_i64, 2_i64),
    ] {
        let case_dir = dir.path().join(case_name);
        let migration_dir = case_dir.join("legacy-migrations");
        std::fs::create_dir_all(&migration_dir).unwrap();
        for entry in std::fs::read_dir(&current_migration_dir).unwrap() {
            let entry = entry.unwrap();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let version: i64 = file_name[..3].parse().unwrap();
            let legacy_name = if version < legacy_start {
                file_name
            } else if (current_start..=current_end).contains(&version) {
                format!("{:03}_{}", version - legacy_shift, &file_name[4..])
            } else {
                continue;
            };
            let legacy_path = migration_dir.join(legacy_name);
            if legacy_start == 17 && matches!(version, 31 | 32) {
                let mut legacy_contents = std::fs::read(entry.path()).unwrap();
                legacy_contents.push(b'\n');
                std::fs::write(legacy_path, legacy_contents).unwrap();
            } else {
                std::fs::copy(entry.path(), legacy_path).unwrap();
            }
        }

        let path = case_dir.join("legacy-roadmap.db");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path).create_if_missing(true))
            .await
            .unwrap();
        Migrator::new(migration_dir).await.unwrap().run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO telegram_topic_bindings \
             (chat_id, message_thread_id, agent_id, bound_by_user_id, created_at, updated_at) \
             VALUES ('-1003977604085', 3, 'codex-agent', 'admin-user', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let db = init_database(&path).await.unwrap();
        let migrations: Vec<(i64, String)> = sqlx::query_as(
            "SELECT version, description FROM _sqlx_migrations WHERE version IN (17, 18, 26, 27, 28, 29) ORDER BY version",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(
            migrations,
            vec![
                (17, "drop conversation assistant identity snapshot".to_string()),
                (18, "reset builtin assistant enabled".to_string()),
                (26, "clear bridge agent command override".to_string()),
                (27, "provider model settings".to_string()),
                (28, "source channel metadata".to_string()),
                (29, "telegram topic single agent".to_string()),
            ]
        );
        let migration_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(migration_count.0, 46);

        let binding: (String,) = sqlx::query_as(
            "SELECT agent_id FROM telegram_topic_bindings WHERE chat_id = '-1003977604085' AND message_thread_id = 3",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(binding.0, "codex-agent");
        db.close().await;
    }
}
