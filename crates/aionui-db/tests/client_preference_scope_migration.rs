//! Migration 030 segment 4: device/account scope for `client_preferences`.
//!
//! Seeds a realistic pre-031 database (two users, each with a machine-level
//! preference, a personal preference and a `system_settings` row), runs the
//! migration, and asserts the promotion/materialization contract from
//! `031_client_preference_scope.sql`.

use std::borrow::Cow;
use std::path::Path;

use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

const USER_A: &str = "system_default_user";
const USER_B: &str = "user-b";

async fn run_migrations_through(pool: &sqlx::SqlitePool, max_version: i64) {
    sqlx::query("PRAGMA foreign_keys = OFF").execute(pool).await.unwrap();
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version <= max_version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator.run(pool).await.unwrap();
}

async fn run_migration(pool: &sqlx::SqlitePool, version: i64) {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version == version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    };
    migrator.run(pool).await.unwrap();
}

async fn insert_pref(pool: &sqlx::SqlitePool, user_id: &str, key: &str, value: &str, updated_at: i64) {
    sqlx::query("INSERT INTO client_preferences (user_id, key, value, updated_at) VALUES (?, ?, ?, ?)")
        .bind(user_id)
        .bind(key)
        .bind(value)
        .bind(updated_at)
        .execute(pool)
        .await
        .unwrap();
}

async fn insert_settings(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    language: &str,
    notification: bool,
    cron_notification: bool,
    command_queue: bool,
    save_upload: bool,
) {
    sqlx::query(
        "INSERT INTO system_settings (
            user_id, language, notification_enabled, cron_notification_enabled,
            command_queue_enabled, save_upload_to_workspace, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, 100)",
    )
    .bind(user_id)
    .bind(language)
    .bind(notification)
    .bind(cron_notification)
    .bind(command_queue)
    .bind(save_upload)
    .execute(pool)
    .await
    .unwrap();
}

/// Pre-031 fixture: two users, machine-level keys copied per user (with
/// different values and update times), personal keys, and settings rows.
async fn seed_pre_031() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 30).await;

    for user_id in [USER_A, USER_B] {
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, '', 'active', 0, 1, 1)",
        )
        .bind(user_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Machine-level keys, duplicated per user. USER_B wrote later, so its
    // value is the machine's most recent truth.
    insert_pref(&pool, USER_A, "system.closeToTray", "true", 100).await;
    insert_pref(&pool, USER_B, "system.closeToTray", "false", 200).await;
    insert_pref(&pool, USER_A, "keepAwake", "true", 300).await;
    insert_pref(&pool, USER_A, "pet.size", "360", 400).await;
    insert_pref(&pool, USER_B, "pet.size", "480", 500).await;
    insert_pref(&pool, USER_B, "autoPreviewOfficeFiles", "true", 600).await;

    // Personal (account-scope) keys.
    insert_pref(&pool, USER_A, "theme", "\"dark\"", 700).await;
    insert_pref(&pool, USER_B, "theme", "\"light\"", 700).await;
    insert_pref(&pool, USER_A, "language", "\"zh-CN\"", 700).await;

    // A preference written after B1 must not be clobbered by the column
    // materialization, even though the column disagrees.
    insert_pref(&pool, USER_A, "system.notificationEnabled", "false", 800).await;

    insert_settings(&pool, USER_A, "zh-CN", true, false, true, false).await;
    insert_settings(&pool, USER_B, "en-US", false, true, false, true).await;

    run_migration(&pool, 31).await;
    pool
}

