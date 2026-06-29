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

async fn seed_legacy_assistant_identity(pool: &sqlx::SqlitePool) {
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at)
         VALUES ('user_1', 'user_1', '', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();

    for (id, backend, agent_type, name, source, sort_order) in [
        ("agent-aionrs", "", "aionrs", "Aion CLI", "internal", 100),
        ("agent-codex", "codex", "acp", "Codex CLI", "builtin", 200),
        ("agent-claude", "claude", "acp", "Claude Code", "builtin", 210),
    ] {
        sqlx::query(
            "INSERT INTO agent_metadata (
                id, name, backend, command, agent_type, enabled, agent_source, sort_order, created_at, updated_at
             ) VALUES (?, ?, NULLIF(?, ''), '', ?, 1, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(backend)
        .bind(agent_type)
        .bind(source)
        .bind(sort_order)
        .execute(pool)
        .await
        .unwrap();
    }

    for (definition_id, assistant_key, agent_backend, source_ref) in [
        ("def-aionrs", "aionui-assistant", "aionrs", "aionui-assistant"),
        ("def-codex", "bare:agent-codex", "codex", "agent-codex"),
        ("def-claude", "bare:agent-claude", "claude", "agent-claude"),
    ] {
        sqlx::query(
            "INSERT INTO assistant_definitions (
                definition_id, assistant_key, source, owner_type, source_ref,
                name, name_i18n, description_i18n, avatar_type, agent_backend,
                rule_resource_type, recommended_prompts, recommended_prompts_i18n,
                default_model_mode, default_permission_mode, default_skills_mode, default_skill_ids,
                custom_skill_names, default_disabled_builtin_skill_ids, default_mcps_mode, default_mcp_ids,
                created_at, updated_at
            ) VALUES (?, ?, 'generated', 'system', ?, ?, '{}', '{}', 'none', ?,
                'none', '[]', '{}', 'auto', 'auto', 'auto', '[]', '[]', '[]', 'auto', '[]', 1, 1)",
        )
        .bind(definition_id)
        .bind(assistant_key)
        .bind(source_ref)
        .bind(assistant_key)
        .bind(agent_backend)
        .execute(pool)
        .await
        .unwrap();
    }

    for (conversation_id, name, agent_type, extra) in [
        ("conv_aionrs", "Aion cron", "aionrs", r#"{"workspace":"/tmp/aionrs"}"#),
        (
            "conv_snapshot",
            "Snapshot cron",
            "acp",
            r#"{"workspace":"/tmp/snapshot"}"#,
        ),
        (
            "conv_agent_type",
            "Agent type cron",
            "acp",
            r#"{"workspace":"/tmp/agent-type"}"#,
        ),
        (
            "conv_missing",
            "Missing assistant cron",
            "acp",
            r#"{"workspace":"/tmp/missing"}"#,
        ),
        (
            "conv_invalid",
            "Invalid JSON cron",
            "acp",
            r#"{"workspace":"/tmp/invalid"}"#,
        ),
    ] {
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, created_at, updated_at)
             VALUES (?, 'user_1', ?, ?, ?, 1, 1)",
        )
        .bind(conversation_id)
        .bind(name)
        .bind(agent_type)
        .bind(extra)
        .execute(pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO conversation_assistant_snapshots (
            conversation_id, assistant_definition_id, assistant_key, assistant_source, assistant_name,
            assistant_avatar_type, agent_backend, rules_content,
            default_model_mode, default_permission_mode, default_skills_mode,
            default_mcps_mode, created_at, updated_at
        ) VALUES (
            'conv_snapshot', 'def-codex', 'bare:agent-codex', 'generated', 'Codex',
            'none', 'codex', '', 'auto', 'auto', 'auto', 'auto', 1, 1
        )",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO acp_session (
            conversation_id, agent_backend, agent_source, agent_id, session_id, session_status, session_config
        ) VALUES ('conv_snapshot', 'claude', 'builtin', '', 'session-1', 'suspended', '{}')",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_015_populates_aionrs_catalog_by_agent_type() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    run_migrations_through(&pool, 14).await;
    sqlx::query(
        "INSERT INTO agent_metadata (
            id, name, backend, command, agent_type, enabled, agent_source, sort_order, created_at, updated_at
         ) VALUES ('agent-aionrs', 'Aion CLI', NULL, '', 'aionrs', 1, 'internal', 100, 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 15).await;

    let row = sqlx::query(
        "SELECT available_modes, config_options
         FROM agent_metadata
         WHERE id = 'agent-aionrs'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let available_modes: String = row.get("available_modes");
    let config_options: String = row.get("config_options");
    assert!(available_modes.contains("\"auto_edit\""));
    assert!(config_options.contains("\"yolo\""));
}

#[tokio::test]
async fn migration_016_clears_internal_aion_cli_overrides_only() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    run_migrations_through(&pool, 15).await;
    sqlx::query(
        "UPDATE agent_metadata
         SET command = 'bad-command',
             command_override = 'bad-override',
             env_override = '[{\"name\":\"ANTHROPIC_API_KEY\",\"value\":\"sk-x\"}]'
         WHERE agent_type = 'aionrs'
           AND agent_source = 'internal'",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agent_metadata (
            id, name, backend, command, command_override, env_override, agent_type,
            enabled, agent_source, sort_order, created_at, updated_at
         ) VALUES (
            'external-override-agent', 'External Override', 'external', 'external-cli',
            '/opt/bin/external-cli', '[{\"name\":\"ANTHROPIC_API_KEY\",\"value\":\"sk-y\"}]',
            'acp', 1, 'builtin', 999, 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 16).await;

    let internal_row = sqlx::query(
        "SELECT command, command_override, env_override
         FROM agent_metadata
         WHERE agent_type = 'aionrs'
           AND agent_source = 'internal'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let internal_command: Option<String> = internal_row.get("command");
    let internal_command_override: Option<String> = internal_row.get("command_override");
    let internal_env_override: Option<String> = internal_row.get("env_override");
    assert_eq!(internal_command, None);
    assert_eq!(internal_command_override, None);
    assert_eq!(internal_env_override, None);

    let external_row = sqlx::query(
        "SELECT command, command_override, env_override
         FROM agent_metadata
         WHERE id = 'external-override-agent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let external_command: String = external_row.get("command");
    let external_command_override: String = external_row.get("command_override");
    let external_env_override: String = external_row.get("env_override");
    assert_eq!(external_command, "external-cli");
    assert_eq!(external_command_override, "/opt/bin/external-cli");
    assert_eq!(
        external_env_override,
        r#"[{"name":"ANTHROPIC_API_KEY","value":"sk-y"}]"#
    );
}

async fn insert_legacy_cron(
    pool: &sqlx::SqlitePool,
    id: &str,
    conversation_id: &str,
    agent_type: &str,
    agent_config: &str,
) {
    sqlx::query(
        "INSERT INTO cron_jobs (
            id, name, enabled, schedule_kind, schedule_value, payload_message,
            execution_mode, agent_config, conversation_id, agent_type, created_by,
            created_at, updated_at, run_count, retry_count, max_retries
        ) VALUES (?, ?, 1, 'every', '60000', 'run', 'new_conversation', ?, ?, ?, 'user', 1, 1, 0, 0, 3)",
    )
    .bind(id)
    .bind(id)
    .bind(agent_config)
    .bind(conversation_id)
    .bind(agent_type)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_013_normalizes_legacy_cron_agent_identity() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    run_migrations_through(&pool, 12).await;
    seed_legacy_assistant_identity(&pool).await;

    insert_legacy_cron(
        &pool,
        "cron_aionrs",
        "conv_aionrs",
        "aionrs",
        r#"{"backend":"provider-1","name":"Aion","assistant_id":"aionui-assistant","model_id":"gpt-5"}"#,
    )
    .await;
    insert_legacy_cron(
        &pool,
        "cron_snapshot",
        "conv_snapshot",
        "codex",
        r#"{"backend":"codex","name":"Codex"}"#,
    )
    .await;
    insert_legacy_cron(
        &pool,
        "cron_agent_type",
        "conv_agent_type",
        "claude",
        r#"{"backend":"claude","name":"Claude"}"#,
    )
    .await;
    insert_legacy_cron(
        &pool,
        "cron_missing",
        "conv_missing",
        "ghost",
        r#"{"backend":"ghost","name":"Ghost"}"#,
    )
    .await;
    insert_legacy_cron(&pool, "cron_invalid", "conv_invalid", "codex", r#"{"backend":"codex""#).await;

    run_migration(&pool, 13).await;

    let cron_columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('cron_jobs')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(!cron_columns.iter().any(|column| column == "agent_type"));

    let acp_session_columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('acp_session')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(!acp_session_columns.iter().any(|column| column == "agent_backend"));
    let recovered_session_agent_id: String =
        sqlx::query_scalar("SELECT agent_id FROM acp_session WHERE conversation_id = 'conv_snapshot'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recovered_session_agent_id, "agent-claude");

    let backend_key_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cron_jobs
         WHERE agent_config IS NOT NULL
           AND json_valid(agent_config)
           AND json_type(agent_config, '$.backend') IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(backend_key_count, 0);

    let aionrs = sqlx::query(
        "SELECT
            json_extract(agent_config, '$.assistant_id') AS assistant_id,
            json_extract(agent_config, '$.model.provider_id') AS provider_id,
            json_extract(agent_config, '$.model.model') AS model,
            enabled
         FROM cron_jobs WHERE id = 'cron_aionrs'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(aionrs.get::<String, _>("assistant_id"), "aionui-assistant");
    assert_eq!(aionrs.get::<String, _>("provider_id"), "provider-1");
    assert_eq!(aionrs.get::<String, _>("model"), "gpt-5");
    assert_eq!(aionrs.get::<i64, _>("enabled"), 1);

    let snapshot_assistant_id: String = sqlx::query_scalar(
        "SELECT json_extract(agent_config, '$.assistant_id')
         FROM cron_jobs WHERE id = 'cron_snapshot'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(snapshot_assistant_id, "bare:agent-codex");

    let recovered_assistant_id: String = sqlx::query_scalar(
        "SELECT json_extract(agent_config, '$.assistant_id')
         FROM cron_jobs WHERE id = 'cron_agent_type'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recovered_assistant_id, "bare:agent-claude");

    let disabled_rows = sqlx::query(
        "SELECT id, enabled, last_status, last_error
         FROM cron_jobs
         WHERE id IN ('cron_missing', 'cron_invalid')
         ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(disabled_rows.len(), 2);
    assert_eq!(disabled_rows[0].get::<String, _>("id"), "cron_invalid");
    assert_eq!(disabled_rows[0].get::<i64, _>("enabled"), 0);
    assert_eq!(disabled_rows[0].get::<String, _>("last_status"), "error");
    assert!(
        disabled_rows[0]
            .get::<String, _>("last_error")
            .contains("invalid agent_config JSON")
    );
    assert_eq!(disabled_rows[1].get::<String, _>("id"), "cron_missing");
    assert_eq!(disabled_rows[1].get::<i64, _>("enabled"), 0);
    assert_eq!(disabled_rows[1].get::<String, _>("last_status"), "error");
    assert!(
        disabled_rows[1]
            .get::<String, _>("last_error")
            .contains("assistant_id could not be recovered")
    );

    let index_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'index' AND name = 'idx_cron_jobs_agent_type'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(index_count, 0);
}

#[tokio::test]
async fn migration_013_recovers_cron_assistant_from_session_agent_id_without_existing_definition() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    run_migrations_through(&pool, 12).await;

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at)
         VALUES ('user_1', 'user_1', '', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agent_metadata (
            id, name, backend, command, agent_type, enabled, agent_source, sort_order, created_at, updated_at
         ) VALUES ('agent-claude', 'Claude Code', 'claude', 'claude', 'acp', 1, 'builtin', 200, 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, created_at, updated_at)
         VALUES (
            'conv_session_only', 'user_1', 'Session-only cron', 'acp',
            '{\"agent_id\":\"agent-claude\",\"backend\":\"claude\",\"session_mode\":\"bypassPermissions\"}',
            1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO acp_session (
            conversation_id, agent_backend, agent_source, agent_id, session_id, session_status, session_config
        ) VALUES (
            'conv_session_only', 'claude', 'builtin', 'agent-claude', 'session-1', 'idle',
            '{\"current_mode_id\":\"bypassPermissions\"}'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    insert_legacy_cron(
        &pool,
        "cron_session_only",
        "conv_session_only",
        "claude",
        r#"{"backend":"claude","name":"Claude Code","mode":"bypassPermissions"}"#,
    )
    .await;

    run_migration(&pool, 13).await;

    let cron = sqlx::query(
        "SELECT
            enabled,
            json_extract(agent_config, '$.assistant_id') AS assistant_id,
            json_type(agent_config, '$.backend') AS backend_key_type,
            last_status,
            last_error
         FROM cron_jobs
         WHERE id = 'cron_session_only'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cron.get::<i64, _>("enabled"), 1);
    assert_eq!(cron.get::<String, _>("assistant_id"), "bare:agent-claude");
    assert!(cron.get::<Option<String>, _>("backend_key_type").is_none());
    assert!(cron.get::<Option<String>, _>("last_status").is_none());
    assert!(cron.get::<Option<String>, _>("last_error").is_none());

    let generated = sqlx::query(
        "SELECT source, source_ref, agent_id
         FROM assistant_definitions
         WHERE assistant_id = 'bare:agent-claude'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(generated.get::<String, _>("source"), "generated");
    assert_eq!(generated.get::<String, _>("source_ref"), "agent-claude");
    assert_eq!(generated.get::<String, _>("agent_id"), "agent-claude");
}
