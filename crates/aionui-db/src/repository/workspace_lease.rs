use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::AgentWorkspaceLeaseRow;

#[derive(Debug, Clone, Default)]
pub struct AgentWorkspaceLeaseUpdate {
    pub lease_status: Option<String>,
    pub cleanup_status: Option<String>,
    pub conflict_files: Option<String>,
    pub last_error: Option<Option<String>>,
    pub released_at: Option<Option<TimestampMs>>,
}

#[async_trait::async_trait]
pub trait IAgentWorkspaceLeaseRepository: Send + Sync {
    async fn create(&self, row: &AgentWorkspaceLeaseRow) -> Result<(), DbError>;
    async fn get(&self, lease_id: &str) -> Result<Option<AgentWorkspaceLeaseRow>, DbError>;
    async fn get_for_team_slot(&self, team_id: &str, slot_id: &str) -> Result<Option<AgentWorkspaceLeaseRow>, DbError>;
    async fn list_for_team(&self, team_id: &str) -> Result<Vec<AgentWorkspaceLeaseRow>, DbError>;
    async fn list_reconcile_candidates(&self) -> Result<Vec<AgentWorkspaceLeaseRow>, DbError>;
    async fn update(&self, lease_id: &str, update: &AgentWorkspaceLeaseUpdate) -> Result<(), DbError>;
}
