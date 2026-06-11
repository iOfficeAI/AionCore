use aionui_conversation::{
    ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationError, ConversationService,
};
use aionui_team::{
    AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest, AgentTurnStatus,
};
use async_trait::async_trait;

pub struct TeamConversationTurnAdapter {
    conversation_service: ConversationService,
}

impl TeamConversationTurnAdapter {
    pub fn new(conversation_service: ConversationService) -> Self {
        Self { conversation_service }
    }
}

#[async_trait]
impl AgentTurnExecutionPort for TeamConversationTurnAdapter {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError> {
        let outcome = self
            .conversation_service
            .run_agent_turn(ConversationAgentTurnRequest {
                user_id: request.user_id,
                conversation_id: request.conversation_id,
                content: request.content,
                files: request.files,
                inject_skills: Vec::new(),
            })
            .await
            .map_err(map_conversation_turn_error)?;

        Ok(AgentTurnOutcome {
            conversation_id: outcome.conversation_id,
            turn_id: outcome.turn_id,
            status: match outcome.status {
                ConversationAgentTurnStatus::Completed => AgentTurnStatus::Completed,
                ConversationAgentTurnStatus::Failed => AgentTurnStatus::Failed,
            },
            runtime: Some(outcome.runtime),
        })
    }
}

fn map_conversation_turn_error(error: ConversationError) -> AgentTurnExecutionError {
    match error {
        ConversationError::Busy { reason } => AgentTurnExecutionError::Skipped { reason },
        other => AgentTurnExecutionError::Failed {
            reason: other.to_string(),
        },
    }
}
