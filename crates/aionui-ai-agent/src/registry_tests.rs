use super::*;
use aionui_db::{
    IAgentMetadataRepository, SqliteAgentMetadataRepository, UpsertAgentMetadataParams, init_database_memory,
};
use std::sync::Arc;

#[test]
fn probe_resolved_command_accepts_bare_npx_when_managed_runtime_is_supported() {
    if !probe_node_runtime_supported().is_supported() {
        return;
    }

    let meta = AgentMetadata {
        id: "agent-1".into(),
        icon: None,
        name: "Test ACP".into(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("custom".into()),
        agent_type: AgentType::Acp,
        agent_source: AgentSource::Custom,
        agent_source_info: AgentSourceInfo::default(),
        enabled: true,
        available: false,
        command: Some("npx".into()),
        resolved_command: None,
        args: vec![],
        env: vec![],
        native_skills_dirs: None,
        behavior_policy: BehaviorPolicy::default(),
        yolo_id: None,
        sort_order: 0,
        team_capable: false,
        last_check_status: None,
        last_check_kind: None,
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_error_details: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        handshake: AgentHandshake::default(),
        has_command_override: false,
        env_override_key_count: 0,
    };

    let resolved = probe_resolved_command(&meta).expect("probe");
    assert_eq!(resolved, PathBuf::from("npx"));
}

#[test]
fn probe_resolved_command_requires_primary_binary_for_builtin_managed_claude() {
    if !probe_node_runtime_supported().is_supported()
        || !probe_managed_acp_tool_supported(ManagedAcpToolId::ClaudeAgentAcp).is_supported()
    {
        return;
    }

    let meta = AgentMetadata {
        id: "agent-claude".into(),
        icon: None,
        name: "Claude Code".into(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude".into()),
        agent_type: AgentType::Acp,
        agent_source: AgentSource::Builtin,
        agent_source_info: AgentSourceInfo {
            binary_name: Some("definitely-missing-claude-cli".into()),
            ..Default::default()
        },
        enabled: true,
        available: false,
        command: None,
        resolved_command: None,
        args: vec![],
        env: vec![],
        native_skills_dirs: None,
        behavior_policy: BehaviorPolicy::default(),
        yolo_id: None,
        sort_order: 0,
        team_capable: false,
        last_check_status: None,
        last_check_kind: None,
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_error_details: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        handshake: AgentHandshake::default(),
        has_command_override: false,
        env_override_key_count: 0,
    };

    let reason = probe_resolved_command(&meta).expect_err("missing claude CLI must hide builtin row");
    assert!(matches!(
        reason,
        UnavailableReason::PrimaryMissing { binary } if binary == "definitely-missing-claude-cli"
    ));
}

#[test]
fn probe_resolved_command_requires_primary_binary_for_builtin_managed_codex() {
    if !probe_node_runtime_supported().is_supported()
        || !probe_managed_acp_tool_supported(ManagedAcpToolId::CodexAcp).is_supported()
    {
        return;
    }

    let meta = AgentMetadata {
        id: "agent-codex".into(),
        icon: None,
        name: "Codex".into(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("codex".into()),
        agent_type: AgentType::Acp,
        agent_source: AgentSource::Builtin,
        agent_source_info: AgentSourceInfo {
            binary_name: Some("definitely-missing-codex-cli".into()),
            ..Default::default()
        },
        enabled: true,
        available: false,
        command: None,
        resolved_command: None,
        args: vec![],
        env: vec![],
        native_skills_dirs: None,
        behavior_policy: BehaviorPolicy::default(),
        yolo_id: None,
        sort_order: 0,
        team_capable: false,
        last_check_status: None,
        last_check_kind: None,
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_error_details: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        handshake: AgentHandshake::default(),
        has_command_override: false,
        env_override_key_count: 0,
    };

    let reason = probe_resolved_command(&meta).expect_err("missing codex CLI must hide builtin row");
    assert!(matches!(
        reason,
        UnavailableReason::PrimaryMissing { binary } if binary == "definitely-missing-codex-cli"
    ));
}

#[tokio::test]
async fn management_rows_derive_missing_diagnostics_from_probe_reason() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

    repo.upsert(&UpsertAgentMetadataParams {
        id: "agent-missing-cli",
        icon: None,
        name: "Missing CLI Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("custom"),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some(r#"{"binary_name":"definitely-missing-cli"}"#),
        enabled: true,
        command: Some("definitely-missing-cli"),
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 100,
    })
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();
    registry.refresh_availability().await;

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-missing-cli")
        .unwrap();

    assert_eq!(row.status, AgentManagementStatus::Missing);
    assert_eq!(row.last_check_error_code.as_deref(), Some("command_missing"));
    assert!(
        row.last_check_error_message
            .as_deref()
            .is_some_and(|message| message.contains("definitely-missing-cli"))
    );
    assert!(
        row.last_check_guidance
            .as_deref()
            .is_some_and(|guidance| guidance.contains("PATH"))
    );
    let row_json = serde_json::to_value(&row).unwrap();
    assert_eq!(
        row_json["last_check_error_details"]["command"].as_str(),
        Some("definitely-missing-cli")
    );
}

#[tokio::test]
async fn management_rows_mark_installed_agents_without_health_check_unchecked() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let command_path = temp.path().join("unchecked-cli");
    std::fs::write(&command_path, "#!/bin/sh\nexit 0\n").unwrap();
    let command = command_path.to_string_lossy().to_string();
    let source_info = serde_json::json!({ "binary_name": command }).to_string();

    repo.upsert(&UpsertAgentMetadataParams {
        id: "agent-unchecked-cli",
        icon: None,
        name: "Unchecked CLI Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("custom"),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some(&source_info),
        enabled: true,
        command: Some(&command),
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 100,
    })
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-unchecked-cli")
        .unwrap();

    let row_json = serde_json::to_value(&row).unwrap();
    assert_eq!(row_json["status"].as_str(), Some("unchecked"));
    assert!(row.installed);
    assert!(row.last_check_status.is_none());
    assert!(row.last_check_error_code.is_none());
}

#[tokio::test]
async fn hydrate_continues_when_agent_metadata_config_options_has_invalid_utf8() {
    let db = init_database_memory().await.unwrap();
    sqlx::query("UPDATE agent_metadata SET config_options = CAST(x'FF' AS TEXT) WHERE id = ?")
        .bind("2d23ff1c")
        .execute(db.pool())
        .await
        .unwrap();

    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo.clone());

    registry.hydrate().await.unwrap();

    let claude = registry.get("2d23ff1c").await.expect("row remains in registry");
    assert_eq!(claude.name, "Claude Code");
    assert!(claude.handshake.config_options.is_none());
    let repaired = repo.get("2d23ff1c").await.unwrap().expect("row remains in database");
    assert!(repaired.config_options.is_none());
}

