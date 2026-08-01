use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use aionui_api_types::{
    AgentManagementRow, AgentMetadata, AgentSnapshotCheckKind, AgentSnapshotCheckStatus, AgentSource,
    TryConnectCustomAgentResponse,
};
use aionui_common::AgentType;
use aionui_common::now_ms;
use aionui_db::{IProviderRepository, UpdateAgentAvailabilitySnapshotParams};

use crate::error::AgentError;
use crate::protocol::custom_agent_probe;
use crate::registry::{AgentRegistry, guidance_for_snapshot_error_code};

#[async_trait::async_trait]
pub trait AgentAvailabilityFeedbackPort: Send + Sync {
    async fn record_session_success(&self, user_id: &str, agent_id: &str) -> Result<(), AgentError>;
    async fn record_session_failure(
        &self,
        user_id: &str,
        agent_id: &str,
        code: &str,
        message: &str,
    ) -> Result<(), AgentError>;
}

struct AvailabilitySnapshot {
    status: &'static str,
    kind: &'static str,
    error_code: Option<String>,
    error_message: Option<String>,
    latency_ms: i64,
    checked_at: i64,
}

#[derive(Clone)]
pub struct AgentAvailabilityService {
    registry: Arc<AgentRegistry>,
    // Used to decide aionrs (built-in, no external CLI) availability: it is
    // usable only when at least one model provider is configured & enabled.
    provider_repo: Arc<dyn IProviderRepository>,
}

impl AgentAvailabilityService {
    pub fn new(registry: Arc<AgentRegistry>, provider_repo: Arc<dyn IProviderRepository>) -> Self {
        Self {
            registry,
            provider_repo,
        }
    }

    pub async fn list_management_rows(&self, user_id: &str) -> Result<Vec<AgentManagementRow>, AgentError> {
        self.registry.list_management_rows_for_user(user_id).await
    }

