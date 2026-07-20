use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aionui_common::now_ms;
use aionui_db::models::{
    AcceptanceCriterionRow, CompletionEvidenceRow, DevelopmentRunRoleRow, DevelopmentRunRow, DevelopmentTaskRow,
    DevelopmentUsageEventRow, PlanRevisionRow, ProjectCommandProfileRow, QualityGateRunRow, RequirementVersionRow,
    ReviewFindingRow, SingleRunWorkspaceRow, TaskArtifactRow, TaskCriterionRow,
};
use aionui_db::{IAgentWorkspaceLeaseRepository, IDevelopmentRepository, IProjectRepository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::error::DevelopmentError;
use crate::executor::{CommandExecutionInput, execute_command};
use crate::operations::{DevelopmentOperationsService, default_policy};
use crate::requirements::build_snapshot;
use crate::runner::{DevelopmentRunner, RunnerContext};
use crate::types::{
    AppendPlanRevisionInput, CompletionEvidenceInput, CreateArtifactInput, CreateDevelopmentRunInput,
    CreateDevelopmentTaskInput, ReviewFindingInput, SubmitReviewInput,
};
use crate::workspace::{DevelopmentWorkspacePort, PrepareDevelopmentWorkspace};

const MAX_GATE_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionEvaluation {
    pub allowed: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone)]
pub struct DevelopmentService {
    development_repo: Arc<dyn IDevelopmentRepository>,
    project_repo: Arc<dyn IProjectRepository>,
    lease_repo: Arc<dyn IAgentWorkspaceLeaseRepository>,
    artifact_root: PathBuf,
    operations: Option<Arc<DevelopmentOperationsService>>,
    workspace: Option<Arc<dyn DevelopmentWorkspacePort>>,
    runner: Option<Arc<DevelopmentRunner>>,
}

impl DevelopmentService {
    pub fn new(
        development_repo: Arc<dyn IDevelopmentRepository>,
        project_repo: Arc<dyn IProjectRepository>,
        lease_repo: Arc<dyn IAgentWorkspaceLeaseRepository>,
        artifact_root: PathBuf,
    ) -> Self {
        Self {
            development_repo,
            project_repo,
            lease_repo,
            artifact_root,
            operations: None,
            workspace: None,
            runner: None,
        }
    }

    pub fn with_operations(mut self, operations: Arc<DevelopmentOperationsService>) -> Self {
        self.operations = Some(operations);
        self
    }

    pub fn with_workspace(mut self, workspace: Arc<dyn DevelopmentWorkspacePort>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_runner(mut self, runner: Arc<DevelopmentRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    pub async fn create_run(
        &self,
        user_id: &str,
        input: CreateDevelopmentRunInput,
    ) -> Result<DevelopmentRunRow, DevelopmentError> {
        if !matches!(input.execution_mode.as_str(), "single" | "team") {
            return Err(DevelopmentError::BadRequest(
                "execution_mode must be single or team".into(),
            ));
        }
        if input.execution_mode == "team" && input.team_id.is_none() {
            return Err(DevelopmentError::BadRequest("team execution requires team_id".into()));
        }
        let summary = required_text(input.request_summary, "request_summary")?;
        let criteria = clean_criteria(input.acceptance_criteria)?;
        let project = self
            .project_repo
            .get_for_user(&input.project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("project {}", input.project_id)))?;
        let baseline_commit = git2::Repository::discover(&project.local_path).ok().and_then(|repo| {
            repo.head()
                .ok()
                .and_then(|head| head.target())
                .map(|oid| oid.to_string())
        });
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = id.split('-').next().unwrap_or(&id).to_owned();
        let now = now_ms();
        let row = DevelopmentRunRow {
            id,
            user_id: user_id.into(),
            project_id: project.id,
            team_id: input.team_id,
            source_channel: input.source_channel,
            source_user_id: input.source_user_id,
            execution_mode: input.execution_mode,
            status: "running".into(),
            request_summary: summary,
            acceptance_criteria: serde_json::to_string(&criteria)
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?,
            baseline_commit,
            integration_branch: Some(format!("aion/run/{short_id}/integration")),
            started_at: Some(now),
            finished_at: None,
            created_at: now,
            updated_at: now,
        };
        self.development_repo.create_run(&row).await?;
        self.append_requirement_version(user_id, &row.id, &row.request_summary, None, &criteria)
            .await?;
        if let Some(operations) = &self.operations {
            operations
                .audit(
                    user_id,
                    "user",
                    user_id,
                    "development_run.create",
                    "development_run",
                    &row.id,
                    &row.project_id,
                    Some(&row.id),
                    None,
                    "success",
                    serde_json::json!({"execution_mode": row.execution_mode}),
                    &[],
                )
                .await?;
        }
        Ok(row)
    }

    pub async fn get_run(&self, user_id: &str, run_id: &str) -> Result<DevelopmentRunRow, DevelopmentError> {
        self.development_repo
            .get_run(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("run {run_id}")))
    }

