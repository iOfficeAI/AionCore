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
