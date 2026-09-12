use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn builtin_aionrs_display_name_is_wework_agent() {
    let db = init_database_memory().await.expect("in-memory database");
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let aionrs = repo.get("632f31d2").await.unwrap().expect("seeded aionrs row");
    assert_eq!(aionrs.name, "Wework Agent");
    assert_eq!(aionrs.agent_type, "aionrs");
    assert_eq!(aionrs.agent_source, "internal");
}
