use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::{
    DevelopmentCiCheckRow, DevelopmentDeliveryRow, DevelopmentRunRow, DevelopmentTaskRow, ProjectRow, TaskArtifactRow,
};
use aionui_db::{IDevelopmentRepository, IProjectRepository};
use async_trait::async_trait;
use git2::{IndexAddOption, Repository, Signature, build::CheckoutBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::DevelopmentError;
use crate::operations::{DevelopmentOperationsService, redact_sensitive};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderPullRequest {
    pub number: i64,
    pub url: String,
    pub status: String,
    pub review_status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCiCheck {
    pub id: String,
    pub name: String,
    pub status: String,
    pub details_url: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryProviderSnapshot {
    pub pull_request: ProviderPullRequest,
    pub checks: Vec<ProviderCiCheck>,
}

#[async_trait]
pub trait DeliveryProvider: Send + Sync {
    async fn preflight(&self, repository: &Path) -> Result<(), String>;
    async fn push(&self, repository: &Path, branch: &str) -> Result<(), String>;
    async fn ensure_pull_request(
        &self,
        repository: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ProviderPullRequest, String>;
    async fn synchronize(&self, repository: &Path, number: i64) -> Result<DeliveryProviderSnapshot, String>;
    async fn merge(&self, repository: &Path, number: i64) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareDeliveryInput {
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePullRequestInput {
    pub title: Option<String>,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Clone)]
pub struct DeliveryService {
    development_repo: Arc<dyn IDevelopmentRepository>,
    project_repo: Arc<dyn IProjectRepository>,
    provider: Arc<dyn DeliveryProvider>,
    operations: Option<Arc<DevelopmentOperationsService>>,
}

impl DeliveryService {
    pub fn new(
        development_repo: Arc<dyn IDevelopmentRepository>,
        project_repo: Arc<dyn IProjectRepository>,
        provider: Arc<dyn DeliveryProvider>,
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

    pub async fn get(&self, user_id: &str, run_id: &str) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        self.development_repo
            .get_delivery(user_id, run_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("delivery for run {run_id}")))
    }

    pub async fn prepare(
        &self,
        user_id: &str,
        run_id: &str,
        input: PrepareDeliveryInput,
    ) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        if let Some(delivery) = self.development_repo.get_delivery(user_id, run_id).await?
            && (delivery.commit_sha.is_some() || delivery.status == "no_change")
        {
            return Ok(delivery);
        }

        let run = self.get_run(user_id, run_id).await?;
        self.require_budget(user_id, run_id, "delivery.prepare").await?;
        let project = self.get_project(user_id, &run).await?;
        let reasons = self.delivery_blockers(&run).await?;
        if !reasons.is_empty() {
            return Err(DevelopmentError::Conflict(reasons.join("; ")));
        }

        let base_branch = project.default_branch.clone().unwrap_or_else(|| "main".into());
        let repository_path = PathBuf::from(&project.local_path);
        let message = input
            .message
            .as_deref()
            .map(clean_commit_message)
            .transpose()?
            .unwrap_or_else(|| format!("chore: deliver {}", clean_summary(&run.request_summary)));
        let prepared = prepare_repository(
            &repository_path,
            run_id,
            run.integration_branch.as_deref(),
            &base_branch,
            &message,
        )?;

        let artifacts = self.development_repo.list_artifacts(run_id, None).await?;
        if prepared.commit_sha.is_none()
            && !artifacts
                .iter()
                .any(|artifact| artifact.artifact_type == "no_code_change")
        {
            return Err(DevelopmentError::Conflict(
                "repository has no candidate change and no reviewer-approved no-code artifact".into(),
            ));
        }
        if let Some(commit_sha) = prepared.commit_sha.as_deref()
            && !artifacts
                .iter()
                .any(|artifact| artifact.artifact_type == "commit" && artifact.path_or_uri == commit_sha)
        {
            self.development_repo
                .create_artifact(&TaskArtifactRow {
                    id: uuid::Uuid::now_v7().to_string(),
                    run_id: run_id.into(),
                    task_id: None,
                    artifact_type: "commit".into(),
                    path_or_uri: commit_sha.into(),
                    checksum: commit_sha.into(),
                    producer_agent_id: Some("aion-delivery".into()),
                    metadata: Some(json!({"branch": prepared.branch, "base_branch": base_branch}).to_string()),
                    created_at: now_ms(),
                })
                .await?;
        }

        let now = now_ms();
        let delivery = DevelopmentDeliveryRow {
            id: uuid::Uuid::now_v7().to_string(),
            run_id: run_id.into(),
            project_id: run.project_id.clone(),
            user_id: user_id.into(),
            provider: "github".into(),
            repository: project.repository_url.clone(),
            branch: prepared.branch,
            base_branch,
            commit_sha: prepared.commit_sha,
            status: if prepared.had_change { "prepared" } else { "no_change" }.into(),
            push_status: "pending".into(),
            pr_number: None,
            pr_url: None,
            pr_status: "not_created".into(),
            ci_status: "not_started".into(),
            review_status: "pending".into(),
            merge_status: "blocked".into(),
            report_json: "{}".into(),
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        self.persist_with_report(delivery).await
    }

    pub async fn push(
        &self,
        user_id: &str,
        run_id: &str,
        confirmed: bool,
    ) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        require_confirmation(confirmed, "push")?;
        let mut delivery = self.get(user_id, run_id).await?;
        if delivery.push_status == "pushed" {
            return Ok(delivery);
        }
        if delivery.commit_sha.is_none() {
            return Err(DevelopmentError::Conflict(
                "delivery has no candidate commit to push".into(),
            ));
        }
        self.require_budget(user_id, run_id, "delivery.push").await?;
        validate_delivery_branch(&delivery.branch, &delivery.base_branch)?;
        let path = self.repository_path(user_id, run_id).await?;
        if let Err(error) = self.provider.preflight(&path).await {
            return self.fail_provider_operation(delivery, error).await;
        }
        if let Err(error) = self.provider.push(&path, &delivery.branch).await {
            return self.fail_provider_operation(delivery, error).await;
        }
        delivery.push_status = "pushed".into();
        delivery.status = "pushed".into();
        delivery.last_error = None;
        delivery.updated_at = now_ms();
        self.persist_with_report(delivery).await
    }

    pub async fn create_pull_request(
        &self,
        user_id: &str,
        run_id: &str,
        input: CreatePullRequestInput,
    ) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        require_confirmation(input.confirmed, "pull request creation")?;
        let mut delivery = self.get(user_id, run_id).await?;
        if delivery.pr_number.is_some() {
            return Ok(delivery);
        }
        if delivery.push_status != "pushed" {
            return Err(DevelopmentError::Conflict(
                "delivery branch must be pushed first".into(),
            ));
        }
        self.require_budget(user_id, run_id, "delivery.pull_request").await?;
        let run = self.get_run(user_id, run_id).await?;
        let path = self.repository_path(user_id, run_id).await?;
        let title = input
            .title
            .as_deref()
            .map(clean_title)
            .transpose()?
            .unwrap_or_else(|| clean_summary(&run.request_summary));
        let body = self.report_value(user_id, run_id, &delivery).await?.to_string();
        let pull_request = match self
            .provider
            .ensure_pull_request(&path, &delivery.branch, &delivery.base_branch, &title, &body)
            .await
        {
            Ok(pull_request) => pull_request,
            Err(error) => return self.fail_provider_operation(delivery, error).await,
        };
        delivery.pr_number = Some(pull_request.number);
        delivery.pr_url = Some(pull_request.url);
        delivery.pr_status = pull_request.status;
        delivery.review_status = pull_request.review_status;
        delivery.status = "pr_open".into();
        delivery.last_error = None;
        delivery.updated_at = now_ms();
        self.persist_with_report(delivery).await
    }

    pub async fn sync(&self, user_id: &str, run_id: &str) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        self.require_budget(user_id, run_id, "delivery.sync").await?;
        let mut delivery = self.get(user_id, run_id).await?;
        let number = delivery
            .pr_number
            .ok_or_else(|| DevelopmentError::Conflict("pull request has not been created".into()))?;
        let run = self.get_run(user_id, run_id).await?;
        let path = self.repository_path(user_id, run_id).await?;
        let snapshot = match self.provider.synchronize(&path, number).await {
            Ok(snapshot) => snapshot,
            Err(error) => return self.fail_provider_operation(delivery, error).await,
        };

        let existing = self
            .development_repo
            .list_ci_checks(&delivery.id)
            .await?
            .into_iter()
            .map(|check| (check.provider_check_id.clone(), check))
            .collect::<HashMap<_, _>>();
        for check in &snapshot.checks {
            let previous = existing.get(&check.id);
            let rework_task_id = if check.status == "failed" {
                Some(
                    self.ensure_rework_task(
                        &run,
                        &delivery,
                        check,
                        previous.and_then(|row| row.rework_task_id.as_deref()),
                    )
                    .await?,
                )
            } else {
                previous.and_then(|row| row.rework_task_id.clone())
            };
            let now = now_ms();
            self.development_repo
                .upsert_ci_check(&DevelopmentCiCheckRow {
                    id: previous
                        .map(|row| row.id.clone())
                        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
                    delivery_id: delivery.id.clone(),
                    provider_check_id: check.id.clone(),
                    name: check.name.clone(),
                    status: normalize_check_status(&check.status),
                    details_url: check.details_url.clone(),
                    summary: check.summary.clone(),
                    rework_task_id,
                    started_at: previous.and_then(|row| row.started_at).or(Some(now)),
                    completed_at: matches!(check.status.as_str(), "passed" | "failed" | "cancelled" | "skipped")
                        .then_some(now),
                    created_at: previous.map(|row| row.created_at).unwrap_or(now),
                    updated_at: now,
                })
                .await?;
        }

        delivery.pr_number = Some(snapshot.pull_request.number);
        delivery.pr_url = Some(snapshot.pull_request.url);
        delivery.pr_status = snapshot.pull_request.status;
        delivery.review_status = snapshot.pull_request.review_status;
        delivery.ci_status = aggregate_ci_status(&snapshot.checks).into();
        let blockers = self.delivery_blockers(&run).await?;
        if delivery.ci_status == "failed" {
            delivery.status = "rework_required".into();
            delivery.merge_status = "blocked".into();
            self.development_repo
                .update_run_status(run_id, user_id, "rework", None)
                .await?;
        } else if delivery.ci_status == "passed" && delivery.review_status == "approved" && blockers.is_empty() {
            delivery.status = "merge_ready".into();
            delivery.merge_status = "ready".into();
        } else {
            delivery.status = "ci_pending".into();
            delivery.merge_status = "blocked".into();
        }
        delivery.last_error = None;
        delivery.updated_at = now_ms();
        self.persist_with_report(delivery).await
    }

    pub async fn merge(
        &self,
        user_id: &str,
        run_id: &str,
        confirmed: bool,
    ) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        require_confirmation(confirmed, "merge")?;
        let mut delivery = self.get(user_id, run_id).await?;
        if delivery.status == "merged" || delivery.merge_status == "merged" {
            return Ok(delivery);
        }
        let run = self.get_run(user_id, run_id).await?;
        let mut blockers = self.delivery_blockers(&run).await?;
        if delivery.ci_status != "passed" {
            blockers.push("CI checks have not passed".into());
        }
        if delivery.review_status != "approved" {
            blockers.push("pull request review has not been approved".into());
        }
        if delivery.merge_status != "ready" {
            blockers.push("delivery has not reached merge-ready state".into());
        }
        if !blockers.is_empty() {
            return Err(DevelopmentError::Conflict(blockers.join("; ")));
        }
        self.require_budget(user_id, run_id, "delivery.merge").await?;
        let number = delivery
            .pr_number
            .ok_or_else(|| DevelopmentError::Conflict("pull request has not been created".into()))?;
        let path = self.repository_path(user_id, run_id).await?;
        if let Err(error) = self.provider.merge(&path, number).await {
            return self.fail_provider_operation(delivery, error).await;
        }
        delivery.status = "merged".into();
        delivery.merge_status = "merged".into();
        delivery.pr_status = "merged".into();
        delivery.last_error = None;
        delivery.updated_at = now_ms();
        self.development_repo
            .update_run_status(run_id, user_id, "succeeded", Some(now_ms()))
            .await?;
        self.persist_with_report(delivery).await
    }

    pub async fn report(&self, user_id: &str, run_id: &str) -> Result<Value, DevelopmentError> {
        let delivery = self.get(user_id, run_id).await?;
        self.report_value(user_id, run_id, &delivery).await
    }

    async fn require_budget(&self, user_id: &str, run_id: &str, operation: &str) -> Result<(), DevelopmentError> {
        if let Some(operations) = &self.operations {
            operations.require_budget(user_id, run_id, operation, 0).await?;
        }
        Ok(())
    }

    async fn get_run(&self, user_id: &str, run_id: &str) -> Result<DevelopmentRunRow, DevelopmentError> {
        self.development_repo
            .get_run(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("run {run_id}")))
    }

    async fn get_project(&self, user_id: &str, run: &DevelopmentRunRow) -> Result<ProjectRow, DevelopmentError> {
        self.project_repo
            .get_for_user(&run.project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("project {}", run.project_id)))
    }

    async fn repository_path(&self, user_id: &str, run_id: &str) -> Result<PathBuf, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        Ok(PathBuf::from(self.get_project(user_id, &run).await?.local_path))
    }

    async fn delivery_blockers(&self, run: &DevelopmentRunRow) -> Result<Vec<String>, DevelopmentError> {
        let tasks = self.development_repo.list_tasks(&run.id).await?;
        let gates = self.development_repo.list_gates(&run.id, None).await?;
        let mut blockers = Vec::new();
        if tasks.is_empty() {
            blockers.push("delivery requires at least one completed task".into());
        }
        if tasks
            .iter()
            .any(|task| task.status != "completed" && task.status != "cancelled")
        {
            blockers.push("one or more development tasks are incomplete".into());
        }
        if tasks.iter().filter(|task| task.status != "cancelled").any(|task| {
            !matches!(task.review_status.as_str(), "approved" | "not_required") || task.verification_status != "passed"
        }) {
            blockers.push("one or more development tasks lack review or verification approval".into());
        }
        let mut latest_required = HashMap::new();
        for gate in gates.into_iter().filter(|gate| gate.required) {
            latest_required.insert((gate.task_id.clone(), gate.gate_type.clone()), gate);
        }
        if latest_required.is_empty() {
            blockers.push("no required quality gate is recorded".into());
        } else if latest_required.values().any(|gate| gate.status != "passed") {
            blockers.push("one or more latest required quality gates have not passed".into());
        }
        for task in &tasks {
            if self
                .development_repo
                .list_findings(&run.id, &task.id)
                .await?
                .iter()
                .any(|finding| finding.status == "open" && matches!(finding.severity.as_str(), "critical" | "blocker"))
            {
                blockers.push("critical or blocker review findings remain open".into());
                break;
            }
        }
        Ok(blockers)
    }

    async fn ensure_rework_task(
        &self,
        run: &DevelopmentRunRow,
        delivery: &DevelopmentDeliveryRow,
        check: &ProviderCiCheck,
        existing_task_id: Option<&str>,
    ) -> Result<String, DevelopmentError> {
        let task_id = existing_task_id
            .map(str::to_owned)
            .unwrap_or_else(|| deterministic_rework_id(&delivery.id, &check.id));
        if let Some(existing) = self.development_repo.get_task(&run.id, &task_id).await? {
            if existing.status == "completed" {
                self.development_repo
                    .update_task_state(&run.id, &task_id, "ready", "pending", "pending")
                    .await?;
            }
            return Ok(task_id);
        }
        let now = now_ms();
        self.development_repo
            .create_task(&DevelopmentTaskRow {
                id: task_id.clone(),
                team_id: run.team_id.clone().unwrap_or_else(|| format!("run:{}", run.id)),
                run_id: Some(run.id.clone()),
                subject: format!("Fix CI check: {}", check.name),
                description: check.summary.clone(),
                status: "ready".into(),
                owner: None,
                blocked_by: "[]".into(),
                blocks: "[]".into(),
                metadata: Some(
                    json!({
                        "delivery_id": delivery.id,
                        "provider_check_id": check.id,
                        "details_url": check.details_url,
                        "summary": check.summary,
                    })
                    .to_string(),
                ),
                acceptance_criteria: json!([format!("CI check '{}' passes", check.name)]).to_string(),
                task_type: "rework".into(),
                risk_level: "high".into(),
                assigned_workspace_lease_id: None,
                review_status: "pending".into(),
                verification_status: "pending".into(),
                created_at: now,
                updated_at: now,
            })
            .await?;
        Ok(task_id)
    }

    async fn persist_with_report(
        &self,
        mut delivery: DevelopmentDeliveryRow,
    ) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        delivery.report_json = self
            .report_value(&delivery.user_id, &delivery.run_id, &delivery)
            .await?
            .to_string();
        self.development_repo.upsert_delivery(&delivery).await?;
        Ok(delivery)
    }

    async fn report_value(
        &self,
        user_id: &str,
        run_id: &str,
        delivery: &DevelopmentDeliveryRow,
    ) -> Result<Value, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        let tasks = self.development_repo.list_tasks(run_id).await?;
        let artifacts = self.development_repo.list_artifacts(run_id, None).await?;
        let gates = self.development_repo.list_gates(run_id, None).await?;
        let checks = self.development_repo.list_ci_checks(&delivery.id).await?;
        let mut findings = Vec::new();
        for task in &tasks {
            findings.extend(self.development_repo.list_findings(run_id, &task.id).await?);
        }
        let unresolved_risks = self.delivery_blockers(&run).await?;
        let acceptance_criteria: Value = serde_json::from_str(&run.acceptance_criteria).unwrap_or_else(|_| json!([]));
        Ok(json!({
            "schema_version": 1,
            "run": {
                "id": run.id,
                "project_id": run.project_id,
                "request_summary": run.request_summary,
                "acceptance_criteria": acceptance_criteria,
                "execution_mode": run.execution_mode,
                "status": run.status,
            },
            "candidate": {
                "provider": delivery.provider,
                "repository": delivery.repository,
                "branch": delivery.branch,
                "base_branch": delivery.base_branch,
                "commit_sha": delivery.commit_sha,
            },
            "pull_request": {
                "number": delivery.pr_number,
                "url": delivery.pr_url,
                "status": delivery.pr_status,
                "review_status": delivery.review_status,
            },
            "delivery": {
                "status": delivery.status,
                "push_status": delivery.push_status,
                "ci_status": delivery.ci_status,
                "merge_status": delivery.merge_status,
                "last_error": delivery.last_error,
            },
            "tasks": tasks,
            "artifacts": artifacts,
            "quality_gates": gates,
            "review_findings": findings,
            "ci_checks": checks,
            "unresolved_risks": unresolved_risks,
        }))
    }

    async fn fail_provider_operation(
        &self,
        mut delivery: DevelopmentDeliveryRow,
        error: String,
    ) -> Result<DevelopmentDeliveryRow, DevelopmentError> {
        let redacted = redact_secret(&error);
        delivery.status = "failed".into();
        delivery.last_error = Some(redacted.clone());
        delivery.updated_at = now_ms();
        self.persist_with_report(delivery).await?;
        Err(DevelopmentError::Internal(format!(
            "delivery provider rejected the operation: {redacted}"
        )))
    }
}

