//! Workspace information shared across factory builders. The conversation
//! domain has already decoded raw DB state into typed context before this
//! layer sees it.

use crate::error::AgentError;
use crate::session_context::AgentSessionContext;

pub(super) struct FactoryContext {
    pub conversation_id: String,
    pub workspace: String,
    pub is_custom_workspace: bool,
}

impl FactoryContext {
    pub async fn resolve(context: &AgentSessionContext) -> Result<Self, AgentError> {
        Ok(Self {
            conversation_id: context.conversation.conversation_id.clone(),
            workspace: context.workspace.path.clone(),
            is_custom_workspace: context.workspace.is_custom,
        })
    }
}
