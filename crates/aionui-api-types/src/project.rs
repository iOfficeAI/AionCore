use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

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
