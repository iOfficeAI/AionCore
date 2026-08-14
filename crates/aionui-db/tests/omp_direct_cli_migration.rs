//! omp launches its local CLI directly rather than through the npx bridge.
//!
//! omp is a non-Registry builtin, so there is no Registry-declared npx
//! distribution to conform to: `@oh-my-pi/pi-coding-agent` ships bin `omp`,
//! and `omp acp` is the vendor's own ACP entrypoint. The row already gated
//! availability on a local `omp` through `binary_name`, so bridging the spawn
//! through npx re-downloaded a CLI the user was required to have installed
//! before the row was even offered.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn omp_spawns_its_local_cli_instead_of_bridging_through_npx() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend("omp")
        .await
        .unwrap()
        .expect("omp is seeded");

    assert_eq!(row.command.as_deref(), Some("omp"), "omp command");
    assert_eq!(row.args.as_deref(), Some(r#"["acp"]"#), "omp args");

    let source: serde_json::Value =
        serde_json::from_str(row.agent_source_info.as_deref().expect("omp agent_source_info")).unwrap();
    assert_eq!(source["binary_name"], "omp", "omp binary_name");
    assert!(
        source.get("bridge_binary").is_none(),
        "a direct-CLI row must not declare a bridge: {source}"
    );
}

/// The re-seed must not reset what a live handshake taught this install. A
/// migration that lists `agent_capabilities` / `auth_methods` in its
/// `ON CONFLICT DO UPDATE` set is dead code on a fresh row and silent data
/// loss on an existing one, so the columns are asserted here rather than
/// trusted to review.
#[tokio::test]
async fn omp_keeps_its_probed_handshake_columns_and_skills_dirs() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend("omp")
        .await
        .unwrap()
        .expect("omp is seeded");

    assert_eq!(
        row.native_skills_dirs.as_deref(),
        Some(r#"[".omp/skills",".claude/skills"]"#),
        "omp skills dirs"
    );
    assert!(
        row.agent_capabilities.is_some(),
        "omp keeps the agent_capabilities its probe seeded"
    );
    assert!(
        row.auth_methods.is_some(),
        "omp keeps the auth_methods its probe seeded"
    );
    assert_eq!(row.yolo_id.as_deref(), None, "omp advertises no yolo mode");
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
