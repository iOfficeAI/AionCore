use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

/// 033 retires the keys the Registry-sync workflow wrote that never meant what
/// they looked like: `team_capable_override` hard-vetoed team mode with no way to
/// lift it (builtin rows reject metadata edits), and `supports_team: false` read
/// like a denial while being a no-op inside an OR.
#[tokio::test]
async fn retired_team_policy_keys_are_stripped_from_every_seeded_policy() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let rows = repo.list_all().await.unwrap();
    assert!(!rows.is_empty(), "seeded agent metadata is present");

    for row in rows {
        let Some(policy) = row.behavior_policy.as_deref() else {
            continue;
        };
        let policy: serde_json::Value = serde_json::from_str(policy).unwrap();
        let backend = row.backend.as_deref().unwrap_or("<no backend>");
        assert!(
            policy.get("team_capable_override").is_none(),
            "{backend} still carries the retired team_capable_override"
        );
        assert_ne!(
            policy.get("supports_team"),
            Some(&serde_json::Value::Bool(false)),
            "{backend} still carries a no-op supports_team: false"
        );
    }
}

#[tokio::test]
async fn remaining_team_whitelist_survives() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let aionrs = repo
        .list_all()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.agent_type == "aionrs")
        .expect("aionrs row is seeded");
    let policy: serde_json::Value = serde_json::from_str(aionrs.behavior_policy.as_deref().unwrap()).unwrap();
    assert_eq!(policy.get("supports_team"), Some(&serde_json::Value::Bool(true)));

    let deepseek = repo
        .find_builtin_by_backend("deepseek")
        .await
        .unwrap()
        .expect("deepseek is seeded");
    let policy: serde_json::Value = serde_json::from_str(deepseek.behavior_policy.as_deref().unwrap()).unwrap();
    assert_eq!(policy.get("supports_team"), Some(&serde_json::Value::Bool(true)));
}

#[tokio::test]
async fn purged_registry_agents_are_absent() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let purged = [
        "claude",
        "codex",
        "gemini",
        "codebuddy",
        "autohand",
        "deepagents",
        "dirac",
        "glm-acp-agent",
        "grok",
        "kilo",
        "mimo-code",
        "nova",
        "omp",
        "sigit",
        "amp-acp",
        "corust-agent",
        "devin",
        "harn",
        "stakpak",
        "cortex-code",
        "dimcode",
        "poolside",
        "vtcode",
        "junie",
    ];
    for backend in purged {
        assert!(
            repo.find_builtin_by_backend(backend).await.unwrap().is_none(),
            "{backend} must not remain a builtin after 045"
        );
    }
}