#[tokio::test]
async fn hydrate_keeps_valid_utf8_invalid_json_config_options_non_fatal() {
    let db = init_database_memory().await.unwrap();
    sqlx::query("UPDATE agent_metadata SET config_options = ? WHERE id = ?")
        .bind("not json")
        .bind("2d23ff1c")
        .execute(db.pool())
        .await
        .unwrap();

    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo.clone());

    registry.hydrate().await.unwrap();

    let claude = registry.get("2d23ff1c").await.expect("row remains in registry");
    assert!(claude.handshake.config_options.is_none());
    let persisted = repo.get("2d23ff1c").await.unwrap().expect("row remains in database");
    assert_eq!(persisted.config_options.as_deref(), Some("not json"));
}

#[tokio::test]
async fn management_rows_project_runtime_catalogs_from_agent_metadata() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

    repo.upsert(&UpsertAgentMetadataParams {
        id: "agent-with-catalog",
        icon: None,
        name: "Catalog Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude"),
        agent_type: "acp",
        agent_source: "builtin",
        agent_source_info: None,
        enabled: true,
        command: None,
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: Some(
            r#"{"config_options":[{"id":"model","type":"select","category":"model","options":[{"value":"claude-opus","label":"Claude Opus"}],"current_value":"claude-opus"}]}"#,
        ),
        available_modes: Some(
            r#"{"current_mode_id":"plan","available_modes":[{"id":"plan","name":"Plan"}]}"#,
        ),
        available_models: Some(
            r#"{"current_model_id":"claude-opus","current_model_label":"Claude Opus","available_models":[{"id":"claude-opus","label":"Claude Opus"}]}"#,
        ),
        available_commands: Some(
            r#"{"available_commands":[{"name":"review","description":"Review the current diff"}]}"#,
        ),
        sort_order: 100,
    })
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-with-catalog")
        .unwrap();
    let row_json = serde_json::to_value(&row).unwrap();

    assert_eq!(
        row_json["available_models"]["current_model_id"].as_str(),
        Some("claude-opus")
    );
    assert_eq!(row_json["available_modes"]["current_mode_id"].as_str(), Some("plan"));
    assert_eq!(
        row_json["config_options"]["config_options"][0]["current_value"].as_str(),
        Some("claude-opus")
    );
    assert_eq!(
        row_json["available_commands"]["available_commands"][0]["name"].as_str(),
        Some("review")
    );
}

#[tokio::test]
async fn management_rows_include_aionrs_builtin_mode_catalog() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.agent_type == AgentType::Aionrs)
        .unwrap();
    let row_json = serde_json::to_value(&row).unwrap();

    assert_eq!(row_json["available_modes"]["current_mode_id"].as_str(), Some("default"));
    assert_eq!(
        row_json["available_modes"]["available_modes"][1]["id"].as_str(),
        Some("auto_edit")
    );
    assert_eq!(
        row_json["config_options"]["config_options"][0]["options"][2]["value"].as_str(),
        Some("yolo")
    );
}
