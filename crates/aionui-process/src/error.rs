//! Mechanism-layer error. This crate is Foundation-layer and must not depend
//! on any domain error type; it owns a small enum covering only what the
//! spawn / lifecycle / reap mechanism produces.

/// Errors produced by the subprocess mechanism layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessError {
    /// Invalid caller input (e.g. a missing / non-directory / whitespace cwd).
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Workspace path contains a whitespace segment the bundled runtime cannot handle.
    #[error("workspace path contains whitespace (runtime unsupported): {0}")]
    WorkspacePathContainsWhitespaceRuntimeUnsupported(String),
    /// An OS / runtime failure (spawn failed, pipe capture failed, kill failed, fs error).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ProcessError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn workspace_path_contains_whitespace_runtime_unsupported(path: impl Into<String>) -> Self {
        Self::WorkspacePathContainsWhitespaceRuntimeUnsupported(path.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl From<std::io::Error> for ProcessError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
