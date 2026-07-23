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

    pub async fn heartbeat(
        &self,
        lease: &mut ExecutionResourceLeaseRow,
        ttl_ms: i64,
    ) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        if lease.owner_instance_id != self.instance_id {
            return Err(DevelopmentError::Conflict(
                "resource lease is owned by another instance".into(),
            ));
        }
        if lease.status != "active" || lease.accepts_work == 0 || lease.terminal_at.is_some() {
            return Err(DevelopmentError::Conflict(
                "resource lease no longer accepts work".into(),
            ));
        }
        let now = now_ms();
        let updated = self
            .repo
            .heartbeat_resource_lease(
                &lease.id,
                &lease.owner_instance_id,
                lease.updated_at,
                now,
                now.saturating_add(ttl_ms.max(1)),
            )
            .await?;
        let Some(current) = updated else {
            return Err(DevelopmentError::Conflict(
                "resource lease ownership or status changed during heartbeat".into(),
            ));
        };
        *lease = current.clone();
        Ok(current)
    }

    pub async fn complete(
        &self,
        lease: &ExecutionResourceLeaseRow,
        cleanup_result: &str,
    ) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        if matches!(lease.status.as_str(), "released" | "cleanup_failed") {
            return Ok(lease.clone());
        }
        if lease.owner_instance_id != self.instance_id {
            return Err(DevelopmentError::Conflict(
                "resource lease is owned by another instance".into(),
            ));
        }
        if lease.status != "active" || lease.accepts_work != 1 || lease.terminal_at.is_some() {
            return Err(DevelopmentError::Conflict(
                "resource lease no longer accepts normal completion".into(),
            ));
        }
        let completed_at = now_ms();
        let updated = self
            .repo
            .complete_resource_lease(
                &lease.id,
                &lease.owner_instance_id,
                lease.updated_at,
                cleanup_result,
                completed_at,
            )
            .await?;
        if !updated {
            return Err(DevelopmentError::Conflict(
                "resource lease ownership or status changed during completion".into(),
            ));
        }
        self.require_lease(&lease.id).await
    }

    pub async fn cancel_run(
        &self,
        user_id: &str,
        run_id: &str,
        controller: &dyn DevelopmentResourceController,
    ) -> Result<Vec<ExecutionResourceLeaseRow>, DevelopmentError> {
        let candidates = self.repo.list_resource_leases(user_id, run_id, true).await?;
        let mut claimed = Vec::new();
        let mut claim_error = None;
        for lease in candidates {
            let mut current = lease;
            let mut acquired = false;
            for _ in 0..3 {
                if !matches!(current.status.as_str(), "active" | "orphaned" | "cleanup_failed") {
                    break;
                }
                let claimed_at = now_ms();
                let updated = self
                    .repo
                    .claim_resource_cleanup(
                        &current.id,
                        &current.owner_instance_id,
                        &current.status,
                        current.updated_at,
                        &self.instance_id,
                        claimed_at,
                    )
                    .await?;
                if let Some(claimed_lease) = updated {
                    claimed.push(claimed_lease);
                    acquired = true;
                    break;
                }
                current = self.require_lease(&current.id).await?;
            }
            if !acquired
                && matches!(
                    current.status.as_str(),
                    "active" | "stopping" | "orphaned" | "cleanup_failed"
                )
            {
                claim_error.get_or_insert_with(|| {
                    format!(
                        "resource lease {} could not be fenced for cleanup from status {}",
                        current.id, current.status
                    )
                });
            }
        }
        if claimed.is_empty() {
            if let Some(error) = claim_error {
                return Err(DevelopmentError::Conflict(error));
            }
            return Ok(Vec::new());
        }

        let mut first_error = claim_error;
        if let Err(error) = controller.signal_agent(run_id).await {
            first_error.get_or_insert(error);
        }
        let mut results = Vec::with_capacity(claimed.len());
        for lease in claimed {
            let target = CleanupTarget {
                resource_kind: &lease.resource_kind,
                resource_identifier: &lease.resource_identifier,
                environment_id: &lease.environment_id,
            };
            let cleanup = controller.cleanup(target).await;
            let (succeeded, cleanup_result) = match cleanup {
                Ok(()) => (true, "ok"),
                Err(error) => {
                    warn!(
                        lease_id = %lease.id,
                        run_id = %lease.run_id,
                        resource_kind = %lease.resource_kind,
                        "execution resource cleanup failed"
                    );
                    first_error.get_or_insert(error);
                    (false, "controller_error")
                }
            };
            let finished = now_ms();
            let finalized = self
                .repo
                .finish_resource_cleanup(
                    &lease.id,
                    &self.instance_id,
                    lease.updated_at,
                    succeeded,
                    cleanup_result,
                    finished,
                )
                .await?;
            if !finalized {
                first_error.get_or_insert_with(|| {
                    format!(
                        "resource lease {} changed owner or recovery epoch during cleanup",
                        lease.id
                    )
                });
            }
            results.push(self.require_lease(&lease.id).await?);
        }
        info!(
            run_id,
            lease_count = results.len(),
            "execution resource cleanup completed"
        );
        if let Some(error) = first_error {
            return Err(DevelopmentError::Internal(error));
        }
        Ok(results)
    }

    pub async fn reconcile_stale(&self, now: i64) -> Result<Vec<ExecutionResourceLeaseRow>, DevelopmentError> {
        let candidates = self.repo.list_stale_resource_leases(now).await?;
        self.reconcile_candidates(candidates, now).await
    }

    pub async fn reconcile_stale_for_user(
        &self,
        user_id: &str,
        now: i64,
    ) -> Result<Vec<ExecutionResourceLeaseRow>, DevelopmentError> {
        let candidates = self
            .repo
            .list_stale_resource_leases(now)
            .await?
            .into_iter()
            .filter(|candidate| candidate.user_id == user_id)
            .collect();
        self.reconcile_candidates(candidates, now).await
    }

    async fn reconcile_candidates(
        &self,
        candidates: Vec<ExecutionResourceLeaseRow>,
        now: i64,
    ) -> Result<Vec<ExecutionResourceLeaseRow>, DevelopmentError> {
        let mut rows = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let orphaned = self
                .repo
                .orphan_resource_lease(&candidate.id, &candidate.owner_instance_id, candidate.expires_at, now)
                .await?;
            let Some(row) = orphaned else {
                continue;
            };
            warn!(
                lease_id = %row.id,
                run_id = %row.run_id,
                resource_kind = %row.resource_kind,
                environment_id = %row.environment_id,
                "stale execution resource lease requires reconciliation"
            );
            rows.push(row);
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
        let row = self.require_lease(lease_id).await?;
        if decision == "takeover" {
            if is_active_takeover_owner(&row, &self.instance_id) {
                return Ok(row);
            }
            if row.status != "orphaned" || row.terminal_at.is_some() {
                return Err(DevelopmentError::Conflict(
                    "only a non-terminal orphaned resource lease can be taken over".into(),
                ));
            }
        }
        if let Some(existing) = row.recovery_decision.as_deref() {
            if existing != decision {
                return Err(DevelopmentError::Conflict(format!(
                    "resource lease recovery is already decided as {existing}"
                )));
            }
            if decision != "takeover" {
                return Ok(row);
            }
        }
        let takeover_owner = (decision == "takeover").then_some(self.instance_id.as_str());
        let claimed = self
            .repo
            .claim_resource_recovery_decision(lease_id, decision, takeover_owner, now_ms())
            .await?;
        if let Some(persisted) = claimed {
            return Ok(persisted);
        }
        let persisted = self.require_lease(lease_id).await?;
        let is_idempotent = if decision == "takeover" {
            is_active_takeover_owner(&persisted, &self.instance_id)
        } else {
            persisted.recovery_decision.as_deref() == Some(decision)
        };
        if !is_idempotent {
            return Err(DevelopmentError::Conflict(format!(
                "resource lease recovery is already decided as {} by {}",
                persisted.recovery_decision.as_deref().unwrap_or("unknown"),
                persisted.owner_instance_id
            )));
        }
        Ok(persisted)
    }

    async fn require_lease(&self, lease_id: &str) -> Result<ExecutionResourceLeaseRow, DevelopmentError> {
        self.repo
            .get_resource_lease(lease_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("resource lease {lease_id}")))
    }
}

fn is_active_takeover_owner(row: &ExecutionResourceLeaseRow, instance_id: &str) -> bool {
    row.recovery_decision.as_deref() == Some("takeover")
        && row.owner_instance_id == instance_id
        && row.status == "active"
        && row.accepts_work == 1
        && row.terminal_at.is_none()
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
