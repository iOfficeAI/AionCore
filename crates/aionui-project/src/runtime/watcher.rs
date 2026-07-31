//! `Watcher` — the watch half of a filesystem runtime.
//!
//! Lazy, non-recursive, per-canonical: one watch per observed directory, no
//! recursive watches (watch count is O(observed), independent of tree size).
//! The watcher only starts/stops watches and emits raw change / overflow
//! signals onto a channel; it does not read directories, produce deltas, or
//! know about projects/sessions. Delta reconciliation is the tree model's job.
//!
//! Debounce/coalesce is intentionally NOT here: per `formal/runtime/runtime.md`
//! the debounce stage sits between the watcher callback and the shard worker in
//! the apply pipeline, so it lives in the actor layer where its window is
//! testable with a paused clock. This watcher emits raw, un-debounced events.

use std::path::Path;

use super::error::FsError;

/// Opaque handle for an active watch, returned by [`Watcher::watch`] and passed
/// back to [`Watcher::unwatch`]. Carries the canonical it watches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchHandle {
    pub canonical: String,
}

/// A raw filesystem signal, pre-reconciliation. `paths` are absolute fs path
/// strings the OS reported changed under `canonical` (used only as a "which
/// children to re-check" hint; the tree model re-reads to find the truth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEvent {
    Changed { canonical: String, paths: Vec<String> },
    Overflow { canonical: String },
}

/// Start/stop non-recursive watches. Concrete impls push [`RawEvent`]s onto an
/// out channel obtained at construction.
pub trait Watcher: Send + Sync {
    /// Begin watching `canonical` (a `file:` URI) non-recursively.
    fn watch(&self, canonical: &str) -> Result<WatchHandle, FsError>;
    /// Stop watching the directory `handle` refers to.
    fn unwatch(&self, handle: &WatchHandle);
}

/// Translate one `notify` event into zero or more [`RawEvent`]s, attributing
/// each affected path to a watched canonical via `resolve`.
///
/// `resolve(path)` returns the watched canonical that owns `path` (an OS event
/// path is a child of a watched directory, or the directory itself), or `None`
/// if no watched directory owns it. Pure and deterministic — the concurrency-
/// free core of event mapping, unit-tested without a real filesystem.
pub fn map_event(event: &notify::Event, resolve: &dyn Fn(&Path) -> Option<String>) -> Vec<RawEvent> {
    // Kernel dropped events (inotify IN_Q_OVERFLOW / RDCW buffer / FSEvents
    // must-rescan): the node's facts are stale → one Overflow per affected
    // canonical, deduped and order-stable.
    if event.need_rescan() {
        let mut seen: Vec<String> = Vec::new();
        for path in &event.paths {
            if let Some(c) = resolve(path)
                && !seen.contains(&c)
            {
                seen.push(c);
            }
        }
        return seen
            .into_iter()
            .map(|canonical| RawEvent::Overflow { canonical })
            .collect();
    }

    // Group changed paths under their owning canonical, preserving first-seen
    // canonical order and per-canonical path order.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for path in &event.paths {
        let Some(canonical) = resolve(path) else { continue };
        let path_str = path.to_string_lossy().into_owned();
        match groups.iter_mut().find(|(c, _)| *c == canonical) {
            Some((_, paths)) => paths.push(path_str),
            None => groups.push((canonical, vec![path_str])),
        }
    }
    groups
        .into_iter()
        .map(|(canonical, paths)| RawEvent::Changed { canonical, paths })
        .collect()
}

#[cfg(test)]
#[path = "watcher_test.rs"]
mod watcher_test;
