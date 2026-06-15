use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use aionui_api_types::{
    AgentManagementRow, AgentMetadata, AgentSnapshotCheckKind, AgentSnapshotCheckStatus, TryConnectCustomAgentResponse,
};
use aionui_common::now_ms;
use aionui_db::UpdateAgentAvailabilitySnapshotParams;
use tokio::time::{Duration, sleep};

use crate::error::AgentError;
use crate::protocol::{cli_detect, custom_agent_probe};
use crate::registry::AgentRegistry;

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
            status: "unavailable",
            kind: "session",
            error_code: Some(code.to_owned()),
            error_message: Some(message.to_owned()),
            latency_ms: 0,
            checked_at,
        };
        self.persist_snapshot(agent_id, &snapshot).await
    }

    async fn management_row_by_id(&self, id: &str) -> Option<AgentManagementRow> {
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
            last_check_guidance: None,
            last_check_latency_ms: Some(snapshot.latency_ms),
            last_check_at: Some(snapshot.checked_at),
            last_success_at: if snapshot.status == "available" {
                Some(snapshot.checked_at)
            } else {
                existing.last_success_at
            },
            last_failure_at: if snapshot.status == "unavailable" {
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

    let (status, error_code, error_message) = if let Some(command) = meta.command.as_deref() {
        let env: HashMap<String, String> = meta
            .env
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect();
        match custom_agent_probe::try_connect_custom_agent(command, &meta.args, &env, data_dir, None).await {
            TryConnectCustomAgentResponse::Success => (AgentSnapshotCheckStatus::Available, None, None),
            TryConnectCustomAgentResponse::FailCli { error } => (
                AgentSnapshotCheckStatus::Unavailable,
                Some("command_not_found".to_owned()),
                Some(error),
            ),
            TryConnectCustomAgentResponse::FailAcp { error } => (
                AgentSnapshotCheckStatus::Unavailable,
                Some("acp_init_failed".to_owned()),
                Some(error),
            ),
        }
    } else if let Some(backend) = meta.backend.as_deref() {
        let result = cli_detect::health_check(registry, backend).await;
        if result.available {
            (AgentSnapshotCheckStatus::Available, None, None)
        } else {
            (
                AgentSnapshotCheckStatus::Unavailable,
                Some("health_check_failed".to_owned()),
                result.error,
            )
        }
    } else {
        (AgentSnapshotCheckStatus::Available, None, None)
    };

    let latency_ms = start.elapsed().as_millis() as i64;
    let status = match status {
        AgentSnapshotCheckStatus::Available => "available",
        AgentSnapshotCheckStatus::Unavailable => "unavailable",
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

#[async_trait::async_trait]
impl AgentAvailabilityFeedbackPort for AgentAvailabilityService {
    async fn record_session_failure(&self, agent_id: &str, code: &str, message: &str) -> Result<(), AgentError> {
        AgentAvailabilityService::record_session_failure(self, agent_id, code, message).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool};

    use aionui_api_types::{AgentManagementStatus, AgentSnapshotCheckKind, AgentSnapshotCheckStatus};
    use aionui_db::{
        IAgentMetadataRepository, SqliteAgentMetadataRepository, UpsertAgentMetadataParams, init_database_memory,
    };
    use tokio::time::Duration;

    use super::AgentAvailabilityService;
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

        assert_eq!(row.status, AgentManagementStatus::Unavailable);
        assert_eq!(row.last_check_status, Some(AgentSnapshotCheckStatus::Unavailable));
        assert_eq!(row.last_check_kind, Some(AgentSnapshotCheckKind::Session));
        assert_eq!(row.last_check_error_code.as_deref(), Some("session_send_failed"));
        assert_eq!(
            row.last_check_error_message.as_deref(),
            Some("provider returned 401 invalid api key")
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
}
