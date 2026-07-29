use super::*;
use aionui_db::{
    IAgentMetadataRepository, SqliteAgentMetadataRepository, UpsertAgentMetadataParams, init_database_memory,
};
use std::sync::Arc;

#[tokio::test]
async fn probe_resolved_command_keeps_bridge_but_version_probe_targets_primary_cli() {
    if !probe_node_runtime_supported().is_supported() {
        return;
    }

    let mut meta = AgentMetadata {
        id: "agent-1".into(),
        icon: None,
        name: "Test ACP".into(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("pi".into()),
        agent_type: AgentType::Acp,
        agent_source: AgentSource::Builtin,
        agent_source_info: AgentSourceInfo {
            binary_name: Some("cargo".into()),
            bridge_binary: Some("npx".into()),
            ..Default::default()
        },
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
    assert_eq!(crate::cli_probe::command_name(&meta), Some("cargo"));

    meta.available = true;
    meta.resolved_command = Some(resolved);
    let (meta, reason, _outcome) = validate_cli_availability(meta, None, ProbePolicy::default()).await;
    assert!(reason.is_none());
    assert_eq!(meta.resolved_command, Some(PathBuf::from("npx")));

    let mut missing_primary = meta;
    missing_primary.agent_source_info.binary_name = Some("aionui-definitely-missing-product-cli".into());
    let reason = probe_resolved_command(&missing_primary).expect_err("npx must not replace the product CLI check");
    assert!(matches!(
        reason,
        UnavailableReason::PrimaryMissing { binary } if binary == "aionui-definitely-missing-product-cli"
    ));
}

#[test]
fn probe_resolved_command_requires_primary_binary_for_builtin_managed_claude() {
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

#[cfg(unix)]
#[tokio::test]
async fn builtin_non_codex_with_broken_wrapper_is_not_installed() {
    use std::os::unix::fs::PermissionsExt;

    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let command_path = temp.path().join("gemini");
    std::fs::write(
        &command_path,
        "#!/bin/sh\nprintf 'native binary missing\\n' >&2\nexit 1\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&command_path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&command_path, permissions).unwrap();
    let command = command_path.to_string_lossy().to_string();
    let source_info = serde_json::json!({ "binary_name": command }).to_string();

    repo.upsert(&UpsertAgentMetadataParams {
        id: "agent-broken-gemini",
        icon: None,
        name: "Broken Gemini",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("gemini"),
        agent_type: "acp",
        agent_source: "builtin",
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
        .find(|item| item.id == "agent-broken-gemini")
        .unwrap();
    // #675: the binary IS on PATH — a failing `--version` proves a corrupted
    // install, not a missing one. Installed + Offline, with the classified code.
    assert!(row.installed);
    assert_eq!(row.status, AgentManagementStatus::Offline);
    assert_eq!(row.last_check_error_code.as_deref(), Some("version_probe_failed"));
    assert!(
        row.last_check_error_message
            .as_deref()
            .is_some_and(|message| message.contains("native binary missing"))
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

// ---- #675: adaptive slow-probe pipeline ----

#[cfg(unix)]
fn upsert_script_agent_params<'a>(
    id: &'a str,
    name: &'a str,
    command: &'a str,
    source_info: &'a str,
) -> UpsertAgentMetadataParams<'a> {
    UpsertAgentMetadataParams {
        id,
        icon: None,
        name,
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("gemini"),
        agent_type: "acp",
        agent_source: "builtin",
        agent_source_info: Some(source_info),
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

#[cfg(unix)]
fn write_executable(dir: &std::path::Path, name: &str, contents: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path.to_string_lossy().to_string()
}

/// A builtin whose `--version` exceeds the inline budget must NOT be
/// condemned to Missing: it stays installed, surfaces as Unchecked, and is
/// queued for the background recheck (#675).
#[cfg(unix)]
#[tokio::test]
async fn slow_version_probe_lands_unchecked_and_pending_not_missing() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let command = write_executable(temp.path(), "slow-cli", "#!/bin/sh\nsleep 10\n");
    let source_info = serde_json::json!({ "binary_name": command }).to_string();
    repo.upsert(&upsert_script_agent_params(
        "agent-slow-cli",
        "Slow CLI",
        &command,
        &source_info,
    ))
    .await
    .unwrap();

    let registry = AgentRegistry::new_with_probe_policy(
        repo,
        ProbePolicy {
            inline_budget: std::time::Duration::from_millis(50),
            recheck_budget: std::time::Duration::from_millis(100),
            slow_threshold_ms: 2000,
        },
    );
    registry.hydrate().await.unwrap();

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-slow-cli")
        .unwrap();
    assert!(row.installed, "slow probe must not uninstall the agent");
    assert_eq!(row.status, AgentManagementStatus::Unchecked);
    assert!(
        registry
            .pending_slow_probe_rechecks()
            .await
            .contains(&"agent-slow-cli".to_string()),
        "timed-out probe must be queued for recheck"
    );
}

/// An agent whose persisted startup snapshot proves the probe is slow skips
/// the inline `--version` entirely (no 5s tax on backend readiness) and goes
/// straight to the recheck queue (#675).
#[cfg(unix)]
#[tokio::test]
async fn slow_probe_history_skips_inline_version_check() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    // The script records every execution: if the inline probe runs it, the
    // marker file appears — a deterministic skip-proof that does not depend
    // on wall-clock under test-runner load.
    let marker = temp.path().join("probe-ran.marker");
    let command = write_executable(
        temp.path(),
        "slow-cli",
        &format!("#!/bin/sh\ntouch '{}'\nsleep 60\n", marker.display()),
    );
    let source_info = serde_json::json!({ "binary_name": command }).to_string();
    repo.upsert(&upsert_script_agent_params(
        "agent-slow-history",
        "Slow History CLI",
        &command,
        &source_info,
    ))
    .await
    .unwrap();
    repo.update_availability_snapshot(
        "agent-slow-history",
        &aionui_db::UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some("online"),
            last_check_kind: Some("startup"),
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_guidance: None,
            last_check_latency_ms: Some(6_800),
            last_check_at: Some(1),
            last_success_at: Some(1),
            last_failure_at: None,
        },
    )
    .await
    .unwrap();

    // Generous inline budget: if the inline probe ran it would take 10s.
    let registry = AgentRegistry::new_with_probe_policy(
        repo,
        ProbePolicy {
            inline_budget: std::time::Duration::from_secs(30),
            recheck_budget: std::time::Duration::from_millis(100),
            slow_threshold_ms: 2000,
        },
    );
    registry.hydrate().await.unwrap();
    assert!(
        !marker.exists(),
        "hydrate must not execute the known-slow CLI's inline version probe"
    );

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-slow-history")
        .unwrap();
    assert!(row.installed);
    // Prior verified snapshot keeps its word until the recheck lands.
    assert_eq!(row.status, AgentManagementStatus::Online);
    assert!(
        registry
            .pending_slow_probe_rechecks()
            .await
            .contains(&"agent-slow-history".to_string()),
        "slow-history agent must be queued for recheck"
    );
}

/// Background recheck with the wider budget settles a pending agent to
/// Online and persists the startup snapshot with the measured duration (#675).
#[cfg(unix)]
#[tokio::test]
async fn background_recheck_settles_pending_agent_online() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    // Slower than the inline budget, faster than the recheck budget.
    let command = write_executable(
        temp.path(),
        "medium-cli",
        "#!/bin/sh\nsleep 0.3\nprintf 'medium-cli 1.0.0\\n'\n",
    );
    let source_info = serde_json::json!({ "binary_name": command }).to_string();
    repo.upsert(&upsert_script_agent_params(
        "agent-medium-cli",
        "Medium CLI",
        &command,
        &source_info,
    ))
    .await
    .unwrap();

    let registry = AgentRegistry::new_with_probe_policy(
        repo.clone(),
        ProbePolicy {
            inline_budget: std::time::Duration::from_millis(50),
            recheck_budget: std::time::Duration::from_secs(10),
            slow_threshold_ms: 2000,
        },
    );
    registry.hydrate().await.unwrap();
    assert!(
        registry
            .pending_slow_probe_rechecks()
            .await
            .contains(&"agent-medium-cli".to_string())
    );

    // Scope the recheck to our agent: on a dev host the tiny inline budget
    // also queues every real builtin CLI, and settling those sequentially
    // would dominate the test.
    *registry.pending_recheck.write().await = vec!["agent-medium-cli".to_string()];
    registry.run_slow_probe_recheck().await;

    assert!(registry.pending_slow_probe_rechecks().await.is_empty());
    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-medium-cli")
        .unwrap();
    assert_eq!(row.status, AgentManagementStatus::Online);
    // Snapshot persisted so the next startup skips the inline probe.
    let persisted = repo.get("agent-medium-cli").await.unwrap().unwrap();
    assert_eq!(persisted.last_check_status.as_deref(), Some("online"));
    assert_eq!(persisted.last_check_kind.as_deref(), Some("startup"));
    assert!(
        persisted.last_check_latency_ms.is_some_and(|ms| ms >= 200),
        "recheck must persist the measured duration, got {:?}",
        persisted.last_check_latency_ms
    );
}

/// Background recheck classifies a corrupted install (exit != 0) as offline
/// with the failure detail persisted (#675).
#[cfg(unix)]
#[tokio::test]
async fn background_recheck_marks_corrupted_install_offline() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let temp = tempfile::tempdir().unwrap();
    let command = write_executable(
        temp.path(),
        "broken-slow-cli",
        "#!/bin/sh\nsleep 0.2\nprintf 'native binary missing\\n' >&2\nexit 1\n",
    );
    let source_info = serde_json::json!({ "binary_name": command }).to_string();
    repo.upsert(&upsert_script_agent_params(
        "agent-broken-slow",
        "Broken Slow CLI",
        &command,
        &source_info,
    ))
    .await
    .unwrap();

    let registry = AgentRegistry::new_with_probe_policy(
        repo.clone(),
        ProbePolicy {
            inline_budget: std::time::Duration::from_millis(50),
            recheck_budget: std::time::Duration::from_secs(10),
            slow_threshold_ms: 2000,
        },
    );
    registry.hydrate().await.unwrap();
    *registry.pending_recheck.write().await = vec!["agent-broken-slow".to_string()];
    registry.run_slow_probe_recheck().await;

    let row = registry
        .list_management_rows()
        .await
        .into_iter()
        .find(|item| item.id == "agent-broken-slow")
        .unwrap();
    assert_eq!(row.status, AgentManagementStatus::Offline);
    assert_eq!(row.last_check_error_code.as_deref(), Some("version_probe_failed"));
    assert!(
        row.last_check_error_message
            .as_deref()
            .is_some_and(|message| message.contains("native binary missing"))
    );
}
