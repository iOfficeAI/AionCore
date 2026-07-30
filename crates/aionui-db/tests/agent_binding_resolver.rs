use aionui_db::{
    AgentBindingResolution, IAgentMetadataRepository, SqliteAgentMetadataRepository, UpsertAgentMetadataParams,
    init_database_memory, resolve_agent_binding, resolve_agent_binding_for_user,
};

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

#[tokio::test]
async fn resolves_agent_binding_for_current_user_scope() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    for user_id in ["user-a", "user-b"] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'hash', 0, 0)",
        )
        .bind(user_id)
        .bind(user_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    // agent_id is globally unique: each user's custom agent is its own id.
    repo.upsert_for_user(
        "user-a",
        &custom_agent_params("agent-of-a", "User A Agent", "a-backend"),
    )
    .await
    .unwrap();
    repo.upsert_for_user(
        "user-b",
        &custom_agent_params("agent-of-b", "User B Agent", "b-backend"),
    )
    .await
    .unwrap();

    let user_a = resolve_agent_binding_for_user(db.pool(), "user-a", "agent-of-a")
        .await
        .unwrap()
        .expect("user-a binding");
    let user_b = resolve_agent_binding_for_user(db.pool(), "user-b", "agent-of-b")
        .await
        .unwrap()
        .expect("user-b binding");

    assert_eq!(user_a.runtime_backend, "a-backend");
    assert_eq!(user_b.runtime_backend, "b-backend");

    // Cross-user resolution stays closed: A cannot bind B's agent.
    assert!(
        resolve_agent_binding_for_user(db.pool(), "user-a", "agent-of-b")
            .await
            .unwrap()
            .is_none(),
        "custom agents must not resolve for non-owners"
    );
}

fn custom_agent_params<'a>(id: &'a str, name: &'a str, backend: &'a str) -> UpsertAgentMetadataParams<'a> {
    UpsertAgentMetadataParams {
        id,
        icon: None,
        name,
        name_i18n: None,
        description: Some("custom"),
        description_i18n: None,
        backend: Some(backend),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some("{}"),
        enabled: true,
        command: Some("custom"),
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: Some("[]"),
        behavior_policy: Some("{}"),
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 1000,
    }
}
