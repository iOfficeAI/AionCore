//! Migration 030: assistant_plugins → channel_connections (connection entity)
//! and assistant_sessions → channel_conversation_bindings.
//!
//! Verifies the segment-1 backfill: legacy platform-type ids become
//! `plugin_key`, connection ids are freshly generated, config/state columns
//! are preserved, and the phase-1 single-instance index holds. Segment 3 is
//! covered below: bindings inherit owner/connection from their channel user,
//! `chat_id` survives as `external_chat_id`, agent config columns are gone,
//! and cross-account conversation bindings are unrepresentable.

use std::borrow::Cow;
use std::path::Path;

use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

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

#[tokio::test]
async fn migration_030_rebuilds_plugins_as_connections() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 30).await;

    // Legacy rows: id IS the platform type (one per owner+platform).
    sqlx::query(
        "INSERT INTO assistant_plugins (
            id, owner_user_id, type, name, enabled, config, status,
            last_connected, created_at, updated_at
         ) VALUES
            ('weixin', 'system_default_user', 'weixin', 'WeChat', 1, 'enc-config', 'running', 111, 1, 2),
            ('telegram', 'system_default_user', 'telegram', 'TG Bot', 0, 'enc-tg', NULL, NULL, 3, 4)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 31).await;

    // Old table is gone; new table holds one connection per legacy row.
    let old_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'assistant_plugins'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old_table, 0);

    let rows = sqlx::query(
        "SELECT id, owner_user_id, plugin_key, name, enabled, config, status, last_connected \
         FROM channel_connections ORDER BY created_at ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);

    let weixin = &rows[0];
    assert_eq!(weixin.get::<String, _>("plugin_key"), "weixin");
    // The connection id is generated and no longer the platform type.
    let conn_id = weixin.get::<String, _>("id");
    assert!(conn_id.starts_with("conn_"), "unexpected id: {conn_id}");
    assert_ne!(conn_id, "weixin");
    assert_eq!(weixin.get::<String, _>("owner_user_id"), "system_default_user");
    assert_eq!(weixin.get::<String, _>("name"), "WeChat");
    assert!(weixin.get::<bool, _>("enabled"));
    assert_eq!(weixin.get::<String, _>("config"), "enc-config");
    assert_eq!(weixin.get::<Option<String>, _>("status").as_deref(), Some("running"));
    assert_eq!(weixin.get::<Option<i64>, _>("last_connected"), Some(111));

    let telegram = &rows[1];
    assert_eq!(telegram.get::<String, _>("plugin_key"), "telegram");
    assert_ne!(telegram.get::<String, _>("id"), conn_id);

    // Phase-1 single-instance guard is present.
    let dup = sqlx::query(
        "INSERT INTO channel_connections (
            id, owner_user_id, plugin_key, name, enabled, config, created_at, updated_at
         ) VALUES ('conn_dup', 'system_default_user', 'weixin', 'Dup', 0, '', 5, 5)",
    )
    .execute(&pool)
    .await;
    let err = dup.unwrap_err().to_string();
    assert!(err.contains("UNIQUE"), "unexpected error: {err}");
}

/// Segment 3: `assistant_sessions` becomes `channel_conversation_bindings`.
#[tokio::test]
async fn migration_030_rebuilds_sessions_as_conversation_bindings() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 30).await;

    for uid in ["system_default_user", "other_core_user"] {
        sqlx::query(
            "INSERT OR IGNORE INTO users \
                (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(uid)
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) VALUES \
            ('conv-own', 'system_default_user', 'c', 'gemini', '{}', 'pending', 1, 1), \
            ('conv-other', 'other_core_user', 'c', 'gemini', '{}', 'pending', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Legacy pre-030 rows: plugin id IS the platform type, sessions carry
    // agent_type/workspace and hang off the user by a bare FK.
    sqlx::query(
        "INSERT INTO assistant_plugins (
            id, owner_user_id, type, name, enabled, config, status,
            last_connected, created_at, updated_at
         ) VALUES ('telegram', 'system_default_user', 'telegram', 'TG Bot', 1, 'enc-tg', NULL, NULL, 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO assistant_users (
            id, owner_user_id, platform_user_id, platform_type, display_name,
            authorized_at, last_active, session_id
         ) VALUES ('usr-1', 'system_default_user', 'tg_1', 'telegram', 'Alice', 10, 11, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO assistant_sessions (
            id, user_id, agent_type, conversation_id, workspace, chat_id,
            created_at, last_activity
         ) VALUES ('sess-1', 'usr-1', 'gemini', 'conv-own', '/tmp/ws', 'chat-abc', 20, 21)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 31).await;

    // The legacy table is gone, replaced by the binding table.
    let old_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'assistant_sessions'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old_table, 0);

    let connection_id: String = sqlx::query_scalar("SELECT id FROM channel_connections WHERE plugin_key = 'telegram'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT id, owner_user_id, connection_id, channel_user_id, external_chat_id, \
                conversation_id, created_at, last_active_at \
         FROM channel_conversation_bindings",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(row.len(), 1);
    let binding = &row[0];
    assert_eq!(binding.get::<String, _>("id"), "sess-1");
    // Owner and connection are derived through the channel user, not stored
    // on the legacy session row.
    assert_eq!(binding.get::<String, _>("owner_user_id"), "system_default_user");
    assert_eq!(binding.get::<String, _>("connection_id"), connection_id);
    assert_eq!(binding.get::<String, _>("channel_user_id"), "usr-1");
    // Renamed columns keep their legacy values.
    assert_eq!(
        binding.get::<Option<String>, _>("external_chat_id").as_deref(),
        Some("chat-abc")
    );
    assert_eq!(
        binding.get::<Option<String>, _>("conversation_id").as_deref(),
        Some("conv-own")
    );
    assert_eq!(binding.get::<i64, _>("created_at"), 20);
    assert_eq!(binding.get::<i64, _>("last_active_at"), 21);

    // agent_type / workspace are gone from the schema, not merely unused.
    let columns: Vec<String> = sqlx::query("PRAGMA table_info(channel_conversation_bindings)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(
        !columns.iter().any(|c| c == "agent_type" || c == "workspace"),
        "agent config columns must not survive the rebuild: {columns:?}"
    );

    // Cross-account guard: binding another Core user's conversation aborts.
    let err = sqlx::query(
        "INSERT INTO channel_conversation_bindings (
            id, owner_user_id, connection_id, channel_user_id, external_chat_id,
            conversation_id, created_at, last_active_at
         ) VALUES ('sess-x', 'system_default_user', ?, 'usr-1', 'chat-x', 'conv-other', 30, 30)",
    )
    .bind(&connection_id)
    .execute(&pool)
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("CROSS_ACCOUNT_REFERENCE"),
        "expected cross-account trigger abort, got: {err}"
    );

    // Same guard on UPDATE.
    let update_err =
        sqlx::query("UPDATE channel_conversation_bindings SET conversation_id = 'conv-other' WHERE id = 'sess-1'")
            .execute(&pool)
            .await
            .unwrap_err()
            .to_string();
    assert!(
        update_err.contains("CROSS_ACCOUNT_REFERENCE"),
        "expected cross-account trigger abort on update, got: {update_err}"
    );
}
