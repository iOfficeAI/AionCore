//! `IFsRuntime` — a provider + watcher pair for one scheme, and the
//! scheme → runtime registry the tree model dispatches through.
//!
//! `LocalFsRuntime` composes [`LocalFsProvider`] + [`LocalWatcher`] for `file:`
//! and declares `Inline` IO (local disk IO is ~ms, safe to `await` on the shard
//! worker). `Offload` is reserved for slow/remote providers.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;

use super::error::FsError;
use super::local_provider::LocalFsProvider;
use super::local_watcher::LocalWatcher;
use super::provider::IFsProvider;
use super::search::IFsSearchProvider;
use super::watcher::{RawEvent, Watcher};

/// How a provider's `read_dir`/`stat` IO is executed on the shard worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDispatch {
    /// `await` inline on the worker (local, ~ms). Blocks the shard briefly.
    Inline,
    /// Spawn IO off-worker with a per-canonical busy flag (slow/remote).
    Offload,
}

/// A filesystem runtime for one scheme: its data-operation provider, its watch
/// half, and its IO dispatch strategy.
pub trait IFsRuntime: Send + Sync {
    fn provider(&self) -> &dyn IFsProvider;
    fn watcher(&self) -> &dyn Watcher;
    fn io_dispatch(&self) -> IoDispatch;

    /// The filename-search capability, when this scheme supports it. Returned as
    /// an owned `Arc` so the search orchestration can move it into a spawned
    /// coordinator task. `None` = scheme does not support search.
    fn search_provider(&self) -> Option<Arc<dyn IFsSearchProvider>>;
}

/// `file:` runtime: local provider + non-recursive local watcher, `Inline` IO.
pub struct LocalFsRuntime {
    /// `Arc`-wrapped so it can serve both the borrowed `provider()` view and an
    /// owned `search_provider()` handle without a second construction.
    provider: Arc<LocalFsProvider>,
    watcher: LocalWatcher,
}

impl LocalFsRuntime {
    /// Build the runtime and the raw-event receiver its watcher feeds. The
    /// caller (actor layer) owns the receiver and pumps it into apply.
    pub fn new() -> Result<(Self, UnboundedReceiver<RawEvent>), FsError> {
        let (watcher, rx) = LocalWatcher::new()?;
        Ok((
            Self {
                provider: Arc::new(LocalFsProvider::new()),
                watcher,
            },
            rx,
        ))
    }
}

impl IFsRuntime for LocalFsRuntime {
    fn provider(&self) -> &dyn IFsProvider {
        self.provider.as_ref()
    }

    fn watcher(&self) -> &dyn Watcher {
        &self.watcher
    }

    fn io_dispatch(&self) -> IoDispatch {
        IoDispatch::Inline
    }

    fn search_provider(&self) -> Option<Arc<dyn IFsSearchProvider>> {
        Some(Arc::clone(&self.provider) as Arc<dyn IFsSearchProvider>)
    }
}

/// scheme → runtime. The tree model derives a canonical's scheme and dispatches
/// provider/watcher calls through the matching runtime.
#[derive(Default)]
pub struct FsRuntimeRegistry {
    by_scheme: HashMap<String, Arc<dyn IFsRuntime>>,
}

impl FsRuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the runtime serving `scheme` (e.g. `"file"`).
    pub fn register(&mut self, scheme: impl Into<String>, runtime: Arc<dyn IFsRuntime>) {
        self.by_scheme.insert(scheme.into(), runtime);
    }

    /// Look up the runtime for `scheme`; `None` if unregistered (the wire layer
    /// maps that to `unsupported_resource_scheme`).
    pub fn get(&self, scheme: &str) -> Option<Arc<dyn IFsRuntime>> {
        self.by_scheme.get(scheme).map(Arc::clone)
    }
}

#[cfg(test)]
#[path = "fs_runtime_test.rs"]
mod fs_runtime_test;
