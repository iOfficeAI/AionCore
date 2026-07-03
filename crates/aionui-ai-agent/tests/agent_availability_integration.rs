use std::sync::Arc;

use aionui_ai_agent::{AgentRegistry, AgentService};
use aionui_api_types::{AgentManagementStatus, AgentSnapshotCheckKind, AgentSnapshotCheckStatus};
use aionui_db::{
    IAgentMetadataRepository, IProviderRepository, SqliteAgentMetadataRepository, SqliteProviderRepository,
    UpdateAgentAvailabilitySnapshotParams, UpsertAgentMetadataParams, init_database_memory,
};
use aionui_realtime::EventBroadcaster;

struct NoopBroadcaster;

impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _msg: aionui_api_types::WebSocketMessage<serde_json::Value>) {}
}

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

fn agent_service(
    registry: Arc<AgentRegistry>,
    provider_repo: Arc<dyn IProviderRepository>,
    data_dir: std::path::PathBuf,
) -> Arc<AgentService> {
    AgentService::new(registry, Arc::new(NoopBroadcaster), provider_repo, [0; 32], data_dir)
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
    registry.refresh_availability().await;

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

#[tokio::test]
async fn hydrate_uses_persisted_availability_without_reprobing_path() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let command_path = temp.path().join("startup-cached-agent-command");
    std::fs::write(&command_path, "#!/bin/sh\nexit 0\n").unwrap();
    let command = command_path.to_string_lossy().to_string();
    let source_info = serde_json::json!({ "binary_name": command }).to_string();

    repo.upsert(&custom_params(
        "agent-startup-cached",
        "Startup Cached Agent",
        &command,
        &source_info,
    ))
    .await
    .unwrap();
    repo.update_availability_snapshot(
        "agent-startup-cached",
        &UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some("online"),
            last_check_kind: Some("manual"),
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_guidance: None,
            last_check_latency_ms: Some(42),
            last_check_at: Some(1_750_000_100_000),
            last_success_at: Some(1_750_000_100_000),
            last_failure_at: None,
        },
    )
    .await
    .unwrap();

    std::fs::remove_file(&command_path).unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let rows = registry.list_management_rows().await;
    let cached = rows.iter().find(|row| row.id == "agent-startup-cached").unwrap();

    assert_eq!(cached.status, AgentManagementStatus::Online);
    assert!(cached.installed, "startup hydrate should not refresh PATH");
    assert_eq!(cached.last_check_status, Some(AgentSnapshotCheckStatus::Online));
}

#[tokio::test]
async fn hydrate_uses_persisted_availability_for_commandless_managed_builtin() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

    repo.upsert(&UpsertAgentMetadataParams {
        id: "agent-managed-cached",
        icon: None,
        name: "Managed Cached Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude"),
        agent_type: "acp",
        agent_source: "builtin",
        agent_source_info: Some(r#"{"binary_name":"claude"}"#),
        enabled: true,
        command: None,
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
    repo.update_availability_snapshot(
        "agent-managed-cached",
        &UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some("online"),
            last_check_kind: Some("manual"),
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
    let cached = rows.iter().find(|row| row.id == "agent-managed-cached").unwrap();

    assert_eq!(cached.status, AgentManagementStatus::Online);
    assert!(
        cached.installed,
        "managed builtin should trust persisted online snapshot"
    );
    assert_eq!(cached.last_check_status, Some(AgentSnapshotCheckStatus::Online));
}

#[tokio::test]
async fn management_list_uses_cached_availability_without_reprobing_path() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let command_path = temp.path().join("cached-agent-command");
    std::fs::write(&command_path, "#!/bin/sh\nexit 0\n").unwrap();
    let command = command_path.to_string_lossy().to_string();
    let source_info = serde_json::json!({ "binary_name": command }).to_string();

    repo.upsert(&custom_params("agent-cached", "Cached Agent", &command, &source_info))
        .await
        .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    std::fs::remove_file(&command_path).unwrap();

    let service = agent_service(registry, provider_repo, temp.path().to_path_buf());
    let rows = service.list_management_agents().await.unwrap();
    let cached = rows.iter().find(|row| row.id == "agent-cached").unwrap();

    assert_eq!(cached.status, AgentManagementStatus::Unchecked);
    assert!(!cached.installed, "management list should not refresh PATH on read");
}

#[tokio::test]
async fn manual_health_check_does_not_refresh_unrelated_agents() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let unrelated_path = temp.path().join("unrelated-agent-command");
    std::fs::write(&unrelated_path, "#!/bin/sh\nexit 0\n").unwrap();
    let unrelated_command = unrelated_path.to_string_lossy().to_string();
    let unrelated_source_info = serde_json::json!({ "binary_name": unrelated_command }).to_string();

    repo.upsert(&custom_params(
        "agent-unrelated",
        "Unrelated Agent",
        &unrelated_command,
        &unrelated_source_info,
    ))
    .await
    .unwrap();
    repo.upsert(&custom_params(
        "agent-target-missing",
        "Target Missing Agent",
        "aionui-definitely-missing-health-check-target",
        r#"{"binary_name":"aionui-definitely-missing-health-check-target"}"#,
    ))
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();
    std::fs::remove_file(&unrelated_path).unwrap();

    let service = agent_service(registry.clone(), provider_repo, temp.path().to_path_buf());
    service.health_check_agent_by_id("agent-target-missing").await.unwrap();

    let rows = registry.list_management_rows().await;
    let unrelated = rows.iter().find(|row| row.id == "agent-unrelated").unwrap();

    assert_eq!(unrelated.status, AgentManagementStatus::Unchecked);
    assert!(
        !unrelated.installed,
        "single-agent health check should not refresh unrelated agents"
    );
}

