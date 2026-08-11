//! `IScmProvider` — the provider-neutral source-control contract.
//!
//! Peer of [`crate::runtime::IFsProvider`] (filesystem data ops), not a
//! specialization of it: source control has its own identity and lifecycle. The
//! core methods are the ones that generalize across version-control systems;
//! anything one system may lack is an optional capability, either a flag on
//! [`ScmCapabilities`] or a sub-trait obtained through [`IScmProvider::staging`]
//! / [`IScmProvider::history`], so an unsupported call fails to compile rather
//! than at runtime.

use async_trait::async_trait;

use super::error::ScmError;
use super::types::{
    ContentRef, DiffContent, FileRef, RepoRef, ResolvedRoot, ScmActionOutcome, ScmCapabilities, ScmRepository,
    ScmStatus,
};

/// Neutral source-control operations. Every method here must be expressible for
/// any version-control system; engine-specific types must not appear in a
/// signature.
///
/// **Precondition on every path.** Implementations assume each [`FileRef`] has
/// already been resolved by the orchestration layer: in scope for the caller, and
/// normalized to a plain repository-relative path (no `.` or `..` segments, not
/// absolute). They deliberately do **not** re-validate it — identity resolution
/// has a single owner, and a second check here would be a copy of it that can
/// drift. Callers that reach a provider directly are responsible for that
/// guarantee themselves.
#[async_trait]
pub trait IScmProvider: Send + Sync {
    /// Stable provider identifier (`"git"`, ...).
    fn provider_id(&self) -> &str;

    /// Which optional capabilities this provider supports.
    fn capabilities(&self) -> ScmCapabilities;

    /// Discover the repositories an already-resolved root surfaces.
    ///
    /// The result is a set, not a single value, but its size is bounded by the
    /// root's discovery policy:
    /// - Always at most one when [`ResolvedRoot::discover_children`] is `false`
    ///   (an attached pe root): the root path itself is a repository or it is not.
    /// - Zero or more when `discover_children` is `true` (a workspace pe root)
    ///   *and* the root path is not itself a repository: each immediate child
    ///   directory that is a non-bare repository is surfaced. When the workspace
    ///   root path is itself a repository, that single repository is returned and
    ///   children are not inspected.
    ///
    /// An empty result means "no repository here" — a normal outcome, not an
    /// error, and never a fabricated repository.
    async fn discover(&self, root: &ResolvedRoot) -> Result<Vec<ScmRepository>, ScmError>;

    /// Full flat change list for a repository (no pre-grouping).
    async fn status(&self, repo: &RepoRef) -> Result<ScmStatus, ScmError>;

    /// Diff one file between two neutral content anchors.
    async fn diff(
        &self,
        repo: &RepoRef,
        file: &FileRef,
        from: ContentRef,
        to: ContentRef,
    ) -> Result<DiffContent, ScmError>;

    /// Read one file's content at a neutral anchor; `None` when the file does
    /// not exist there (e.g. an added file has no committed version).
    async fn original(&self, repo: &RepoRef, file: &FileRef, at: ContentRef) -> Result<Option<Vec<u8>>, ScmError>;

    /// Discard working-tree changes, restoring the committed version. Files the
    /// version-control system does not track are moved to the system trash
    /// rather than deleted outright.
    ///
    /// Best effort per file: every file is attempted and the ones that could not
    /// be processed come back in the outcome (see [`ScmActionOutcome`]). An `Err`
    /// means the request as a whole was refused, before anything was touched.
    async fn revert(&self, repo: &RepoRef, files: &[FileRef]) -> Result<ScmActionOutcome, ScmError>;

    /// Staging operations, or `None` when this provider has no staging area.
    fn staging(&self) -> Option<&dyn IScmStaging> {
        None
    }

    /// History/commit-graph operations, or `None` when unsupported. Stage 2;
    /// every stage-0 provider returns `None`.
    fn history(&self) -> Option<&dyn IScmHistory> {
        None
    }
}

/// Staging-area operations, available only from providers that declare
/// [`ScmCapabilities::staging`].
#[async_trait]
pub trait IScmStaging: Send + Sync {
    /// Move working-tree changes into the staging area.
    ///
    /// Best effort per file, with the same semantics as
    /// [`IScmProvider::revert`] — the three multi-file actions behave alike so a
    /// client handles one shape, not three.
    async fn stage(&self, repo: &RepoRef, files: &[FileRef]) -> Result<ScmActionOutcome, ScmError>;

    /// Remove changes from the staging area, keeping the working tree as-is.
    async fn unstage(&self, repo: &RepoRef, files: &[FileRef]) -> Result<ScmActionOutcome, ScmError>;
}

/// Commit-graph / history reads. Stage 2 — declared here so adding it later is
/// an added capability rather than a change to the core contract.
pub trait IScmHistory: Send + Sync {}
