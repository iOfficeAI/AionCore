//! Neutral source-control data model.
//!
//! Provider-agnostic by construction: no engine-specific type (raw commit SHA,
//! DAG, index entry) appears here. Engine concepts that cannot be generalized
//! are expressed either as an optional capability (see [`ScmCapabilities`]) or
//! as a capability-relative field ([`ScmResource::staged`]).
//!
//! Shapes mirror the wire types in `formal/runtime/protocol.md`; serialization
//! to the `scm/*` protocol is the WS handler's job, not this module's.

use serde::{Deserialize, Serialize};

/// A file's identity across features: pe-relative, never an absolute path.
///
/// Same identity authority as the explorer link (`formal/bind/data-model.md`):
/// `project_id` / `folder_id` / `absolute_path` are all derived from `pe_id`
/// and stay out of the identity itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileRef {
    pub pe_id: String,
    /// Normalized path relative to the folder root (`""` is the root itself).
    pub relative_path: String,
}

/// Reference to one repository.
///
/// `repo_id` is an **opaque token**: consumers must not parse it to recover a
/// `pe_id` (use [`ScmRepository::root`] / [`FileRef::pe_id`], both carried
/// separately). The current generation rule is a `scm:` prefix over the pe id,
/// but that is a generation rule, not a parsing contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    pub repo_id: String,
}

/// A root already resolved by the bind layer (identity + containment done),
/// handed to a provider for repository discovery.
///
/// Carries the pe identity for the outward model plus the local absolute path
/// the engine needs to open the repository. Providers must not re-derive
/// identity from the path.
#[derive(Debug, Clone)]
pub struct ResolvedRoot {
    pub pe_id: String,
    /// Local absolute path of the pe root.
    pub absolute_path: String,
    /// Display label, derived from the pe by the caller. Always present: falls
    /// back through the folder's derived name to the opaque id, so it never
    /// empties out.
    pub label: String,
    /// The pe entry's own explicit display name, carried raw for the outward
    /// model. `None` when the entry has no name of its own (the derived `label`
    /// is then the only sensible thing to show). Never `Some("")`: the caller
    /// filters an empty/blank name to `None`, so a consumer can treat "present"
    /// as "meaningful".
    pub pe_name: Option<String>,
    /// Whether repository discovery may relax by one level for this root. Set for
    /// a workspace pe root: if the root's own path is not a repository, the
    /// provider reads its immediate child directories and surfaces each one that
    /// is a (non-bare) repository. Left `false` for an attached pe root, whose
    /// discovery stays exactly one-repo-or-none at the root path — attach
    /// behaviour is unchanged. Never recurses past one level.
    pub discover_children: bool,
}

/// Neutral content anchor: which version of a file to read or compare.
///
/// Providers map these onto their own version concepts (git: `Working` → work
/// tree, `Committed` → HEAD, `Staged` → index). `Working` and `Committed` exist
/// for every provider; `Staged` is only meaningful when
/// [`ScmCapabilities::staging`] is set, and providers without a staging area
/// must reject it (see [`super::error::ScmError::CapabilityUnsupported`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentRef {
    Working,
    Committed,
    Staged,
}

/// Optional capabilities a provider declares support for.
///
/// Core operations (discover / status / diff / original / revert) are mandatory
/// and therefore absent here. Everything a version-control system may lack is a
/// flag, so neither the protocol nor the UI hard-codes one engine's model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmCapabilities {
    /// Has a staging area (index) — gates [`ContentRef::Staged`] and
    /// `IScmStaging`.
    pub staging: bool,
    /// Local branch switching (git yes; svn models branches as paths).
    pub local_branches: bool,
    /// Commit graph / DAG history (stage 2).
    pub history_graph: bool,
    /// Remote operations: push / pull / fetch (stage 2).
    pub remote_ops: bool,
}

/// Head position of a repository, as far as the neutral model cares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmHead {
    /// Branch name; `None` when detached or unborn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether head points directly at a revision rather than a branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
}

/// Lifecycle state of a repository as surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScmRepositoryState {
    Idle,
    Refreshing,
    Operation,
    Error,
}

/// One discovered repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmRepository {
    pub repo_id: String,
    /// Which provider serves it (`"git"`, ...).
    pub provider_id: String,
    /// Repository root. For a pe root that is itself a repository this is the pe
    /// root (`relative_path: ""`). For a workspace root whose own path is not a
    /// repository, one-level discovery surfaces each child repository with
    /// `relative_path` set to the child directory name.
    pub root: FileRef,
    pub label: String,
    /// The pe entry's own explicit name, when it has one. The client prefers it
    /// over `label` for display and falls back to `label` otherwise; absent
    /// (never empty) when the entry carries no name of its own. Always `None` for
    /// a child repository discovered under a workspace root — the entry name
    /// belongs to the pe root, not to a repository found inside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pe_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<ScmHead>,
    /// Whether this repository is a linked worktree (as opposed to a primary
    /// clone). Only surfaced under one-level workspace discovery; omitted from the
    /// wire when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_worktree: bool,
    /// When this is a linked worktree *and* its primary repository is also in the
    /// same project's surfaced set, the primary repository's `repo_id`. `None`
    /// when the primary is outside the current view (the client then renders the
    /// worktree at the outer level). Matched by real git directory, never by path
    /// text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_of: Option<String>,
    pub capabilities: ScmCapabilities,
    pub state: ScmRepositoryState,
}

