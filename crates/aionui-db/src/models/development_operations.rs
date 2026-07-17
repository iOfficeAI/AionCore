use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentPolicyRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub isolation_mode: String,
    pub container_image: Option<String>,
    pub devcontainer_config_path: Option<String>,
    pub container_cpu_millis: i64,
    pub container_memory_mb: i64,
    pub container_pids_limit: i64,
    pub network_mode: String,
    pub allowed_secret_keys_json: String,
    pub max_duration_ms: i64,
    pub max_parallel_agents: i64,
    pub max_retries: i64,
    pub max_cost_microunits: i64,
    pub alert_percent: i64,
    pub over_limit_action: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentUsageEventRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub usage_type: String,
    pub source: String,
    pub confidence: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_microunits: i64,
    pub duration_ms: i64,
    pub retry_count: i64,
    pub metadata_json: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentUsageSummary {
    pub event_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_microunits: i64,
    pub duration_ms: i64,
    pub retry_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentAuditEventRow {
    pub id: String,
    pub user_id: String,
    pub actor_type: String,
    pub actor_id: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub project_id: String,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub result: String,
    pub redacted_payload_json: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentAlertRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub run_id: Option<String>,
    pub alert_type: String,
    pub severity: String,
    pub status: String,
    pub message: String,
    pub dedupe_key: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub resolved_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentRecoveryRecordRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub run_id: Option<String>,
    pub recovery_key: String,
    pub finding: String,
    pub decision: String,
    pub status_before: Option<String>,
    pub status_after: Option<String>,
    pub details_json: String,
    pub created_at: TimestampMs,
}
