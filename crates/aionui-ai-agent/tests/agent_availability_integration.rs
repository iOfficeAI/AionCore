use std::sync::Arc;

use aionui_ai_agent::AgentRegistry;
use aionui_api_types::{AgentManagementStatus, AgentSnapshotCheckKind, AgentSnapshotCheckStatus};
use aionui_db::{
    IAgentMetadataRepository, SqliteAgentMetadataRepository, UpdateAgentAvailabilitySnapshotParams,
    UpsertAgentMetadataParams, init_database_memory,
};

fn custom_params<'a>(
    id: &'a str,
    name: &'a str,
    command: &'a str,
    agent_source_info: &'a str,
) -> UpsertAgentMetadataParams<'a> {
    UpsertAgentMetadataParams {
        id,
        icon: None,
        name,
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude"),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some(agent_source_info),
        enabled: true,
        command: Some(command),
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
    }
}

#[tokio::test]
async fn management_rows_derive_missing_available_and_unavailable_statuses() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

    repo.upsert(&custom_params(
        "agent-missing",
        "Missing Agent",
        "aionui-missing-agent-binary",
        r#"{"binary_name":"aionui-missing-agent-binary"}"#,
    ))
    .await
    .unwrap();
    repo.upsert(&custom_params(
        "agent-unavailable",
        "Unavailable Agent",
        "cargo",
        r#"{"binary_name":"cargo"}"#,
    ))
    .await
    .unwrap();
    repo.upsert(&custom_params(
        "agent-available",
        "Available Agent",
        "cargo",
        r#"{"binary_name":"cargo"}"#,
    ))
    .await
    .unwrap();

    repo.update_availability_snapshot(
        "agent-unavailable",
        &UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some("offline"),
            last_check_kind: Some("manual"),
            last_check_error_code: Some("auth_required"),
            last_check_error_message: Some("Login required"),
            last_check_guidance: Some("Run cargo login"),
            last_check_latency_ms: Some(320),
            last_check_at: Some(1_750_000_000_000),
            last_success_at: None,
            last_failure_at: Some(1_750_000_000_000),
        },
    )
    .await
    .unwrap();

    repo.update_availability_snapshot(
        "agent-available",
        &UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some("online"),
            last_check_kind: Some("scheduled"),
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_guidance: None,
            last_check_latency_ms: Some(120),
            last_check_at: Some(1_750_000_100_000),
            last_success_at: Some(1_750_000_100_000),
            last_failure_at: None,
        },
    )
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let rows = registry.list_management_rows().await;

    let missing = rows.iter().find(|row| row.id == "agent-missing").unwrap();
    assert_eq!(missing.status, AgentManagementStatus::Missing);
    assert_eq!(missing.last_check_status, None);

    let unavailable = rows.iter().find(|row| row.id == "agent-unavailable").unwrap();
    assert_eq!(unavailable.status, AgentManagementStatus::Offline);
    assert_eq!(unavailable.last_check_status, Some(AgentSnapshotCheckStatus::Offline));
    assert_eq!(unavailable.last_check_kind, Some(AgentSnapshotCheckKind::Manual));
    assert_eq!(unavailable.last_check_error_code.as_deref(), Some("auth_required"));
    let unavailable_json = serde_json::to_value(unavailable).unwrap();
    assert_eq!(
        unavailable_json["last_check_error_details"]["code"].as_str(),
        Some("auth_required")
    );

    let available = rows.iter().find(|row| row.id == "agent-available").unwrap();
    assert_eq!(available.status, AgentManagementStatus::Online);
    assert_eq!(available.last_check_status, Some(AgentSnapshotCheckStatus::Online));
    assert_eq!(available.last_check_kind, Some(AgentSnapshotCheckKind::Scheduled));
    assert_eq!(available.last_check_latency_ms, Some(120));
}
