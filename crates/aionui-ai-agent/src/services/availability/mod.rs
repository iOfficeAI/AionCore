use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use aionui_api_types::{
    AgentManagementRow, AgentMetadata, AgentSnapshotCheckKind, AgentSnapshotCheckStatus, AgentSource,
    TryConnectCustomAgentResponse,
};
use aionui_common::now_ms;
use aionui_common::{CommandSpec, EnvVar};
use aionui_db::UpdateAgentAvailabilitySnapshotParams;
use aionui_runtime::{
    ManagedAcpToolId, ensure_managed_acp_tool_with_reporter, ensure_node_runtime_with_reporter, resolve_command_path,
};
use tokio::time::{Duration, sleep};

use crate::error::AgentError;
use crate::protocol::{cli_detect, custom_agent_probe};
use crate::registry::{AgentRegistry, guidance_for_snapshot_error_code};

const DEFAULT_STARTUP_DELAY: Duration = Duration::from_secs(15);
const DEFAULT_SCHEDULED_INTERVAL: Duration = Duration::from_secs(300);

#[async_trait::async_trait]
pub trait AgentAvailabilityFeedbackPort: Send + Sync {
    async fn record_session_failure(&self, agent_id: &str, code: &str, message: &str) -> Result<(), AgentError>;
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
    data_dir: PathBuf,
    scheduler_started: Arc<AtomicBool>,
    startup_delay: Duration,
    scheduled_interval: Duration,
}

impl AgentAvailabilityService {
    pub fn new(registry: Arc<AgentRegistry>, data_dir: PathBuf) -> Self {
        Self {
            registry,
            data_dir,
            scheduler_started: Arc::new(AtomicBool::new(false)),
            startup_delay: DEFAULT_STARTUP_DELAY,
            scheduled_interval: DEFAULT_SCHEDULED_INTERVAL,
        }
    }

    pub fn start_background_scheduler(&self) {
        if self
            .scheduler_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let service = self.clone();
        tokio::spawn(async move {
            sleep(service.startup_delay).await;
            loop {
                if let Err(error) = service.run_scheduled_probe_pass().await {
                    tracing::warn!(error = %error, "agent availability scheduled probe pass failed");
                }
                sleep(service.scheduled_interval).await;
            }
        });
    }

    pub async fn list_management_rows(&self) -> Vec<AgentManagementRow> {
        self.registry.refresh_availability().await;
        self.registry.list_management_rows().await
    }

    pub async fn run_manual_health_check(&self, id: &str) -> Result<AgentManagementRow, AgentError> {
        self.registry.invalidate_and_rehydrate().await?;
        let meta = self
            .registry
            .get(id)
            .await
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;

        if !meta.available {
            return self
                .management_row_by_id(id)
                .await
                .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")));
        }

        let snapshot = run_probe(&self.registry, &meta, &self.data_dir, AgentSnapshotCheckKind::Manual).await;
        self.persist_snapshot(id, &snapshot).await?;
        self.management_row_by_id(id)
            .await
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))
    }

    pub async fn record_session_failure(&self, agent_id: &str, code: &str, message: &str) -> Result<(), AgentError> {
        let checked_at = now_ms();
        let snapshot = AvailabilitySnapshot {
            status: "offline",
            kind: "session",
            error_code: Some(code.to_owned()),
            error_message: Some(message.to_owned()),
            latency_ms: 0,
            checked_at,
        };
        self.persist_snapshot(agent_id, &snapshot).await
    }

    pub async fn management_row_by_id(&self, id: &str) -> Option<AgentManagementRow> {
        self.registry
            .list_management_rows()
            .await
            .into_iter()
            .find(|row| row.id == id)
    }

    async fn run_scheduled_probe_pass(&self) -> Result<(), AgentError> {
        self.registry.invalidate_and_rehydrate().await?;
        let rows = self.registry.list_all_including_hidden().await;
        for meta in rows
            .into_iter()
            .filter(|item| item.enabled && item.available && item.agent_type.supports_new_conversation())
        {
            let snapshot = run_probe(&self.registry, &meta, &self.data_dir, AgentSnapshotCheckKind::Scheduled).await;
            self.persist_snapshot(&meta.id, &snapshot).await?;
        }
        Ok(())
    }

    async fn persist_snapshot(&self, id: &str, snapshot: &AvailabilitySnapshot) -> Result<(), AgentError> {
        let existing = self
            .registry
            .repo_handle()
            .get(id)
            .await
            .map_err(|error| AgentError::internal(format!("repo.get: {error}")))?
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
            .update_availability_snapshot(id, &params)
            .await
            .map_err(|error| AgentError::internal(format!("repo.update_availability_snapshot: {error}")))?;
        self.registry.invalidate_and_rehydrate().await?;
        Ok(())
    }
}

