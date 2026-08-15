//! Downgrade detection: opening a database written by a NEWER app version.
//!
//! When `_sqlx_migrations` contains a version this binary's embedded migrator
//! does not know, sqlx fails with `MigrateError::VersionMissing`. Startup must
//! surface the dedicated `database.newer_than_app` stage (Sentry ELECTRON-31Z)
//! instead of the generic `database.migration` one, and must NOT treat the
//! intact-but-newer database as corruption to back up and rebuild.

use aionui_db::{
    DATABASE_NEWER_THAN_APP_STAGE, DatabaseInitOptions, DbError, init_database_staged,
    init_database_staged_with_options, latest_known_migration_version,
};

/// A migration version far above anything this binary will ever ship.
const FUTURE_MIGRATION_VERSION: i64 = 999_999;

async fn seed_future_migration_row(path: &std::path::Path) {
    let db = init_database_staged(path).await.unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time)
         VALUES (?, 'from a newer app version', CURRENT_TIMESTAMP, TRUE, x'00', 0)",
    )
    .bind(FUTURE_MIGRATION_VERSION)
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;
}

#[tokio::test]
async fn newer_database_fails_with_dedicated_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    seed_future_migration_row(&path).await;

    let err = init_database_staged(&path).await.expect_err("downgrade must fail");
    assert_eq!(err.stage(), DATABASE_NEWER_THAN_APP_STAGE);

    let source = err.into_source();
    assert_eq!(source.missing_migration_version(), Some(FUTURE_MIGRATION_VERSION));
    assert!(
        matches!(
            &source,
            DbError::Migration(sqlx::migrate::MigrateError::VersionMissing(v)) if *v == FUTURE_MIGRATION_VERSION
        ),
        "unexpected source error: {source}"
    );
}

#[tokio::test]
async fn newer_database_is_not_recovered_as_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    seed_future_migration_row(&path).await;

    // Even with the rebuild flag authorized, an intact newer database must not
    // be backed up and replaced — that would destroy the user's data when the
    // actual fix is upgrading the app.
    let err = init_database_staged_with_options(
        &path,
        DatabaseInitOptions {
            recover_corrupted_database: true,
        },
    )
    .await
    .expect_err("downgrade must fail even with recovery authorized");
    assert_eq!(err.stage(), DATABASE_NEWER_THAN_APP_STAGE);
    assert!(path.exists(), "database file must be left in place");

    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".backup."))
        .collect();
    assert!(backups.is_empty(), "no backup/rebuild for a newer database");
}

#[tokio::test]
async fn genuine_migration_failures_keep_generic_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    // Tamper with an applied migration's checksum: sqlx reports
    // VersionMismatch, which is not a downgrade and must keep the generic
    // migration stage.
    let db = init_database_staged(&path).await.unwrap();
    sqlx::query(
        "UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    let err = init_database_staged(&path)
        .await
        .expect_err("checksum mismatch must fail");
    assert_eq!(err.stage(), "database.migration");
    assert_eq!(err.into_source().missing_migration_version(), None);
}

#[test]
fn latest_known_migration_version_is_present_and_sane() {
    let version = latest_known_migration_version().expect("embedded migrations must not be empty");
    // 037 is the newest migration at the time this test was written; the
    // constant only moves forward.
    assert!(version >= 37, "unexpectedly low migration version: {version}");
    assert!(version < FUTURE_MIGRATION_VERSION);
}