    pub async fn run_manual_health_check(&self, user_id: &str, id: &str) -> Result<AgentManagementRow, AgentError> {
        let meta = self
            .registry
            .get_for_user(user_id, id)
            .await?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;

        // #675: never short-circuit on a stale availability verdict — the
        // manual check is the user's self-rescue path. `run_probe` handles a
        // missing binary itself (persisted command_not_found snapshot), and a
        // success restores the agent.
        let snapshot = run_probe(
            &self.registry,
            &self.provider_repo,
            &meta,
            user_id,
            AgentSnapshotCheckKind::Manual,
        )
        .await;
        self.persist_snapshot_for_user(user_id, id, &snapshot).await?;
        self.management_row_by_id(user_id, id)
            .await?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))
    }

    pub async fn record_session_failure(
        &self,
        user_id: &str,
        agent_id: &str,
        code: &str,
        message: &str,
    ) -> Result<(), AgentError> {
        let checked_at = now_ms();
        let snapshot = AvailabilitySnapshot {
            status: "offline",
            kind: "session",
            error_code: Some(code.to_owned()),
            error_message: Some(message.to_owned()),
            latency_ms: 0,
            checked_at,
        };
        self.persist_snapshot_for_user(user_id, agent_id, &snapshot).await
    }

    pub async fn record_session_success(&self, user_id: &str, agent_id: &str) -> Result<(), AgentError> {
        let checked_at = now_ms();
        let snapshot = AvailabilitySnapshot {
            status: "online",
            kind: "session",
            error_code: None,
            error_message: None,
            latency_ms: 0,
            checked_at,
        };
        self.persist_snapshot_for_user(user_id, agent_id, &snapshot).await
    }

    pub async fn management_row_by_id(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Option<AgentManagementRow>, AgentError> {
        self.registry.management_row_by_id_for_user(user_id, id).await
    }

    async fn persist_snapshot_for_user(
        &self,
        user_id: &str,
        id: &str,
        snapshot: &AvailabilitySnapshot,
    ) -> Result<(), AgentError> {
        let existing = self
            .registry
            .repo_handle()
            .get_for_user(user_id, id)
            .await
            .map_err(|error| AgentError::internal(format!("repo.get_for_user: {error}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;

        let params = UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some(snapshot.status),
            last_check_kind: Some(snapshot.kind),
            last_check_error_code: snapshot.error_code.as_deref(),
            last_check_error_message: snapshot.error_message.as_deref(),
            last_check_guidance: snapshot.error_code.as_deref().and_then(|code| {
                let guidance = guidance_for_snapshot_error_code(code);
                (!guidance.is_empty()).then_some(guidance)
            }),
            last_check_latency_ms: Some(snapshot.latency_ms),
            last_check_at: Some(snapshot.checked_at),
            last_success_at: if snapshot.status == "online" {
                Some(snapshot.checked_at)
            } else {
                existing.last_success_at
            },
            last_failure_at: if snapshot.status == "offline" {
                Some(snapshot.checked_at)
            } else {
                existing.last_failure_at
            },
        };
        self.registry
            .repo_handle()
            .update_availability_snapshot_for_user(user_id, id, &params)
            .await
            .map_err(|error| AgentError::internal(format!("repo.update_availability_snapshot_for_user: {error}")))?;
        Ok(())
    }
}

async fn run_probe(
    registry: &Arc<AgentRegistry>,
    provider_repo: &Arc<dyn IProviderRepository>,
    meta: &AgentMetadata,
    user_id: &str,
    kind: AgentSnapshotCheckKind,
) -> AvailabilitySnapshot {
    let started_at = now_ms();
    let start = Instant::now();

    let (status, error_code, error_message) = if meta.agent_source == AgentSource::Builtin
        && matches!(meta.backend.as_deref(), Some("claude") | Some("codex"))
    {
        // Builtin claude/codex are direct CLIs that do not speak ACP, so
        // their deep check is PATH + `--version` (integrity), never a
        // session/new-style handshake (#675). Uses the wide recheck budget:
        // the user is explicitly waiting and large Node CLIs load slowly.
        match crate::cli_probe::validate_with_budget(meta, crate::cli_probe::CLI_VERSION_RECHECK_TIMEOUT).await {
            Ok(_) => (AgentSnapshotCheckStatus::Online, None, None),
            Err(failure) => (
                AgentSnapshotCheckStatus::Offline,
                Some(failure.error_code().to_owned()),
                Some(failure.detail()),
            ),
        }
    } else if let Some(command) = meta.command.as_deref() {
        let env: HashMap<String, String> = meta
            .env
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect();
        match explicit_probe_args(meta) {
            Err(error) => (
                AgentSnapshotCheckStatus::Offline,
                Some("package_lock_invalid".to_owned()),
                Some(error),
            ),
            Ok(args) => {
                match custom_agent_probe::try_connect_custom_agent_with_catalog(command, &args, &env, None).await {
                    // The probe opened a real session to reach this verdict, so its
                    // `session/new` already carried whatever modes / models / config
                    // options the agent advertises. Persist them through the same
                    // channel a live conversation uses, so the pickers are populated
                    // before the user ever opens one. Best-effort and additive:
                    // `apply_handshake` skips `None` fields, so this never blanks a
                    // catalog a real session had filled in, and an agent that
                    // advertises nothing sends nothing.
                    (TryConnectCustomAgentResponse::Success, catalog) => {
                        if let Some(partial) = catalog {
                            registry
                                .catalog_sender()
                                .send_partial(user_id.to_owned(), meta.id.clone(), *partial);
                        }
                        (AgentSnapshotCheckStatus::Online, None, None)
                    }
                    (TryConnectCustomAgentResponse::FailCli { error }, _) => (
                        AgentSnapshotCheckStatus::Offline,
                        Some("command_not_found".to_owned()),
                        Some(error),
                    ),
                    (TryConnectCustomAgentResponse::FailAcp { error }, _) => (
                        AgentSnapshotCheckStatus::Offline,
                        Some("acp_init_failed".to_owned()),
                        Some(error),
                    ),
                    // Reachable but not authorized: still offline (unusable), but a
                    // dedicated code lets the UI guide the user to log in.
                    (TryConnectCustomAgentResponse::FailAuth { error }, _) => (
                        AgentSnapshotCheckStatus::Offline,
                        Some("auth_required".to_owned()),
                        Some(error),
                    ),
                }
            }
        }
    } else if meta.backend.is_some() {
        // Commandless builtin fallback: same PATH + `--version` treatment as
        // the direct CLIs — no PATH-only side door (#675).
        match crate::cli_probe::validate_with_budget(meta, crate::cli_probe::CLI_VERSION_RECHECK_TIMEOUT).await {
            Ok(_) => (AgentSnapshotCheckStatus::Online, None, None),
            Err(failure) => (
                AgentSnapshotCheckStatus::Offline,
                Some(failure.error_code().to_owned()),
                Some(failure.detail()),
            ),
        }
    } else if meta.agent_type == AgentType::Aionrs {
        // aionrs is the built-in Rust agent: there is no external CLI to probe,
        // so its usability hinges entirely on having a configured model. It is
        // online only when at least one model provider is enabled — otherwise
        // it cannot run a single turn.
        probe_aionrs_provider_readiness(provider_repo, user_id).await
    } else {
        (AgentSnapshotCheckStatus::Online, None, None)
    };

    let latency_ms = start.elapsed().as_millis() as i64;
    let status = match status {
        AgentSnapshotCheckStatus::Online => "online",
        AgentSnapshotCheckStatus::Offline => "offline",
    };

    AvailabilitySnapshot {
        status,
        kind: match kind {
            AgentSnapshotCheckKind::Startup => "startup",
            AgentSnapshotCheckKind::Scheduled => "scheduled",
            AgentSnapshotCheckKind::Manual => "manual",
            AgentSnapshotCheckKind::Session => "session",
        },
        error_code,
        error_message,
        latency_ms,
        checked_at: started_at,
    }
}

fn explicit_probe_args(meta: &AgentMetadata) -> Result<Vec<String>, String> {
    if meta.agent_source == AgentSource::Builtin && meta.agent_source_info.bridge_binary.as_deref() == Some("npx") {
        let backend = meta
            .backend
            .as_deref()
            .ok_or_else(|| "builtin npx agent has no backend".to_owned())?;
        return aionui_runtime::pin_registry_npx_args(backend, &meta.args).map_err(|error| error.to_string());
    }
    Ok(meta.args.clone())
}

/// Readiness check for the built-in aionrs agent.
///
/// aionrs has no external CLI; it runs models through configured providers.
/// Mirrors `AssistantService::resolve_default_agent_type`, which treats aionrs
/// as usable exactly when at least one provider is enabled. With no enabled
/// provider it cannot complete a turn, so we report it offline with a
/// `no_provider` code the UI maps to "configure a model" guidance.
async fn probe_aionrs_provider_readiness(
    provider_repo: &Arc<dyn IProviderRepository>,
    user_id: &str,
) -> (AgentSnapshotCheckStatus, Option<String>, Option<String>) {
    match provider_repo.list(user_id).await {
        Ok(providers) if providers.iter().any(|p| p.enabled) => (AgentSnapshotCheckStatus::Online, None, None),
        Ok(_) => (
            AgentSnapshotCheckStatus::Offline,
            Some("no_provider".to_owned()),
            Some("No model provider is configured. Add and enable a provider to use the built-in agent.".to_owned()),
        ),
        Err(e) => (
            AgentSnapshotCheckStatus::Offline,
            Some("no_provider".to_owned()),
            Some(format!("Failed to read model providers: {e}")),
        ),
    }
}

#[async_trait::async_trait]
impl AgentAvailabilityFeedbackPort for AgentAvailabilityService {
    async fn record_session_success(&self, user_id: &str, agent_id: &str) -> Result<(), AgentError> {
        AgentAvailabilityService::record_session_success(self, user_id, agent_id).await
    }

    async fn record_session_failure(
        &self,
        user_id: &str,
        agent_id: &str,
        code: &str,
        message: &str,
    ) -> Result<(), AgentError> {
        AgentAvailabilityService::record_session_failure(self, user_id, agent_id, code, message).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{AgentAvailabilityService, explicit_probe_args, probe_aionrs_provider_readiness, run_probe};
    use crate::registry::AgentRegistry;
    use aionui_api_types::{
        AgentHandshake, AgentManagementStatus, AgentMetadata, AgentSnapshotCheckKind, AgentSnapshotCheckStatus,
        AgentSource, AgentSourceInfo, BehaviorPolicy,
    };
    use aionui_common::AgentType;
    use aionui_db::{
        CreateProviderParams, IAgentMetadataRepository, IProviderRepository, SqliteAgentMetadataRepository,
        SqliteProviderRepository, UpsertAgentMetadataParams, init_database_memory,
    };

    const TEST_USER_ID: &str = "system_default_user";

    fn enabled_provider_params() -> CreateProviderParams<'static> {
        CreateProviderParams {
            id: None,
            user_id: TEST_USER_ID,
            platform: "openai",
            name: "OpenAI",
            base_url: "https://api.openai.com",
            api_key_encrypted: "enc",
            models: r#"["gpt-4"]"#,
            enabled: true,
            capabilities: r#"[{"type":"text"}]"#,
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        }
    }

    #[tokio::test]
    async fn aionrs_is_offline_without_an_enabled_provider() {
        let db = init_database_memory().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));

        let (status, code, _msg) = probe_aionrs_provider_readiness(&provider_repo, TEST_USER_ID).await;

        assert_eq!(status, AgentSnapshotCheckStatus::Offline);
        assert_eq!(code.as_deref(), Some("no_provider"));
    }

    #[tokio::test]
    async fn aionrs_is_online_when_a_provider_is_enabled() {
        let db = init_database_memory().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        provider_repo.create(enabled_provider_params()).await.unwrap();

        let (status, code, _msg) = probe_aionrs_provider_readiness(&provider_repo, TEST_USER_ID).await;

        assert_eq!(status, AgentSnapshotCheckStatus::Online);
        assert!(code.is_none());
    }

    #[tokio::test]
    async fn record_session_failure_persists_unavailable_snapshot() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

        repo.upsert(&UpsertAgentMetadataParams {
            id: "agent-session-failure",
            icon: None,
            name: "Session Failure Agent",
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude"),
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(r#"{"binary_name":"cargo"}"#),
            enabled: true,
            command: Some("cargo"),
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

        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AgentAvailabilityService::new(registry.clone(), provider_repo);
        service
            .record_session_failure(
                TEST_USER_ID,
                "agent-session-failure",
                "session_send_failed",
                "provider returned 401 invalid api key",
            )
            .await
            .unwrap();

        let row = service
            .list_management_rows(TEST_USER_ID)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.id == "agent-session-failure")
            .unwrap();

        assert_eq!(row.status, AgentManagementStatus::Offline);
        assert_eq!(row.last_check_status, Some(AgentSnapshotCheckStatus::Offline));
        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Session));
        assert_eq!(row.last_check_error_code.as_deref(), Some("session_send_failed"));
        assert_eq!(
            row.last_check_error_message.as_deref(),
            Some("provider returned 401 invalid api key")
        );
        assert_eq!(
            row.last_check_guidance.as_deref(),
            Some(
                "Fix the provider credentials or network issue that caused the last session failure, then start a new conversation."
            )
        );
        assert!(row.last_failure_at.is_some());
    }

    #[tokio::test]
    async fn record_session_success_persists_online_snapshot() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

        repo.upsert(&UpsertAgentMetadataParams {
            id: "agent-session-success",
            icon: None,
            name: "Session Success Agent",
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude"),
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(r#"{"binary_name":"cargo"}"#),
            enabled: true,
            command: Some("cargo"),
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

        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AgentAvailabilityService::new(registry.clone(), provider_repo);
        service
            .record_session_failure(
                TEST_USER_ID,
                "agent-session-success",
                "session_send_failed",
                "provider returned 401 invalid api key",
            )
            .await
            .unwrap();

        service
            .record_session_success(TEST_USER_ID, "agent-session-success")
            .await
            .unwrap();

        let row = service
            .list_management_rows(TEST_USER_ID)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.id == "agent-session-success")
            .unwrap();

        assert_eq!(row.status, AgentManagementStatus::Online);
        assert_eq!(row.last_check_status, Some(AgentSnapshotCheckStatus::Online));
        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Session));
        assert!(row.last_check_error_code.is_none());
        assert!(row.last_check_error_message.is_none());
        assert!(row.last_check_guidance.is_none());
        assert!(row.last_success_at.is_some());
        assert!(row.last_failure_at.is_some());
    }

    #[tokio::test]
    async fn managed_builtin_probe_checks_primary_binary_before_running_bridge_command() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();

        let meta = AgentMetadata {
            id: "agent-managed-builtin".into(),
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
                bridge_binary: Some("npx".into()),
                hub_package_id: None,
                version: None,
            },
            enabled: true,
            available: true,
            command: Some("npx".into()),
            resolved_command: None,
            args: vec!["--yes".into(), "@agentclientprotocol/claude-agent-acp@0.58.1".into()],
            env: vec![],
            native_skills_dirs: Some(vec![".claude/skills".into()]),
            behavior_policy: BehaviorPolicy::default(),
            yolo_id: Some("bypassPermissions".into()),
            sort_order: 3100,
            team_capable: true,
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

        let snapshot = run_probe(
            &registry,
            &provider_repo,
            &meta,
            "system_default_user",
            AgentSnapshotCheckKind::Manual,
        )
        .await;

        assert_eq!(snapshot.status, "offline");
        assert_eq!(snapshot.error_code.as_deref(), Some("command_not_found"));
        assert!(
            snapshot
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("definitely-missing-claude-cli")),
            "expected missing primary binary message, got {:?}",
            snapshot.error_message
        );

        let mut pi = meta.clone();
        pi.name = "Pi".into();
        pi.backend = Some("pi".into());
        pi.agent_source_info.binary_name = Some("pi".into());
        pi.agent_source_info.bridge_binary = Some("npx".into());
        pi.args = vec!["-y".into(), "pi-acp".into()];
        assert_eq!(explicit_probe_args(&pi).unwrap(), ["-y", "pi-acp@0.0.33"]);
    }

    // ---- #675: manual health check runs --version for direct CLIs and is
    // never short-circuited by a stale availability verdict ----

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

    #[cfg(unix)]
    fn upsert_builtin_claude_params<'a>(id: &'a str, source_info: &'a str) -> UpsertAgentMetadataParams<'a> {
        UpsertAgentMetadataParams {
            id,
            icon: None,
            name: "Claude Code",
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude"),
            agent_type: "acp",
            agent_source: "builtin",
            agent_source_info: Some(source_info),
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
        }
    }

    /// Manual health check on a direct-CLI builtin (claude/codex) must run
    /// `--version` — a corrupted install on PATH is offline with the
    /// classified code, not online-by-PATH (#675).
    #[cfg(unix)]
    #[tokio::test]
    async fn manual_check_flags_corrupted_direct_cli_offline() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        let temp = tempfile::tempdir().unwrap();
        let command = write_executable(
            temp.path(),
            "claude",
            "#!/bin/sh\nprintf 'native binary missing\\n' >&2\nexit 1\n",
        );
        let source_info = serde_json::json!({ "binary_name": command }).to_string();
        repo.upsert(&upsert_builtin_claude_params("agent-corrupted-claude", &source_info))
            .await
            .unwrap();

        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AgentAvailabilityService::new(registry, provider_repo);

        let row = service
            .run_manual_health_check(TEST_USER_ID, "agent-corrupted-claude")
            .await
            .unwrap();

        assert_eq!(row.status, AgentManagementStatus::Offline);
        assert_eq!(row.last_check_status, Some(AgentSnapshotCheckStatus::Offline));
        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Manual));
        assert_eq!(row.last_check_error_code.as_deref(), Some("version_probe_failed"));
        assert!(
            row.last_check_error_message
                .as_deref()
                .is_some_and(|message| message.contains("native binary missing"))
        );
    }

    /// Manual health check on a healthy direct-CLI builtin stays online and
    /// records the measured `--version` cost (#675).
    #[cfg(unix)]
    #[tokio::test]
    async fn manual_check_confirms_healthy_direct_cli_online() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        let temp = tempfile::tempdir().unwrap();
        let command = write_executable(temp.path(), "claude", "#!/bin/sh\nprintf 'claude 1.0.0\\n'\n");
        let source_info = serde_json::json!({ "binary_name": command }).to_string();
        repo.upsert(&upsert_builtin_claude_params("agent-healthy-claude", &source_info))
            .await
            .unwrap();

        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AgentAvailabilityService::new(registry, provider_repo);

        let row = service
            .run_manual_health_check(TEST_USER_ID, "agent-healthy-claude")
            .await
            .unwrap();

        assert_eq!(row.status, AgentManagementStatus::Online);
        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Manual));
        assert!(row.last_check_error_code.is_none());
    }

    /// Manual health check must reach its real probe even when the binary is
    /// missing entirely: the outcome is a persisted command_not_found manual
    /// snapshot, not a silent early return (#675).
    #[tokio::test]
    async fn manual_check_persists_command_not_found_instead_of_short_circuit() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        let source_info = r#"{"binary_name":"definitely-missing-claude-cli"}"#;
        repo.upsert(&{
            let mut params = UpsertAgentMetadataParams {
                id: "agent-missing-claude",
                icon: None,
                name: "Claude Code",
                name_i18n: None,
                description: None,
                description_i18n: None,
                backend: Some("claude"),
                agent_type: "acp",
                agent_source: "builtin",
                agent_source_info: Some(source_info),
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
            };
            params.sort_order = 100;
            params
        })
        .await
        .unwrap();

        let registry = AgentRegistry::new(repo.clone());
        registry.hydrate().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AgentAvailabilityService::new(registry, provider_repo);

        let row = service
            .run_manual_health_check(TEST_USER_ID, "agent-missing-claude")
            .await
            .unwrap();

        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Manual));
        assert_eq!(row.last_check_error_code.as_deref(), Some("command_not_found"));
        let persisted = repo.get("agent-missing-claude").await.unwrap().unwrap();
        assert_eq!(persisted.last_check_error_code.as_deref(), Some("command_not_found"));
    }

    /// A builtin without an explicit spawn command (the non-claude/codex
    /// fallback branch) gets the same PATH + `--version` treatment with
    /// classified errors — no PATH-only side door (#675).
    #[cfg(unix)]
    #[tokio::test]
    async fn commandless_builtin_fallback_probe_runs_version_check() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        let temp = tempfile::tempdir().unwrap();
        let command = write_executable(
            temp.path(),
            "hermes",
            "#!/bin/sh\nprintf 'wrapper broken\\n' >&2\nexit 1\n",
        );
        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));

        let meta = AgentMetadata {
            id: "agent-fallback-builtin".into(),
            icon: None,
            name: "Fallback Builtin".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("hermes".into()),
            agent_type: AgentType::Acp,
            agent_source: AgentSource::Builtin,
            agent_source_info: AgentSourceInfo {
                binary_name: Some(command.clone()),
                ..Default::default()
            },
            enabled: true,
            available: true,
            command: None,
            resolved_command: Some(std::path::PathBuf::from(&command)),
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

        let snapshot = run_probe(
            &registry,
            &provider_repo,
            &meta,
            TEST_USER_ID,
            AgentSnapshotCheckKind::Manual,
        )
        .await;
        assert_eq!(snapshot.status, "offline");
        assert_eq!(snapshot.error_code.as_deref(), Some("version_probe_failed"));
        assert!(
            snapshot
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("wrapper broken"))
        );
    }

    /// A previously-offline agent (stale probe verdict persisted) is restored
    /// to online by a successful manual check — the self-rescue path (#675).
    #[cfg(unix)]
    #[tokio::test]
    async fn manual_check_restores_previously_offline_agent() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        let temp = tempfile::tempdir().unwrap();
        let command = write_executable(temp.path(), "claude", "#!/bin/sh\nprintf 'claude 1.0.0\\n'\n");
        let source_info = serde_json::json!({ "binary_name": command }).to_string();
        repo.upsert(&upsert_builtin_claude_params("agent-restored-claude", &source_info))
            .await
            .unwrap();
        repo.update_availability_snapshot(
            "agent-restored-claude",
            &aionui_db::UpdateAgentAvailabilitySnapshotParams {
                last_check_status: Some("offline"),
                last_check_kind: Some("startup"),
                last_check_error_code: Some("version_probe_timeout"),
                last_check_error_message: Some("version_probe_timeout@5000ms"),
                last_check_guidance: None,
                last_check_latency_ms: Some(5_000),
                last_check_at: Some(1),
                last_success_at: None,
                last_failure_at: Some(1),
            },
        )
        .await
        .unwrap();

        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AgentAvailabilityService::new(registry, provider_repo);

        let before = service
            .management_row_by_id(TEST_USER_ID, "agent-restored-claude")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.status, AgentManagementStatus::Offline);

        let row = service
            .run_manual_health_check(TEST_USER_ID, "agent-restored-claude")
            .await
            .unwrap();
        assert_eq!(row.status, AgentManagementStatus::Online);
        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Manual));
        assert!(row.last_check_error_code.is_none());
    }
}
