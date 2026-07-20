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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectRepositoryFactsRow {
    pub project_id: String,
    pub repository_url: Option<String>,
    pub default_branch: Option<String>,
    pub baseline_commit: Option<String>,
    pub repository_dirty: bool,
    pub dirty_worktree_choice: String,
    pub dirty_snapshot_ref: Option<String>,
    pub credential_reference: Option<String>,
    pub detected_languages_json: String,
    pub detected_package_managers_json: String,
    pub detected_rules_files_json: String,
    pub monorepo_packages_json: String,
    pub submodules_json: String,
    pub lfs_detected: bool,
    pub detected_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectKnowledgeIndexRow {
    pub project_id: String,
    pub provider: String,
    pub provider_project_name: String,
    pub provider_version: Option<String>,
    pub status: String,
    pub generation: i64,
    pub source_commit: Option<String>,
    pub indexed_at: Option<TimestampMs>,
    pub changed_paths_json: String,
    pub error_category: Option<String>,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectKnowledgeFactRow {
    pub id: String,
    pub project_id: String,
    pub generation: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: Option<String>,
    pub source_path: String,
    pub source_line: Option<i64>,
    pub indexed_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectKnowledgeContextRow {
    pub id: String,
    pub project_id: String,
    pub provider_project_name: String,
    pub generation: i64,
    pub query: String,
    pub symbols_json: String,
    pub callers_json: String,
    pub tests_json: String,
    pub routes_json: String,
    pub data_entities_json: String,
    pub created_at: TimestampMs,
}
