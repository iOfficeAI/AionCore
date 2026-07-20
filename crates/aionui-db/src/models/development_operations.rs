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
    pub allowed_commands_json: String,
    pub protected_paths_json: String,
    pub allowed_network_hosts_json: String,
    pub protected_branches_json: String,
    pub dangerous_confirmation_count: i64,
    pub max_duration_ms: i64,
    pub max_parallel_agents: i64,
    pub max_retries: i64,
    pub max_cost_microunits: i64,
    pub max_total_tokens: i64,
    pub fallback_model: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ExecutionResourceLeaseRow {
    pub id: String,
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
    pub status: String,
    pub accepts_work: i64,
    pub owner_instance_id: String,
    pub heartbeat_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub cleanup_order: i64,
    pub cleanup_status: Option<String>,
    pub cleanup_result: Option<String>,
    pub recovery_decision: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub terminal_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentSecretRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub name: String,
    pub encrypted_value: String,
    pub key_version: String,
    pub status: String,
    pub expires_at: Option<TimestampMs>,
    pub revoked_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentSecretGrantRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub secret_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub environment_key: String,
    pub status: String,
    pub expires_at: Option<TimestampMs>,
    pub revoked_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentModelPriceRow {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub input_per_million_microunits: i64,
    pub output_per_million_microunits: i64,
    pub cache_read_per_million_microunits: i64,
    pub cache_write_per_million_microunits: i64,
    pub source_id: String,
    pub version: String,
    pub effective_at: TimestampMs,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentPricedUsageEventRow {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub conversation_id: Option<String>,
    pub agent_id: Option<String>,
    pub team_id: Option<String>,
    pub usage_type: String,
    pub source: String,
    pub confidence: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_microunits: i64,
    pub cost_status: String,
    pub cost_origin: String,
    pub price_source_id: Option<String>,
    pub price_version: Option<String>,
    pub price_effective_at: Option<TimestampMs>,
    pub duration_ms: i64,
    pub retry_count: i64,
    pub metadata_json: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageDimension {
    Project(String),
    Run(String),
    Task(String),
    Conversation(String),
    Agent(String),
    Team(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevelopmentUsageDimensionSummary {
    pub event_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub known_cost_microunits: i64,
    pub unknown_cost_events: i64,
    pub duration_ms: i64,
    pub retry_count: i64,
}
