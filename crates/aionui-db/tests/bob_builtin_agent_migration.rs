//! Migration 045 seeds IBM Bob as a Registry-listed builtin ACP agent.
//!
//! The assertions pin the fields that silently change behaviour if wrong: the
//! launch argv (the npx bridge pins its exact version from the release lock at
//! spawn time, and deliberately carries no `--trust`), the product CLI that
//! availability detection looks for, and the two handshake columns that a
//! re-seed must never clobber.

use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

const BACKEND: &str = "bob";

#[tokio::test]
async fn seeds_bob_as_a_bridged_registry_builtin() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend(BACKEND)
        .await
        .unwrap()
        .expect("bob is seeded by migration 045");

    assert_eq!(row.id, "be4da246");
    assert_eq!(row.user_id, None, "builtin rows are machine-level, user_id stays NULL");
    assert_eq!(row.name, "Bob");
    assert_eq!(
        row.description, None,
        "builtin rows do not persist Registry descriptions"
    );
    assert_eq!(row.agent_type, "acp");
    assert_eq!(row.agent_source, "builtin");
    assert!(row.enabled);
    assert_eq!(
        row.icon.as_deref(),
        Some("/api/assets/logos/acp-registry/bob.svg"),
        "icon is keyed by the local backend, not the Registry lookup id"
    );

    // The Registry npx distribution is the ACP connection; the release lock
    // pins its exact version at launch, so the stored argv carries none.
    assert_eq!(row.command.as_deref(), Some("npx"));
    assert_eq!(row.args.as_deref(), Some(r#"["-y","@ibm/bob","acp"]"#));
    assert_eq!(row.env.as_deref(), Some("[]"));

    // PATH detection looks for the product CLI the vendor installs (`bob`),
    // while the bridge that speaks ACP is npx.
    let source: serde_json::Value =
        serde_json::from_str(row.agent_source_info.as_deref().expect("agent_source_info")).unwrap();
    assert_eq!(source["binary_name"], "bob");
    assert_eq!(source["bridge_binary"], "npx");
    assert!(
        source.get("registry_json_id").is_none() && source.get("package_name").is_none(),
        "agent_source_info must not carry Registry identity fields: {source}"
    );
}

/// Bad path: auto-trusting every workspace is a security decision the seed
/// does not make, so the stored argv must stay the Registry's. A later change
/// that adds `--trust` (or the approval-weakening `--auto-approve`) has to be
/// a deliberate edit that breaks this test, not a silent drift.
#[tokio::test]
async fn launch_argv_does_not_trust_workspaces_or_auto_approve() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend(BACKEND)
        .await
        .unwrap()
        .expect("bob is seeded");

    let args: Vec<String> = serde_json::from_str(row.args.as_deref().expect("args")).unwrap();
    for flag in ["--trust", "--auto-approve", "--accept-license"] {
        assert!(
            !args.iter().any(|arg| arg == flag),
            "{flag} must not be in the seeded argv: {args:?}"
        );
    }
    assert!(
        args.iter().all(|arg| !arg.contains('@') || arg == "@ibm/bob"),
        "no version may be pinned in the stored argv: {args:?}"
    );
}

/// `initialize` was probed at integration time, so both handshake columns are
/// seeded in the snake_case contract of migration 003. Neither column may be
/// listed in the migration's `ON CONFLICT DO UPDATE` set — a re-seed must not
/// reset what a live handshake later teaches this install.
#[tokio::test]
async fn seeds_probed_handshake_columns_in_snake_case() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend(BACKEND)
        .await
        .unwrap()
        .expect("bob is seeded");

    let caps_raw = row.agent_capabilities.as_deref().expect("agent_capabilities seeded");
    let caps: serde_json::Value = serde_json::from_str(caps_raw).unwrap();
    assert_eq!(caps["load_session"], true);
    assert_eq!(
        caps["mcp_capabilities"]["http"], true,
        "Team must route it to the MCP transport"
    );
    assert!(
        caps["mcp_capabilities"].get("sse").is_none(),
        "initialize advertised http only; nothing is synthesized"
    );
    assert_eq!(caps["prompt_capabilities"]["image"], true);
    assert_eq!(caps["prompt_capabilities"]["embedded_context"], true);
    for capability in ["list", "delete", "resume", "close"] {
        assert!(
            caps["session_capabilities"].get(capability).is_some(),
            "session capability {capability} was advertised"
        );
    }
    assert_eq!(
        caps["_meta"]["bob/extensions"]["version"], 1,
        "_meta inner keys stay verbatim"
    );
    assert!(
        caps_raw.contains("load_session") && !caps_raw.contains("loadSession"),
        "handshake columns are stored snake_case (migration 003 contract)"
    );

    let auth: serde_json::Value =
        serde_json::from_str(row.auth_methods.as_deref().expect("auth_methods seeded")).unwrap();
    let methods = auth.as_array().expect("auth_methods is an array");
    assert_eq!(methods.len(), 1, "initialize advertised exactly one auth method");
    assert_eq!(methods[0]["id"], "sso");
    assert_eq!(methods[0]["name"], "Log in with SSO");
}

/// Fields that need runtime proof stay conservative: no bypass mode exists, so
/// `yolo_id` is NULL; the skills directory is the vendor's own, from source.
#[tokio::test]
async fn evidence_bound_fields_stay_conservative() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let row = repo
        .find_builtin_by_backend(BACKEND)
        .await
        .unwrap()
        .expect("bob is seeded");

    assert_eq!(
        row.yolo_id, None,
        "its session modes are agent/plan/ask and none bypasses approvals"
    );
    assert_eq!(
        row.native_skills_dirs.as_deref(),
        Some(r#"[".bob/skills"]"#),
        "only the vendor's own workspace skills directory is declared"
    );
    assert_eq!(
        row.skill_delivery, None,
        "NULL reads as the safe `injected` default (migration 043)"
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
fn bob_is_pinned_in_the_npx_release_lock() {
    let lock = include_str!("../../aionui-runtime/resources/acp-registry-npx-lock.json");
    let parsed: serde_json::Value = serde_json::from_str(lock).unwrap();

    let entry = parsed["agents"]
        .get(BACKEND)
        .unwrap_or_else(|| panic!("{BACKEND} must be pinned in the npx release lock"));
    assert_eq!(entry["package"], "@ibm/bob");
    assert_eq!(
        entry["registry_json_id"], "bob",
        "lookup alias lives in the lock, not in metadata"
    );
    assert!(
        entry.get("skip_version_probe").is_none(),
        "`bob --version` is documented and was verified to exit promptly"
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

/// Bad path: the vendor brand, the reported agent name, and the npm scope are
/// not backends. A lookup by any of them must miss, so nothing can accidentally
/// seed a second row under an alias and split the agent's identity.
#[tokio::test]
async fn aliases_are_not_registered_as_backends() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    for alias in ["ibm-bob", "bob-shell", "ibm", "@ibm/bob"] {
        assert!(
            repo.find_builtin_by_backend(alias).await.unwrap().is_none(),
            "{alias} must not resolve to a builtin row"
        );
    }
}
