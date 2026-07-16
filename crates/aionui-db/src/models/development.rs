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
