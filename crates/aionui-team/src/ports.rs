use aionui_api_types::ConversationRuntimeSummary;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTurnSource {
    Mailbox {
        unread_message_ids: Vec<String>,
        unread_count: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurnRequest {
    pub team_id: String,
    pub slot_id: String,
    pub conversation_id: String,
    pub user_id: String,
    pub content: String,
    pub files: Vec<String>,
    pub source: AgentTurnSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTurnStatus {
    Completed,
    Failed,
    Skipped,
}

impl AgentTurnStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }
}

#[derive(Debug, Clone)]
pub struct AgentTurnOutcome {
    pub conversation_id: String,
    pub turn_id: String,
    pub status: AgentTurnStatus,
    pub runtime: Option<ConversationRuntimeSummary>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentTurnExecutionError {
    #[error("agent turn skipped: {reason}")]
    Skipped { reason: String },
    #[error("agent turn failed: {reason}")]
    Failed { reason: String },
}

#[async_trait]
pub trait AgentTurnExecutionPort: Send + Sync {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError>;
}
