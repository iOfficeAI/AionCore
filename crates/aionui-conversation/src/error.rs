use aionui_ai_agent::{AcpError, AgentError};
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

    #[error("ACP config option confirmation timed out")]
    ConfigConfirmationTimeout {
        conversation_id: String,
        option_id: String,
        requested: String,
        last_observed: Option<String>,
    },

    #[error("ACP config update is already in progress")]
    ConfigUpdateInProgress {
        conversation_id: String,
        option_id: String,
        requested: String,
    },

    #[error("Team runtime is required for conversation: {conversation_id}")]
    TeamRuntimeRequired { conversation_id: String, team_id: String },

    #[error("Unprocessable entity: {reason}")]
    Unprocessable { reason: String },

    #[error("Internal error: {reason}")]
    Internal { reason: String },

    #[error("Workspace path is unavailable: {path}")]
    WorkspacePathUnavailable { path: String },

    #[error("Workspace path is unavailable during execution: {path}")]
    WorkspacePathRuntimeUnavailable { path: String },

    #[error("OpenClaw Gateway is not reachable: {detail}")]
    OpenClawGatewayUnreachable { detail: String },

    #[error("ACP error")]
    Acp(#[from] AcpError),
}

impl ConversationError {
    pub(crate) fn internal(reason: impl Into<String>) -> Self {
        Self::Internal { reason: reason.into() }
    }

    pub(crate) fn bad_request(reason: impl Into<String>) -> Self {
        Self::BadRequest { reason: reason.into() }
    }

    pub(crate) fn not_found_reason(reason: impl Into<String>) -> Self {
        Self::NotFoundReason { reason: reason.into() }
    }

    pub(crate) fn to_agent_error(&self) -> AgentError {
        match self {
            Self::NotFound { id } => AgentError::not_found(format!("Conversation {id} not found")),
            Self::MessageNotFound { id } => AgentError::not_found(format!("Message {id} not found")),
            Self::ArtifactNotFound { id } => AgentError::not_found(format!("Artifact {id} not found")),
            Self::ActiveAgentNotFound { .. } => AgentError::not_found("No active agent for this conversation"),
            Self::Archived { reason, .. } => AgentError::conversation_archived(reason.clone()),
            Self::BadRequest { reason } => AgentError::bad_request(reason.clone()),
            Self::Busy { reason } => AgentError::conflict(reason.clone()),
            Self::Forbidden { reason } => AgentError::forbidden(reason.clone()),
            Self::NotFoundReason { reason } => AgentError::not_found(reason.clone()),
            Self::Unauthorized { reason } => AgentError::unauthorized(reason.clone()),
            Self::RateLimited => AgentError::RateLimited,
            Self::BadGateway { reason } => AgentError::bad_gateway(reason.clone()),
            Self::Timeout { reason } => AgentError::timeout(reason.clone()),
            Self::ConfigConfirmationTimeout { .. } => AgentError::timeout("ACP config option confirmation timed out"),
            Self::ConfigUpdateInProgress { .. } => AgentError::conflict("ACP config update is already in progress"),
            Self::TeamRuntimeRequired { .. } => {
                AgentError::conflict("This conversation belongs to a team; use the team runtime session")
            }
            Self::Unprocessable { reason } => AgentError::bad_request(reason.clone()),
            Self::Internal { reason } => AgentError::internal(reason.clone()),
            Self::WorkspacePathUnavailable { path } => {
                AgentError::bad_request(format!("Workspace path is unavailable: {path}"))
            }
            Self::WorkspacePathRuntimeUnavailable { path } => {
                AgentError::workspace_path_runtime_unavailable(path.clone())
            }
            Self::OpenClawGatewayUnreachable { detail } => AgentError::bad_gateway(detail.clone()),
            Self::Acp(err) => AgentError::bad_gateway(err.to_string()),
        }
    }

    pub(crate) fn error_code(&self) -> &'static str {
        match self {
            Self::NotFound { .. }
            | Self::MessageNotFound { .. }
            | Self::ArtifactNotFound { .. }
            | Self::ActiveAgentNotFound { .. }
            | Self::NotFoundReason { .. } => "NOT_FOUND",
            Self::BadRequest { .. } => "BAD_REQUEST",
            Self::Unauthorized { .. } => "UNAUTHORIZED",
            Self::Forbidden { .. } => "FORBIDDEN",
            Self::Busy { .. } => "CONFLICT",
            Self::RateLimited => "RATE_LIMITED",
            Self::Internal { .. } | Self::Acp(_) => "INTERNAL_ERROR",
            Self::BadGateway { .. } => "BAD_GATEWAY",
            Self::Timeout { .. } => "TIMEOUT",
            Self::ConfigConfirmationTimeout { .. } => "confirmation_timeout",
            Self::ConfigUpdateInProgress { .. } => "config_update_in_progress",
            Self::TeamRuntimeRequired { .. } => "TEAM_RUNTIME_REQUIRED",
            Self::Unprocessable { .. } => "UNPROCESSABLE_ENTITY",
            Self::Archived { .. } => "CONVERSATION_ARCHIVED",
            Self::WorkspacePathUnavailable { .. } => "WORKSPACE_PATH_UNAVAILABLE",
            Self::WorkspacePathRuntimeUnavailable { .. } => "WORKSPACE_PATH_RUNTIME_UNAVAILABLE",
            Self::OpenClawGatewayUnreachable { .. } => "USER_AGENT_OPENCLAW_GATEWAY_UNREACHABLE",
        }
    }
}

impl From<AgentError> for ConversationError {
    fn from(error: AgentError) -> Self {
        match error {
            AgentError::NotFound(reason) => Self::NotFoundReason { reason },
            AgentError::BadRequest(reason) => Self::BadRequest { reason },
            AgentError::Unauthorized(reason) => Self::Unauthorized { reason },
            AgentError::Forbidden(reason) => Self::Forbidden { reason },
            AgentError::Conflict(reason) => Self::Busy { reason },
            AgentError::RateLimited => Self::RateLimited,
            AgentError::Internal(reason) => Self::Internal { reason },
            AgentError::BadGateway(reason) => Self::BadGateway { reason },
            AgentError::Timeout(reason) => Self::Timeout { reason },
            AgentError::ConversationArchived(reason) => Self::Archived {
                id: String::new(),
                reason,
            },
            AgentError::WorkspacePathRuntimeUnavailable(path) => Self::WorkspacePathRuntimeUnavailable { path },
            AgentError::Acp(err) => Self::Acp(err),
            _ => Self::Internal {
                reason: error.to_string(),
            },
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

    fn assert_from_agent<T: From<AgentError>>() {}

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
    fn conversation_error_has_agent_from_impl() {
        assert_from_agent::<ConversationError>();
    }

    #[test]
    fn conversation_error_has_db_from_impl() {
        assert_from_db::<ConversationError>();
    }
}
