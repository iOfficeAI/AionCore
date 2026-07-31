//! `LocalWatcher` — `file:` [`Watcher`] backed by `notify` (non-recursive).
//!
//! One shared `RecommendedWatcher` instance backs all watches so macOS uses a
//! single FSEvents stream/thread (O(1) regardless of watch count). Each
//! `watch()` registers a directory `NonRecursive`; the shared callback maps raw
//! `notify` events to [`RawEvent`]s (via [`super::watcher::map_event`]) and
//! pushes them onto the out channel returned by [`LocalWatcher::new`].
//!
//! Event-path attribution folds case through [`crate::canonical`] rather than
//! comparing raw paths: on case-insensitive platforms the watched canonical is
//! folded while `notify` reports real-case paths, so a lexical canonicalize on
//! both sides is the only correct match.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::canonical;

use super::error::FsError;
use super::watcher::{RawEvent, WatchHandle, Watcher, map_event};

/// A watched directory. `notify_path` is the path handed to `notify` (needed to
/// `unwatch`); `lexical` is the identity canonical the tree model keys on.
#[derive(Clone)]
struct WatchEntry {
    lexical: String,
    notify_path: PathBuf,
}

/// Watched directories keyed by *reported* canonical. A single watch registers
/// two keys — its lexical canonical and its realpath-folded canonical — so
/// events can be attributed whether the OS reports lexical (Linux inotify) or
/// realpath'd (macOS FSEvents resolves `/var` → `/private/var`) paths. Both map
/// back to the same lexical identity. Shared with the event callback.
type Watched = Arc<Mutex<HashMap<String, WatchEntry>>>;

/// Local-disk non-recursive watcher for the `file:` scheme.
pub(crate) struct LocalWatcher {
    inner: Mutex<RecommendedWatcher>,
    watched: Watched,
}

/// Canonical string of a path with symlinks resolved (realpath), or `None` if
/// the path cannot be realpath'd (e.g. does not exist).
fn realpath_canonical(path: &Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    canonical_of(&real)
}

/// Canonical string of a path (lexical, case-folded per platform); `None` if it
/// is not a representable `file:` resource.
fn canonical_of(path: &Path) -> Option<String> {
    let uri = canonical::to_file_uri(path).ok()?;
    canonical::canonicalize(&uri).ok().map(|c| c.as_str().to_owned())
}

/// Attribute an event path to the watched directories that must reconcile it,
/// as lexical canonicals. A create/delete/rename of an entry changes the listing
/// of its **parent** directory, so the parent (when watched) is always an owner.
/// When the path is **itself** a watched directory (an event on the directory
/// entry), that directory is an owner too. Both are returned so a removed/renamed
/// watched subdir emits a change on its parent — not only on itself — which is
/// what keeps the parent's listing from going stale. Matched by reported
/// canonical against the watched keys; the returned order is self-then-parent.
fn resolve_owner(watched: &HashMap<String, WatchEntry>, path: &Path) -> Vec<String> {
    let mut owners = Vec::new();
    // The path itself, when it is a watched directory.
    if let Some(c) = canonical_of(path)
        && let Some(entry) = watched.get(&c)
    {
        owners.push(entry.lexical.clone());
    }
    // The path's parent — the directory whose listing contains this entry.
    if let Some(parent) = path.parent()
        && let Some(c) = canonical_of(parent)
        && let Some(entry) = watched.get(&c)
        && !owners.contains(&entry.lexical)
    {
        owners.push(entry.lexical.clone());
    }
    owners
}

impl LocalWatcher {
    /// Build a watcher and its out channel. Raw events arrive on the receiver.
    pub fn new() -> Result<(Self, UnboundedReceiver<RawEvent>), FsError> {
        let (tx, rx): (UnboundedSender<RawEvent>, UnboundedReceiver<RawEvent>) = unbounded_channel();
        let watched: Watched = Arc::new(Mutex::new(HashMap::new()));
        let cb_watched = Arc::clone(&watched);

        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };
            let snapshot = {
                let guard = cb_watched.lock().unwrap();
                guard.clone()
            };
            let resolve = |p: &Path| resolve_owner(&snapshot, p);
            for raw in map_event(&event, &resolve) {
                // Unbounded, non-blocking: the callback must never block the
                // watcher thread. A closed receiver means shutdown → drop.
                let _ = tx.send(raw);
            }
        })
        .map_err(|e| FsError::Io {
            uri: String::new(),
            message: e.to_string(),
        })?;

        Ok((
            Self {
                inner: Mutex::new(watcher),
                watched,
            },
            rx,
        ))
    }
}

impl Watcher for LocalWatcher {
    fn watch(&self, canonical: &str) -> Result<WatchHandle, FsError> {
        let path = canonical::uri_to_path(canonical).map_err(|_| FsError::Io {
            uri: canonical.to_owned(),
            message: "invalid file uri".to_owned(),
        })?;
        self.inner
            .lock()
            .unwrap()
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(|e| FsError::Io {
                uri: canonical.to_owned(),
                message: e.to_string(),
            })?;
        let entry = WatchEntry {
            lexical: canonical.to_owned(),
            notify_path: path.clone(),
        };
        let mut watched = self.watched.lock().unwrap();
        // Register the lexical canonical and (if different) the realpath-folded
        // canonical so both inotify- and FSEvents-reported paths attribute back.
        watched.insert(canonical.to_owned(), entry.clone());
        if let Some(real) = realpath_canonical(&path) {
            watched.entry(real).or_insert(entry);
        }
        drop(watched);
        Ok(WatchHandle {
            canonical: canonical.to_owned(),
        })
    }

    fn unwatch(&self, handle: &WatchHandle) {
        // Best-effort: idempotent, tolerant of an already-removed watch. Remove
        // every alias key pointing at this lexical canonical.
        let mut watched = self.watched.lock().unwrap();
        let notify_path = watched
            .values()
            .find(|e| e.lexical == handle.canonical)
            .map(|e| e.notify_path.clone());
        watched.retain(|_, e| e.lexical != handle.canonical);
        drop(watched);
        if let Some(path) = notify_path {
            let _ = self.inner.lock().unwrap().unwatch(&path);
        }
    }
}

#[cfg(test)]
#[path = "local_watcher_test.rs"]
mod local_watcher_test;
