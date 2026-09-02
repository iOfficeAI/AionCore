//! Migration 034 seeds Antigravity; migration 045 removes it from the builtin catalog.
//! Hub can still install it. These assertions pin the post-045 catalog shape.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn antigravity_is_no_longer_a_builtin_after_catalog_trim() {
    let db = init_database_memory().await.expect("in-memory database");
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    assert!(
        repo.find_builtin_by_backend("antigravity").await.unwrap().is_none(),
        "antigravity must not remain a builtin after 045"
    );
}