    pub async fn list_runs(
        &self,
        user_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<DevelopmentRunRow>, DevelopmentError> {
        Ok(self.development_repo.list_runs(user_id, project_id).await?)
    }

    async fn append_requirement_version(
        &self,
        user_id: &str,
        run_id: &str,
        content: &str,
        change_summary: Option<String>,
        criteria: &[String],
    ) -> Result<RequirementVersionRow, DevelopmentError> {
        let versions = self.development_repo.list_requirement_versions(run_id).await?;
        let now = now_ms();
        let row = RequirementVersionRow {
            id: uuid::Uuid::now_v7().to_string(),
            run_id: run_id.into(),
            version: versions.last().map_or(1, |version| version.version + 1),
            content: required_text(content.into(), "requirement content")?,
            change_summary: change_summary.and_then(clean_optional),
            created_by: user_id.into(),
            created_at: now,
        };
        let criteria = criteria
            .iter()
            .enumerate()
            .map(|(ordinal, statement)| AcceptanceCriterionRow {
                id: uuid::Uuid::now_v7().to_string(),
                run_id: run_id.into(),
                requirement_version_id: row.id.clone(),
                ordinal: ordinal as i64,
                statement: statement.clone(),
                required: true,
                created_at: now,
            })
            .collect::<Vec<_>>();
        self.development_repo
            .append_requirement_version(&row, &criteria)
            .await?;
        Ok(row)
    }

    pub async fn append_requirement_revision(
        &self,
        user_id: &str,
        run_id: &str,
        content: &str,
        change_summary: &str,
        criteria: Vec<String>,
    ) -> Result<aionui_api_types::RequirementVersion, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        if matches!(run.status.as_str(), "succeeded" | "failed" | "cancelled") {
            return Err(DevelopmentError::Conflict(
                "terminal run requirements cannot be revised".into(),
            ));
        }
        let criteria = clean_criteria(criteria)?;
        let summary = required_text(change_summary.into(), "change_summary")?;
        let row = self
            .append_requirement_version(user_id, run_id, content, Some(summary), &criteria)
            .await?;
        Ok(aionui_api_types::RequirementVersion {
            id: row.id,
            version: row.version,
            content: row.content,
            change_summary: row.change_summary,
            created_at: row.created_at,
        })
    }