struct PreparedGit {
    branch: String,
    commit_sha: Option<String>,
    had_change: bool,
}

fn prepare_repository(
    path: &Path,
    run_id: &str,
    integration_branch: Option<&str>,
    base_branch: &str,
    message: &str,
) -> Result<PreparedGit, DevelopmentError> {
    let repository = Repository::open(path)
        .map_err(|error| DevelopmentError::BadRequest(format!("project is not a Git repository: {error}")))?;
    if let Some(branch) = integration_branch {
        validate_delivery_branch(branch, base_branch)?;
        let reference = repository
            .find_branch(branch, git2::BranchType::Local)
            .map_err(|_| DevelopmentError::Conflict(format!("integration branch {branch} does not exist")))?;
        let commit_sha = reference
            .get()
            .target()
            .ok_or_else(|| DevelopmentError::Conflict("integration branch has no commit".into()))?
            .to_string();
        return Ok(PreparedGit {
            branch: branch.into(),
            commit_sha: Some(commit_sha),
            had_change: true,
        });
    }

    let target_branch = format!("aion/run/{}/delivery", sanitize_branch_component(run_id));
    validate_delivery_branch(&target_branch, base_branch)?;
    let current_branch = repository
        .head()
        .ok()
        .and_then(|head| head.shorthand().map(str::to_owned))
        .ok_or_else(|| DevelopmentError::Conflict("delivery requires a named local branch".into()))?;
    if current_branch == base_branch {
        if repository.find_branch(&target_branch, git2::BranchType::Local).is_err() {
            let head = repository
                .head()
                .and_then(|reference| reference.peel_to_commit())
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
            repository
                .branch(&target_branch, &head, false)
                .map_err(|error| DevelopmentError::Conflict(format!("cannot create delivery branch: {error}")))?;
        }
        repository
            .set_head(&format!("refs/heads/{target_branch}"))
            .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
        repository
            .checkout_head(Some(CheckoutBuilder::new().safe()))
            .map_err(|error| DevelopmentError::Conflict(format!("cannot safely switch to delivery branch: {error}")))?;
    } else if current_branch != target_branch {
        return Err(DevelopmentError::Conflict(format!(
            "repository is on unrelated branch {current_branch}; expected {base_branch} or {target_branch}"
        )));
    }

    let mut index = repository
        .index()
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    index
        .update_all(["*"].iter(), None)
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    index
        .write()
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    let tree_id = index
        .write_tree()
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    let tree = repository
        .find_tree(tree_id)
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    let parent = repository
        .head()
        .and_then(|reference| reference.peel_to_commit())
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    if parent.tree_id() == tree_id {
        return Ok(PreparedGit {
            branch: target_branch,
            commit_sha: None,
            had_change: false,
        });
    }
    let signature = Signature::now("Aion Delivery", "delivery@aionui.local")
        .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
    let commit = repository
        .commit(Some("HEAD"), &signature, &signature, message, &tree, &[&parent])
        .map_err(|error| DevelopmentError::Internal(format!("cannot create delivery commit: {error}")))?;
    Ok(PreparedGit {
        branch: target_branch,
        commit_sha: Some(commit.to_string()),
        had_change: true,
    })
}

