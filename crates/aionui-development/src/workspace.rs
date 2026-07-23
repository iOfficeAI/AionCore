use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareDevelopmentWorkspace {
    pub user_id: String,
    pub run_id: String,
    pub repository_path: String,
    pub baseline_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedDevelopmentWorkspace {
    pub lease_id: String,
    pub workspace_path: String,
    pub branch: String,
    pub safe_point: String,
}

#[async_trait]
pub trait DevelopmentWorkspacePort: Send + Sync {
    async fn prepare(&self, input: PrepareDevelopmentWorkspace) -> Result<PreparedDevelopmentWorkspace, String>;
    async fn restore(&self, lease_id: &str, safe_point: &str) -> Result<String, String>;
}
