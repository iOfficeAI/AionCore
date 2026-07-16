use aionui_db::DbError;

#[derive(Debug, thiserror::Error)]
pub enum DevelopmentError {
    #[error("Invalid development request: {0}")]
    BadRequest(String),
    #[error("Development resource not found: {0}")]
    NotFound(String),
    #[error("Development operation conflicts with current state: {0}")]
    Conflict(String),
    #[error("Development operation failed: {0}")]
    Internal(String),
}

impl From<DbError> for DevelopmentError {
    fn from(value: DbError) -> Self {
        match value {
            DbError::NotFound(message) => Self::NotFound(message),
            DbError::Conflict(message) => Self::Conflict(message),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<std::io::Error> for DevelopmentError {
    fn from(value: std::io::Error) -> Self {
        Self::Internal(value.to_string())
    }
}
