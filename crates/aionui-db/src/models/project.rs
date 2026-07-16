use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub local_path: String,
    pub repository_url: Option<String>,
    pub default_branch: Option<String>,
    pub project_type: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectCommandProfileRow {
    pub project_id: String,
    pub install_command: Option<String>,
    pub format_command: Option<String>,
    pub lint_command: Option<String>,
    pub typecheck_command: Option<String>,
    pub unit_test_command: Option<String>,
    pub integration_test_command: Option<String>,
    pub e2e_command: Option<String>,
    pub build_command: Option<String>,
    pub security_scan_command: Option<String>,
    pub command_timeout_seconds: i64,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectRuntimeProfileRow {
    pub project_id: String,
    pub environment_kind: String,
    pub language: Option<String>,
    pub package_manager: Option<String>,
    pub runtime_version: Option<String>,
    pub env_keys: String,
    pub metadata: String,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectResourceLinkRow {
    pub project_id: String,
    pub user_id: String,
    pub resource_type: String,
    pub resource_id: String,
    pub created_at: TimestampMs,
}
