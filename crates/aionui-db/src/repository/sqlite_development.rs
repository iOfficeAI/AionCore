use aionui_common::{TimestampMs, now_ms};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    AcceptanceCriterionRow, CompletionEvidenceRow, DevelopmentCiCheckRow, DevelopmentDeliveryRow,
    DevelopmentDeliveryTagRow, DevelopmentDeploymentRow, DevelopmentRunRoleRow, DevelopmentRunRow, DevelopmentTaskRow,
    PlanRevisionRow, QualityGateRunRow, RequirementVersionRow, ReviewFindingRow, SingleRunWorkspaceRow,
    TaskArtifactRow, TaskCriterionRow,
};
use crate::repository::development::IDevelopmentRepository;

#[derive(Clone, Debug)]
pub struct SqliteDevelopmentRepository {
    pool: SqlitePool,
}

impl SqliteDevelopmentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IDevelopmentRepository for SqliteDevelopmentRepository {
    async fn create_run(&self, row: &DevelopmentRunRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_runs (id, user_id, project_id, team_id, source_channel, source_user_id, \
             execution_mode, status, request_summary, acceptance_criteria, baseline_commit, integration_branch, \
             started_at, finished_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.team_id)
        .bind(&row.source_channel)
        .bind(&row.source_user_id)
        .bind(&row.execution_mode)
        .bind(&row.status)
        .bind(&row.request_summary)
        .bind(&row.acceptance_criteria)
        .bind(&row.baseline_commit)
        .bind(&row.integration_branch)
        .bind(row.started_at)
        .bind(row.finished_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_run(&self, run_id: &str, user_id: &str) -> Result<Option<DevelopmentRunRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_runs WHERE id = ? AND user_id = ?")
                .bind(run_id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_runs(&self, user_id: &str, project_id: Option<&str>) -> Result<Vec<DevelopmentRunRow>, DbError> {
        let rows = match project_id {
            Some(project_id) => sqlx::query_as(
                "SELECT * FROM development_runs WHERE user_id = ? AND project_id = ? ORDER BY updated_at DESC, id ASC",
            )
            .bind(user_id)
            .bind(project_id)
            .fetch_all(&self.pool)
            .await?,
            None => {
                sqlx::query_as("SELECT * FROM development_runs WHERE user_id = ? ORDER BY updated_at DESC, id ASC")
                    .bind(user_id)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(rows)
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        user_id: &str,
        status: &str,
        finished_at: Option<TimestampMs>,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_runs SET status = ?, finished_at = ?, updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(status)
        .bind(finished_at)
        .bind(now_ms())
        .bind(run_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn assign_role(&self, row: &DevelopmentRunRoleRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_run_roles (run_id, slot_id, role, assigned_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(run_id, slot_id, role) DO NOTHING",
        )
        .bind(&row.run_id)
        .bind(&row.slot_id)
        .bind(&row.role)
        .bind(row.assigned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_roles(&self, run_id: &str) -> Result<Vec<DevelopmentRunRoleRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_run_roles WHERE run_id = ? ORDER BY role ASC, slot_id ASC")
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn create_task(&self, row: &DevelopmentTaskRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, run_id, subject, description, status, owner, blocked_by, blocks, \
             metadata, acceptance_criteria, task_type, risk_level, assigned_workspace_lease_id, review_status, \
             verification_status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.team_id)
        .bind(&row.run_id)
        .bind(&row.subject)
        .bind(&row.description)
        .bind(&row.status)
        .bind(&row.owner)
        .bind(&row.blocked_by)
        .bind(&row.blocks)
        .bind(&row.metadata)
        .bind(&row.acceptance_criteria)
        .bind(&row.task_type)
        .bind(&row.risk_level)
        .bind(&row.assigned_workspace_lease_id)
        .bind(&row.review_status)
        .bind(&row.verification_status)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_task(&self, run_id: &str, task_id: &str) -> Result<Option<DevelopmentTaskRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM team_tasks WHERE run_id = ? AND id = ?")
            .bind(run_id)
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_tasks(&self, run_id: &str) -> Result<Vec<DevelopmentTaskRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM team_tasks WHERE run_id = ? ORDER BY created_at ASC, id ASC")
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn update_task_state(
        &self,
        run_id: &str,
        task_id: &str,
        status: &str,
        review_status: &str,
        verification_status: &str,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE team_tasks SET status = ?, review_status = ?, verification_status = ?, updated_at = ? \
             WHERE run_id = ? AND id = ?",
        )
        .bind(status)
        .bind(review_status)
        .bind(verification_status)
        .bind(now_ms())
        .bind(run_id)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn create_artifact(&self, row: &TaskArtifactRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO task_artifacts (id, run_id, task_id, artifact_type, path_or_uri, checksum, \
             producer_agent_id, metadata, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.artifact_type)
        .bind(&row.path_or_uri)
        .bind(&row.checksum)
        .bind(&row.producer_agent_id)
        .bind(&row.metadata)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_artifacts(&self, run_id: &str, task_id: Option<&str>) -> Result<Vec<TaskArtifactRow>, DbError> {
        let rows = match task_id {
            Some(task_id) => {
                sqlx::query_as(
                    "SELECT * FROM task_artifacts WHERE run_id = ? AND task_id = ? ORDER BY created_at ASC, id ASC",
                )
                .bind(run_id)
                .bind(task_id)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as("SELECT * FROM task_artifacts WHERE run_id = ? ORDER BY created_at ASC, id ASC")
                    .bind(run_id)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(rows)
    }

    async fn create_gate(&self, row: &QualityGateRunRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO quality_gate_runs (id, run_id, task_id, gate_type, command, working_directory, exit_code, \
             status, stdout_artifact_id, stderr_artifact_id, duration_ms, isolation_mode, execution_id, required, \
             started_at, finished_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.gate_type)
        .bind(&row.command)
        .bind(&row.working_directory)
        .bind(row.exit_code)
        .bind(&row.status)
        .bind(&row.stdout_artifact_id)
        .bind(&row.stderr_artifact_id)
        .bind(row.duration_ms)
        .bind(&row.isolation_mode)
        .bind(&row.execution_id)
        .bind(row.required)
        .bind(row.started_at)
        .bind(row.finished_at)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_gate(&self, row: &QualityGateRunRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE quality_gate_runs SET exit_code = ?, status = ?, stdout_artifact_id = ?, stderr_artifact_id = ?, \
             duration_ms = ?, isolation_mode = ?, execution_id = ?, required = ?, started_at = ?, finished_at = ? \
             WHERE id = ? AND run_id = ?",
        )
        .bind(row.exit_code)
        .bind(&row.status)
        .bind(&row.stdout_artifact_id)
        .bind(&row.stderr_artifact_id)
        .bind(row.duration_ms)
        .bind(&row.isolation_mode)
        .bind(&row.execution_id)
        .bind(row.required)
        .bind(row.started_at)
        .bind(row.finished_at)
        .bind(&row.id)
        .bind(&row.run_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_gates(&self, run_id: &str, task_id: Option<&str>) -> Result<Vec<QualityGateRunRow>, DbError> {
        let rows =
            match task_id {
                Some(task_id) => sqlx::query_as(
                    "SELECT * FROM quality_gate_runs WHERE run_id = ? AND task_id = ? ORDER BY created_at ASC, id ASC",
                )
                .bind(run_id)
                .bind(task_id)
                .fetch_all(&self.pool)
                .await?,
                None => {
                    sqlx::query_as("SELECT * FROM quality_gate_runs WHERE run_id = ? ORDER BY created_at ASC, id ASC")
                        .bind(run_id)
                        .fetch_all(&self.pool)
                        .await?
                }
            };
        Ok(rows)
    }

    async fn create_finding(&self, row: &ReviewFindingRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO review_findings (id, run_id, task_id, reviewer_agent_id, producer_agent_id, severity, \
             file_path, line_number, reason, suggestion, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ).bind(&row.id).bind(&row.run_id).bind(&row.task_id).bind(&row.reviewer_agent_id)
        .bind(&row.producer_agent_id).bind(&row.severity).bind(&row.file_path).bind(row.line_number)
        .bind(&row.reason).bind(&row.suggestion).bind(&row.status).bind(row.created_at)
        .bind(row.updated_at).execute(&self.pool).await?;
        Ok(())
    }

    async fn list_findings(&self, run_id: &str, task_id: &str) -> Result<Vec<ReviewFindingRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM review_findings WHERE run_id = ? AND task_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(run_id)
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_finding_status(&self, run_id: &str, finding_id: &str, status: &str) -> Result<bool, DbError> {
        let result = sqlx::query("UPDATE review_findings SET status = ?, updated_at = ? WHERE id = ? AND run_id = ?")
            .bind(status)
            .bind(now_ms())
            .bind(finding_id)
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert_delivery(&self, row: &DevelopmentDeliveryRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_deliveries (id, run_id, project_id, user_id, provider, repository, branch, \
             base_branch, commit_sha, status, push_status, pr_number, pr_url, pr_status, ci_status, review_status, \
             merge_status, report_json, last_error, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(run_id) DO UPDATE SET provider=excluded.provider, repository=excluded.repository, \
             branch=excluded.branch, base_branch=excluded.base_branch, commit_sha=excluded.commit_sha, \
             status=excluded.status, push_status=excluded.push_status, pr_number=excluded.pr_number, \
             pr_url=excluded.pr_url, pr_status=excluded.pr_status, ci_status=excluded.ci_status, \
             review_status=excluded.review_status, merge_status=excluded.merge_status, \
             report_json=excluded.report_json, last_error=excluded.last_error, updated_at=excluded.updated_at",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(&row.provider)
        .bind(&row.repository)
        .bind(&row.branch)
        .bind(&row.base_branch)
        .bind(&row.commit_sha)
        .bind(&row.status)
        .bind(&row.push_status)
        .bind(row.pr_number)
        .bind(&row.pr_url)
        .bind(&row.pr_status)
        .bind(&row.ci_status)
        .bind(&row.review_status)
        .bind(&row.merge_status)
        .bind(&row.report_json)
        .bind(&row.last_error)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_delivery(&self, user_id: &str, run_id: &str) -> Result<Option<DevelopmentDeliveryRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_deliveries WHERE user_id = ? AND run_id = ?")
                .bind(user_id)
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn upsert_ci_check(&self, row: &DevelopmentCiCheckRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_ci_checks (id, delivery_id, provider_check_id, name, status, details_url, \
             summary, rework_task_id, started_at, completed_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(delivery_id, provider_check_id) DO UPDATE SET name=excluded.name, status=excluded.status, \
             details_url=excluded.details_url, summary=excluded.summary, \
             rework_task_id=COALESCE(excluded.rework_task_id, development_ci_checks.rework_task_id), \
             started_at=excluded.started_at, completed_at=excluded.completed_at, updated_at=excluded.updated_at",
        )
        .bind(&row.id)
        .bind(&row.delivery_id)
        .bind(&row.provider_check_id)
        .bind(&row.name)
        .bind(&row.status)
        .bind(&row.details_url)
        .bind(&row.summary)
        .bind(&row.rework_task_id)
        .bind(row.started_at)
        .bind(row.completed_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_ci_checks(&self, delivery_id: &str) -> Result<Vec<DevelopmentCiCheckRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_ci_checks WHERE delivery_id = ? ORDER BY name ASC, provider_check_id ASC",
        )
        .bind(delivery_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_delivery_tag(&self, row: &DevelopmentDeliveryTagRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO development_delivery_tags \
             (id, delivery_id, user_id, name, commit_sha, remote_url, status, last_error, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(delivery_id, name) DO NOTHING",
        )
        .bind(&row.id)
        .bind(&row.delivery_id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.commit_sha)
        .bind(&row.remote_url)
        .bind(&row.status)
        .bind(&row.last_error)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn get_delivery_tag(
        &self,
        user_id: &str,
        delivery_id: &str,
        name: &str,
    ) -> Result<Option<DevelopmentDeliveryTagRow>, DbError> {
        Ok(
            sqlx::query_as(
                "SELECT * FROM development_delivery_tags WHERE user_id = ? AND delivery_id = ? AND name = ?",
            )
            .bind(user_id)
            .bind(delivery_id)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?,
        )
    }

    async fn list_delivery_tags(
        &self,
        user_id: &str,
        delivery_id: &str,
    ) -> Result<Vec<DevelopmentDeliveryTagRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_delivery_tags WHERE user_id = ? AND delivery_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .bind(delivery_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_delivery_tag_result(
        &self,
        user_id: &str,
        tag_id: &str,
        status: &str,
        remote_url: Option<&str>,
        last_error: Option<&str>,
        now: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_delivery_tags SET status = ?, remote_url = COALESCE(?, remote_url), \
             last_error = ?, updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(status)
        .bind(remote_url)
        .bind(last_error)
        .bind(now)
        .bind(tag_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn create_deployment(&self, row: &DevelopmentDeploymentRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO development_deployments \
             (id, deployment_key, run_id, project_id, user_id, environment, commit_sha, status, requested_by, \
              approved_by, approval_run_id, approval_environment, approval_commit_sha, approval_requester, \
              approval_deployment_key, approval_expires_at, approved_at, remote_id, attempt_count, last_error, \
              started_at, finished_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, deployment_key) DO NOTHING",
        )
        .bind(&row.id)
        .bind(&row.deployment_key)
        .bind(&row.run_id)
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(&row.environment)
        .bind(&row.commit_sha)
        .bind(&row.status)
        .bind(&row.requested_by)
        .bind(&row.approved_by)
        .bind(&row.approval_run_id)
        .bind(&row.approval_environment)
        .bind(&row.approval_commit_sha)
        .bind(&row.approval_requester)
        .bind(&row.approval_deployment_key)
        .bind(row.approval_expires_at)
        .bind(row.approved_at)
        .bind(&row.remote_id)
        .bind(row.attempt_count)
        .bind(&row.last_error)
        .bind(row.started_at)
        .bind(row.finished_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn get_deployment(
        &self,
        user_id: &str,
        deployment_id: &str,
    ) -> Result<Option<DevelopmentDeploymentRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_deployments WHERE user_id = ? AND id = ?")
                .bind(user_id)
                .bind(deployment_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn get_deployment_by_key(
        &self,
        user_id: &str,
        deployment_key: &str,
    ) -> Result<Option<DevelopmentDeploymentRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_deployments WHERE user_id = ? AND deployment_key = ?")
                .bind(user_id)
                .bind(deployment_key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_deployments(&self, user_id: &str, run_id: &str) -> Result<Vec<DevelopmentDeploymentRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_deployments WHERE user_id = ? AND run_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn approve_deployment(&self, user_id: &str, deployment_id: &str, now: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_deployments SET status = 'approved', approved_by = ?, approved_at = ?, updated_at = ? \
             WHERE id = ? AND user_id = ? AND status = 'pending_approval' AND approval_expires_at > ?",
        )
        .bind(user_id)
        .bind(now)
        .bind(now)
        .bind(deployment_id)
        .bind(user_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn claim_deployment(&self, user_id: &str, deployment_id: &str, now: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_deployments SET status = 'running', attempt_count = attempt_count + 1, \
             started_at = COALESCE(started_at, ?), updated_at = ? \
             WHERE id = ? AND user_id = ? AND status = 'approved' AND approval_expires_at > ?",
        )
        .bind(now)
        .bind(now)
        .bind(deployment_id)
        .bind(user_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn update_deployment_result(
        &self,
        user_id: &str,
        deployment_id: &str,
        status: &str,
        remote_id: Option<&str>,
        last_error: Option<&str>,
        now: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_deployments SET status = ?, remote_id = COALESCE(?, remote_id), last_error = ?, \
             finished_at = CASE WHEN ? IN ('succeeded', 'failed', 'cancelled') THEN ? ELSE finished_at END, updated_at = ? \
             WHERE id = ? AND user_id = ?",
        )
        .bind(status)
        .bind(remote_id)
        .bind(last_error)
        .bind(status)
        .bind(now)
        .bind(now)
        .bind(deployment_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn append_requirement_version(
        &self,
        row: &RequirementVersionRow,
        criteria: &[AcceptanceCriterionRow],
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO development_requirement_versions \
             (id, run_id, version, content, change_summary, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(row.version)
        .bind(&row.content)
        .bind(&row.change_summary)
        .bind(&row.created_by)
        .bind(row.created_at)
        .execute(&mut *tx)
        .await?;
        for criterion in criteria {
            sqlx::query(
                "INSERT INTO development_acceptance_criteria \
                 (id, run_id, requirement_version_id, ordinal, statement, required, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&criterion.id)
            .bind(&criterion.run_id)
            .bind(&criterion.requirement_version_id)
            .bind(criterion.ordinal)
            .bind(&criterion.statement)
            .bind(criterion.required)
            .bind(criterion.created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn list_requirement_versions(&self, run_id: &str) -> Result<Vec<RequirementVersionRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_requirement_versions WHERE run_id = ? ORDER BY version ASC")
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn list_active_criteria(&self, run_id: &str) -> Result<Vec<AcceptanceCriterionRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT c.* FROM development_acceptance_criteria c \
             JOIN development_requirement_versions v ON v.id = c.requirement_version_id \
             WHERE c.run_id = ? AND v.version = (SELECT MAX(version) FROM development_requirement_versions WHERE run_id = ?) \
             ORDER BY c.ordinal ASC",
        )
        .bind(run_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn append_plan_revision(&self, row: &PlanRevisionRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_plan_revisions \
             (id, run_id, revision, summary, content, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(row.revision)
        .bind(&row.summary)
        .bind(&row.content)
        .bind(&row.created_by)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_plan_revisions(&self, run_id: &str) -> Result<Vec<PlanRevisionRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_plan_revisions WHERE run_id = ? ORDER BY revision ASC")
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn map_task_criteria(&self, rows: &[TaskCriterionRow]) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for row in rows {
            sqlx::query(
                "INSERT INTO development_task_criteria (run_id, task_id, criterion_id, mapped_at) VALUES (?, ?, ?, ?)",
            )
            .bind(&row.run_id)
            .bind(&row.task_id)
            .bind(&row.criterion_id)
            .bind(row.mapped_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn list_task_criteria(&self, run_id: &str) -> Result<Vec<TaskCriterionRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT run_id, task_id, criterion_id, mapped_at FROM development_task_criteria \
             WHERE run_id = ? ORDER BY mapped_at ASC, task_id ASC, criterion_id ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_completion_evidence(&self, row: &CompletionEvidenceRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_completion_evidence \
             (id, run_id, task_id, criterion_id, evidence_type, artifact_id, reference, accepted, reviewer_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.criterion_id)
        .bind(&row.evidence_type)
        .bind(&row.artifact_id)
        .bind(&row.reference)
        .bind(row.accepted)
        .bind(&row.reviewer_id)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_completion_evidence(&self, run_id: &str) -> Result<Vec<CompletionEvidenceRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_completion_evidence WHERE run_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_single_run_workspace(&self, row: &SingleRunWorkspaceRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_single_run_workspaces \
             (run_id, user_id, project_id, baseline_commit, initial_diff_checksum, initial_diff_path, \
              workspace_lease_id, workspace_path, branch, candidate_commit, safe_point, cleanup_status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.run_id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.baseline_commit)
        .bind(&row.initial_diff_checksum)
        .bind(&row.initial_diff_path)
        .bind(&row.workspace_lease_id)
        .bind(&row.workspace_path)
        .bind(&row.branch)
        .bind(&row.candidate_commit)
        .bind(&row.safe_point)
        .bind(&row.cleanup_status)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_single_run_workspace(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<Option<SingleRunWorkspaceRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_single_run_workspaces WHERE run_id = ? AND user_id = ?")
                .bind(run_id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn update_single_run_workspace(
        &self,
        run_id: &str,
        user_id: &str,
        candidate_commit: Option<&str>,
        cleanup_status: &str,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_single_run_workspaces SET candidate_commit = COALESCE(?, candidate_commit), \
             cleanup_status = ?, updated_at = ? WHERE run_id = ? AND user_id = ?",
        )
        .bind(candidate_commit)
        .bind(cleanup_status)
        .bind(now_ms())
        .bind(run_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}
