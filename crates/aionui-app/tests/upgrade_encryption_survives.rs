//! Regression for ELECTRON-3T0 + proof of the encryption/signing-secret split.
//!
//! Reproduces the field upgrade end-to-end inside one test, no old binary
//! needed: build a database migrated only up to 028 (the last pre-user-scope
//! version) with the CURRENT migration files, store a provider through the
//! real secret-resolution + encryption path, then reopen it through the real
//! `init_database` (which applies 029+, rebuilding `users` and adding the
//! `encryption_secret` column) and assert the stored API key still decrypts.
//!
//! Two properties are locked here:
//!
//! 1. **Upgrade keeps decryption.** A pre-split database has only `jwt_secret`
//!    and a NULL `encryption_secret`. On first boot the encryption root is
//!    SEEDED from the effective JWT secret (the zero-re-encrypt path), so every
//!    credential encrypted under the old scheme still decrypts. Before the
//!    ELECTRON-3T0 fix, connections opened prior to the DDL served a stale
//!    `users` layout after migration 030's table rebuild; startup then saw "no
//!    system user", silently derived a brand-new key, and every stored
//!    credential failed with "Decryption failed".
//!
//! 2. **Signing rotation does not touch the encryption root.** After the seed
//!    is persisted, rotating the JWT *signing* secret (as change-password does)
//!    must leave the persisted encryption secret — and therefore decryption —
//!    unchanged. This is the whole point of the split: the landmine where
//!    change-password silently orphaned every stored credential is defused.
use aionui_app::{AppConfig, AppServices};
use aionui_db::SqliteProviderRepository;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;

const LAST_PRE_USER_SCOPE_MIGRATION: i64 = 28;
const LEGACY_SECRET: &str = "legacy-secret-0123456789";

#[tokio::test]
async fn upgrade_seeds_encryption_secret_and_survives_jwt_rotation() {
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
        // pre-upgrade install has after its first boot). No encryption_secret
        // column exists at 028 — it is added by migration 042 during the upgrade.
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, jwt_secret, created_at, updated_at) \
             VALUES ('system_default_user', 'admin', '', ?, 1, 1)",
        )
        .bind(LEGACY_SECRET)
        .execute(&pool)
        .await
        .unwrap();

        // Store a provider exactly as the OLD install did: encrypt with the
        // key derived from the persisted secret and write the 028-era row
        // shape directly (the current ProviderService writes user-scope
        // columns that do not exist yet at 028).
        let key = aionui_app::derive_encryption_key(LEGACY_SECRET);
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

    // The encryption root must be SEEDED from the pre-upgrade jwt_secret —
    // never a freshly generated replacement.
    assert_eq!(
        services.encryption_secret_raw, LEGACY_SECRET,
        "upgrade must seed the encryption secret from the persisted jwt_secret"
    );

    // And the pre-upgrade credential must still decrypt.
    assert_provider_decrypts(&services).await;

    // The seed must have been PERSISTED to the new column so it survives the
    // next boot regardless of the signing secret.
    let persisted = services
        .user_repo
        .get_system_user()
        .await
        .unwrap()
        .expect("system user present")
        .encryption_secret;
    assert_eq!(
        persisted.as_deref(),
        Some(LEGACY_SECRET),
        "the seeded encryption secret must be persisted, not just held in memory"
    );

    // ── Phase 3: rotate the SIGNING secret; encryption must NOT follow ───
    // This is exactly what POST /api/auth/change-password does. Before the
    // split it silently rotated the encryption root too, orphaning credentials
    // on the next restart. Now it only touches jwt_secret.
    services
        .user_repo
        .update_jwt_secret("system_default_user", "rotated-signing-secret-xyz")
        .await
        .unwrap();
    services.database.close().await;

    // Reboot on the rotated database: the encryption root is read back from the
    // persisted column, unaffected by the rotated signing secret.
    let db2 = aionui_db::init_database(&db_path).await.unwrap();
    let services2 = AppServices::from_config(db2, &AppConfig::default())
        .await
        .expect("startup must not fail after a signing-secret rotation");
    assert_eq!(
        services2.encryption_secret_raw, LEGACY_SECRET,
        "rotating the signing secret must NOT change the persisted encryption root"
    );
    assert_provider_decrypts(&services2).await;

    services2.database.close().await;
}

/// Decrypt the seeded provider through the real service and assert the plaintext
/// key survives — the observable proof the encryption key is intact.
async fn assert_provider_decrypts(services: &AppServices) {
    let key = aionui_app::derive_encryption_key(&services.encryption_secret_raw);
    let repo = Arc::new(SqliteProviderRepository::new(services.database.pool().clone()));
    let svc = aionui_system::ProviderService::new(repo, key);
    let list = svc
        .list("system_default_user")
        .await
        .expect("provider list must not fail");
    let p = list
        .iter()
        .find(|p| p.id == "upgrade-prov-1")
        .expect("provider present");
    assert_eq!(p.api_key, "sk-upgrade-SECRET", "decryption must survive");
}
