//! Project Explorer filesystem runtime (runtime link).
//!
//! Backend file-fact runtime kept for the app lifetime: single-level provider
//! data ops, lazy non-recursive watch, a sparse canonical-keyed tree model, a
//! set-reconciled subscription registry, and actor/shard concurrency. This
//! layer produces canonical-domain deltas/snapshots; fan-out to the pe-keyed
//! wire is the WS handler's job (stage 1). See `formal/runtime/*`.
//!
//! Module files hold implementation; this file only declares and re-exports.

mod actor;
mod error;
mod fs_runtime;
mod local_provider;
mod local_watcher;
mod noise;
mod provider;
mod search;
mod subscription;
mod tree_model;
mod watcher;

pub use actor::{Command, Debouncer, Shard, ShardOutput, raw_to_command};
pub use error::FsError;
pub use fs_runtime::{FsRuntimeRegistry, IFsRuntime, IoDispatch, LocalFsRuntime};
pub use provider::{EntryFact, IFsProvider, Kind};
pub use search::{Budget, CancellationToken, IFsSearchProvider, MatchMode, NameMatcher, SearchSink};
// Note: the concrete `file:` impls `LocalFsProvider` / `LocalWatcher` are
// `pub(crate)` (internal) — external callers build via `LocalFsRuntime` and see
// only the trait objects it returns, so they are not re-exported here.
// `SubscriptionRegistry` and its outcome enums are internal to `Shard`; only the
// boundary types that appear in `Command`'s public API are exported.
pub use subscription::{GRACE_TTL_MS, Millis, Subscriber};
pub use tree_model::{Change, DeltaBatch, Hint, Snapshot, TreeModel};
pub use watcher::{RawEvent, WatchHandle, Watcher};
