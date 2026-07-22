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
async fn migration_027_adds_model_settings_without_losing_existing_providers() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 25).await;

    sqlx::query(
        "INSERT INTO providers (
            id, platform, name, base_url, api_key_encrypted, created_at, updated_at
         ) VALUES ('provider-1', 'openai', 'OpenAI', 'https://api.openai.com/v1', 'encrypted', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migrations_through(&pool, 27).await;

    let row: (String, String) = sqlx::query_as("SELECT name, model_settings FROM providers WHERE id = 'provider-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "OpenAI");
    assert_eq!(row.1, "{}");
}

#[tokio::test]
async fn migration_027_rejects_invalid_model_settings_json() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 27).await;

    let result = sqlx::query(
        "INSERT INTO providers (
            id, platform, name, base_url, api_key_encrypted, model_settings, created_at, updated_at
         ) VALUES ('provider-1', 'openai', 'OpenAI', 'https://api.openai.com/v1', 'encrypted', 'invalid', 1, 1)",
    )
    .execute(&pool)
    .await;

    assert!(result.is_err());
}
