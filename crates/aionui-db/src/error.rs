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

/// Case-insensitive substrings identifying transient SQLite contention
/// ("database is locked"/"busy"). Single source of truth shared by
/// [`DbError::is_busy`] and the assistant service's `AssistantError` text
/// classification (Sentry 135525166 Option B).
pub const SQLITE_BUSY_MESSAGE_MARKERS: &[&str] = &["database is locked", "database is busy"];

/// Case-insensitive substring identifying a SQLite UNIQUE constraint violation.
/// Shared by [`DbError::is_unique_violation`] and the assistant service.
pub const SQLITE_UNIQUE_VIOLATION_MARKER: &str = "unique constraint failed";

/// Whether an error message indicates transient SQLite busy/locked contention.
/// Reused across layers so busy detection has one definition, not two.
pub fn message_indicates_busy(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    SQLITE_BUSY_MESSAGE_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Whether an error message indicates a SQLite UNIQUE constraint violation.
pub fn message_indicates_unique_violation(message: &str) -> bool {
    message.to_ascii_lowercase().contains(SQLITE_UNIQUE_VIOLATION_MARKER)
}

impl DbError {
    /// True when this error is a transient SQLite busy/locked contention that is
    /// safe to retry (SQLITE_BUSY, primary result code 5, or a "database is
    /// locked/busy" message). Used by concurrent bootstrap retry (Sentry 135525166).
    pub fn is_busy(&self) -> bool {
        match self {
            DbError::Query(err) => match err.as_database_error() {
                Some(db_err) => db_err.code().as_deref() == Some("5") || message_indicates_busy(db_err.message()),
                None => false,
            },
            _ => false,
        }
    }

    /// True when this error is a UNIQUE constraint violation — either the
    /// explicit [`DbError::Conflict`] variant or a UNIQUE message from the
    /// underlying database error. Bootstrap treats these as "already done".
    pub fn is_unique_violation(&self) -> bool {
        match self {
            DbError::Conflict(_) => true,
            DbError::Query(err) => match err.as_database_error() {
                Some(db_err) => message_indicates_unique_violation(db_err.message()),
                None => false,
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_is_unique_violation_but_not_busy() {
        let err = DbError::Conflict("duplicate assistant_definitions.id".into());
        assert!(err.is_unique_violation());
        assert!(!err.is_busy());
    }

    #[test]
    fn message_markers_detect_busy_and_unique_case_insensitively() {
        assert!(message_indicates_busy("Database query failed: database is locked"));
        assert!(message_indicates_busy("DATABASE IS BUSY"));
        assert!(!message_indicates_busy("some unrelated error"));

        assert!(message_indicates_unique_violation(
            "UNIQUE constraint failed: assistant_definitions.id"
        ));
        assert!(message_indicates_unique_violation("unique constraint failed"));
        assert!(!message_indicates_unique_violation("no such column"));
    }

    #[test]
    fn non_contention_errors_are_neither_busy_nor_unique() {
        for err in [DbError::NotFound("missing".into()), DbError::Init("boom".into())] {
            assert!(!err.is_busy());
            assert!(!err.is_unique_violation());
        }
    }
}