#[tokio::test]
async fn custom_enabled_toggle_does_not_refresh_unrelated_agents() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let unrelated_path = temp.path().join("unrelated-agent-command");
    std::fs::write(&unrelated_path, "#!/bin/sh\nexit 0\n").unwrap();
    let unrelated_command = unrelated_path.to_string_lossy().to_string();
    let unrelated_source_info = serde_json::json!({ "binary_name": unrelated_command }).to_string();

    repo.upsert(&custom_params(
        "agent-unrelated",
        "Unrelated Agent",
        &unrelated_command,
        &unrelated_source_info,
    ))
    .await
    .unwrap();
    repo.upsert(&custom_params(
        "agent-target-toggle",
        "Target Toggle Agent",
        "aionui-target-toggle-command",
        r#"{"binary_name":"aionui-target-toggle-command"}"#,
    ))
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();
    std::fs::remove_file(&unrelated_path).unwrap();

    let service = agent_service(registry.clone(), provider_repo, temp.path().to_path_buf());
    service.set_agent_enabled("agent-target-toggle", false).await.unwrap();

    let rows = registry.list_management_rows().await;
    let unrelated = rows.iter().find(|row| row.id == "agent-unrelated").unwrap();

    assert_eq!(unrelated.status, AgentManagementStatus::Unchecked);
    assert!(
        !unrelated.installed,
        "custom enabled toggle should not refresh unrelated agents"
    );
}

#[tokio::test]
async fn custom_delete_does_not_refresh_unrelated_agents() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let unrelated_path = temp.path().join("unrelated-agent-command");
    std::fs::write(&unrelated_path, "#!/bin/sh\nexit 0\n").unwrap();
    let unrelated_command = unrelated_path.to_string_lossy().to_string();
    let unrelated_source_info = serde_json::json!({ "binary_name": unrelated_command }).to_string();

    repo.upsert(&custom_params(
        "agent-unrelated",
        "Unrelated Agent",
        &unrelated_command,
        &unrelated_source_info,
    ))
    .await
    .unwrap();
    repo.upsert(&custom_params(
        "agent-target-delete",
        "Target Delete Agent",
        "aionui-target-delete-command",
        r#"{"binary_name":"aionui-target-delete-command"}"#,
    ))
    .await
    .unwrap();

    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();
    std::fs::remove_file(&unrelated_path).unwrap();

    let service = agent_service(registry.clone(), provider_repo, temp.path().to_path_buf());
    service.delete_custom_agent("agent-target-delete").await.unwrap();

    let rows = registry.list_management_rows().await;
    let unrelated = rows.iter().find(|row| row.id == "agent-unrelated").unwrap();

    assert_eq!(unrelated.status, AgentManagementStatus::Unchecked);
    assert!(
        !unrelated.installed,
        "custom delete should not refresh unrelated agents"
    );
    assert!(rows.iter().all(|row| row.id != "agent-target-delete"));
}
