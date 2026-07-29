//! Content-free errors used by the Memory domain below the HTTP boundary.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    #[error("memory resource not found")]
    NotFound,

    #[error("memory operation forbidden")]
    Forbidden,

    #[error("memory input is invalid")]
    InvalidInput,

    #[error("memory job lease was lost")]
    LeaseLost,

    #[error("memory revision is stale")]
    StaleRevision,

    #[error("memory operation conflicts with current state")]
    Conflict,

    #[error("memory operation failed")]
    Internal,
}
