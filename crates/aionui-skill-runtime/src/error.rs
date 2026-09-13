use aionui_api_types::SkillRuntimeErrorCode;

/// Crate-owned error, mapped to an HTTP status + wire code only at the route
/// boundary (per the domain-crate convention: service code stays framework-free).
#[derive(Debug, thiserror::Error)]
pub enum SkillRuntimeError {
    #[error("runtime auth failed")]
    RuntimeAuthFailed,

    #[error("conversation not found")]
    ConversationNotFound,

    /// Distinct from `SkillNotFound` on purpose. "Enabled somewhere else" and
    /// "does not exist" call for different agent behaviour (stop asking vs.
    /// report a broken install) and different operator diagnosis (a snapshot
    /// mismatch vs. a missing directory).
    #[error("skill '{name}' is not enabled in this conversation")]
    SkillNotEnabled { name: String },

    #[error("skill '{name}' has no resolvable source directory")]
    SkillNotFound { name: String },

    #[error("invalid path: {reason}")]
    InvalidPath { reason: String },

    #[error("schema validation failed: {reason}")]
    SchemaValidation { reason: String },

    #[error("read failed: {reason}")]
    ReadFailed { reason: String },
}

impl SkillRuntimeError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::RuntimeAuthFailed => 401,
            // 403, not 404: the caller is authenticated and the skill may well
            // exist -- it is simply outside this conversation's allow-list. A 404
            // would invite the agent to retry with variations.
            Self::SkillNotEnabled { .. } => 403,
            Self::ConversationNotFound | Self::SkillNotFound { .. } => 404,
            Self::InvalidPath { .. } | Self::SchemaValidation { .. } => 400,
            Self::ReadFailed { .. } => 500,
        }
    }

    pub fn code(&self) -> SkillRuntimeErrorCode {
        match self {
            Self::RuntimeAuthFailed => SkillRuntimeErrorCode::RuntimeAuthFailed,
            Self::ConversationNotFound => SkillRuntimeErrorCode::ConversationNotFound,
            Self::SkillNotEnabled { .. } => SkillRuntimeErrorCode::SkillNotEnabled,
            Self::SkillNotFound { .. } => SkillRuntimeErrorCode::SkillNotFound,
            Self::InvalidPath { .. } => SkillRuntimeErrorCode::InvalidPath,
            Self::SchemaValidation { .. } => SkillRuntimeErrorCode::SchemaValidationFailed,
            Self::ReadFailed { .. } => SkillRuntimeErrorCode::TransportUnavailable,
        }
    }
}

impl From<aionui_db::DbError> for SkillRuntimeError {
    fn from(error: aionui_db::DbError) -> Self {
        Self::ReadFailed {
            reason: error.to_string(),
        }
    }
}
