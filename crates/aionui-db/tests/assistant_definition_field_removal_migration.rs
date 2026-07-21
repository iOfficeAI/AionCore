use std::borrow::Cow;
use std::path::Path;

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
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_024_removes_unused_fields_and_preserves_definition() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 23).await;

    sqlx::query(
        "INSERT INTO assistant_definitions (
            id, assistant_id, source, owner_type, source_ref, source_version, source_hash,
            name, name_i18n, description_i18n, avatar_type, agent_id,
            rule_resource_type, rule_inline_content,
            recommended_prompts, recommended_prompts_i18n,
            default_model_mode, default_permission_mode,
            default_skills_mode, default_skill_ids, custom_skill_names,
            default_disabled_builtin_skill_ids, default_mcps_mode, default_mcp_ids,
            created_at, updated_at
        ) VALUES (
            'definition-1', 'custom-1', 'user', 'user', 'custom-1', '1.0.0', 'hash',
            'Custom', '{}', '{}', 'none', 'aionrs',
            'inline', '# stale inline rule',
            '[]', '{}',
            'auto', 'auto',
            'fixed', '[]', '[]',
            '[]', 'auto', '[]',
            1, 1
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO assistant_definitions (
            id, assistant_id, source, owner_type, source_ref,
            name, name_i18n, description_i18n, avatar_type, agent_id,
            rule_resource_type, rule_resource_ref, rule_inline_content,
            recommended_prompts, recommended_prompts_i18n,
            default_model_mode, default_permission_mode,
            default_skills_mode, default_skill_ids, custom_skill_names,
            default_disabled_builtin_skill_ids, default_mcps_mode, default_mcp_ids,
            created_at, updated_at
        ) VALUES (
            'definition-2', 'builtin-1', 'builtin', 'system', 'builtin-1',
            'Builtin', '{}', '{}', 'none', 'aionrs',
            'inline', 'stale-ref', '# stale inline rule',
            '[]', '{}',
            'auto', 'auto',
            'fixed', '[]', '[]',
            '[]', 'auto', '[]',
            1, 1
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migrations_through(&pool, 24).await;

    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('assistant_definitions')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(!columns.iter().any(|column| column == "rule_inline_content"));
    assert!(!columns.iter().any(|column| column == "source_version"));
    assert!(!columns.iter().any(|column| column == "source_hash"));

    let row: (String, String, Option<String>) = sqlx::query_as(
        "SELECT name, rule_resource_type, rule_resource_ref
         FROM assistant_definitions
         WHERE id = 'definition-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Custom");
    assert_eq!(row.1, "user_file");
    assert_eq!(row.2.as_deref(), Some("custom-1"));

    let builtin: (String, Option<String>) = sqlx::query_as(
        "SELECT rule_resource_type, rule_resource_ref
         FROM assistant_definitions
         WHERE id = 'definition-2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(builtin.0, "none");
    assert_eq!(builtin.1, None);
}