    pub async fn append_plan_revision(
        &self,
        user_id: &str,
        run_id: &str,
        input: AppendPlanRevisionInput,
    ) -> Result<aionui_api_types::PlanRevision, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        if matches!(run.status.as_str(), "succeeded" | "failed" | "cancelled") {
            return Err(DevelopmentError::Conflict(
                "terminal run plans cannot be revised".into(),
            ));
        }
        let existing = self.development_repo.list_plan_revisions(run_id).await?;
        let row = PlanRevisionRow {
            id: uuid::Uuid::now_v7().to_string(),
            run_id: run_id.into(),
            revision: existing.last().map_or(1, |revision| revision.revision + 1),
            summary: required_text(input.summary, "plan summary")?,
            content: required_text(input.content, "plan content")?,
            created_by: user_id.into(),
            created_at: now_ms(),
        };
        self.development_repo.append_plan_revision(&row).await?;
        Ok(aionui_api_types::PlanRevision {
            id: row.id,
            revision: row.revision,
            summary: row.summary,
            content: row.content,
            created_at: row.created_at,
        })
    }

    pub async fn requirements_snapshot(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<aionui_api_types::RequirementsSnapshot, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(build_snapshot(
            run_id,
            self.development_repo.list_requirement_versions(run_id).await?,
            self.development_repo.list_active_criteria(run_id).await?,
            self.development_repo.list_plan_revisions(run_id).await?,
            self.development_repo.list_task_criteria(run_id).await?,
            self.development_repo.list_completion_evidence(run_id).await?,
        ))
    }

    pub async fn record_completion_evidence(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
        input: CompletionEvidenceInput,
    ) -> Result<CompletionEvidenceRow, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        if !matches!(input.evidence_type.as_str(), "code" | "test" | "no_change") {
            return Err(DevelopmentError::BadRequest(
                "evidence_type must be code, test, or no_change".into(),
            ));
        }
        let task = self
            .development_repo
            .get_task(run_id, task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))?;
        let mappings = self.development_repo.list_task_criteria(run_id).await?;
        if !mappings
            .iter()
            .any(|mapping| mapping.task_id == task.id && mapping.criterion_id == input.criterion_id)
        {
            return Err(DevelopmentError::BadRequest(
                "completion evidence must reference a criterion owned by this task".into(),
            ));
        }
        if let Some(artifact_id) = input.artifact_id.as_deref()
            && !self
                .development_repo
                .list_artifacts(run_id, Some(task_id))
                .await?
                .iter()
                .any(|artifact| artifact.id == artifact_id)
        {
            return Err(DevelopmentError::BadRequest(
                "evidence artifact is not owned by this task".into(),
            ));
        }
        let row = CompletionEvidenceRow {
            id: uuid::Uuid::now_v7().to_string(),
            run_id: run_id.into(),
            task_id: task_id.into(),
            criterion_id: input.criterion_id,
            evidence_type: input.evidence_type,
            artifact_id: input.artifact_id,
            reference: required_text(input.reference, "evidence reference")?,
            accepted: input.accepted,
            reviewer_id: input.reviewer_id.and_then(clean_optional),
            created_at: now_ms(),
        };
        if row.accepted && row.reviewer_id.is_none() {
            return Err(DevelopmentError::BadRequest(
                "accepted evidence requires reviewer_id".into(),
            ));
        }
        self.development_repo.create_completion_evidence(&row).await?;
        Ok(row)
    }

    pub async fn complete_run(&self, user_id: &str, run_id: &str) -> Result<DevelopmentRunRow, DevelopmentError> {
        let snapshot = self.requirements_snapshot(user_id, run_id).await?;
        let missing: Vec<_> = snapshot
            .coverage
            .iter()
            .filter(|item| !item.accepted || item.task_ids.is_empty())
            .map(|item| item.statement.clone())
            .collect();
        if !missing.is_empty() {
            return Err(DevelopmentError::Conflict(format!(
                "required acceptance evidence is missing: {}",
                missing.join(", ")
            )));
        }
        let gates = self.development_repo.list_gates(run_id, None).await?;
        let mut required = HashMap::new();
        for gate in gates.iter().filter(|gate| gate.required) {
            required.insert((gate.task_id.as_deref(), gate.gate_type.as_str()), gate);
        }
        if required.values().any(|gate| gate.status != "passed") {
            return Err(DevelopmentError::Conflict(
                "one or more required quality gates have not passed".into(),
            ));
        }
        if let Some(runner) = &self.runner {
            runner.cleanup_run(user_id, run_id).await?;
        }
        self.development_repo
            .update_run_status(run_id, user_id, "succeeded", Some(now_ms()))
            .await?;
        self.get_run(user_id, run_id).await
    }

    pub async fn assign_role(
        &self,
        user_id: &str,
        run_id: &str,
        slot_id: &str,
        role: &str,
    ) -> Result<DevelopmentRunRoleRow, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        if let Some(operations) = &self.operations {
            operations.require_budget(user_id, run_id, "assign_role", 0).await?;
        }
        if !matches!(role, "implementer" | "tester" | "reviewer" | "integrator") {
            return Err(DevelopmentError::BadRequest(format!(
                "unsupported development role: {role}"
            )));
        }
        let row = DevelopmentRunRoleRow {
            run_id: run_id.into(),
            slot_id: required_text(slot_id.into(), "slot_id")?,
            role: role.into(),
            assigned_at: now_ms(),
        };
        self.development_repo.assign_role(&row).await?;
        Ok(row)
    }

    pub async fn list_roles(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Vec<DevelopmentRunRoleRow>, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(self.development_repo.list_roles(run_id).await?)
    }

    pub async fn create_task(
        &self,
        user_id: &str,
        run_id: &str,
        input: CreateDevelopmentTaskInput,
    ) -> Result<DevelopmentTaskRow, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        let subject = required_text(input.subject, "subject")?;
        let criteria = clean_criteria(input.acceptance_criteria)?;
        let owned_criteria = self.development_repo.list_active_criteria(run_id).await?;
        let mapped_criteria: Vec<_> = criteria
            .iter()
            .filter_map(|statement| {
                owned_criteria
                    .iter()
                    .find(|criterion| criterion.statement == *statement)
            })
            .cloned()
            .collect();
        if mapped_criteria.len() != criteria.len() || mapped_criteria.is_empty() {
            return Err(DevelopmentError::BadRequest(
                "every task acceptance criterion must belong to the active run requirement version".into(),
            ));
        }
        if !matches!(input.risk_level.as_str(), "low" | "medium" | "high" | "critical") {
            return Err(DevelopmentError::BadRequest("unsupported risk_level".into()));
        }
        let existing = self.development_repo.list_tasks(run_id).await?;
        for dependency in &input.blocked_by {
            if !existing.iter().any(|task| &task.id == dependency) {
                return Err(DevelopmentError::BadRequest(format!(
                    "dependency {dependency} is not in this run"
                )));
            }
        }
        if let Some(lease_id) = input.assigned_workspace_lease_id.as_deref() {
            self.validate_lease(user_id, &run, lease_id).await?;
        }
        let now = now_ms();
        let row = DevelopmentTaskRow {
            id: uuid::Uuid::now_v7().to_string(),
            team_id: run.team_id.clone().unwrap_or_else(|| format!("run:{run_id}")),
            run_id: Some(run_id.into()),
            subject,
            description: input.description.and_then(clean_optional),
            status: if input.blocked_by.is_empty() {
                "ready"
            } else {
                "pending"
            }
            .into(),
            owner: input.owner.and_then(clean_optional),
            blocked_by: serde_json::to_string(&input.blocked_by)
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?,
            blocks: "[]".into(),
            metadata: None,
            acceptance_criteria: serde_json::to_string(&criteria)
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?,
            task_type: required_text(input.task_type, "task_type")?,
            risk_level: input.risk_level,
            assigned_workspace_lease_id: input.assigned_workspace_lease_id,
            review_status: "pending".into(),
            verification_status: "pending".into(),
            created_at: now,
            updated_at: now,
        };
        self.development_repo.create_task(&row).await?;
        self.development_repo
            .map_task_criteria(
                &mapped_criteria
                    .into_iter()
                    .map(|criterion| TaskCriterionRow {
                        run_id: run_id.into(),
                        task_id: row.id.clone(),
                        criterion_id: criterion.id,
                        mapped_at: now,
                    })
                    .collect::<Vec<_>>(),
            )
            .await?;
        Ok(row)
    }

    pub async fn list_tasks(&self, user_id: &str, run_id: &str) -> Result<Vec<DevelopmentTaskRow>, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(self.development_repo.list_tasks(run_id).await?)
    }

    pub async fn transition_task(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
        target: &str,
    ) -> Result<DevelopmentTaskRow, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        let task = self
            .development_repo
            .get_task(run_id, task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))?;
        if target == "completed" {
            return Err(DevelopmentError::BadRequest(
                "use the completion endpoint so quality evidence can be verified".into(),
            ));
        }
        if !valid_task_transition(&task.status, target) {
            return Err(DevelopmentError::Conflict(format!(
                "task cannot transition from {} to {target}",
                task.status
            )));
        }
        if matches!(target, "ready" | "claimed" | "in_progress") {
            let dependencies: Vec<String> = serde_json::from_str(&task.blocked_by)
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
            let all_tasks = self.development_repo.list_tasks(run_id).await?;
            if dependencies.iter().any(|dependency| {
                !all_tasks
                    .iter()
                    .any(|candidate| candidate.id == *dependency && candidate.status == "completed")
            }) {
                return Err(DevelopmentError::Conflict(
                    "task still has incomplete dependencies".into(),
                ));
            }
        }
        self.development_repo
            .update_task_state(run_id, task_id, target, &task.review_status, &task.verification_status)
            .await?;
        self.development_repo
            .get_task(run_id, task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))
    }

    pub async fn create_artifact(
        &self,
        user_id: &str,
        run_id: &str,
        input: CreateArtifactInput,
    ) -> Result<TaskArtifactRow, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        validate_artifact_type(&input.artifact_type)?;
        let task = match input.task_id.as_deref() {
            Some(task_id) => Some(
                self.development_repo
                    .get_task(run_id, task_id)
                    .await?
                    .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))?,
            ),
            None => None,
        };
        let mut path_or_uri = required_text(input.path_or_uri, "path_or_uri")?;
        let checksum = required_text(input.checksum, "checksum")?;
        if input.artifact_type == "commit" {
            if !(7..=64).contains(&path_or_uri.len()) || !path_or_uri.chars().all(|value| value.is_ascii_hexdigit()) {
                return Err(DevelopmentError::BadRequest(
                    "commit artifact must contain a Git object id".into(),
                ));
            }
        } else if input.artifact_type != "no_code_change" {
            let project = self
                .project_repo
                .get_for_user(&run.project_id, user_id)
                .await?
                .ok_or_else(|| DevelopmentError::NotFound(format!("project {}", run.project_id)))?;
            let canonical = Path::new(&path_or_uri)
                .canonicalize()
                .map_err(|error| DevelopmentError::BadRequest(format!("artifact file cannot be resolved: {error}")))?;
            if !canonical.is_file() {
                return Err(DevelopmentError::BadRequest("artifact path is not a file".into()));
            }
            let mut allowed_roots = vec![PathBuf::from(project.local_path)];
            if let Some(lease_id) = task
                .as_ref()
                .and_then(|task| task.assigned_workspace_lease_id.as_deref())
            {
                allowed_roots.push(PathBuf::from(
                    self.validate_lease(user_id, &run, lease_id).await?.worktree_path,
                ));
            }
            if self.artifact_root.exists() {
                allowed_roots.push(self.artifact_root.clone());
            }
            let allowed = allowed_roots
                .iter()
                .filter_map(|root| root.canonicalize().ok())
                .any(|root| canonical == root || canonical.starts_with(&root));
            if !allowed {
                return Err(DevelopmentError::BadRequest(
                    "artifact file is outside the project, assigned workspace, and managed artifact directory".into(),
                ));
            }
            let actual = checksum_file(&canonical).await?;
            if checksum != actual {
                return Err(DevelopmentError::BadRequest(
                    "artifact checksum does not match file content".into(),
                ));
            }
            path_or_uri = canonical.to_string_lossy().into_owned();
        }
        let row = TaskArtifactRow {
            id: uuid::Uuid::now_v7().to_string(),
            run_id: run_id.into(),
            task_id: input.task_id,
            artifact_type: input.artifact_type,
            path_or_uri,
            checksum,
            producer_agent_id: input.producer_agent_id,
            metadata: input.metadata.map(|value| value.to_string()),
            created_at: now_ms(),
        };
        self.development_repo.create_artifact(&row).await?;
        Ok(row)
    }

    pub async fn execute_gate(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: Option<&str>,
        gate_type: &str,
        workspace_lease_id: Option<&str>,
        required: bool,
    ) -> Result<QualityGateRunRow, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        if let Some(task_id) = task_id
            && self.development_repo.get_task(run_id, task_id).await?.is_none()
        {
            return Err(DevelopmentError::NotFound(format!("task {task_id}")));
        }
        let project = self
            .project_repo
            .get_for_user(&run.project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("project {}", run.project_id)))?;
        let profile = self
            .project_repo
            .get_command_profile(&run.project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::BadRequest("project command profile is not configured".into()))?;
        let command = command_for_gate(&profile, gate_type)
            .ok_or_else(|| DevelopmentError::BadRequest(format!("gate {gate_type} has no configured command")))?;
        let previous_attempts = self
            .development_repo
            .list_gates(run_id, task_id)
            .await?
            .into_iter()
            .filter(|gate| gate.gate_type == gate_type)
            .count() as i64;
        if let Some(operations) = &self.operations {
            operations
                .require_budget(user_id, run_id, "quality_gate", previous_attempts)
                .await?;
        }
        let lease = match workspace_lease_id {
            Some(lease_id) => Some(self.validate_lease(user_id, &run, lease_id).await?),
            None if run.execution_mode == "team" => {
                return Err(DevelopmentError::BadRequest(
                    "team quality gates require an assigned workspace lease".into(),
                ));
            }
            None => None,
        };
        let working_directory = lease
            .as_ref()
            .map(|lease| lease.worktree_path.clone())
            .unwrap_or(project.local_path);
        if let Some(lease) = lease.as_ref() {
            let roles = self.development_repo.list_roles(run_id).await?;
            if !roles.is_empty() {
                let allowed_roles: &[&str] = if task_id.is_some() {
                    &["implementer", "tester"]
                } else {
                    &["integrator"]
                };
                if !roles
                    .iter()
                    .any(|role| role.slot_id == lease.slot_id && allowed_roles.contains(&role.role.as_str()))
                {
                    return Err(DevelopmentError::BadRequest(format!(
                        "slot {} is not assigned an allowed quality role",
                        lease.slot_id
                    )));
                }
            }
        }
        let started_at = now_ms();
        let started = Instant::now();
        let gate_id = uuid::Uuid::now_v7().to_string();
        let policy = match &self.operations {
            Some(operations) => operations.get_policy(user_id, &run.project_id).await?,
            None => default_policy(user_id, &run.project_id),
        };
        let runtime_profile = self.project_repo.get_runtime_profile(&run.project_id, user_id).await?;
        let environment = runtime_environment(runtime_profile.as_ref())?;
        let mut row = QualityGateRunRow {
            id: gate_id.clone(),
            run_id: run_id.into(),
            task_id: task_id.map(str::to_owned),
            gate_type: gate_type.into(),
            command: command.into(),
            working_directory: working_directory.clone(),
            exit_code: None,
            status: "running".into(),
            stdout_artifact_id: None,
            stderr_artifact_id: None,
            duration_ms: None,
            isolation_mode: policy.isolation_mode.clone(),
            execution_id: Some(gate_id.clone()),
            required,
            started_at: Some(started_at),
            finished_at: None,
            created_at: started_at,
        };
        self.development_repo.create_gate(&row).await?;
        let command_input = CommandExecutionInput {
            execution_id: &gate_id,
            run_id,
            command,
            working_directory: Path::new(&working_directory),
            timeout_seconds: profile.command_timeout_seconds,
            policy: &policy,
            runtime_profile: runtime_profile.as_ref(),
            environment,
        };
        let output_result = match &self.runner {
            Some(runner) => {
                runner
                    .execute(
                        command_input,
                        &RunnerContext {
                            user_id: user_id.into(),
                            project_id: run.project_id.clone(),
                            run_id: run_id.into(),
                            task_id: task_id.map(str::to_owned),
                            turn_id: None,
                            gate_id: Some(gate_id.clone()),
                        },
                    )
                    .await
            }
            None => execute_command(command_input).await,
        };
        let output = match output_result {
            Ok(output) => output,
            Err(error) => {
                row.status = "failed".into();
                row.duration_ms = Some(started.elapsed().as_millis().min(i64::MAX as u128) as i64);
                row.finished_at = Some(now_ms());
                self.development_repo.update_gate(&row).await?;
                if let Some(operations) = &self.operations {
                    operations
                        .audit(
                            user_id,
                            "system",
                            "development-command-executor",
                            "quality_gate.execute",
                            "quality_gate",
                            &row.id,
                            &run.project_id,
                            Some(run_id),
                            task_id,
                            "failed",
                            serde_json::json!({
                                "gate_type": gate_type,
                                "isolation_mode": row.isolation_mode,
                                "error": error.to_string(),
                            }),
                            &[],
                        )
                        .await?;
                }
                return Err(error);
            }
        };
        let stdout = self
            .persist_gate_output(run_id, task_id, &gate_id, "stdout", output.stdout.as_bytes())
            .await?;
        let stderr = self
            .persist_gate_output(run_id, task_id, &gate_id, "stderr", output.stderr.as_bytes())
            .await?;
        row.exit_code = output.exit_code;
        row.status = output.status;
        row.stdout_artifact_id = Some(stdout.id);
        row.stderr_artifact_id = Some(stderr.id);
        row.duration_ms = Some(started.elapsed().as_millis().min(i64::MAX as u128) as i64);
        row.isolation_mode = output.isolation_mode;
        row.execution_id = Some(output.execution_id);
        row.finished_at = Some(now_ms());
        self.development_repo.update_gate(&row).await?;
        if let Some(operations) = &self.operations {
            operations
                .record_usage(DevelopmentUsageEventRow {
                    id: format!("gate:{}", row.id),
                    user_id: user_id.into(),
                    project_id: run.project_id.clone(),
                    run_id: Some(run_id.into()),
                    task_id: task_id.map(str::to_owned),
                    usage_type: "quality_gate".into(),
                    source: "platform".into(),
                    confidence: "measured".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_microunits: 0,
                    duration_ms: row.duration_ms.unwrap_or_default().max(0),
                    retry_count: i64::from(previous_attempts > 0),
                    metadata_json: serde_json::json!({
                        "gate_type": gate_type,
                        "status": row.status,
                    })
                    .to_string(),
                    created_at: now_ms(),
                })
                .await?;
            operations
                .audit(
                    user_id,
                    "system",
                    "development-command-executor",
                    "quality_gate.execute",
                    "quality_gate",
                    &row.id,
                    &run.project_id,
                    Some(run_id),
                    task_id,
                    if row.status == "passed" { "success" } else { "failed" },
                    serde_json::json!({
                        "gate_type": gate_type,
                        "status": row.status,
                        "isolation_mode": row.isolation_mode,
                        "duration_ms": row.duration_ms,
                    }),
                    &[],
                )
                .await?;
        }
        if let Some(task_id) = task_id {
            let task = self
                .development_repo
                .get_task(run_id, task_id)
                .await?
                .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))?;
            let verification = if row.status == "passed" { "passed" } else { "failed" };
            self.development_repo
                .update_task_state(run_id, task_id, &task.status, &task.review_status, verification)
                .await?;
        }
        Ok(row)
    }

    pub async fn submit_review(
        &self,
        user_id: &str,
        run_id: &str,
        input: SubmitReviewInput,
    ) -> Result<DevelopmentTaskRow, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        let task = self
            .development_repo
            .get_task(run_id, &input.task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {}", input.task_id)))?;
        if task.owner.as_deref() == Some(input.reviewer_agent_id.as_str()) {
            return Err(DevelopmentError::BadRequest(
                "reviewer must be different from the task implementer".into(),
            ));
        }
        let roles = self.development_repo.list_roles(run_id).await?;
        if !roles.is_empty()
            && !roles
                .iter()
                .any(|role| role.slot_id == input.reviewer_agent_id && role.role == "reviewer")
        {
            return Err(DevelopmentError::BadRequest(format!(
                "slot {} is not assigned the reviewer role",
                input.reviewer_agent_id
            )));
        }
        let reviewer_agent_id = input.reviewer_agent_id.clone();
        let producer_agent_id = input.producer_agent_id.clone();
        for finding in input.findings {
            self.create_finding(
                run_id,
                &input.task_id,
                &reviewer_agent_id,
                producer_agent_id.as_deref(),
                finding,
            )
            .await?;
        }
        let review_status = if input.approved {
            "approved"
        } else {
            "changes_requested"
        };
        let status = if input.approved { "review" } else { "rework" };
        self.development_repo
            .update_task_state(run_id, &input.task_id, status, review_status, &task.verification_status)
            .await?;
        self.development_repo
            .get_task(run_id, &input.task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {}", input.task_id)))
    }

    async fn create_finding(
        &self,
        run_id: &str,
        task_id: &str,
        reviewer_agent_id: &str,
        producer_agent_id: Option<&str>,
        finding: ReviewFindingInput,
    ) -> Result<(), DevelopmentError> {
        if !matches!(
            finding.severity.as_str(),
            "info" | "warning" | "major" | "critical" | "blocker"
        ) {
            return Err(DevelopmentError::BadRequest("unsupported finding severity".into()));
        }
        let now = now_ms();
        self.development_repo
            .create_finding(&ReviewFindingRow {
                id: uuid::Uuid::now_v7().to_string(),
                run_id: run_id.into(),
                task_id: task_id.into(),
                reviewer_agent_id: reviewer_agent_id.into(),
                producer_agent_id: producer_agent_id.map(str::to_owned),
                severity: finding.severity,
                file_path: finding.file_path,
                line_number: finding.line_number,
                reason: required_text(finding.reason, "finding reason")?,
                suggestion: finding.suggestion,
                status: "open".into(),
                created_at: now,
                updated_at: now,
            })
            .await?;
        Ok(())
    }

    pub async fn evaluate_completion(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
    ) -> Result<CompletionEvaluation, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        let task = self
            .development_repo
            .get_task(run_id, task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))?;
        let artifacts = self.development_repo.list_artifacts(run_id, Some(task_id)).await?;
        let gates = self.development_repo.list_gates(run_id, Some(task_id)).await?;
        let findings = self.development_repo.list_findings(run_id, task_id).await?;
        let mut reasons = Vec::new();
        let criteria: Vec<String> = serde_json::from_str(&task.acceptance_criteria)
            .map_err(|error| DevelopmentError::Internal(error.to_string()))?;
        if criteria.is_empty()
            || !artifacts
                .iter()
                .any(|artifact| matches!(artifact.artifact_type.as_str(), "test" | "report" | "review"))
        {
            reasons.push("acceptance criteria require a test, report, or review artifact".into());
        }
        let mut latest_required = HashMap::new();
        for gate in gates.iter().filter(|gate| gate.required) {
            latest_required.insert(gate.gate_type.as_str(), gate);
        }
        if latest_required.is_empty() {
            reasons.push("no required quality gate has passed".into());
        } else if latest_required.values().any(|gate| gate.status != "passed") {
            reasons.push("one or more required quality gates have not passed".into());
        }
        if findings
            .iter()
            .any(|finding| finding.status == "open" && matches!(finding.severity.as_str(), "critical" | "blocker"))
        {
            reasons.push("critical or blocker review findings remain open".into());
        }
        if !matches!(task.review_status.as_str(), "approved" | "not_required") {
            reasons.push("review has not been approved".into());
        }
        let has_commit = artifacts.iter().any(|artifact| artifact.artifact_type == "commit");
        let has_accepted_no_code = artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "no_code_change")
            && task.review_status == "approved";
        if !has_commit && !has_accepted_no_code {
            reasons.push("a commit or reviewer-approved no-code artifact is required".into());
        }
        if artifacts.iter().any(|artifact| artifact.checksum.trim().is_empty()) {
            reasons.push("one or more artifacts have no checksum".into());
        }
        for artifact in artifacts
            .iter()
            .filter(|artifact| !matches!(artifact.artifact_type.as_str(), "commit" | "no_code_change"))
        {
            match checksum_file(Path::new(&artifact.path_or_uri)).await {
                Ok(checksum) if checksum == artifact.checksum => {}
                _ => reasons.push(format!(
                    "artifact {} is missing or its checksum is invalid",
                    artifact.id
                )),
            }
        }
        Ok(CompletionEvaluation {
            allowed: reasons.is_empty(),
            reasons,
        })
    }

    pub async fn complete_task(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
    ) -> Result<DevelopmentTaskRow, DevelopmentError> {
        let evaluation = self.evaluate_completion(user_id, run_id, task_id).await?;
        if !evaluation.allowed {
            return Err(DevelopmentError::Conflict(evaluation.reasons.join("; ")));
        }
        self.development_repo
            .update_task_state(run_id, task_id, "completed", "approved", "passed")
            .await?;
        self.development_repo
            .get_task(run_id, task_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("task {task_id}")))
    }

    pub async fn list_artifacts(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: Option<&str>,
    ) -> Result<Vec<TaskArtifactRow>, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(self.development_repo.list_artifacts(run_id, task_id).await?)
    }

    pub async fn list_gates(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: Option<&str>,
    ) -> Result<Vec<QualityGateRunRow>, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(self.development_repo.list_gates(run_id, task_id).await?)
    }

    pub async fn list_findings(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
    ) -> Result<Vec<ReviewFindingRow>, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(self.development_repo.list_findings(run_id, task_id).await?)
    }

    pub async fn resolve_finding(
        &self,
        user_id: &str,
        run_id: &str,
        finding_id: &str,
        status: &str,
    ) -> Result<(), DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        if !matches!(status, "resolved" | "dismissed") {
            return Err(DevelopmentError::BadRequest(
                "finding status must be resolved or dismissed".into(),
            ));
        }
        if !self
            .development_repo
            .update_finding_status(run_id, finding_id, status)
            .await?
        {
            return Err(DevelopmentError::NotFound(format!("finding {finding_id}")));
        }
        Ok(())
    }

    pub async fn prepare_single_workspace(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<aionui_api_types::SingleRunWorkspace, DevelopmentError> {
        let run = self.get_run(user_id, run_id).await?;
        if run.execution_mode != "single" {
            return Err(DevelopmentError::BadRequest(
                "only single-Agent runs use a single workspace".into(),
            ));
        }
        if let Some(existing) = self.development_repo.get_single_run_workspace(run_id, user_id).await? {
            return Ok(single_workspace_dto(existing));
        }
        let project = self
            .project_repo
            .get_for_user(&run.project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("project {}", run.project_id)))?;
        let repository = git2::Repository::discover(&project.local_path)
            .map_err(|error| DevelopmentError::BadRequest(format!("project repository is unavailable: {error}")))?;
        let baseline_commit = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| oid.to_string())
            .ok_or_else(|| DevelopmentError::BadRequest("project repository has no baseline commit".into()))?;
        let snapshot = initial_user_diff(&repository)?;
        let checksum = format!("sha256:{:x}", Sha256::digest(&snapshot));
        let directory = self.artifact_root.join(run_id).join("workspace");
        tokio::fs::create_dir_all(&directory).await?;
        let snapshot_path = directory.join("initial-user.diff");
        tokio::fs::write(&snapshot_path, &snapshot).await?;
        let workspace = self
            .workspace
            .as_ref()
            .ok_or_else(|| DevelopmentError::Conflict("development workspace provider is unavailable".into()))?
            .prepare(PrepareDevelopmentWorkspace {
                user_id: user_id.into(),
                run_id: run_id.into(),
                repository_path: repository
                    .workdir()
                    .unwrap_or_else(|| Path::new(&project.local_path))
                    .to_string_lossy()
                    .into_owned(),
                baseline_commit: baseline_commit.clone(),
            })
            .await
            .map_err(DevelopmentError::Conflict)?;
        let now = now_ms();
        let row = SingleRunWorkspaceRow {
            run_id: run_id.into(),
            user_id: user_id.into(),
            project_id: run.project_id,
            baseline_commit,
            initial_diff_checksum: checksum,
            initial_diff_path: snapshot_path.to_string_lossy().into_owned(),
            workspace_lease_id: Some(workspace.lease_id),
            workspace_path: Some(workspace.workspace_path),
            branch: Some(workspace.branch),
            candidate_commit: None,
            safe_point: workspace.safe_point,
            cleanup_status: "active".into(),
            created_at: now,
            updated_at: now,
        };
        self.development_repo.create_single_run_workspace(&row).await?;
        Ok(single_workspace_dto(row))
    }

    pub async fn get_single_workspace(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<aionui_api_types::SingleRunWorkspace>, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        Ok(self
            .development_repo
            .get_single_run_workspace(run_id, user_id)
            .await?
            .map(single_workspace_dto))
    }

    pub async fn cancel_single_workspace(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<aionui_api_types::SingleRunWorkspace, DevelopmentError> {
        self.get_run(user_id, run_id).await?;
        let row = self
            .development_repo
            .get_single_run_workspace(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("single workspace for run {run_id}")))?;
        if let Some(runner) = &self.runner {
            runner.cleanup_run(user_id, run_id).await?;
        }
        let lease_id = row
            .workspace_lease_id
            .as_deref()
            .ok_or_else(|| DevelopmentError::Conflict("single workspace has no active lease".into()))?;
        let cleanup = self
            .workspace
            .as_ref()
            .ok_or_else(|| DevelopmentError::Conflict("development workspace provider is unavailable".into()))?
            .restore(lease_id, &row.safe_point)
            .await
            .map_err(DevelopmentError::Conflict)?;
        self.development_repo
            .update_single_run_workspace(run_id, user_id, None, &cleanup)
            .await?;
        self.development_repo
            .update_run_status(run_id, user_id, "cancelled", Some(now_ms()))
            .await?;
        let updated = self
            .development_repo
            .get_single_run_workspace(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("single workspace for run {run_id}")))?;
        Ok(single_workspace_dto(updated))
    }

    async fn validate_lease(
        &self,
        user_id: &str,
        run: &DevelopmentRunRow,
        lease_id: &str,
    ) -> Result<aionui_db::models::AgentWorkspaceLeaseRow, DevelopmentError> {
        let lease = self
            .lease_repo
            .get(lease_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("workspace lease {lease_id}")))?;
        let expected_namespace = run.team_id.clone().unwrap_or_else(|| format!("run:{}", run.id));
        if lease.user_id != user_id || lease.team_id != expected_namespace {
            return Err(DevelopmentError::NotFound(format!("workspace lease {lease_id}")));
        }
        if lease.lease_status != "active" {
            return Err(DevelopmentError::Conflict(format!(
                "workspace lease {lease_id} is not active"
            )));
        }
        Ok(lease)
    }

    async fn persist_gate_output(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        gate_id: &str,
        stream: &str,
        bytes: &[u8],
    ) -> Result<TaskArtifactRow, DevelopmentError> {
        let directory = self.artifact_root.join(run_id).join("quality-gates");
        tokio::fs::create_dir_all(&directory).await?;
        let filename = format!("{gate_id}-{stream}.log");
        let path = directory.join(&filename);
        tokio::fs::write(&path, bytes).await?;
        let checksum = format!("sha256:{:x}", Sha256::digest(bytes));
        let row = TaskArtifactRow {
            id: uuid::Uuid::now_v7().to_string(),
            run_id: run_id.into(),
            task_id: task_id.map(str::to_owned),
            artifact_type: "log".into(),
            path_or_uri: path.to_string_lossy().into_owned(),
            checksum,
            producer_agent_id: None,
            metadata: Some(serde_json::json!({ "stream": stream, "truncated_at": MAX_GATE_OUTPUT_BYTES }).to_string()),
            created_at: now_ms(),
        };
        self.development_repo.create_artifact(&row).await?;
        Ok(row)
    }
}

