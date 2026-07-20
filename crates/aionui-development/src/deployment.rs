use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::DevelopmentDeploymentRow;
use aionui_db::{IDevelopmentRepository, IProjectRepository};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::DevelopmentError;
use crate::operations::{DevelopmentOperationsService, redact_sensitive};
use crate::policy::{PolicyDecision, PolicyOperation};

const DEPLOYMENT_APPROVAL_TTL_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentRequestInput {
    pub environment: String,
    pub deployment_key: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentExecution {
    pub deployment_id: String,
    pub deployment_key: String,
    pub run_id: String,
    pub project_id: String,
    pub environment: String,
    pub commit_sha: String,
}

#[async_trait]
pub trait DeploymentProvider: Send + Sync {
    async fn deploy(&self, execution: &DeploymentExecution) -> Result<Option<String>, String>;
    async fn cancel(&self, remote_id: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Default)]
pub struct UnconfiguredDeploymentProvider;

#[async_trait]
impl DeploymentProvider for UnconfiguredDeploymentProvider {
    async fn deploy(&self, _execution: &DeploymentExecution) -> Result<Option<String>, String> {
        Err("deployment provider is not configured for this installation".into())
    }

    async fn cancel(&self, _remote_id: &str) -> Result<(), String> {
        Err("deployment provider is not configured for this installation".into())
    }
}

#[derive(Clone)]
pub struct DeploymentService {
    development_repo: Arc<dyn IDevelopmentRepository>,
    project_repo: Arc<dyn IProjectRepository>,
    provider: Arc<dyn DeploymentProvider>,
    operations: Option<Arc<DevelopmentOperationsService>>,
}

impl DeploymentService {
    pub fn new(
        development_repo: Arc<dyn IDevelopmentRepository>,
        project_repo: Arc<dyn IProjectRepository>,
        provider: Arc<dyn DeploymentProvider>,
    ) -> Self {
        Self {
            development_repo,
            project_repo,
            provider,
            operations: None,
        }
    }

    pub fn with_operations(mut self, operations: Arc<DevelopmentOperationsService>) -> Self {
        self.operations = Some(operations);
        self
    }

    pub async fn request(
        &self,
        user_id: &str,
        run_id: &str,
        input: DeploymentRequestInput,
    ) -> Result<DevelopmentDeploymentRow, DevelopmentError> {
        validate_identifier("environment", &input.environment)?;
        validate_identifier("deployment key", &input.deployment_key)?;
        validate_commit(&input.commit_sha)?;

        if let Some(existing) = self
            .development_repo
            .get_deployment_by_key(user_id, &input.deployment_key)
            .await?
        {
            if existing.run_id != run_id
                || existing.environment != input.environment
                || existing.commit_sha != input.commit_sha
            {
                return Err(DevelopmentError::Conflict(
                    "deployment idempotency key belongs to a different immutable request".into(),
                ));
            }
            return Ok(existing);
        }

        let run = self
            .development_repo
            .get_run(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("run {run_id}")))?;
        self.project_repo
            .get_for_user(&run.project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("project {}", run.project_id)))?;
        let delivery = self
            .development_repo
            .get_delivery(user_id, run_id)
            .await?
            .ok_or_else(|| DevelopmentError::Conflict("run has no delivery record".into()))?;
        if delivery.status != "merged"
            || delivery.merge_status != "merged"
            || delivery.ci_status != "passed"
            || delivery.review_status != "approved"
        {
            return Err(DevelopmentError::Conflict(
                "deployment requires a merged, reviewed delivery with passing CI".into(),
            ));
        }
        if delivery.commit_sha.as_deref() != Some(input.commit_sha.as_str()) {
            return Err(DevelopmentError::Conflict(
                "deployment commit does not match the immutable delivery commit".into(),
            ));
        }

        let now = now_ms();
        let row = DevelopmentDeploymentRow {
            id: uuid::Uuid::now_v7().to_string(),
            deployment_key: input.deployment_key.clone(),
            run_id: run_id.into(),
            project_id: run.project_id,
            user_id: user_id.into(),
            environment: input.environment.clone(),
            commit_sha: input.commit_sha.clone(),
            status: "pending_approval".into(),
            requested_by: user_id.into(),
            approved_by: None,
            approval_run_id: run_id.into(),
            approval_environment: input.environment,
            approval_commit_sha: input.commit_sha,
            approval_requester: user_id.into(),
            approval_deployment_key: input.deployment_key,
            approval_expires_at: now + DEPLOYMENT_APPROVAL_TTL_MS,
            approved_at: None,
            remote_id: None,
            attempt_count: 0,
            last_error: None,
            started_at: None,
            finished_at: None,
            created_at: now,
            updated_at: now,
        };
        if !self.development_repo.create_deployment(&row).await? {
            return self
                .development_repo
                .get_deployment_by_key(user_id, &row.deployment_key)
                .await?
                .ok_or_else(|| DevelopmentError::Conflict("deployment request is already being created".into()));
        }
        self.audit(&row, "deployment.request", "success", json!({"status": row.status}))
            .await?;
        Ok(row)
    }

    pub async fn get(&self, user_id: &str, deployment_id: &str) -> Result<DevelopmentDeploymentRow, DevelopmentError> {
        self.development_repo
            .get_deployment(user_id, deployment_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("deployment {deployment_id}")))
    }

    pub async fn list(&self, user_id: &str, run_id: &str) -> Result<Vec<DevelopmentDeploymentRow>, DevelopmentError> {
        self.development_repo
            .get_run(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("run {run_id}")))?;
        Ok(self.development_repo.list_deployments(user_id, run_id).await?)
    }

    pub async fn approve(
        &self,
        user_id: &str,
        run_id: &str,
        deployment_id: &str,
        confirmation_count: u8,
    ) -> Result<DevelopmentDeploymentRow, DevelopmentError> {
        if confirmation_count < 2 {
            return Err(DevelopmentError::BadRequest(
                "deployment approval requires two explicit human confirmations".into(),
            ));
        }
        let row = self.get(user_id, deployment_id).await?;
        require_run_scope(&row, run_id)?;
        if let Some(operations) = &self.operations {
            let decision = operations
                .evaluate_policy(
                    user_id,
                    &row.project_id,
                    &row.run_id,
                    &PolicyOperation::Deploy {
                        target: row.environment.clone(),
                    },
                    confirmation_count,
                )
                .await?;
            if decision != PolicyDecision::Allowed {
                return Err(DevelopmentError::BadRequest(
                    "deployment approval does not satisfy the Project policy".into(),
                ));
            }
        }
        if row.status == "approved" {
            return Ok(row);
        }
        if row.status != "pending_approval" || row.approval_expires_at <= now_ms() {
            return Err(DevelopmentError::Conflict(
                "deployment approval is no longer pending or has expired".into(),
            ));
        }
        if !self
            .development_repo
            .approve_deployment(user_id, deployment_id, now_ms())
            .await?
        {
            return Err(DevelopmentError::Conflict(
                "deployment approval was not consumed".into(),
            ));
        }
        let approved = self.get(user_id, deployment_id).await?;
        self.audit(
            &approved,
            "deployment.approve",
            "success",
            json!({"confirmation_count": confirmation_count, "expires_at": approved.approval_expires_at}),
        )
        .await?;
        Ok(approved)
    }

    pub async fn execute(
        &self,
        user_id: &str,
        run_id: &str,
        deployment_id: &str,
    ) -> Result<DevelopmentDeploymentRow, DevelopmentError> {
        let row = self.get(user_id, deployment_id).await?;
        require_run_scope(&row, run_id)?;
        if matches!(row.status.as_str(), "running" | "succeeded") {
            return Ok(row);
        }
        validate_approval_binding(&row, user_id)?;
        if let Some(operations) = &self.operations {
            operations
                .require_budget(user_id, &row.run_id, "deployment.execute", row.attempt_count)
                .await?;
        }
        let now = now_ms();
        if !self
            .development_repo
            .claim_deployment(user_id, deployment_id, now)
            .await?
        {
            let current = self.get(user_id, deployment_id).await?;
            if matches!(current.status.as_str(), "running" | "succeeded") {
                return Ok(current);
            }
            return Err(DevelopmentError::Conflict("deployment could not be claimed".into()));
        }

        let execution = DeploymentExecution {
            deployment_id: row.id.clone(),
            deployment_key: row.deployment_key.clone(),
            run_id: row.run_id.clone(),
            project_id: row.project_id.clone(),
            environment: row.environment.clone(),
            commit_sha: row.commit_sha.clone(),
        };
        match self.provider.deploy(&execution).await {
            Ok(remote_id) => {
                self.development_repo
                    .update_deployment_result(
                        user_id,
                        deployment_id,
                        "succeeded",
                        remote_id.as_deref(),
                        None,
                        now_ms(),
                    )
                    .await?;
                let completed = self.get(user_id, deployment_id).await?;
                self.audit(
                    &completed,
                    "deployment.execute",
                    "success",
                    json!({"status": completed.status, "attempt_count": completed.attempt_count}),
                )
                .await?;
            }
            Err(error) => {
                let error = redact_sensitive(&error, &[]);
                self.development_repo
                    .update_deployment_result(user_id, deployment_id, "failed", None, Some(&error), now_ms())
                    .await?;
                let failed = self.get(user_id, deployment_id).await?;
                self.audit(
                    &failed,
                    "deployment.execute",
                    "failed",
                    json!({"status": failed.status, "error": error}),
                )
                .await?;
                return Err(DevelopmentError::Internal(error));
            }
        }
        let cancelled = self.get(user_id, deployment_id).await?;
        self.audit(
            &cancelled,
            "deployment.cancel",
            "success",
            json!({"status": cancelled.status}),
        )
        .await?;
        Ok(cancelled)
    }

    pub async fn cancel(
        &self,
        user_id: &str,
        run_id: &str,
        deployment_id: &str,
        confirmed: bool,
    ) -> Result<DevelopmentDeploymentRow, DevelopmentError> {
        if !confirmed {
            return Err(DevelopmentError::BadRequest(
                "deployment cancellation requires confirmation".into(),
            ));
        }
        let row = self.get(user_id, deployment_id).await?;
        require_run_scope(&row, run_id)?;
        if row.status == "cancelled" {
            return Ok(row);
        }
        if matches!(row.status.as_str(), "succeeded" | "failed") {
            return Err(DevelopmentError::Conflict(
                "completed deployment cannot be cancelled".into(),
            ));
        }
        if let Some(remote_id) = row.remote_id.as_deref() {
            self.provider
                .cancel(remote_id)
                .await
                .map_err(|error| DevelopmentError::Internal(redact_sensitive(&error, &[])))?;
        }
        self.development_repo
            .update_deployment_result(user_id, deployment_id, "cancelled", None, None, now_ms())
            .await?;
        self.get(user_id, deployment_id).await
    }

    pub async fn recover_stale(
        &self,
        user_id: &str,
        run_id: &str,
        stale_after_ms: i64,
    ) -> Result<Vec<DevelopmentDeploymentRow>, DevelopmentError> {
        let now = now_ms();
        let rows = self.list(user_id, run_id).await?;
        for row in rows
            .iter()
            .filter(|row| row.status == "running" && row.updated_at.saturating_add(stale_after_ms.max(0)) <= now)
        {
            self.development_repo
                .update_deployment_result(
                    user_id,
                    &row.id,
                    "unknown_remote_state",
                    None,
                    Some("process restarted while deployment was running"),
                    now,
                )
                .await?;
            let recovered = self.get(user_id, &row.id).await?;
            self.audit(
                &recovered,
                "deployment.recover",
                "unknown_remote_state",
                json!({"status": recovered.status}),
            )
            .await?;
        }
        self.list(user_id, run_id).await
    }

    async fn audit(
        &self,
        row: &DevelopmentDeploymentRow,
        action: &str,
        result: &str,
        payload: Value,
    ) -> Result<(), DevelopmentError> {
        if let Some(operations) = &self.operations {
            operations
                .audit(
                    &row.user_id,
                    "user",
                    &row.user_id,
                    action,
                    "deployment",
                    &row.id,
                    &row.project_id,
                    Some(&row.run_id),
                    None,
                    result,
                    payload,
                    &[],
                )
                .await?;
        }
        Ok(())
    }
}

