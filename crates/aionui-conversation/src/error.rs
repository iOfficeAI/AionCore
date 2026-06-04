use aionui_ai_agent::AcpError;
use aionui_common::ApiError;
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

    #[error("Not found: {reason}")]
    NotFoundReason { reason: String },

    #[error("Unauthorized: {reason}")]
    Unauthorized { reason: String },

    #[error("Rate limited")]
    RateLimited,

    #[error("Bad gateway: {reason}")]
    BadGateway { reason: String },

    #[error("Request timeout: {reason}")]
    Timeout { reason: String },

    #[error("Unprocessable entity: {reason}")]
    Unprocessable { reason: String },

    #[error("Internal error: {reason}")]
    Internal { reason: String },

    #[error("Workspace path contains whitespace: {path}")]
    WorkspacePathContainsWhitespace { path: String },

    #[error("Workspace path contains whitespace and is unsupported at runtime: {path}")]
    WorkspacePathContainsWhitespaceRuntimeUnsupported { path: String },

    #[error("ACP error")]
    Acp(#[from] AcpError),
}

impl From<ApiError> for ConversationError {
    fn from(error: ApiError) -> Self {
        match error {
            ApiError::NotFound(reason) => Self::NotFoundReason { reason },
            ApiError::BadRequest(reason) => Self::BadRequest { reason },
            ApiError::Unauthorized(reason) => Self::Unauthorized { reason },
            ApiError::Forbidden(reason) => Self::Forbidden { reason },
            ApiError::Conflict(reason) => Self::Busy { reason },
            ApiError::RateLimited => Self::RateLimited,
            ApiError::Internal(reason) => Self::Internal { reason },
            ApiError::BadGateway(reason) => Self::BadGateway { reason },
            ApiError::Timeout(reason) => Self::Timeout { reason },
            ApiError::UnprocessableEntity(reason) => Self::Unprocessable { reason },
            ApiError::ConversationArchived(reason) => Self::Archived {
                id: String::new(),
                reason,
            },
            ApiError::WorkspacePathContainsWhitespace(path) => Self::WorkspacePathContainsWhitespace { path },
            ApiError::WorkspacePathContainsWhitespaceRuntimeUnsupported(path) => {
                Self::WorkspacePathContainsWhitespaceRuntimeUnsupported { path }
            }
        }
    }
}

impl From<DbError> for ConversationError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::NotFound(reason) => Self::NotFoundReason { reason },
            DbError::Conflict(reason) => Self::Busy { reason },
            DbError::Query(e) => Self::Internal {
                reason: format!("Database error: {e}"),
            },
            DbError::Migration(e) => Self::Internal {
                reason: format!("Migration error: {e}"),
            },
            DbError::Init(reason) => Self::Internal {
                reason: format!("Database init error: {reason}"),
            },
        }
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