fn required_text(value: String, field: &str) -> Result<String, DevelopmentError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(DevelopmentError::BadRequest(format!("{field} must not be empty")))
    } else {
        Ok(value)
    }
}

fn clean_optional(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn clean_criteria(values: Vec<String>) -> Result<Vec<String>, DevelopmentError> {
    let values: Vec<_> = values.into_iter().filter_map(clean_optional).collect();
    if values.is_empty() {
        Err(DevelopmentError::BadRequest(
            "at least one acceptance criterion is required".into(),
        ))
    } else {
        Ok(values)
    }
}

fn validate_artifact_type(value: &str) -> Result<(), DevelopmentError> {
    if matches!(
        value,
        "diff" | "test" | "log" | "report" | "commit" | "review" | "no_code_change"
    ) {
        Ok(())
    } else {
        Err(DevelopmentError::BadRequest(format!(
            "unsupported artifact type: {value}"
        )))
    }
}

fn valid_task_transition(current: &str, target: &str) -> bool {
    matches!(
        (current, target),
        ("pending", "ready")
            | ("pending", "cancelled")
            | ("ready", "claimed")
            | ("ready", "cancelled")
            | ("claimed", "in_progress")
            | ("claimed", "cancelled")
            | ("in_progress", "waiting_approval")
            | ("in_progress", "verifying")
            | ("in_progress", "review")
            | ("in_progress", "failed")
            | ("in_progress", "cancelled")
            | ("waiting_approval", "in_progress")
            | ("waiting_approval", "cancelled")
            | ("verifying", "review")
            | ("verifying", "rework")
            | ("verifying", "failed")
            | ("review", "rework")
            | ("review", "cancelled")
            | ("rework", "claimed")
            | ("rework", "in_progress")
            | ("rework", "cancelled")
    )
}

fn single_workspace_dto(row: SingleRunWorkspaceRow) -> aionui_api_types::SingleRunWorkspace {
    aionui_api_types::SingleRunWorkspace {
        run_id: row.run_id,
        baseline_commit: row.baseline_commit,
        initial_diff_checksum: row.initial_diff_checksum,
        initial_diff_path: row.initial_diff_path,
        workspace_lease_id: row.workspace_lease_id,
        workspace_path: row.workspace_path,
        branch: row.branch,
        candidate_commit: row.candidate_commit,
        safe_point: row.safe_point,
        cleanup_status: row.cleanup_status,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn initial_user_diff(repository: &git2::Repository) -> Result<Vec<u8>, DevelopmentError> {
    let head = repository
        .head()
        .and_then(|head| head.peel_to_tree())
        .map_err(|error| DevelopmentError::BadRequest(format!("cannot read baseline tree: {error}")))?;
    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let diff = repository
        .diff_tree_to_workdir_with_index(Some(&head), Some(&mut options))
        .map_err(|error| DevelopmentError::Internal(format!("cannot capture initial diff: {error}")))?;
    let mut bytes = Vec::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        bytes.extend_from_slice(line.content());
        true
    })
    .map_err(|error| DevelopmentError::Internal(format!("cannot serialize initial diff: {error}")))?;
    Ok(bytes)
}

fn command_for_gate<'a>(profile: &'a ProjectCommandProfileRow, gate_type: &str) -> Option<&'a str> {
    match gate_type {
        "install" => profile.install_command.as_deref(),
        "format" => profile.format_command.as_deref(),
        "lint" => profile.lint_command.as_deref(),
        "typecheck" => profile.typecheck_command.as_deref(),
        "unit_test" => profile.unit_test_command.as_deref(),
        "integration_test" => profile.integration_test_command.as_deref(),
        "e2e" => profile.e2e_command.as_deref(),
        "build" => profile.build_command.as_deref(),
        "security_scan" => profile.security_scan_command.as_deref(),
        _ => None,
    }
}

fn runtime_environment(
    profile: Option<&aionui_db::models::ProjectRuntimeProfileRow>,
) -> Result<BTreeMap<String, String>, DevelopmentError> {
    let keys: Vec<String> = match profile {
        Some(profile) => serde_json::from_str(&profile.env_keys)
            .map_err(|error| DevelopmentError::BadRequest(format!("invalid runtime env keys: {error}")))?,
        None => Vec::new(),
    };
    Ok(keys
        .into_iter()
        .filter_map(|key| std::env::var(&key).ok().map(|value| (key, value)))
        .collect())
}

async fn checksum_file(path: &Path) -> Result<String, DevelopmentError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        digest.update(&chunk[..count]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}