fn require_confirmation(confirmed: bool, operation: &str) -> Result<(), DevelopmentError> {
    if confirmed {
        Ok(())
    } else {
        Err(DevelopmentError::BadRequest(format!(
            "explicit confirmation is required for {operation}"
        )))
    }
}

fn validate_delivery_branch(branch: &str, base_branch: &str) -> Result<(), DevelopmentError> {
    if branch == base_branch || matches!(branch, "main" | "master") {
        return Err(DevelopmentError::Conflict(
            "delivery cannot write directly to a protected branch".into(),
        ));
    }
    if branch.is_empty()
        || branch.len() > 240
        || branch.starts_with('-')
        || branch.contains("..")
        || branch.contains("@{")
        || branch.ends_with('/')
        || branch.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
    {
        return Err(DevelopmentError::BadRequest("delivery branch name is unsafe".into()));
    }
    Ok(())
}

fn sanitize_branch_component(value: &str) -> String {
    let result = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(64)
        .collect::<String>();
    if result.is_empty() { "run".into() } else { result }
}

fn clean_commit_message(value: &str) -> Result<String, DevelopmentError> {
    let message = value.lines().next().unwrap_or_default().trim();
    if message.is_empty() {
        return Err(DevelopmentError::BadRequest("commit message cannot be empty".into()));
    }
    Ok(message.chars().take(200).collect())
}