/// Neutral per-resource change state.
///
/// **Open set.** Stage 2 may add values (e.g. a merge state); consumers must
/// tolerate unknown ones and treat them as opaque/blocked (display only, no
/// actions) rather than coercing them into a regular state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScmResourceState {
    Created,
    Modified,
    Deleted,
    /// Conflict from an external merge/rebase. Opaque: no stage/discard in
    /// stage 1 — folding it into `Modified` would let a user stage or discard a
    /// half-finished conflict resolution.
    Conflicted,
    /// Rename, carrying the previous path. Folding it into `Modified` would be
    /// semantically wrong (the path changed).
    Renamed,
}

impl ScmResourceState {
    /// Whether stage/unstage/discard must be refused for this state.
    ///
    /// Data-safety gate, not a UI preference: acting on a conflicted resource
    /// can destroy a conflict resolution.
    pub(crate) fn is_opaque(self) -> bool {
        matches!(self, Self::Conflicted)
    }
}

/// One changed resource. Flat: no pre-grouping into staged/unstaged, so a
/// provider without a staging area produces a single clean list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmResource {
    pub file: FileRef,
    /// Path relative to the repository root. Equal to
    /// [`FileRef::relative_path`] while a repo is the pe root itself, but kept
    /// distinct because the two roots are different concepts.
    pub repo_relative_path: String,
    pub state: ScmResourceState,
    /// Capability-relative: `Some` only for providers with a staging area
    /// (`true` = staged, `false` = unstaged); `None` when the concept does not
    /// apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<bool>,
    /// Previous path when `state` is [`ScmResourceState::Renamed`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_from: Option<String>,
}

/// A repository's full change list. Replacement semantics: a newer status
/// wholly replaces an older one (no delta).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmStatus {
    pub repository: RepoRef,
    /// Flat resource list; grouping is the presentation layer's derivation.
    pub resources: Vec<ScmResource>,
    /// Head position at the moment this status was computed, same shape as
    /// [`ScmRepository::head`]. Carried on the status frame — not only on the
    /// repository descriptor — because a `git checkout` in the work tree moves
    /// head and is picked up by the watch → debounce → refresh path, which only
    /// emits a status frame; the descriptor (and its `head`) is re-sent solely on
    /// `repositoriesChanged` (attach/detach). Without it a branch switch never
    /// reaches a subscribed client. `None` only when head could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<ScmHead>,
    /// Monotonic per repository, allocated by the orchestration layer inside the
    /// same critical section that computes the status. Two refresh sources (an
    /// action completing, a debounced watch signal) can deliver out of order, so
    /// consumers keep the highest sequence they have applied and drop anything
    /// older — otherwise a slow earlier recompute could overwrite a newer one.
    ///
    /// A provider leaves this at `0`; it has no business allocating it.
    pub seq: u64,
    /// Whether the list was cut off by the resource cap. Applies to untracked
    /// entries too: scan cost is driven by non-ignored files
    /// (tracked ∪ untracked), and bulk file creation goes down that path.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Whether this status was computed in the degraded no-index-writeback mode
    /// (see [`crate::scm::ScmError`] docs on stale-index fallback). Observability
    /// only — the resource list is complete either way.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub degraded: bool,
}

/// One file a multi-file action could not complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmActionFailure {
    pub file: FileRef,
    /// Why this file was not processed, in terms a user can act on.
    pub reason: String,
}

/// Outcome of a multi-file action (discard / stage / unstage).
///
/// **Best effort per file.** The files in one request have no transactional
/// relationship — each is an independent filesystem or index operation — so the
/// action attempts every one and reports those it could not complete, rather than
/// stopping at the first failure. Stopping midway is the one behaviour nobody
/// would choose: it leaves earlier files already changed while reporting only
/// "failed", with no way to tell which.
///
/// All-or-nothing is not on the table: discarding an untracked file moves it to
/// the platform trash, and on macOS the trash offers no programmatic restore (the
/// `trash` crate gates its restore API to Windows and Freedesktop platforms), so
/// a rollback could not be honoured. Restoring a tracked file is equally
/// irreversible — the user's edit is already overwritten.
///
/// Failures **of the whole request** (unknown repository, out-of-scope reference,
/// malformed parameters, an unsupported capability, or the conflicted-resource
/// pre-check) are not represented here: those are errors, and they happen before
/// anything is touched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmActionOutcome {
    /// Files that could not be processed. Empty means every file succeeded;
    /// successful files are never listed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<ScmActionFailure>,
}

impl ScmActionOutcome {
    /// Whether every file in the request was processed.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Content of a diff between two [`ContentRef`] anchors.
///
/// Either a unified patch or a content pair, per the wire shape; binary and
/// oversized files are reported as such instead of inlining bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}
