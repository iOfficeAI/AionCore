//! Runtime-link filesystem errors.
//!
//! Distinct from the bind-domain [`crate::types::ProjectError`]: these describe
//! provider IO outcomes (read_dir/stat/read/write/...). Wire-layer mapping to
//! JSON-RPC protocol codes (`resource_not_found`, `provider_unavailable`, ...)
//! lives in the WS handler (runtime link stage 1), not here.

/// A filesystem provider operation error.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("resource not found: {uri}")]
    NotFound { uri: String },

    #[error("resource already exists: {uri}")]
    AlreadyExists { uri: String },

    #[error("permission denied: {uri}")]
    PermissionDenied { uri: String },

    #[error("not a directory: {uri}")]
    NotADirectory { uri: String },

    #[error("unsupported resource scheme: {scheme}")]
    UnsupportedScheme { scheme: String },

    #[error("io error on {uri}: {message}")]
    Io { uri: String, message: String },
}
