//! Source control (runtime link).
//!
//! Provider-neutral source control: repository discovery, a flat change list,
//! file diff, and local staging/discard actions. A peer of the explorer fs
//! runtime, not part of it — source control has its own identity, lifecycle and
//! refresh semantics ("repo dirty → recompute status → replace", never a file
//! tree delta). See `formal/runtime/source-control.md`.
//!
//! Stage 0 is this backend floor: contract plus the git implementation. Watch,
//! debounce, orchestration and the `scm/*` protocol are later stages.
//!
//! Module files hold implementation; this file only declares and re-exports.

mod actor;
mod debounce;
mod error;
mod git_provider;
mod provider;
mod runtime;
mod trash_sink;
mod types;
mod watch;
mod wire;

pub use actor::{ScmActor, ScmInbound, ScmSessionId, ScmWirePush};
pub use error::ScmError;
pub use provider::{IScmHistory, IScmProvider, IScmStaging};
pub use types::{
    ContentRef, DiffContent, FileRef, RepoRef, ResolvedRoot, ScmActionFailure, ScmActionOutcome, ScmCapabilities,
    ScmHead, ScmRepository, ScmRepositoryState, ScmResource, ScmResourceState, ScmStatus,
};
// The concrete git implementation stays module-private: callers will build it
// through the stage-1 orchestration layer and consume only the trait objects it
// hands back, mirroring how `LocalFsProvider` stays behind `LocalFsRuntime`. The
// re-export lands with that first consumer rather than ahead of it.
