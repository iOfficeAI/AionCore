use aionui_common::{TimestampMs, now_ms};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{DevelopmentRunRow, DevelopmentTaskRow, QualityGateRunRow, ReviewFindingRow, TaskArtifactRow};
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
             status, stdout_artifact_id, stderr_artifact_id, duration_ms, required, started_at, finished_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ).bind(&row.id).bind(&row.run_id).bind(&row.task_id).bind(&row.gate_type).bind(&row.command)
        .bind(&row.working_directory).bind(row.exit_code).bind(&row.status).bind(&row.stdout_artifact_id)
        .bind(&row.stderr_artifact_id).bind(row.duration_ms).bind(row.required).bind(row.started_at)
        .bind(row.finished_at).bind(row.created_at).execute(&self.pool).await?;
        Ok(())
    }

    async fn update_gate(&self, row: &QualityGateRunRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE quality_gate_runs SET exit_code = ?, status = ?, stdout_artifact_id = ?, stderr_artifact_id = ?, \
             duration_ms = ?, required = ?, started_at = ?, finished_at = ? WHERE id = ? AND run_id = ?",
        )
        .bind(row.exit_code)
        .bind(&row.status)
        .bind(&row.stdout_artifact_id)
        .bind(&row.stderr_artifact_id)
        .bind(row.duration_ms)
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
}