async fn device_value(pool: &sqlx::SqlitePool, key: &str) -> Option<(Option<String>, String)> {
    let row = sqlx::query("SELECT user_id, value FROM client_preferences WHERE scope = 'device' AND key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap();
    row.map(|row| (row.get::<Option<String>, _>("user_id"), row.get::<String, _>("value")))
}

async fn account_value(pool: &sqlx::SqlitePool, user_id: &str, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM client_preferences WHERE scope = 'account' AND user_id = ? AND key = ?")
        .bind(user_id)
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn row_count(pool: &sqlx::SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn migration_031_promotes_device_keys_to_a_single_machine_row() {
    let pool = seed_pre_031().await;

    // Latest write wins across users; the row is owned by no account.
    assert_eq!(
        device_value(&pool, "system.closeToTray").await,
        Some((None, "false".to_owned())),
        "USER_B's later value must win"
    );
    assert_eq!(device_value(&pool, "pet.size").await, Some((None, "480".to_owned())));
    assert_eq!(device_value(&pool, "keepAwake").await, Some((None, "true".to_owned())));
    assert_eq!(
        device_value(&pool, "autoPreviewOfficeFiles").await,
        Some((None, "true".to_owned()))
    );

    // Exactly one copy of each device key survives.
    for key in ["system.closeToTray", "pet.size", "keepAwake", "autoPreviewOfficeFiles"] {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_preferences WHERE key = ?")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "{key} must exist exactly once after promotion");
    }
}

#[tokio::test]
async fn migration_031_drops_the_per_user_copies_of_device_keys() {
    let pool = seed_pre_031().await;

    for user_id in [USER_A, USER_B] {
        for key in ["system.closeToTray", "pet.size", "keepAwake", "autoPreviewOfficeFiles"] {
            assert_eq!(
                account_value(&pool, user_id, key).await,
                None,
                "{user_id} must not keep an account copy of the device key {key}"
            );
        }
    }

    assert_eq!(
        row_count(
            &pool,
            "SELECT COUNT(*) FROM client_preferences WHERE scope = 'account' AND user_id IS NULL"
        )
        .await,
        0,
        "account rows must always have an owner"
    );
    assert_eq!(
        row_count(
            &pool,
            "SELECT COUNT(*) FROM client_preferences WHERE scope = 'device' AND user_id IS NOT NULL"
        )
        .await,
        0,
        "device rows must never have an owner"
    );
}

#[tokio::test]
async fn migration_031_leaves_personal_keys_untouched() {
    let pool = seed_pre_031().await;

    assert_eq!(account_value(&pool, USER_A, "theme").await, Some("\"dark\"".to_owned()));
    assert_eq!(
        account_value(&pool, USER_B, "theme").await,
        Some("\"light\"".to_owned())
    );
    assert_eq!(
        account_value(&pool, USER_A, "language").await,
        Some("\"zh-CN\"".to_owned())
    );
    assert_eq!(account_value(&pool, USER_B, "language").await, None);

    // The personal key must not have leaked into the device scope.
    assert_eq!(device_value(&pool, "theme").await, None);
}

#[tokio::test]
async fn migration_031_materializes_the_four_switches_per_user() {
    let pool = seed_pre_031().await;

    // USER_A columns: notification=1, cron=0, queue=1, save=0.
    // `system.notificationEnabled` was already stored as `false` and must win.
    assert_eq!(
        account_value(&pool, USER_A, "system.notificationEnabled").await,
        Some("false".to_owned()),
        "an existing preference is the newer truth and must survive the migration"
    );
    assert_eq!(
        account_value(&pool, USER_A, "cron.notificationEnabled").await,
        Some("false".to_owned())
    );
    assert_eq!(
        account_value(&pool, USER_A, "system.commandQueueEnabled").await,
        Some("true".to_owned())
    );
    assert_eq!(
        account_value(&pool, USER_A, "system.saveUploadToWorkspace").await,
        Some("false".to_owned())
    );

    // USER_B columns: notification=0, cron=1, queue=0, save=1.
    assert_eq!(
        account_value(&pool, USER_B, "system.notificationEnabled").await,
        Some("false".to_owned())
    );
    assert_eq!(
        account_value(&pool, USER_B, "cron.notificationEnabled").await,
        Some("true".to_owned())
    );
    assert_eq!(
        account_value(&pool, USER_B, "system.commandQueueEnabled").await,
        Some("false".to_owned())
    );
    assert_eq!(
        account_value(&pool, USER_B, "system.saveUploadToWorkspace").await,
        Some("true".to_owned())
    );

    // The legacy columns are still there (B3 drops them later).
    let notification: bool = sqlx::query_scalar("SELECT notification_enabled FROM system_settings WHERE user_id = ?")
        .bind(USER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(notification, "the legacy column must be left as-is by 031");
}

#[tokio::test]
async fn migration_031_enforces_scope_uniqueness_and_ownership() {
    let pool = seed_pre_031().await;

    // One device row per key.
    let duplicate = sqlx::query("INSERT INTO client_preferences (scope, user_id, key, value, updated_at) VALUES ('device', NULL, 'keepAwake', 'false', 900)")
        .execute(&pool)
        .await;
    assert!(
        duplicate.is_err(),
        "a second device row for the same key must be rejected"
    );

    // One account row per (user, key).
    let duplicate = sqlx::query("INSERT INTO client_preferences (scope, user_id, key, value, updated_at) VALUES ('account', ?, 'theme', '\"blue\"', 900)")
        .bind(USER_A)
        .execute(&pool)
        .await;
    assert!(
        duplicate.is_err(),
        "a second account row for the same (user, key) must be rejected"
    );

    // But the same key for a different user is fine.
    sqlx::query("INSERT INTO client_preferences (scope, user_id, key, value, updated_at) VALUES ('account', ?, 'keepAwake', 'true', 900)")
        .bind(USER_B)
        .execute(&pool)
        .await
        .expect("account scope is independent of the device row");

    // Ownership CHECK: device rows may not carry a user, account rows must.
    let bad_device = sqlx::query("INSERT INTO client_preferences (scope, user_id, key, value, updated_at) VALUES ('device', ?, 'window.mode', 'x', 900)")
        .bind(USER_A)
        .execute(&pool)
        .await;
    assert!(bad_device.is_err(), "device rows must have a NULL user_id");

    let bad_account = sqlx::query("INSERT INTO client_preferences (scope, user_id, key, value, updated_at) VALUES ('account', NULL, 'window.mode', 'x', 900)")
        .execute(&pool)
        .await;
    assert!(bad_account.is_err(), "account rows must have a user_id");
}
