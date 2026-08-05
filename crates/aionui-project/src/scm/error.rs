//! Source-control operation errors.
//!
//! Distinct from [`crate::runtime::FsError`] (filesystem provider IO) and from
//! [`crate::types::ProjectError`] (bind domain): these describe outcomes of
//! version-control operations. Mapping to JSON-RPC protocol codes
//! (`not_a_repository` / `scm_operation_failed` / `capability_unsupported`)
//! belongs to the WS handler, not here.

/// A source-control operation error.
#[derive(Debug, thiserror::Error)]
pub enum ScmError {
    /// The root is not a repository of this provider. Discovery reports this as
    /// `Ok(None)`; this variant is for operations invoked on a non-repository.
    #[error("not a repository: {root}")]
    NotARepository { root: String },

    /// The referenced resource is outside what this connection may access — a
    /// stale or forged pe reference. Distinct from "not found" so the client is
    /// not sent looking for a file it was never allowed to name.
    #[error("out of scope: {pe_id}")]
    OutOfScope { pe_id: String },

    /// A required parameter is missing or malformed. Distinct from an unsupported
    /// capability: the method exists and is allowed, the request is just not
    /// well-formed.
    #[error("invalid params: {what}")]
    InvalidParams { what: &'static str },

    /// Unknown repository id (never discovered, or already released).
    #[error("unknown repository: {repo_id}")]
    UnknownRepository { repo_id: String },

    /// The file does not exist at the requested anchor.
    #[error("resource not found: {path}")]
    NotFound { path: String },

    /// A capability the provider does not declare was requested — e.g. staging
    /// on a provider without an index, or [`super::types::ContentRef::Staged`] against one.
    /// Backstop only: the primary gate is the declared capability set plus the
    /// absent staging sub-trait.
    #[error("capability unsupported: {capability}")]
    CapabilityUnsupported { capability: &'static str },

    /// The operation is refused because the resource is opaque (a conflicted
    /// resource in stage 1) — a policy refusal, not an engine failure.
    #[error("operation refused on opaque resource: {path}")]
    OpaqueResource { path: String },

    /// The underlying engine failed. `context` names the operation so the WS
    /// layer can attach it without re-deriving it.
    #[error("scm operation failed during {context}: {message}")]
    OperationFailed { context: &'static str, message: String },

    /// Local IO failed outside the engine (e.g. moving a file to the trash).
    #[error("io error on {path}: {message}")]
    Io { path: String, message: String },
}

impl ScmError {
    /// Build a [`ScmError::CapabilityUnsupported`] for [`super::types::ContentRef::Staged`]
    /// against a provider that has no staging area.
    pub(super) fn unsupported_staged_anchor() -> Self {
        Self::CapabilityUnsupported { capability: "staging" }
    }
}
