use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentRunRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub team_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_user_id: Option<String>,
    pub execution_mode: String,
    pub status: String,
    pub request_summary: String,
    pub acceptance_criteria: String,
    pub baseline_commit: Option<String>,
    pub integration_branch: Option<String>,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentTaskRow {
    pub id: String,
    pub team_id: String,
    pub run_id: Option<String>,
    pub subject: String,
    pub description: Option<String>,
    pub status: String,
    pub owner: Option<String>,
    pub blocked_by: String,
    pub blocks: String,
    pub metadata: Option<String>,
    pub acceptance_criteria: String,
    pub task_type: String,
    pub risk_level: String,
    pub assigned_workspace_lease_id: Option<String>,
    pub review_status: String,
    pub verification_status: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentRunRoleRow {
    pub run_id: String,
    pub slot_id: String,
    pub role: String,
    pub assigned_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskArtifactRow {
    pub id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub artifact_type: String,
    pub path_or_uri: String,
    pub checksum: String,
    pub producer_agent_id: Option<String>,
    pub metadata: Option<String>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct QualityGateRunRow {
    pub id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub gate_type: String,
    pub command: String,
    pub working_directory: String,
    pub exit_code: Option<i64>,
    pub status: String,
    pub stdout_artifact_id: Option<String>,
    pub stderr_artifact_id: Option<String>,
    pub duration_ms: Option<i64>,
    pub isolation_mode: String,
    pub execution_id: Option<String>,
    pub required: bool,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ReviewFindingRow {
    pub id: String,
    pub run_id: String,
    pub task_id: String,
    pub reviewer_agent_id: String,
    pub producer_agent_id: Option<String>,
    pub severity: String,
    pub file_path: Option<String>,
    pub line_number: Option<i64>,
    pub reason: String,
    pub suggestion: Option<String>,
    pub status: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentDeliveryRow {
    pub id: String,
    pub run_id: String,
    pub project_id: String,
    pub user_id: String,
    pub provider: String,
    pub repository: Option<String>,
    pub branch: String,
    pub base_branch: String,
    pub commit_sha: Option<String>,
    pub status: String,
    pub push_status: String,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub pr_status: String,
    pub ci_status: String,
    pub review_status: String,
    pub merge_status: String,
    pub report_json: String,
    pub last_error: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentCiCheckRow {
    pub id: String,
    pub delivery_id: String,
    pub provider_check_id: String,
    pub name: String,
    pub status: String,
    pub details_url: Option<String>,
    pub summary: Option<String>,
    pub rework_task_id: Option<String>,
    pub started_at: Option<TimestampMs>,
    pub completed_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct RequirementVersionRow {
    pub id: String,
    pub run_id: String,
    pub version: i64,
    pub content: String,
    pub change_summary: Option<String>,
    pub created_by: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct AcceptanceCriterionRow {
    pub id: String,
    pub run_id: String,
    pub requirement_version_id: String,
    pub ordinal: i64,
    pub statement: String,
    pub required: bool,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct PlanRevisionRow {
    pub id: String,
    pub run_id: String,
    pub revision: i64,
    pub summary: String,
    pub content: String,
    pub created_by: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskCriterionRow {
    pub run_id: String,
    pub task_id: String,
    pub criterion_id: String,
    pub mapped_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct CompletionEvidenceRow {
    pub id: String,
    pub run_id: String,
    pub task_id: String,
    pub criterion_id: String,
    pub evidence_type: String,
    pub artifact_id: Option<String>,
    pub reference: String,
    pub accepted: bool,
    pub reviewer_id: Option<String>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct SingleRunWorkspaceRow {
    pub run_id: String,
    pub user_id: String,
    pub project_id: String,
    pub baseline_commit: String,
    pub initial_diff_checksum: String,
    pub initial_diff_path: String,
    pub workspace_lease_id: Option<String>,
    pub workspace_path: Option<String>,
    pub branch: Option<String>,
    pub candidate_commit: Option<String>,
    pub safe_point: String,
    pub cleanup_status: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
