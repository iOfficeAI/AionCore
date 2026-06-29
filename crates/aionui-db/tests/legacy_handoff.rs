use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use aionui_db::{init_database, init_database_staged, maybe_copy_legacy_database};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Row, SqlitePool};

async fn create_raw_legacy_backend_missing_team_session_mode(path: &Path, user_id: &str) {
    let pool = open_raw_create(path).await;
    let statements = [
        "CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL, username TEXT NOT NULL UNIQUE, email TEXT UNIQUE, password_hash TEXT NOT NULL, avatar_path TEXT, jwt_secret TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, last_login INTEGER)",
        "CREATE TABLE conversations (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, type TEXT NOT NULL, extra TEXT NOT NULL DEFAULT '{}', model TEXT, status TEXT NOT NULL DEFAULT 'pending', source TEXT, channel_chat_id TEXT, pinned INTEGER NOT NULL DEFAULT 0, pinned_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
        "CREATE TABLE messages (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, msg_id TEXT, type TEXT NOT NULL, content TEXT NOT NULL DEFAULT '{}', position TEXT, status TEXT, hidden INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
        "CREATE TABLE assistant_sessions (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, agent_type TEXT NOT NULL, conversation_id TEXT, workspace TEXT, chat_id TEXT, created_at INTEGER NOT NULL, last_activity INTEGER NOT NULL)",
        "CREATE TABLE teams (id TEXT PRIMARY KEY NOT NULL, user_id TEXT NOT NULL DEFAULT 'system_default_user', name TEXT NOT NULL, workspace TEXT NOT NULL DEFAULT '', workspace_mode TEXT NOT NULL DEFAULT 'shared', agents TEXT NOT NULL DEFAULT '[]', lead_agent_id TEXT, agents_version TEXT NOT NULL DEFAULT '1.0.0', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
        "CREATE TABLE mailbox (id TEXT PRIMARY KEY NOT NULL, team_id TEXT NOT NULL, to_agent_id TEXT NOT NULL, from_agent_id TEXT NOT NULL, type TEXT NOT NULL, content TEXT NOT NULL, summary TEXT, files TEXT, read INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
        "CREATE TABLE team_tasks (id TEXT PRIMARY KEY NOT NULL, team_id TEXT NOT NULL, subject TEXT NOT NULL, description TEXT, status TEXT NOT NULL DEFAULT 'pending', owner TEXT, blocked_by TEXT NOT NULL DEFAULT '[]', blocks TEXT NOT NULL DEFAULT '[]', metadata TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
        "CREATE TABLE assistant_plugins (id TEXT PRIMARY KEY NOT NULL, type TEXT NOT NULL, name TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0, config TEXT NOT NULL, status TEXT, last_connected INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
        "CREATE TABLE remote_agents (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, protocol TEXT NOT NULL, url TEXT NOT NULL, auth_type TEXT NOT NULL, auth_token TEXT, allow_insecure INTEGER NOT NULL DEFAULT 0, avatar TEXT, description TEXT, device_id TEXT, device_public_key TEXT, device_private_key TEXT, device_token TEXT, status TEXT NOT NULL DEFAULT 'unknown', last_connected_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
        "CREATE TABLE cron_jobs (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, schedule_kind TEXT NOT NULL, schedule_value TEXT NOT NULL, schedule_tz TEXT, schedule_description TEXT, payload_message TEXT NOT NULL, execution_mode TEXT NOT NULL DEFAULT 'existing', agent_config TEXT, conversation_id TEXT NOT NULL, conversation_title TEXT, agent_type TEXT NOT NULL, created_by TEXT NOT NULL, skill_content TEXT, description TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, next_run_at INTEGER, last_run_at INTEGER, last_status TEXT, last_error TEXT, run_count INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0, max_retries INTEGER NOT NULL DEFAULT 3)",
        "CREATE TABLE acp_session (conversation_id TEXT PRIMARY KEY, agent_backend TEXT NOT NULL, agent_source TEXT NOT NULL, agent_id TEXT NOT NULL, session_id TEXT, session_status TEXT NOT NULL DEFAULT 'idle', session_config TEXT NOT NULL DEFAULT '{}', last_active_at INTEGER, suspended_at INTEGER)",
    ];
    for statement in statements {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) VALUES (?, ?, NULL, '', 1, 1)",
    )
    .bind(user_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, workspace_mode, agents, lead_agent_id, agents_version, created_at, updated_at) VALUES ('team_1', ?, 'Legacy Team', '', 'shared', '[]', NULL, '1.0.0', 1, 1)",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

async fn open_raw_create(path: &Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(false)
        .busy_timeout(Duration::from_millis(5000))
        .journal_mode(SqliteJournalMode::Wal);

    SqlitePool::connect_with(opts).await.unwrap()
}

async fn has_column(pool: &SqlitePool, table: &str, column: &str) -> bool {
    sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn advanced_legacy_db_missing_team_session_mode_still_initializes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    create_raw_legacy_backend_missing_team_session_mode(&path, "system_default_user").await;

    let repaired = init_database_staged(&path).await.unwrap();
    assert!(has_column(repaired.pool(), "teams", "session_mode").await);

    let row = sqlx::query("SELECT name, session_mode FROM teams WHERE id = 'team_1'")
        .fetch_one(repaired.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("name"), "Legacy Team");
    assert!(row.get::<Option<String>, _>("session_mode").is_none());
    repaired.close().await;
}

#[tokio::test]
async fn existing_backend_db_is_repaired_in_place_without_recopied_legacy_source() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    create_raw_legacy_backend_missing_team_session_mode(&legacy, "legacy_only").await;
    create_raw_legacy_backend_missing_team_session_mode(&target, "backend_only").await;

    maybe_copy_legacy_database(&target).unwrap();
    let repaired = init_database_staged(&target).await.unwrap();

    let backend_user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = 'backend_only'")
        .fetch_one(repaired.pool())
        .await
        .unwrap();
    let legacy_user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = 'legacy_only'")
        .fetch_one(repaired.pool())
        .await
        .unwrap();

    assert_eq!(backend_user_count, 1, "existing backend DB must be preserved");
    assert_eq!(
        legacy_user_count, 0,
        "existing backend DB must not be overwritten from aionui.db"
    );
    assert!(has_column(repaired.pool(), "teams", "session_mode").await);
    repaired.close().await;
}

#[tokio::test]
async fn upgraded_backend_db_reinit_is_noop_for_handoff_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database(&path).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('stable', 'stable', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    let reopened = init_database_staged(&path).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = 'stable'")
        .fetch_one(reopened.pool())
        .await
        .unwrap();

    assert_eq!(count, 1);
    assert!(has_column(reopened.pool(), "teams", "session_mode").await);
    reopened.close().await;
}
