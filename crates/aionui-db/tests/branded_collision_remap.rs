//! 0.1.73 reused versions 038/039 for branded files after those numbers had
//! already shipped as upstream migrations. Opening a 0.1.70 database must
//! remap the old rows onto 043/044 instead of aborting with VersionMismatch.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_staged};

async fn migration_description(pool: &sqlx::SqlitePool, version: i64) -> String {
    sqlx::query_scalar("SELECT description FROM _sqlx_migrations WHERE version = ?")
        .bind(version)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn conversation_name_source_applied_as_038_opens_after_073_renumber() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 43")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE _sqlx_migrations
         SET description = 'conversation name source', checksum = x'00'
         WHERE version = 38",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE agent_metadata
         SET agent_capabilities = json_remove(agent_capabilities, '$.session_capabilities')
         WHERE id = '632f31d2'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    let db = init_database_staged(&path)
        .await
        .expect("0.1.70 conversation-name-source at version 38 must remap and open");

    assert_eq!(migration_description(db.pool(), 38).await, "aionrs fork capability");
    assert_eq!(migration_description(db.pool(), 43).await, "conversation name source");

    let has_name_source: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('conversations') WHERE name = 'name_source'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(has_name_source, 1, "name_source must survive the remap");

    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let aionrs = repo.get("632f31d2").await.unwrap().expect("seeded Aion CLI row");
    let capabilities: serde_json::Value =
        serde_json::from_str(aionrs.agent_capabilities.as_deref().expect("constructed capabilities")).unwrap();
    assert_eq!(
        capabilities["session_capabilities"]["fork"],
        serde_json::json!({"at_turn": true}),
        "reused 038 must still apply the aionrs fork capability"
    );
    db.close().await;
}

#[tokio::test]
async fn omp_direct_cli_applied_as_039_opens_after_073_renumber() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 44")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE _sqlx_migrations
         SET description = 'omp direct cli launch', checksum = x'00'
         WHERE version = 39",
    )
    .execute(db.pool())
    .await
    .unwrap();
    // The original 039 SQL already launched omp directly. Remapping only
    // relocates that history row to 044; it must not re-run the UPDATE.
    db.close().await;

    let db = init_database_staged(&path)
        .await
        .expect("duplicate 039 omp-direct-cli must remap and open");

    assert_eq!(
        migration_description(db.pool(), 39).await,
        "brand internal workspace skills path"
    );
    assert_eq!(migration_description(db.pool(), 44).await, "omp direct cli launch");

    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let omp = repo
        .find_builtin_by_backend("omp")
        .await
        .unwrap()
        .expect("omp is seeded");
    assert_eq!(omp.command.as_deref(), Some("omp"));
    assert_eq!(omp.args.as_deref(), Some(r#"["acp"]"#));
    db.close().await;
}

#[tokio::test]
async fn current_073_history_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    let before: Vec<(i64, String, Vec<u8>)> =
        sqlx::query_as("SELECT version, description, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(db.pool())
            .await
            .unwrap();
    db.close().await;

    let db = init_database_staged(&path).await.unwrap();
    let after: Vec<(i64, String, Vec<u8>)> =
        sqlx::query_as("SELECT version, description, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(before, after);
    db.close().await;
}

#[tokio::test]
async fn unrelated_038_checksum_mismatch_still_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 38")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let err = init_database_staged(&path)
        .await
        .expect_err("a real 038 checksum mismatch must not be remapped");
    assert_eq!(err.stage(), "database.migration");
}
