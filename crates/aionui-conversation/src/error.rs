use aionui_ai_agent::AcpError;
use aionui_common::AppError;

/// Application-level error contract for the conversation domain.
///
/// This type may preserve structured lower-layer errors for domain decisions,
/// but HTTP and WebSocket boundaries must map it through an explicit public
/// output mapper. Do not render `ConversationError::Acp` directly to clients.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Conversation not found: {id}")]
    NotFound { id: String },

    #[error("Conversation is archived: {id}")]
    Archived { id: String },

    #[error("Forbidden: {reason}")]
    Forbidden { reason: String },

    #[error("ACP error")]
    Acp(#[from] AcpError),

    #[error(transparent)]
    App(#[from] AppError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

    fn assert_from_acp<T: From<AcpError>>() {}

    #[test]
    fn conversation_error_is_error_contract() {
        assert_error::<ConversationError>();
    }

    #[test]
    fn conversation_error_has_acp_from_impl() {
        assert_from_acp::<ConversationError>();
    }
}
