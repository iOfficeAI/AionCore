use std::borrow::Cow;
use std::path::Path;

use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

async fn run_migrations_through(pool: &sqlx::SqlitePool, max_version: i64) {
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

async fn memory_pool() -> sqlx::SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

async fn codex_yolo_id(pool: &sqlx::SqlitePool) -> String {
    let row = sqlx::query(
        "SELECT yolo_id
         FROM agent_metadata
         WHERE agent_source = 'builtin'
           AND agent_type = 'acp'
           AND backend = 'codex'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    row.get("yolo_id")
}

#[tokio::test]
async fn new_install_seeds_codex_yolo_id_as_agent_full_access() {
    let pool = memory_pool().await;

    run_migrations_through(&pool, 21).await;

    assert_eq!(codex_yolo_id(&pool).await, "agent-full-access");
}

#[tokio::test]
async fn migration_021_updates_only_builtin_codex_yolo_id() {
    let pool = memory_pool().await;

    run_migrations_through(&pool, 20).await;
    sqlx::query(
        "UPDATE agent_metadata
         SET yolo_id = 'full-access'
         WHERE agent_source = 'builtin'
           AND agent_type = 'acp'
           AND backend = 'codex'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_metadata (
             id, name, backend, agent_type, agent_source, yolo_id, enabled, sort_order, created_at, updated_at
         ) VALUES (
             'custom-codex', 'Custom Codex', 'codex', 'acp', 'custom', 'full-access', 1, 9999, 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 21).await;

    assert_eq!(codex_yolo_id(&pool).await, "agent-full-access");
    let custom = sqlx::query("SELECT yolo_id FROM agent_metadata WHERE id = 'custom-codex'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(custom.get::<String, _>("yolo_id"), "full-access");
}

#[tokio::test]
async fn migration_021_does_not_backfill_user_mode_fields() {
    let pool = memory_pool().await;

    run_migrations_through(&pool, 20).await;
    sqlx::query(
        "UPDATE agent_metadata
         SET yolo_id = 'full-access'
         WHERE agent_source = 'builtin'
           AND agent_type = 'acp'
           AND backend = 'codex'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at)
         VALUES ('user_1', 'user_1', '', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, created_at, updated_at)
         VALUES (
             'conv_1', 'user_1', 'Codex conversation', 'acp',
             '{\"session_mode\":\"full-access\",\"current_mode_id\":\"full-access\"}', 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, session_mode, created_at, updated_at)
         VALUES ('team_1', 'user_1', 'Codex Team', '/tmp/workspace', 'full-access', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO cron_jobs (
             id, name, schedule_kind, schedule_value, payload_message, agent_config,
             conversation_id, created_by, created_at, updated_at
         ) VALUES (
             'cron_1', 'Codex Cron', 'cron', '* * * * *', 'run',
             '{\"mode\":\"full-access\"}', 'conv_1', 'user', 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO acp_session (
             conversation_id, agent_source, agent_id, session_id, session_status, session_config
         ) VALUES (
             'conv_1', 'builtin', '8e1acf31', 'session_1', 'suspended',
             '{\"runtime\":{\"current_mode_id\":\"full-access\"}}'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 21).await;

    let cron = sqlx::query("SELECT agent_config FROM cron_jobs WHERE id = 'cron_1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cron.get::<String, _>("agent_config"), r#"{"mode":"full-access"}"#);
    let conversation = sqlx::query("SELECT extra FROM conversations WHERE id = 'conv_1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        conversation.get::<String, _>("extra"),
        r#"{"session_mode":"full-access","current_mode_id":"full-access"}"#
    );
    let team = sqlx::query("SELECT session_mode FROM teams WHERE id = 'team_1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(team.get::<String, _>("session_mode"), "full-access");
    let session = sqlx::query("SELECT session_config FROM acp_session WHERE conversation_id = 'conv_1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        session.get::<String, _>("session_config"),
        r#"{"runtime":{"current_mode_id":"full-access"}}"#
    );
    assert_eq!(codex_yolo_id(&pool).await, "agent-full-access");
}
