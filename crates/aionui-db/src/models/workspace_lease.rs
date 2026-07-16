use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq, Eq)]
pub struct AgentWorkspaceLeaseRow {
    pub id: String,
    pub team_id: String,
    pub user_id: String,
    pub slot_id: String,
    pub workspace_mode: String,
    pub repository_path: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub base_commit: String,
    pub allowed_paths: String,
    pub lease_status: String,
    pub cleanup_status: String,
    pub conflict_files: String,
    pub last_error: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub released_at: Option<TimestampMs>,
}
