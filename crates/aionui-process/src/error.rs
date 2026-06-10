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

#[cfg(test)]
mod tests {
    use super::*;

    /// INPUTVAL-B4: the named constructors build the matching variant and the
    /// `Display` impl renders the documented prefix (callers/log scrapers rely
    /// on these exact prefixes).
    #[test]
    fn constructors_build_matching_variant_and_render_prefix() {
        let bad = ProcessError::bad_request("nope");
        assert!(matches!(bad, ProcessError::BadRequest(ref m) if m == "nope"));
        assert_eq!(bad.to_string(), "bad request: nope");

        let ws = ProcessError::workspace_path_contains_whitespace_runtime_unsupported("/a b");
        assert!(matches!(
            ws,
            ProcessError::WorkspacePathContainsWhitespaceRuntimeUnsupported(ref p) if p == "/a b"
        ));
        assert_eq!(
            ws.to_string(),
            "workspace path contains whitespace (runtime unsupported): /a b"
        );

        let internal = ProcessError::internal("boom");
        assert!(matches!(internal, ProcessError::Internal(ref m) if m == "boom"));
        assert_eq!(internal.to_string(), "internal error: boom");
    }

    /// `From<io::Error>` maps to `Internal` carrying the io error's text.
    #[test]
    fn io_error_maps_to_internal() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: ProcessError = io.into();
        assert!(matches!(err, ProcessError::Internal(ref m) if m.contains("denied")));
    }
}
