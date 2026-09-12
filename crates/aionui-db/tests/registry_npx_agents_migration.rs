use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn verified_registry_npx_agents_were_removed_from_the_builtin_catalog() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let purged = [
        "autohand",
        "deepagents",
        "dimcode",
        "dirac",
        "glm-acp-agent",
        "grok",
        "kilo",
        "mimo-code",
        "nova",
        "sigit",
    ];
    for backend in purged {
        assert!(
            repo.find_builtin_by_backend(backend).await.unwrap().is_none(),
            "{backend} must not remain a builtin after 045"
        );
    }
}

#[tokio::test]
async fn pi_is_the_remaining_builtin_npx_agent() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let row = repo.find_builtin_by_backend("pi").await.unwrap().unwrap();
    assert_eq!(row.command.as_deref(), Some("npx"));
    assert_eq!(row.args.as_deref(), Some(r#"["-y","pi-acp"]"#));
    let source: serde_json::Value = serde_json::from_str(row.agent_source_info.as_deref().unwrap()).unwrap();
    assert_eq!(source["binary_name"], "pi");
    assert_eq!(source["bridge_binary"], "npx");
}
