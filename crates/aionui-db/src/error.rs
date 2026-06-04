use aionui_common::ApiError;

/// Database-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("Database query failed: {0}")]
    Query(#[from] sqlx::Error),

    #[error("Migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("Record not found: {0}")]
    NotFound(String),

    #[error("Duplicate record: {0}")]
    Conflict(String),

    #[error("Database initialization failed: {0}")]
    Init(String),
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound(msg) => ApiError::NotFound(msg),
            DbError::Conflict(msg) => ApiError::Conflict(msg),
            DbError::Query(e) => ApiError::Internal(format!("Database error: {e}")),
            DbError::Migration(e) => ApiError::Internal(format!("Migration error: {e}")),
            DbError::Init(msg) => ApiError::Internal(format!("Database init error: {msg}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_converts_to_app_not_found() {
        let db_err = DbError::NotFound("user".into());
        let api_err: ApiError = db_err.into();
        assert!(matches!(api_err, ApiError::NotFound(msg) if msg == "user"));
    }

    #[test]
    fn conflict_converts_to_app_conflict() {
        let db_err = DbError::Conflict("duplicate".into());
        let api_err: ApiError = db_err.into();
        assert!(matches!(api_err, ApiError::Conflict(msg) if msg == "duplicate"));
    }

    #[test]
    fn init_converts_to_app_internal() {
        let db_err = DbError::Init("broken".into());
        let api_err: ApiError = db_err.into();
        assert!(matches!(api_err, ApiError::Internal(_)));
    }
}
