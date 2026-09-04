//! omp launched its local CLI directly rather than through the npx bridge.
//! Migration 045 removed it from the builtin catalog; Hub can still install it.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn omp_is_no_longer_a_builtin_agent() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    assert!(
        repo.find_builtin_by_backend("omp").await.unwrap().is_none(),
        "omp must not remain a builtin after 045"
    );
}

/// The lock manifest pins npx packages. A direct-CLI row has no package to
/// pin, so leaving the entry behind would keep asserting a version nothing
/// launches.
#[test]
fn omp_is_no_longer_pinned_in_the_npx_release_lock() {
    let lock = include_str!("../../aionui-runtime/resources/acp-registry-npx-lock.json");
    let parsed: serde_json::Value = serde_json::from_str(lock).unwrap();

    assert!(
        parsed["agents"].get("omp").is_none(),
        "omp must not remain in the npx release lock once it launches directly"
    );
}
