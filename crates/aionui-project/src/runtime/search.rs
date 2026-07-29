//! `IFsSearchProvider` — the filename-search capability of a filesystem runtime.
//!
//! Kept a separate trait from [`super::provider::IFsProvider`] so the single-level
//! non-recursive data-op contract is not polluted by the recursive/streaming shape
//! of search. A provider walks its own subtree the most efficient way it can
//! (`LocalFsProvider` = in-process `ignore` walk; a future remote provider = one
//! request + a frame stream), emitting each filename hit through a [`SearchSink`].
//! The provider only *produces* `(relative_path, name)`; batching, pe-id stamping,
//! merging into one `fs/search` stream, and pushing to the wire are the
//! orchestration layer's job (see `monitor::search`).
//!
//! Feature semantics / engine / chat-ref identity: `formal/runtime/search.md`;
//! protocol: `formal/runtime/protocol.md` `fs/search`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

use super::error::FsError;

/// How the cheap backend filename predicate matches a candidate name against the
/// query. Backend only *bounds* the hit set cheaply; final fuzzy ranking is the
/// frontend's job (see `search.md` "backend filters, frontend ranks").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Case-insensitive substring — `query` appears contiguously in the name.
    Substring,
    /// Case-insensitive subsequence — `query`'s chars appear in order, gaps ok.
    Subsequence,
}

/// A precompiled, cheap filename predicate derived from the search `query`.
/// Case-insensitive. An empty query matches every file (panel browse mode).
#[derive(Debug, Clone)]
pub struct NameMatcher {
    /// Lowercased query needle; empty = match-all.
    needle: String,
    mode: MatchMode,
}

impl NameMatcher {
    /// Compile a matcher from the raw query and mode.
    pub fn new(query: &str, mode: MatchMode) -> Self {
        Self {
            needle: query.to_lowercase(),
            mode,
        }
    }

    /// Whether `name` matches. Empty query always matches (browse).
    pub fn matches(&self, name: &str) -> bool {
        if self.needle.is_empty() {
            return true;
        }
        let hay = name.to_lowercase();
        match self.mode {
            MatchMode::Substring => hay.contains(&self.needle),
            MatchMode::Subsequence => is_subsequence(&self.needle, &hay),
        }
    }
}

/// Whether every char of `needle` appears in `hay` in order (both prelowered).
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut needle_chars = needle.chars().peekable();
    for c in hay.chars() {
        match needle_chars.peek() {
            Some(&n) if n == c => {
                needle_chars.next();
            }
            Some(_) => {}
            None => return true,
        }
    }
    needle_chars.peek().is_none()
}

/// A hit budget shared across all roots of one search: a global cap on how many
/// files may be emitted before the walk stops. Cloning shares the same counter
/// (cheap `Arc` handle) so concurrent per-root walks draw from one pool.
#[derive(Debug, Clone, Default)]
pub struct Budget(Arc<BudgetInner>);

#[derive(Debug, Default)]
struct BudgetInner {
    /// Remaining emit slots; reaching 0 ends the walk.
    remaining: AtomicUsize,
    /// Set once a walk tried to emit while `remaining == 0` — distinguishes
    /// "hit exactly the cap and there were more" from "found fewer than cap".
    hit_cap: AtomicBool,
}

impl Budget {
    /// A budget allowing at most `limit` total hits across all roots.
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(BudgetInner {
            remaining: AtomicUsize::new(limit),
            hit_cap: AtomicBool::new(false),
        }))
    }

    /// Reserve one emit slot. Returns `true` if a slot was taken; `false` when
    /// the budget is exhausted (and records that the cap forced a stop).
    pub fn try_take(&self) -> bool {
        let taken = self
            .0
            .remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| cur.checked_sub(1))
            .is_ok();
        if !taken {
            self.0.hit_cap.store(true, Ordering::Relaxed);
        }
        taken
    }

    /// Whether any walk was forced to stop because the cap was reached.
    pub fn limit_reached(&self) -> bool {
        self.0.hit_cap.load(Ordering::Relaxed)
    }
}

/// Cooperative cancel signal, cascaded to every per-root walk of a search.
/// Explicit `fs/searchCancel` or a superseding new search flips it; each walk
/// checks it and stops (a future remote provider kills its remote request).
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Hit outlet. The provider calls [`SearchSink::emit`] per matching file with the
/// root-relative path (forward-slash normalized, no leading slash) and file name;
/// the orchestration layer stamps `pe_id`, batches, and pushes `fs/searchMatch`.
pub trait SearchSink: Send + Sync {
    /// Emit one filename hit within the current root.
    fn emit(&self, relative_path: String, name: String);
}

/// Filename-search capability for one provider scheme. Distinct from
/// [`IFsProvider`](super::provider::IFsProvider): recursive + streaming.
#[async_trait]
pub trait IFsSearchProvider: Send + Sync {
    /// Walk `root_uri`'s subtree, emitting each file whose name satisfies
    /// `matcher` through `sink`, until the subtree is exhausted, `budget` runs
    /// out, or `cancel` fires. `budget` and `cancel` are shared across all roots
    /// of the search; the sink merges every root into one stream.
    async fn search_names(
        &self,
        root_uri: &str,
        matcher: &NameMatcher,
        sink: &Arc<dyn SearchSink>,
        budget: &Budget,
        cancel: &CancellationToken,
    ) -> Result<(), FsError>;
}

#[cfg(test)]
#[path = "search_test.rs"]
mod search_test;
