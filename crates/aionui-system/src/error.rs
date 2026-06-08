use aionui_common::CryptoError;
use aionui_db::DbError;

/// Crate-owned error contract for system domain services.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Bad gateway: {0}")]
    BadGateway(String),

    #[error("Request timeout: {0}")]
    Timeout(String),

    #[error("Unprocessable entity: {0}")]
    UnprocessableEntity(String),
}

impl From<DbError> for SystemError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::NotFound(reason) => Self::NotFound(reason),
            DbError::Conflict(reason) => Self::Conflict(reason),
            DbError::Query(e) => Self::Internal(format!("Database error: {e}")),
            DbError::Migration(e) => Self::Internal(format!("Migration error: {e}")),
            DbError::Init(reason) => Self::Internal(format!("Database init error: {reason}")),
        }
    }
}

impl From<CryptoError> for SystemError {
    fn from(error: CryptoError) -> Self {
        if error.is_bad_request() {
            Self::BadRequest(error.to_string())
        } else {
            Self::Internal(error.to_string())
        }
    }
}
