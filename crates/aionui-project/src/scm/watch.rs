//! `.git`-aware watch: the "should we recompute?" signal for source control.
//!
//! Deliberately **not** a work-tree watch. Watching a project tree recursively
//! costs one descriptor per directory on Linux and drowns in events during a
//! build or a dependency install, while telling us nothing a recompute would not
//! (see `formal/runtime/source-control.md`). Instead this watches only the
//! repository's own metadata, which is what every version-control command
//! touches: staging, committing, checking out, merging and resetting all rewrite
//! `.git/index` or a ref.
//!
//! Two registrations per repository:
//! - the git directory itself, **non-recursively** — covers `index`, `HEAD`,
//!   `MERGE_HEAD`, `ORIG_HEAD`, `FETCH_HEAD`, …
//! - `refs/`, **recursively** — branch and stash ref updates.
//!
//! A separate `notify` instance from the explorer's: that one is a non-recursive
//! stream keyed on observed directories, and mixing a recursive subtree into it
//! would deliver events its attribution logic does not expect.
//!
//! Events are signals, never facts. Nothing here interprets *what* changed —
//! platforms replay stale events and coalesce unpredictably, so the only sound
//! reading is "something moved, recompute the whole status".

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::error::ScmError;

/// One repository's watch registration.
struct WatchEntry {
    /// Every repository reporting dirt for this path.
    ///
    /// A set, not a single id: one git directory can back **several**
    /// repositories at once — the same checkout opened under two projects yields
    /// two repo ids over one path. Keeping a single id there means the second
    /// registration silently replaces the first, and the first repository's
    /// subscribers then stop receiving refreshes with no error anywhere.
    repo_ids: BTreeSet<String>,
    /// Paths handed to `notify`, needed to unwatch them again.
    registered: Vec<PathBuf>,
}

/// Registrations keyed by every path prefix an event might be reported under.
///
/// A single repository contributes several keys: the git directory as passed in
/// and, when it differs, its realpath. macOS reports FSEvents paths with symlinks
/// resolved (`/tmp` → `/private/tmp`), while Linux reports them as registered, so
/// matching only one form would silently attribute nothing on one platform.
type Registry = Arc<Mutex<HashMap<String, WatchEntry>>>;

/// Signal that a repository's metadata changed and its status must be recomputed.
///
/// Carries no detail on purpose: consumers recompute in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScmDirty {
    pub(super) repo_id: String,
}

/// Watches repository metadata and emits [`ScmDirty`] per affected repository.
pub(super) struct GitWatcher {
    inner: Mutex<RecommendedWatcher>,
    registry: Registry,
}

/// Whether an event path is churn that never changes what a status would report.
///
/// Git writes through lock files and temporary index copies: `index.lock`,
/// `HEAD.lock`, `packed-refs.lock`, `index.stash.<pid>`. They appear and vanish
/// around every real change, so keeping them would mostly re-arm the debounce
/// window without adding information — measured as roughly two thirds of all
/// events during ordinary git usage.
fn is_noise(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        // A non-UTF-8 name cannot be one of git's own metadata files, so treat it
        // as a real signal rather than filtering it out silently.
        return false;
    };
    name.ends_with(".lock") || name.starts_with("index.stash.")
}

/// Key a path is registered/looked-up under: the path as a string, plus its
/// realpath form when that differs.
fn keys_for(path: &Path) -> Vec<String> {
    let mut keys = vec![path.to_string_lossy().into_owned()];
    if let Ok(real) = std::fs::canonicalize(path) {
        let real = real.to_string_lossy().into_owned();
        if !keys.contains(&real) {
            keys.push(real);
        }
    }
    keys
}

/// Find which repository an event path belongs to, by longest matching prefix.
///
/// Longest-first so a nested registration (`.git/refs`) is preferred over its
/// parent (`.git`) — either resolves to the same repository, but matching the
/// most specific key keeps attribution stable if that ever stops being true.
fn attribute(registry: &HashMap<String, WatchEntry>, path: &Path) -> BTreeSet<String> {
    let candidates = keys_for(path);
    let mut best: Option<(usize, &WatchEntry)> = None;
    for (key, entry) in registry.iter() {
        for candidate in &candidates {
            if candidate.starts_with(key.as_str())
                && let Some(rest) = candidate.get(key.len()..)
                // Either the path is the registered path itself, or it sits
                // beneath it — never a sibling sharing a name prefix.
                && (rest.is_empty() || rest.starts_with(std::path::MAIN_SEPARATOR) || rest.starts_with('/'))
                && best.as_ref().is_none_or(|(len, _)| key.len() > *len)
            {
                best = Some((key.len(), entry));
            }
        }
    }
    // Every repository behind that path, not just one: a shared git directory
    // must wake all of its repositories, or the ones left out go stale silently.
    best.map(|(_, entry)| entry.repo_ids.clone()).unwrap_or_default()
}

