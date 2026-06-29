use aionui_db::{AgentBindingResolution, init_database_memory, resolve_agent_binding};

#[tokio::test]
async fn resolves_legacy_backend_to_agent_metadata_id() {
    let db = init_database_memory().await.unwrap();

    let resolved = resolve_agent_binding(db.pool(), "codex")
        .await
        .unwrap()
        .expect("codex should resolve");

    assert_eq!(
        resolved,
        AgentBindingResolution {
            agent_id: "8e1acf31".to_owned(),
            agent_source: "builtin".to_owned(),
            agent_type: "acp".to_owned(),
            runtime_backend: "codex".to_owned(),
        }
    );
}

#[tokio::test]
async fn resolves_internal_agent_type_when_backend_is_null() {
    let db = init_database_memory().await.unwrap();

    let resolved = resolve_agent_binding(db.pool(), "aionrs")
        .await
        .unwrap()
        .expect("aionrs should resolve");

    assert_eq!(resolved.agent_id, "632f31d2");
    assert_eq!(resolved.agent_source, "internal");
    assert_eq!(resolved.agent_type, "aionrs");
    assert_eq!(resolved.runtime_backend, "aionrs");
}
