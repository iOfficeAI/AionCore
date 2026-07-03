use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aionui_api_types::{ConversationRuntimeSummary, TeamRunTargetRole};
use async_trait::async_trait;

use crate::error::TeamError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamConversationBindingLookup {
    pub conversation_id: String,
    pub user_id: String,
    pub team_id: Option<String>,
    pub slot_id: Option<String>,
    pub role: Option<String>,
}

#[async_trait]
pub trait TeamConversationLookupPort: Send + Sync {
    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, TeamError>;
}

#[async_trait]
pub trait TeamAssistantCatalogPort: Send + Sync {
    async fn list_team_selectable_assistants(&self) -> Result<Vec<TeamAssistantCatalogEntry>, TeamError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamAssistantCatalogEntry {
    pub assistant_id: String,
    pub name: String,
    pub backend: String,
    pub description: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTurnSource {
    Mailbox {
        unread_message_ids: Vec<String>,
        unread_count: usize,
    },
}

#[derive(Clone)]
pub struct AgentTurnRequest {
    pub team_run_id: Option<String>,
    pub team_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub user_id: String,
    pub content: String,
    pub files: Vec<String>,
    pub source: AgentTurnSource,
    pub on_started: Option<AgentTurnStartedCallback>,
}

impl fmt::Debug for AgentTurnRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTurnRequest")
            .field("team_run_id", &self.team_run_id)
            .field("team_id", &self.team_id)
            .field("slot_id", &self.slot_id)
            .field("role", &self.role)
            .field("conversation_id", &self.conversation_id)
            .field("user_id", &self.user_id)
            .field("files", &self.files)
            .field("source", &self.source)
            .field("has_on_started", &self.on_started.is_some())
            .finish_non_exhaustive()
    }
}

pub type AgentTurnStartedCallback =
    Arc<dyn Fn(AgentTurnStarted) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurnStarted {
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub turn_id: String,
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

#[async_trait]
pub trait AgentTurnCancellationPort: Send + Sync {
    async fn cancel_agent_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<(), AgentTurnExecutionError>;
}
