//! Mechanism-layer error. This crate is Foundation-layer and must not depend
//! on any domain error type; it owns a small enum covering only what the
//! spawn / lifecycle / reap mechanism produces.

/// Errors produced by the subprocess mechanism layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessError {
    /// Invalid caller input (e.g. an empty cwd).
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The spawn cwd (workspace) is missing, not a directory, or not
    /// accessible. Its own class (not `BadRequest`) so callers can carry the
    /// legacy #410 workspace-unavailable UX across the seam instead of an
    /// opaque transport error. Payload = the path (mirrors the legacy
    /// `AgentError::WorkspacePathRuntimeUnavailable` contract).
    #[error("workspace unavailable: {0}")]
    WorkspaceUnavailable(String),
    /// An OS / runtime failure (spawn failed, pipe capture failed, kill failed, fs error).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ProcessError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn workspace_unavailable(path: impl Into<String>) -> Self {
        Self::WorkspaceUnavailable(path.into())
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
