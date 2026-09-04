use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn verified_registry_binary_agents_were_removed_from_the_builtin_catalog() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let cases = [
        "amp-acp",
        "cortex-code",
        "corust-agent",
        "devin",
        "harn",
        "junie",
        "poolside",
        "stakpak",
        "vtcode",
    ];
    for backend in cases {
        assert!(
            repo.find_builtin_by_backend(backend).await.unwrap().is_none(),
            "{backend} must not remain a builtin after 045"
        );
    }
}
