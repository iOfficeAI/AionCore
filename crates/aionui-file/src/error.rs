/// File crate application errors.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Internal(String),
}
