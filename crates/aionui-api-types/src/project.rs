use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DirtyWorktreeChoice {
    Preserve,
    Snapshot,
    #[default]
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositorySource {
    Local {
        path: String,
    },
    Clone {
        url: String,
        destination_name: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        credential_reference: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepositoryOnboardingInput {
    pub name: String,
    pub source: RepositorySource,
    #[serde(default)]
    pub dirty_worktree_choice: DirtyWorktreeChoice,
    #[serde(default = "default_project_type")]
    pub project_type: String,
}

fn default_project_type() -> String {
    "unknown".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySubmodule {
    pub path: String,
    pub url: Option<String>,
    pub initialized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepositoryFacts {
    pub local_path: String,
    pub repository_url: Option<String>,
    pub default_branch: Option<String>,
    pub baseline_commit: Option<String>,
    pub dirty: bool,
    pub dirty_worktree_choice: DirtyWorktreeChoice,
    pub dirty_snapshot_ref: Option<String>,
    pub credential_reference: Option<String>,
    pub languages: Vec<String>,
    pub package_managers: Vec<String>,
    pub rules_files: Vec<String>,
    pub monorepo_packages: Vec<String>,
    pub submodules: Vec<RepositorySubmodule>,
    pub lfs_detected: bool,
    pub detected_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKnowledgeFact {
    pub kind: String,
    pub name: String,
    pub qualified_name: Option<String>,
    pub source_path: String,
    pub source_line: Option<i64>,
    pub indexed_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKnowledgeStatus {
    pub project_id: String,
    pub provider: String,
    pub provider_project_name: String,
    pub provider_version: Option<String>,
    pub status: String,
    pub generation: i64,
    pub source_commit: Option<String>,
    pub indexed_at: Option<TimestampMs>,
    pub changed_paths: Vec<String>,
    pub error_category: Option<String>,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskContext {
    pub id: String,
    pub project_id: String,
    pub provider_project_name: String,
    pub generation: i64,
    pub query: String,
    pub symbols: Vec<String>,
    pub callers: Vec<String>,
    pub tests: Vec<String>,
    pub routes: Vec<String>,
    pub data_entities: Vec<String>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskContextRequest {
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectExportManifest {
    pub format_version: u32,
    pub schema_version: i64,
    pub app_version: String,
    pub source_instance_id: String,
    pub exported_at: TimestampMs,
    pub project_id: String,
    pub record_counts: BTreeMap<String, usize>,
    pub payload_checksum: String,
    pub signer_public_key: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectExportBundle {
    pub manifest: ProjectExportManifest,
    pub records: BTreeMap<String, Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportProjectBundleRequest {
    pub bundle: ProjectExportBundle,
    pub local_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectImportReport {
    pub project_id: String,
    pub owner_id: String,
    pub imported: bool,
    pub imported_counts: BTreeMap<String, usize>,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicyInput {
    pub conversation_history_days: i64,
    pub artifact_days: i64,
    pub evaluation_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentRetentionPolicy {
    pub user_id: String,
    pub project_id: String,
    pub conversation_history_days: i64,
    pub artifact_days: i64,
    pub evaluation_days: i64,
    pub immutable_audit_log: bool,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionCleanupRequest {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub confirmation_count: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionCleanupReport {
    pub execution_id: Option<String>,
    pub project_id: String,
    pub dry_run: bool,
    pub message_count: i64,
    pub artifact_count: i64,
    pub evaluation_count: i64,
    pub audit_events_retained: i64,
    pub completed_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInstanceSummary {
    pub instance_id: String,
    pub schema_version: i64,
    pub app_version: String,
    pub first_started_at: TimestampMs,
    pub last_started_at: TimestampMs,
    pub data_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationRecordInput {
    pub project_id: String,
    pub release_id: String,
    pub scenario_id: String,
    pub result: String,
    pub duration_ms: i64,
    pub failure_category: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_microunits: i64,
    pub cost_source: String,
    #[serde(default)]
    pub accepted_baseline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentEvaluation {
    pub id: String,
    pub user_id: String,
    pub project_id: String,
    pub release_id: String,
    pub scenario_id: String,
    pub result: String,
    pub duration_ms: i64,
    pub failure_category: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_microunits: i64,
    pub cost_source: String,
    pub accepted_baseline: bool,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationComparisonRequest {
    pub project_id: String,
    pub release_id: String,
    pub required_scenarios: Vec<String>,
    pub max_duration_regression_percent: i64,
    pub max_cost_regression_percent: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationRegression {
    pub scenario_id: String,
    pub category: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationComparison {
    pub allowed: bool,
    pub release_id: String,
    pub baseline_release_ids: Vec<String>,
    pub regressions: Vec<EvaluationRegression>,
}
