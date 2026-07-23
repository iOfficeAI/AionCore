use aionui_db::DbError;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("Invalid project request: {0}")]
    BadRequest(String),
    #[error("Project resource not found: {0}")]
    NotFound(String),
    #[error("Project conflict: {0}")]
    Conflict(String),
    #[error("Project operation failed: {0}")]
    Internal(String),
}

impl From<DbError> for ProjectError {
    fn from(value: DbError) -> Self {
        match value {
            DbError::NotFound(message) => Self::NotFound(message),
            DbError::Conflict(message) => Self::Conflict(message),
            other => Self::Internal(other.to_string()),
        }
    }
}