fn validate_approval_binding(row: &DevelopmentDeploymentRow, user_id: &str) -> Result<(), DevelopmentError> {
    let valid = row.status == "approved"
        && row.approved_by.as_deref() == Some(user_id)
        && row.approved_at.is_some()
        && row.approval_expires_at > now_ms()
        && row.approval_run_id == row.run_id
        && row.approval_environment == row.environment
        && row.approval_commit_sha == row.commit_sha
        && row.approval_requester == row.requested_by
        && row.approval_requester == user_id
        && row.approval_deployment_key == row.deployment_key;
    if !valid {
        return Err(DevelopmentError::Conflict(
            "deployment approval is missing, expired, or does not match the immutable request".into(),
        ));
    }
    Ok(())
}

fn require_run_scope(row: &DevelopmentDeploymentRow, run_id: &str) -> Result<(), DevelopmentError> {
    if row.run_id != run_id {
        return Err(DevelopmentError::NotFound(format!(
            "deployment {} for run {run_id}",
            row.id
        )));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> Result<(), DevelopmentError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/'));
    if !valid {
        return Err(DevelopmentError::BadRequest(format!("invalid {label}")));
    }
    Ok(())
}

fn validate_commit(value: &str) -> Result<(), DevelopmentError> {
    if value.len() < 7 || value.len() > 64 || !value.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(DevelopmentError::BadRequest("invalid deployment commit SHA".into()));
    }
    Ok(())
}