impl GitWatcher {
    /// Create the watcher and the channel its signals arrive on.
    pub(super) fn new() -> Result<(Self, UnboundedReceiver<ScmDirty>), ScmError> {
        let (tx, rx) = unbounded_channel();
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let watcher = build_watcher(Arc::clone(&registry), tx)?;
        Ok((
            Self {
                inner: Mutex::new(watcher),
                registry,
            },
            rx,
        ))
    }

    /// Start watching one repository's metadata.
    ///
    /// `git_dir` must be the **real** git directory, which for a linked worktree
    /// or a submodule is not the `.git` entry beside the work tree (that one is a
    /// file pointing elsewhere) — callers get it from the provider, which asks
    /// the engine.
    pub(super) fn watch(&self, repo_id: &str, git_dir: &Path) -> Result<(), ScmError> {
        let refs_dir = git_dir.join("refs");
        let mut registered = Vec::new();
        {
            let mut watcher = self.inner.lock().expect("scm watcher poisoned");
            watcher
                .watch(git_dir, RecursiveMode::NonRecursive)
                .map_err(|err| ScmError::Io {
                    path: git_dir.to_string_lossy().into_owned(),
                    message: format!("watch git dir failed: {err}"),
                })?;
            registered.push(git_dir.to_path_buf());
            // A repository with no refs yet (freshly initialised) has no `refs`
            // directory; branch signals then arrive once it appears, and `index`
            // already covers the staging changes that can happen meanwhile.
            match watcher.watch(&refs_dir, RecursiveMode::Recursive) {
                Ok(()) => registered.push(refs_dir),
                Err(err) => tracing::debug!(
                    repo_id,
                    error = %err,
                    "scm watch: refs directory not watchable yet, continuing with the git dir only"
                ),
            }
        }

        let mut registry = self.registry.lock().expect("scm watch registry poisoned");
        for path in &registered {
            for key in keys_for(path) {
                // Merged, never replaced: another repository may already be
                // registered under this exact path.
                registry
                    .entry(key)
                    .or_insert_with(|| WatchEntry {
                        repo_ids: BTreeSet::new(),
                        registered: registered.clone(),
                    })
                    .repo_ids
                    .insert(repo_id.to_owned());
            }
        }
        Ok(())
    }

    /// Whether a repository currently has an armed registration. Lets the
    /// orchestration layer's tests assert that release actually happened, which
    /// the subscriber list alone cannot show.
    #[cfg(test)]
    pub(super) fn is_watching(&self, repo_id: &str) -> bool {
        self.registry
            .lock()
            .expect("scm watch registry poisoned")
            .values()
            .any(|entry| entry.repo_ids.contains(repo_id))
    }

    /// Stop watching a repository. Idempotent: unwatching an unknown repository
    /// is a no-op, so release paths need not check first.
    pub(super) fn unwatch(&self, repo_id: &str) {
        let released = {
            let mut registry = self.registry.lock().expect("scm watch registry poisoned");
            let mut released: Vec<PathBuf> = Vec::new();
            for entry in registry.values_mut() {
                entry.repo_ids.remove(repo_id);
            }
            // Only the paths nobody is left watching are handed back to `notify`.
            // Dropping a path still claimed by another repository would blind that
            // repository — the failure mode this whole set exists to prevent.
            registry.retain(|_, entry| {
                if entry.repo_ids.is_empty() {
                    released.extend(entry.registered.iter().cloned());
                    false
                } else {
                    true
                }
            });
            released
        };

        if released.is_empty() {
            return;
        }
        let mut watcher = self.inner.lock().expect("scm watcher poisoned");
        for path in released {
            // Already-gone paths (repository deleted) report an error we do not
            // care about: the registration is being dropped either way.
            let _ = watcher.unwatch(&path);
        }
    }
}

/// Build the `notify` instance whose callback attributes events to repositories.
fn build_watcher(registry: Registry, tx: UnboundedSender<ScmDirty>) -> Result<RecommendedWatcher, ScmError> {
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        let registry = registry.lock().expect("scm watch registry poisoned");
        let mut emitted: Vec<String> = Vec::new();
        for path in &event.paths {
            if is_noise(path) {
                continue;
            }
            for repo_id in attribute(&registry, path) {
                if emitted.contains(&repo_id) {
                    continue;
                }
                emitted.push(repo_id.clone());
                // A closed receiver means the runtime is gone; the signal is
                // dropped rather than panicking inside a watcher callback.
                let _ = tx.send(ScmDirty { repo_id });
            }
        }
    })
    .map_err(|err| ScmError::Io {
        path: String::new(),
        message: format!("scm watcher init failed: {err}"),
    })
}

#[cfg(test)]
#[path = "watch_test.rs"]
mod watch_test;
