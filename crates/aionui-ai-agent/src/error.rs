use crate::protocol::error::AcpError;

/// Crate-owned error model for ai-agent business, runtime, and protocol
/// orchestration code.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Conflict: {0}")]
    Conflict(String),
    #[error("Bad gateway: {0}")]
    BadGateway(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Rate limited")]
    RateLimited,
    #[error("Conversation archived: {0}")]
    ConversationArchived(String),
    #[error("Workspace path is unavailable during execution: {0}")]
    WorkspacePathRuntimeUnavailable(String),
    #[error("Internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Acp(#[from] AcpError),
}

impl AgentError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Unauthorized(message.into())
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden(message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::BadGateway(message.into())
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Timeout(message.into())
    }

    pub fn conversation_archived(message: impl Into<String>) -> Self {
        Self::ConversationArchived(message.into())
    }

    pub fn workspace_path_runtime_unavailable(path: impl Into<String>) -> Self {
        Self::WorkspacePathRuntimeUnavailable(path.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub(crate) fn public_message(&self) -> String {
        match self {
            Self::BadRequest(message)
            | Self::Unauthorized(message)
            | Self::Forbidden(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::BadGateway(message)
            | Self::Timeout(message)
            | Self::ConversationArchived(message)
            | Self::WorkspacePathRuntimeUnavailable(message)
            | Self::Internal(message) => message.clone(),
            Self::RateLimited => "Rate limited".to_owned(),
            Self::Acp(err) => err.to_string(),
        }
    }
}