fn clean_title(value: &str) -> Result<String, DevelopmentError> {
    let title = value.lines().next().unwrap_or_default().trim();
    if title.is_empty() {
        return Err(DevelopmentError::BadRequest(
            "pull request title cannot be empty".into(),
        ));
    }
    Ok(title.chars().take(200).collect())
}

fn clean_summary(value: &str) -> String {
    let value = value.lines().next().unwrap_or("Development delivery").trim();
    if value.is_empty() {
        "Development delivery".into()
    } else {
        value.chars().take(160).collect()
    }
}

fn deterministic_rework_id(delivery_id: &str, check_id: &str) -> String {
    let digest = Sha256::digest(format!("{delivery_id}:{check_id}").as_bytes());
    format!("ci-rework-{}", hex_prefix(&digest, 16))
}

fn hex_prefix(bytes: &[u8], length: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| format!("{byte:02x}").chars().collect::<Vec<_>>())
        .take(length)
        .collect()
}

fn normalize_check_status(status: &str) -> String {
    match status.to_ascii_lowercase().as_str() {
        "success" | "passed" => "passed",
        "failure" | "failed" | "error" | "timed_out" | "action_required" => "failed",
        "cancelled" => "cancelled",
        "skipped" | "neutral" => "skipped",
        "in_progress" | "pending" => "in_progress",
        _ => "queued",
    }
    .into()
}

