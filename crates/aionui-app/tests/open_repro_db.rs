//! Manual gold-standard probe for ELECTRON-3T0 (env-gated; skipped in CI).
//! Open a REAL v0.1.52-created database (REPRO_DB) through the production
//! startup path and assert the pre-upgrade credential still decrypts.
use aionui_app::{AppConfig, AppServices};
use aionui_db::SqliteProviderRepository;
use std::sync::Arc;

#[tokio::test]
async fn open_old_db_and_decrypt_provider() {
    let Ok(path) = std::env::var("REPRO_DB") else {
        eprintln!("REPRO_DB not set; skipping");
        return;
    };
    let db = aionui_db::init_database(std::path::Path::new(&path)).await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let key = aionui_app::derive_encryption_key(&services.jwt_secret_raw);
    eprintln!("[cur] secret prefix: {}", &services.jwt_secret_raw[..12]);
    let repo = Arc::new(SqliteProviderRepository::new(services.database.pool().clone()));
    let svc = aionui_system::ProviderService::new(repo, key);
    let list = svc.list("system_default_user").await.expect("list must not fail");
    let p = list.iter().find(|p| p.id == "repro-prov-1").expect("provider present");
    assert_eq!(
        p.api_key, "sk-repro-XYZ-123",
        "REPRO: pre-upgrade credential must decrypt (empty means key rotated)"
    );
    eprintln!("[cur] DECRYPTION OK");
    services.database.close().await;
}
