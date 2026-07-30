//! Regression for ELECTRON-3T0: upgrading a pre-user-scope database must not
//! rotate the encryption key.
//!
//! Reproduces the field failure end-to-end inside one test, no old binary
//! needed: build a database migrated only up to 028 (the last pre-user-scope
//! version) with the CURRENT migration files, store a provider through the
//! real secret-resolution + encryption path, then reopen it through the real
//! `init_database` (which applies 029+, rebuilding `users`) and assert the
//! stored API key still decrypts.
//!
//! Before the fix, connections opened prior to the DDL served a stale `users`
//! layout after migration 030's table rebuild; startup then saw "no system
//! user", silently derived a brand-new key, and every stored credential
//! failed with "Decryption failed: invalid key or corrupted data".
use aionui_app::{AppConfig, AppServices};
use aionui_db::SqliteProviderRepository;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;

const LAST_PRE_USER_SCOPE_MIGRATION: i64 = 28;

#[tokio::test]
async fn upgrade_from_pre_user_scope_keeps_encryption_key() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("upgrade.db");

    // ── Phase 1: a genuine pre-user-scope (≤028) database ────────────────
    {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .unwrap()
            .create_if_missing(true);
        // Single connection: mirrors run_migrations_staged's PRAGMA setup for
        // the legacy table rebuilds inside the early migrations.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
            .execute(&pool)
            .await
            .unwrap();

        let mut migrator: Migrator = sqlx::migrate!("../aionui-db/migrations");
        migrator.migrations = migrator
            .migrations
            .iter()
            .filter(|m| m.version <= LAST_PRE_USER_SCOPE_MIGRATION)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();

        // Old-shape system user with a persisted jwt_secret (what a real
        // pre-upgrade install has after its first boot).
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, jwt_secret, created_at, updated_at) \
             VALUES ('system_default_user', 'admin', '', 'legacy-secret-0123456789', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Store a provider exactly as the OLD install did: encrypt with the
        // key derived from the persisted secret and write the 028-era row
        // shape directly (the current ProviderService writes user-scope
        // columns that do not exist yet at 028).
        let key = aionui_app::derive_encryption_key("legacy-secret-0123456789");
        let enc = aionui_common::encrypt_string("sk-upgrade-SECRET", &key).unwrap();
        sqlx::query(
            "INSERT INTO providers                 (id, platform, name, base_url, api_key_encrypted, models, enabled, capabilities, created_at, updated_at)              VALUES ('upgrade-prov-1', 'custom', 'Upgrade', 'http://localhost:1', ?, '[\"m\"]', 1, '[]', 1, 1)",
        )
        .bind(&enc)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    // ── Phase 2: the upgrade — real init_database applies 029+ ───────────
    let db = aionui_db::init_database(&db_path).await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default())
        .await
        .expect("startup must not fail on an upgraded database");

    // The secret must be the one persisted before the upgrade — never a
    // freshly generated replacement.
    assert_eq!(
        services.jwt_secret_raw, "legacy-secret-0123456789",
        "upgrade must keep reading the persisted jwt_secret"
    );

    // And the pre-upgrade credential must still decrypt.
    let key = aionui_app::derive_encryption_key(&services.jwt_secret_raw);
    let repo = Arc::new(SqliteProviderRepository::new(services.database.pool().clone()));
    let svc = aionui_system::ProviderService::new(repo, key);
    let list = svc
        .list("system_default_user")
        .await
        .expect("provider list must not fail after upgrade");
    let p = list
        .iter()
        .find(|p| p.id == "upgrade-prov-1")
        .expect("provider present");
    assert_eq!(p.api_key, "sk-upgrade-SECRET", "decryption must survive the upgrade");

    services.database.close().await;
}