fn aggregate_ci_status(checks: &[ProviderCiCheck]) -> &'static str {
    if checks
        .iter()
        .any(|check| normalize_check_status(&check.status) == "failed")
    {
        "failed"
    } else if !checks.is_empty()
        && checks
            .iter()
            .all(|check| matches!(normalize_check_status(&check.status).as_str(), "passed" | "skipped"))
    {
        "passed"
    } else {
        "pending"
    }
}

fn redact_secret(value: &str) -> String {
    let mut words = redact_sensitive(value, &[]);
    if words.len() > 2000 {
        words.truncate(2000);
    }
    words
}

#[derive(Debug, Clone, Default)]
pub struct GhCliDeliveryProvider;

#[async_trait]
impl DeliveryProvider for GhCliDeliveryProvider {
    async fn preflight(&self, repository: &Path) -> Result<(), String> {
        Repository::open(repository).map_err(|error| format!("Git repository unavailable: {error}"))?;
        let mut command = Command::new("gh");
        command.arg("auth").arg("status").current_dir(repository);
        checked_output(command).await.map(|_| ())
    }

    async fn push(&self, repository: &Path, branch: &str) -> Result<(), String> {
        validate_delivery_branch(branch, "main").map_err(|error| error.to_string())?;
        let mut command = Command::new("git");
        command
            .arg("push")
            .arg("--set-upstream")
            .arg("origin")
            .arg(branch)
            .current_dir(repository);
        checked_output(command).await.map(|_| ())
    }

