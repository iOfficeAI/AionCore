use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn pi_acp_builtin_metadata_is_seeded() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let pi = repo.get("484e4bf2").await.unwrap().expect("seeded Pi ACP row");

    assert_eq!(pi.name, "Pi");
    assert_eq!(pi.backend.as_deref(), Some("pi"));
    assert_eq!(pi.agent_type, "acp");
    assert_eq!(pi.agent_source, "builtin");
    assert_eq!(pi.command.as_deref(), Some("npx"));
    assert_eq!(pi.args.as_deref(), Some(r#"["-y","pi-acp@0.0.31"]"#));
    assert_eq!(
        pi.agent_source_info.as_deref(),
        Some(r#"{"binary_name":"pi","bridge_binary":"npx","version":"0.0.31"}"#)
    );
    assert_eq!(pi.native_skills_dirs.as_deref(), Some(r#"[".pi/skills"]"#));
    assert_eq!(pi.yolo_id, None);

    let behavior_policy: serde_json::Value =
        serde_json::from_str(pi.behavior_policy.as_deref().expect("seeded behavior policy")).unwrap();
    assert_eq!(behavior_policy["team_capable_override"], false);

    let capabilities: serde_json::Value =
        serde_json::from_str(pi.agent_capabilities.as_deref().expect("seeded capabilities")).unwrap();
    assert_eq!(capabilities["load_session"], true);
    assert_eq!(capabilities["session_capabilities"]["list"], serde_json::json!({}));
    assert_eq!(capabilities["mcp_capabilities"]["http"], false);
    assert_eq!(capabilities["mcp_capabilities"]["sse"], false);

    let auth_methods: serde_json::Value =
        serde_json::from_str(pi.auth_methods.as_deref().expect("seeded auth methods")).unwrap();
    assert_eq!(auth_methods[0]["id"], "pi_terminal_login");
    assert_eq!(auth_methods[0]["type"], "terminal");
}
