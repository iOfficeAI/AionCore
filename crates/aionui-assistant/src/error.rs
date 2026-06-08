use aionui_db::DbError;

/// Assistant-domain error used below the HTTP boundary.
#[derive(Debug, thiserror::Error)]
pub enum AssistantError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<DbError> for AssistantError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::NotFound(message) => Self::NotFound(message),
            DbError::Conflict(message) => Self::Conflict(message),
            other => Self::Internal(other.to_string()),
        }
    }
}
