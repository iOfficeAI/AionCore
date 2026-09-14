//! Migration 044 seeds MiniMax Code as a Registry-listed builtin ACP agent.
//!
//! The assertions pin the fields that silently change behaviour if wrong: the
//! launch argv (the npx bridge pins its exact version from the release lock at
//! spawn time), the product CLI that availability detection looks for, and the
//! two handshake columns that a re-seed must never clobber.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

const BACKEND: &str = "minimax-code";

#[tokio::test]
async fn seeds_minimax_code_as_a_bridged_registry_builtin() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend(BACKEND)
        .await
        .unwrap()
        .expect("minimax-code is seeded by migration 044");

    assert_eq!(row.id, "ec619063");
    assert_eq!(row.user_id, None, "builtin rows are machine-level, user_id stays NULL");
    assert_eq!(row.name, "MiniMax Code");
    assert_eq!(row.agent_type, "acp");
    assert_eq!(row.agent_source, "builtin");
    assert!(row.enabled);
    assert_eq!(
        row.icon.as_deref(),
        Some("/api/assets/logos/acp-registry/minimax-code.svg"),
        "icon is keyed by the local backend, not the Registry lookup id"
    );

    // The Registry npx distribution is the ACP connection; the release lock
    // pins its exact version at launch, so the stored argv carries none.
    assert_eq!(row.command.as_deref(), Some("npx"));
    assert_eq!(row.args.as_deref(), Some(r#"["-y","@minimax-ai/code","acp"]"#));
    assert_eq!(row.env.as_deref(), Some("[]"));

    // PATH detection looks for the product CLI the vendor installs (`mcode`),
    // while the bridge that speaks ACP is npx.
    let source: serde_json::Value =
        serde_json::from_str(row.agent_source_info.as_deref().expect("agent_source_info")).unwrap();
    assert_eq!(source["binary_name"], "mcode");
    assert_eq!(source["bridge_binary"], "npx");
    assert!(
        source.get("registry_json_id").is_none() && source.get("package_name").is_none(),
        "agent_source_info must not carry Registry identity fields: {source}"
    );
}

/// `initialize` was probed at integration time, so `agent_capabilities` is
/// seeded; `initialize` advertised no auth methods, so `auth_methods` stays NULL
/// rather than carrying a synthesized blob. Neither column may be listed in the
/// migration's `ON CONFLICT DO UPDATE` set — a re-seed must not reset what a
/// live handshake later teaches this install.
#[tokio::test]
async fn seeds_probed_capabilities_and_leaves_unprobed_fields_null() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend(BACKEND)
        .await
        .unwrap()
        .expect("minimax-code is seeded");

    let caps: serde_json::Value =
        serde_json::from_str(row.agent_capabilities.as_deref().expect("agent_capabilities seeded")).unwrap();
    assert_eq!(caps["load_session"], true);
    assert_eq!(
        caps["mcp_capabilities"]["http"], true,
        "Team must route it to the MCP transport"
    );
    assert_eq!(caps["mcp_capabilities"]["sse"], true);
    assert_eq!(caps["prompt_capabilities"]["image"], false);
    assert!(caps["session_capabilities"].get("resume").is_some());
    assert!(
        row.agent_capabilities.as_deref().unwrap().contains("load_session")
            && !row.agent_capabilities.as_deref().unwrap().contains("loadSession"),
        "handshake columns are stored snake_case (migration 003 contract)"
    );

    assert_eq!(
        row.auth_methods, None,
        "initialize advertised no auth methods; nothing is synthesized"
    );
    assert_eq!(
        row.yolo_id, None,
        "its ACP session modes are default/plan only; bypassPermissions is a config option"
    );
    assert_eq!(
        row.native_skills_dirs, None,
        "no project-relative skills directory is documented or in source"
    );

    let policy: serde_json::Value =
        serde_json::from_str(row.behavior_policy.as_deref().expect("behavior_policy")).unwrap();
    assert_eq!(policy["supports_side_question"], false);
    assert!(
        policy.get("supports_team").is_none(),
        "no negative team flag (retired by 033)"
    );
    assert!(
        policy.get("team_capable_override").is_none(),
        "team_capable_override was retired by 033"
    );
}

/// The lock manifest is the only place the Registry version lives. The entry
/// must exist under the local backend key, name the stable package, and pin an
/// exact semver — a tag such as `latest` would defeat the release lock.
#[test]
fn minimax_code_is_pinned_in_the_npx_release_lock() {
    let lock = include_str!("../../aionui-runtime/resources/acp-registry-npx-lock.json");
    let parsed: serde_json::Value = serde_json::from_str(lock).unwrap();

    let entry = parsed["agents"]
        .get(BACKEND)
        .unwrap_or_else(|| panic!("{BACKEND} must be pinned in the npx release lock"));
    assert_eq!(entry["package"], "@minimax-ai/code");
    assert_eq!(
        entry["registry_json_id"], "minimax-code",
        "lookup alias lives in the lock, not in metadata"
    );

    let version = entry["version"].as_str().expect("version is a string");
    let parts: Vec<&str> = version.split('.').collect();
    assert!(
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "version must be an exact semver, got {version:?}"
    );
}

/// Bad path: the product CLI name and the npm scope are not backends. A lookup
/// by either must miss, so nothing can accidentally seed a second row under an
/// alias and split the agent's identity.
#[tokio::test]
async fn aliases_are_not_registered_as_backends() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    for alias in ["mcode", "minimax", "minimax-ai", "@minimax-ai/code"] {
        assert!(
            repo.find_builtin_by_backend(alias).await.unwrap().is_none(),
            "{alias} must not resolve to a builtin row"
        );
    }
}
