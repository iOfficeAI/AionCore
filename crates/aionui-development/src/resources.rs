use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::{ExecutionResourceLeaseRow, IDevelopmentOperationsRepository};
use async_trait::async_trait;
use tracing::{info, warn};

use crate::DevelopmentError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLeaseInput {
    pub user_id: String,
    pub project_id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub gate_id: Option<String>,
    pub environment_id: String,
    pub environment_kind: String,
    pub resource_kind: String,
    pub resource_identifier: String,
    pub cleanup_order: i64,
    pub ttl_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct CleanupTarget<'a> {
    pub resource_kind: &'a str,
    pub resource_identifier: &'a str,
    pub environment_id: &'a str,
}

#[async_trait]
pub trait DevelopmentResourceController: Send + Sync {
    async fn signal_agent(&self, run_id: &str) -> Result<(), String>;
    async fn cleanup(&self, target: CleanupTarget<'_>) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct SystemDevelopmentResourceController;

#[async_trait]
impl DevelopmentResourceController for SystemDevelopmentResourceController {
    async fn signal_agent(&self, _run_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn cleanup(&self, target: CleanupTarget<'_>) -> Result<(), String> {
        match target.resource_kind {
            "process" => {
                let pid = target
                    .resource_identifier
                    .parse::<u32>()
                    .map_err(|_| "invalid persisted process identifier".to_owned())?;
                aionui_runtime::kill_process_tree_by_id(pid)
                    .await
                    .map_err(|error| error.to_string())
            }
            "container" => {
                let mut command = aionui_runtime::Builder::clean_cli("docker");
                command.args(["rm", "--force", target.resource_identifier]);
                let output = command.output().await.map_err(|error| error.to_string())?;
                if output.status.success() || String::from_utf8_lossy(&output.stderr).contains("No such container") {
                    Ok(())
                } else {
                    Err("container cleanup failed".into())
                }
            }
            "service" => cleanup_devcontainer_service(target.resource_identifier).await,
            "port" | "lock" | "workspace" => Ok(()),
            _ => Err("unsupported persisted resource kind".into()),
        }
    }
}

async fn cleanup_devcontainer_service(workspace: &str) -> Result<(), String> {
    let filter = format!("label=devcontainer.local_folder={workspace}");
    let mut list = aionui_runtime::Builder::clean_cli("docker");
    list.args(["ps", "--quiet", "--filter", &filter]);
    let output = list.output().await.map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("dev container discovery failed".into());
    }
    for container_id in String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let mut remove = aionui_runtime::Builder::clean_cli("docker");
        remove.args(["rm", "--force", container_id]);
        let removed = remove.output().await.map_err(|error| error.to_string())?;
        if !removed.status.success() && !String::from_utf8_lossy(&removed.stderr).contains("No such container") {
            return Err("dev container cleanup failed".into());
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct ResourceLeaseCoordinator {
    repo: Arc<dyn IDevelopmentOperationsRepository>,
    instance_id: String,
}

impl ResourceLeaseCoordinator {
    pub fn new(repo: Arc<dyn IDevelopmentOperationsRepository>, instance_id: impl Into<String>) -> Self {
        Self {
            repo,
            instance_id: instance_id.into(),
        }
    }

    pub async fn create(&self, input: ResourceLeaseInput) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        validate_kind(&input.environment_kind, &input.resource_kind)?;
        if input.ttl_ms <= 0 {
            return Err(DevelopmentError::BadRequest(
                "resource lease ttl must be positive".into(),
            ));
        }
        let expected_cleanup_order = cleanup_order(&input.resource_kind);
        if input.cleanup_order != expected_cleanup_order {
            return Err(DevelopmentError::BadRequest(format!(
                "{} resources require cleanup order {expected_cleanup_order}",
                input.resource_kind
            )));
        }
        let now = now_ms();
        let row = ExecutionResourceLeaseRow {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: input.user_id,
            project_id: input.project_id,
            run_id: input.run_id,
            task_id: input.task_id,
            turn_id: input.turn_id,
            gate_id: input.gate_id,
            environment_id: input.environment_id,
            environment_kind: input.environment_kind,
            cleanup_order: expected_cleanup_order,
            resource_kind: input.resource_kind,
            resource_identifier: input.resource_identifier,
            status: "active".into(),
            accepts_work: 1,
            owner_instance_id: self.instance_id.clone(),
            heartbeat_at: now,
            expires_at: now.saturating_add(input.ttl_ms),
            cleanup_status: None,
            cleanup_result: None,
            recovery_decision: None,
            created_at: now,
            updated_at: now,
            terminal_at: None,
        };
        self.repo.upsert_resource_lease(&row).await?;
        info!(
            lease_id = %row.id,
            run_id = %row.run_id,
            resource_kind = %row.resource_kind,
            environment_id = %row.environment_id,
            "execution resource lease created"
        );
        Ok(row)
    }

    pub async fn heartbeat(&self, lease_id: &str, ttl_ms: i64) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        let mut row = self.require_lease(lease_id).await?;
        if row.status != "active" || row.accepts_work == 0 {
            return Err(DevelopmentError::Conflict(
                "resource lease no longer accepts work".into(),
            ));
        }
        let now = now_ms();
        row.heartbeat_at = now;
        row.expires_at = now.saturating_add(ttl_ms.max(1));
        row.updated_at = now;
        self.repo.upsert_resource_lease(&row).await?;
        Ok(row)
    }

    pub async fn complete(
        &self,
        lease_id: &str,
        cleanup_result: &str,
    ) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        let mut row = self.require_lease(lease_id).await?;
        if matches!(row.status.as_str(), "released" | "cleanup_failed") {
            return Ok(row);
        }
        let now = now_ms();
        row.accepts_work = 0;
        row.status = "released".into();
        row.cleanup_status = Some("released".into());
        row.cleanup_result = Some(cleanup_result.into());
        row.updated_at = now;
        row.terminal_at = Some(now);
        self.repo.upsert_resource_lease(&row).await?;
        Ok(row)
    }

    pub async fn cancel_run(
        &self,
        user_id: &str,
        run_id: &str,
        controller: &dyn DevelopmentResourceController,
    ) -> Result<Vec<ExecutionResourceLeaseRow>, DevelopmentError> {
        let mut leases = self.repo.list_resource_leases(user_id, run_id, true).await?;
        leases.retain(|lease| {
            matches!(
                lease.status.as_str(),
                "active" | "stopping" | "orphaned" | "cleanup_failed"
            )
        });
        if leases.is_empty() {
            return Ok(Vec::new());
        }
        let now = now_ms();
        for lease in &mut leases {
            lease.accepts_work = 0;
            lease.status = "stopping".into();
            lease.updated_at = now;
            self.repo.upsert_resource_lease(lease).await?;
        }

        let mut first_error = controller.signal_agent(run_id).await.err();
        for lease in &mut leases {
            let target = CleanupTarget {
                resource_kind: &lease.resource_kind,
                resource_identifier: &lease.resource_identifier,
                environment_id: &lease.environment_id,
            };
            match controller.cleanup(target).await {
                Ok(()) => {
                    lease.status = "released".into();
                    lease.cleanup_status = Some("released".into());
                    lease.cleanup_result = Some("ok".into());
                }
                Err(error) => {
                    warn!(
                        lease_id = %lease.id,
                        run_id = %lease.run_id,
                        resource_kind = %lease.resource_kind,
                        "execution resource cleanup failed"
                    );
                    lease.status = "cleanup_failed".into();
                    lease.cleanup_status = Some("failed".into());
                    lease.cleanup_result = Some("controller_error".into());
                    first_error.get_or_insert(error);
                }
            }
            let finished = now_ms();
            lease.updated_at = finished;
            lease.terminal_at = Some(finished);
            self.repo.upsert_resource_lease(lease).await?;
        }
        info!(
            run_id,
            lease_count = leases.len(),
            "execution resource cleanup completed"
        );
        if let Some(error) = first_error {
            return Err(DevelopmentError::Internal(error));
        }
        Ok(leases)
    }

    pub async fn reconcile_stale(&self, now: i64) -> Result<Vec<ExecutionResourceLeaseRow>, DevelopmentError> {
        let mut rows = self.repo.list_stale_resource_leases(now).await?;
        for row in &mut rows {
            row.status = "orphaned".into();
            row.accepts_work = 0;
            row.updated_at = now;
            self.repo.upsert_resource_lease(row).await?;
            warn!(
                lease_id = %row.id,
                run_id = %row.run_id,
                resource_kind = %row.resource_kind,
                environment_id = %row.environment_id,
                "stale execution resource lease requires reconciliation"
            );
        }
        Ok(rows)
    }

    pub async fn record_recovery_decision(
        &self,
        lease_id: &str,
        decision: &str,
    ) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        if !matches!(decision, "retry" | "rollback" | "takeover" | "terminate") {
            return Err(DevelopmentError::BadRequest("unsupported recovery decision".into()));
        }
        let mut row = self.require_lease(lease_id).await?;
        if let Some(existing) = row.recovery_decision.as_deref() {
            if existing != decision {
                return Err(DevelopmentError::Conflict(format!(
                    "resource lease recovery is already decided as {existing}"
                )));
            }
            return Ok(row);
        }
        row.recovery_decision = Some(decision.into());
        row.updated_at = now_ms();
        if decision == "takeover" {
            row.owner_instance_id = self.instance_id.clone();
        }
        self.repo.upsert_resource_lease(&row).await?;
        self.require_lease(lease_id).await
    }

    async fn require_lease(&self, lease_id: &str) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        self.repo
            .get_resource_lease(lease_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("resource lease {lease_id}")))
    }
}

fn validate_kind(environment_kind: &str, resource_kind: &str) -> Result<(), DevelopmentError> {
    if !matches!(environment_kind, "host" | "docker" | "devcontainer") {
        return Err(DevelopmentError::BadRequest("unsupported execution environment".into()));
    }
    if !matches!(
        resource_kind,
        "process" | "container" | "service" | "port" | "lock" | "workspace"
    ) {
        return Err(DevelopmentError::BadRequest("unsupported resource kind".into()));
    }
    Ok(())
}

fn cleanup_order(resource_kind: &str) -> i64 {
    match resource_kind {
        "process" => 20,
        "service" => 30,
        "container" => 40,
        "port" => 50,
        "lock" => 55,
        "workspace" => 60,
        _ => i64::MAX,
    }
}