async fn run_probe(
    registry: &Arc<AgentRegistry>,
    meta: &AgentMetadata,
    data_dir: &std::path::Path,
    kind: AgentSnapshotCheckKind,
) -> AvailabilitySnapshot {
    let started_at = now_ms();
    let start = Instant::now();

    let (status, error_code, error_message) = if meta.agent_source == AgentSource::Builtin
        && let Some(backend) = meta.backend.as_deref()
        && let Some(tool) = ManagedAcpToolId::from_backend(backend)
    {
        match try_connect_builtin_managed_agent(meta, data_dir, tool).await {
            TryConnectCustomAgentResponse::Success => (AgentSnapshotCheckStatus::Online, None, None),
            TryConnectCustomAgentResponse::FailCli { error } => (
                AgentSnapshotCheckStatus::Offline,
                Some("command_not_found".to_owned()),
                Some(error),
            ),
            TryConnectCustomAgentResponse::FailAcp { error } => (
                AgentSnapshotCheckStatus::Offline,
                Some("acp_init_failed".to_owned()),
                Some(error),
            ),
        }
    } else if let Some(command) = meta.command.as_deref() {
        let env: HashMap<String, String> = meta
            .env
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect();
        match custom_agent_probe::try_connect_custom_agent(command, &meta.args, &env, data_dir, None).await {
            TryConnectCustomAgentResponse::Success => (AgentSnapshotCheckStatus::Online, None, None),
            TryConnectCustomAgentResponse::FailCli { error } => (
                AgentSnapshotCheckStatus::Offline,
                Some("command_not_found".to_owned()),
                Some(error),
            ),
            TryConnectCustomAgentResponse::FailAcp { error } => (
                AgentSnapshotCheckStatus::Offline,
                Some("acp_init_failed".to_owned()),
                Some(error),
            ),
        }
    } else if let Some(backend) = meta.backend.as_deref() {
        let result = cli_detect::health_check(registry, backend).await;
        if result.available {
            (AgentSnapshotCheckStatus::Online, None, None)
        } else {
            (
                AgentSnapshotCheckStatus::Offline,
                Some("health_check_failed".to_owned()),
                result.error,
            )
        }
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

async fn try_connect_builtin_managed_agent(
    meta: &AgentMetadata,
    data_dir: &std::path::Path,
    tool: ManagedAcpToolId,
) -> TryConnectCustomAgentResponse {
    if let Some(primary) = meta.agent_source_info.binary_name.as_deref()
        && resolve_command_path(primary).is_none()
    {
        return TryConnectCustomAgentResponse::FailCli {
            error: format!("`{primary}` not found on PATH"),
        };
    }

    let node_runtime = match ensure_node_runtime_with_reporter(None).await {
        Ok(runtime) => runtime,
        Err(error) => {
            return TryConnectCustomAgentResponse::FailCli {
                error: error.to_string(),
            };
        }
    };

    let managed_tool = match ensure_managed_acp_tool_with_reporter(tool, None).await {
        Ok(tool) => tool,
        Err(error) => {
            return TryConnectCustomAgentResponse::FailCli {
                error: error.to_string(),
            };
        }
    };

    let resolved = managed_tool.command(&node_runtime);
    let mut env: Vec<EnvVar> = meta
        .env
        .iter()
        .map(|entry| EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    let spec = CommandSpec {
        command: resolved.program,
        args: resolved
            .args_prefix
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        env,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
    };

    match tokio::time::timeout(
        Duration::from_secs(35),
        custom_agent_probe::acp_initialize_command_spec(spec, data_dir),
    )
    .await
    {
        Ok(Ok(())) => TryConnectCustomAgentResponse::Success,
        Ok(Err(error)) => TryConnectCustomAgentResponse::FailAcp { error },
        Err(_) => TryConnectCustomAgentResponse::FailAcp {
            error: "ACP initialize did not complete within 35s".to_owned(),
        },
    }
}

#[async_trait::async_trait]
impl AgentAvailabilityFeedbackPort for AgentAvailabilityService {
    async fn record_session_failure(&self, agent_id: &str, code: &str, message: &str) -> Result<(), AgentError> {
        AgentAvailabilityService::record_session_failure(self, agent_id, code, message).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool};

    use aionui_api_types::{
        AgentHandshake, AgentManagementStatus, AgentMetadata, AgentSnapshotCheckKind, AgentSnapshotCheckStatus,
        AgentSource, AgentSourceInfo, BehaviorPolicy,
    };
    use aionui_common::AgentType;
    use aionui_db::{
        IAgentMetadataRepository, SqliteAgentMetadataRepository, UpsertAgentMetadataParams, init_database_memory,
    };
    use tokio::time::Duration;

    use super::{AgentAvailabilityService, run_probe};
    use crate::registry::AgentRegistry;

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

        let service = AgentAvailabilityService::new(registry.clone(), std::env::temp_dir());
        service
            .record_session_failure(
                "agent-session-failure",
                "session_send_failed",
                "provider returned 401 invalid api key",
            )
            .await
            .unwrap();

        let row = service
            .list_management_rows()
            .await
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
    async fn background_scheduler_persists_scheduled_snapshot() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));

        repo.upsert(&UpsertAgentMetadataParams {
            id: "agent-scheduled-check",
            icon: None,
            name: "Scheduled Check Agent",
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

        let service = AgentAvailabilityService {
            registry: registry.clone(),
            data_dir: std::env::temp_dir(),
            scheduler_started: Arc::new(AtomicBool::new(false)),
            startup_delay: Duration::from_millis(10),
            scheduled_interval: Duration::from_secs(60),
        };
        service.start_background_scheduler();

        let mut row = None;
        for _ in 0..20 {
            let candidate = service
                .list_management_rows()
                .await
                .into_iter()
                .find(|item| item.id == "agent-scheduled-check")
                .unwrap();
            if candidate.last_check_kind == Some(AgentSnapshotCheckKind::Scheduled) {
                row = Some(candidate);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let row = row.expect("scheduled probe should persist a snapshot");

        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Scheduled));
        assert!(row.last_check_status.is_some());
        assert!(row.last_check_at.is_some());
    }
    #[tokio::test]
    async fn managed_builtin_probe_checks_primary_binary_before_running_bridge_command() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
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
                bridge_binary: Some("bun".into()),
                hub_package_id: None,
                version: None,
            },
            enabled: true,
            available: true,
            command: Some("bun".into()),
            resolved_command: None,
            args: vec![
                "x".into(),
                "--bun".into(),
                "@agentclientprotocol/claude-agent-acp@0.39.0".into(),
            ],
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
            &meta,
            std::env::temp_dir().as_path(),
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
    }
}
