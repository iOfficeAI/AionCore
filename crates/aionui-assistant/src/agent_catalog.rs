use aionui_api_types::AgentManagementRow;

use crate::error::AssistantError;

#[async_trait::async_trait]
pub trait AssistantAgentCatalogPort: Send + Sync {
    async fn list_management_agents(&self) -> Result<Vec<AgentManagementRow>, AssistantError>;
}
