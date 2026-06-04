use aionui_ai_agent::AcpError;
use aionui_common::AppError;
use aionui_db::DbError;

/// Application-level error contract for the conversation domain.
///
/// This type may preserve structured lower-layer errors for domain decisions,
/// but HTTP and WebSocket boundaries must map it through an explicit public
/// output mapper. Do not render `ConversationError::Acp` directly to clients.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Conversation not found: {id}")]
    NotFound { id: String },

    #[error("Message not found: {id}")]
    MessageNotFound { id: String },

    #[error("Artifact not found: {id}")]
    ArtifactNotFound { id: String },

    #[error("Active agent not found for conversation: {conversation_id}")]
    ActiveAgentNotFound { conversation_id: String },

    #[error("Conversation is archived: {reason}")]
    Archived { id: String, reason: String },

    #[error("Bad request: {reason}")]
    BadRequest { reason: String },

    #[error("Conversation is busy: {reason}")]
    Busy { reason: String },

    #[error("Forbidden: {reason}")]
    Forbidden { reason: String },

    #[error("ACP error")]
    Acp(#[from] AcpError),

    #[error("{0}")]
    App(#[source] AppError),
}

impl From<AppError> for ConversationError {
    fn from(error: AppError) -> Self {
        match error {
            AppError::BadRequest(reason) => Self::BadRequest { reason },
            AppError::Conflict(reason) => Self::Busy { reason },
            AppError::Forbidden(reason) => Self::Forbidden { reason },
            other => Self::App(other),
        }
    }
}

impl From<DbError> for ConversationError {
    fn from(error: DbError) -> Self {
        AppError::from(error).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

    fn assert_from_acp<T: From<AcpError>>() {}

    fn assert_from_db<T: From<DbError>>() {}

    #[test]
    fn conversation_error_is_error_contract() {
        assert_error::<ConversationError>();
    }

    #[test]
    fn conversation_error_has_acp_from_impl() {
        assert_from_acp::<ConversationError>();
    }

    #[test]
    fn conversation_error_has_db_from_impl() {
        assert_from_db::<ConversationError>();
    }
}