    async fn ensure_pull_request(
        &self,
        repository: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ProviderPullRequest, String> {
        let mut list = Command::new("gh");
        list.arg("pr")
            .arg("list")
            .arg("--head")
            .arg(head)
            .arg("--base")
            .arg(base)
            .arg("--state")
            .arg("open")
            .arg("--json")
            .arg("number,url,state,reviewDecision")
            .current_dir(repository);
        let existing: Value = serde_json::from_slice(&checked_output(list).await?)
            .map_err(|error| format!("cannot parse GitHub pull request list: {error}"))?;
        if let Some(value) = existing.as_array().and_then(|values| values.first()) {
            return parse_pull_request(value);
        }
        let mut create = Command::new("gh");
        create
            .arg("pr")
            .arg("create")
            .arg("--base")
            .arg(base)
            .arg("--head")
            .arg(head)
            .arg("--title")
            .arg(title)
            .arg("--body")
            .arg(body)
            .current_dir(repository);
        checked_output(create).await?;
        let mut view = Command::new("gh");
        view.arg("pr")
            .arg("view")
            .arg(head)
            .arg("--json")
            .arg("number,url,state,reviewDecision")
            .current_dir(repository);
        let value: Value = serde_json::from_slice(&checked_output(view).await?)
            .map_err(|error| format!("cannot parse created GitHub pull request: {error}"))?;
        parse_pull_request(&value)
    }

    async fn synchronize(&self, repository: &Path, number: i64) -> Result<DeliveryProviderSnapshot, String> {
        let mut command = Command::new("gh");
        command
            .arg("pr")
            .arg("view")
            .arg(number.to_string())
            .arg("--json")
            .arg("number,url,state,reviewDecision,statusCheckRollup")
            .current_dir(repository);
        let value: Value = serde_json::from_slice(&checked_output(command).await?)
            .map_err(|error| format!("cannot parse GitHub pull request status: {error}"))?;
        let pull_request = parse_pull_request(&value)?;
        let checks = value
            .get("statusCheckRollup")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(parse_ci_check)
            .collect();
        Ok(DeliveryProviderSnapshot { pull_request, checks })
    }

    async fn merge(&self, repository: &Path, number: i64) -> Result<(), String> {
        let mut command = Command::new("gh");
        command
            .arg("pr")
            .arg("merge")
            .arg(number.to_string())
            .arg("--merge")
            .current_dir(repository);
        checked_output(command).await.map(|_| ())
    }
}

async fn checked_output(mut command: Command) -> Result<Vec<u8>, String> {
    let output = command.output().await.map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn parse_pull_request(value: &Value) -> Result<ProviderPullRequest, String> {
    let number = value
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| "GitHub response has no pull request number".to_string())?;
    Ok(ProviderPullRequest {
        number,
        url: value.get("url").and_then(Value::as_str).unwrap_or_default().into(),
        status: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("OPEN")
            .to_ascii_lowercase(),
        review_status: match value.get("reviewDecision").and_then(Value::as_str).unwrap_or_default() {
            "APPROVED" => "approved",
            "CHANGES_REQUESTED" => "changes_requested",
            _ => "pending",
        }
        .into(),
    })
}

fn parse_ci_check(value: &Value) -> ProviderCiCheck {
    let name = value
        .get("name")
        .or_else(|| value.get("workflowName"))
        .and_then(Value::as_str)
        .unwrap_or("GitHub check")
        .to_owned();
    let details_url = value
        .get("detailsUrl")
        .or_else(|| value.get("targetUrl"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let raw_status = value
        .get("conclusion")
        .and_then(Value::as_str)
        .filter(|status| !status.is_empty())
        .or_else(|| value.get("status").and_then(Value::as_str))
        .unwrap_or("QUEUED");
    let id_source = format!("{name}:{}", details_url.as_deref().unwrap_or_default());
    let digest = Sha256::digest(id_source.as_bytes());
    ProviderCiCheck {
        id: format!("github-{}", hex_prefix(&digest, 24)),
        name,
        status: normalize_check_status(raw_status),
        details_url,
        summary: value.get("description").and_then(Value::as_str).map(str::to_owned),
    }
}
