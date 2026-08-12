//! `LocalFsProvider` — `file:` scheme [`IFsProvider`] over the local disk.
//!
//! Canonical `file:` URIs are turned into filesystem paths via
//! [`crate::canonical::fs_path`]; all IO goes through `tokio::fs`.
//!
//! TODO(stage-1): realpath containment. Lexical containment already lives in
//! [`crate::containment`] (the reference layer). The access-time boundary —
//! realpath the target before IO and reject symlink/alias escapes out of the
//! Folder root — belongs on the command path and is deferred to the WS handler
//! stage. This provider currently performs no realpath containment.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use ignore::WalkBuilder;

use crate::canonical;

use super::error::FsError;
use super::noise::should_hide;
use super::provider::{EntryFact, IFsProvider, Kind};
use super::search::{Budget, CancellationToken, IFsSearchProvider, NameMatcher, SearchSink};

/// How often, in walked entries, the blocking walk re-checks the cancel token
/// (checking every entry would be needless overhead on a large tree).
const CANCEL_CHECK_STRIDE: usize = 128;

/// Local-disk provider for the `file:` scheme.
#[derive(Debug, Default, Clone)]
pub(crate) struct LocalFsProvider;

impl LocalFsProvider {
    pub fn new() -> Self {
        Self
    }
}

/// Resolve a `file:` URI to a filesystem path, mapping parse failure to
/// [`FsError::Io`] (a malformed URI is a caller/plumbing fault, not a fs state).
fn path_of(uri: &str) -> Result<PathBuf, FsError> {
    canonical::uri_to_path(uri).map_err(|_| FsError::Io {
        uri: uri.to_owned(),
        message: "invalid file uri".to_owned(),
    })
}

/// Map a std IO error against `uri` to the provider error taxonomy.
fn map_io(uri: &str, err: &io::Error) -> FsError {
    match err.kind() {
        io::ErrorKind::NotFound => FsError::NotFound { uri: uri.to_owned() },
        io::ErrorKind::PermissionDenied => FsError::PermissionDenied { uri: uri.to_owned() },
        io::ErrorKind::AlreadyExists => FsError::AlreadyExists { uri: uri.to_owned() },
        io::ErrorKind::NotADirectory => FsError::NotADirectory { uri: uri.to_owned() },
        _ => FsError::Io {
            uri: uri.to_owned(),
            message: err.to_string(),
        },
    }
}

/// Inode of a file's metadata (0 on platforms without a stable inode).
#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}
#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

/// Last-modified time of a file's metadata as epoch milliseconds, or `None` when
/// it cannot be represented.
///
/// Unlike [`inode_of`], this needs no `#[cfg]` split: `Metadata::modified()` is
/// available on all supported targets (macOS / Windows / Linux × x64 / arm64).
/// It still returns `None` on filesystems that do not record a modification time
/// at all, on pre-epoch timestamps, and on values too large for `i64` — every
/// such case degrades that entry to "never reports modified" (see
/// [`EntryFact::mtime_ms`]).
///
/// Precision caveat: some filesystems only record whole seconds, so two writes
/// inside the same second can leave the timestamp unchanged and the second write
/// goes unreported. That is the under-report direction, which this signal
/// deliberately prefers. Should some platform turn out to under-report as a
/// matter of course, the fallback is to compare `len()` alongside mtime — also
/// free from the metadata already in hand, so a field addition covers it without
/// any change to the surrounding design.
fn mtime_ms_of(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

/// Recursively copy a directory tree using an explicit work stack (avoids
/// boxing an async-recursive fn). Symlinks are copied as their link target
/// content via `fs::copy`, matching shallow-copy semantics.
async fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        tokio::fs::create_dir_all(&to).await?;
        let mut rd = tokio::fs::read_dir(&from).await?;
        while let Some(entry) = rd.next_entry().await? {
            let child_from = entry.path();
            let child_to = to.join(entry.file_name());
            if entry.file_type().await?.is_dir() {
                stack.push((child_from, child_to));
            } else {
                tokio::fs::copy(&child_from, &child_to).await?;
            }
        }
    }
    Ok(())
}

/// Build an [`EntryFact`] from a path via `symlink_metadata` (does not follow
/// symlinks — a symlink is its own kind, matching the folder-identity rule that
/// realpath folding is deferred to the access-time containment boundary).
async fn fact_of(uri: &str, path: &Path) -> Result<EntryFact, FsError> {
    let meta = tokio::fs::symlink_metadata(path).await.map_err(|e| map_io(uri, &e))?;
    let ft = meta.file_type();
    let (kind, symlink_target) = if ft.is_symlink() {
        let target = tokio::fs::read_link(path)
            .await
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        (Kind::Symlink, target)
    } else if ft.is_dir() {
        (Kind::Dir, None)
    } else {
        (Kind::File, None)
    };
    Ok(EntryFact {
        kind,
        inode: inode_of(&meta),
        symlink_target,
        // Read off the metadata already fetched above — no extra syscall.
        mtime_ms: mtime_ms_of(&meta),
    })
}

#[async_trait]
impl IFsProvider for LocalFsProvider {
    fn scheme(&self) -> &str {
        "file"
    }

    async fn read_dir(&self, uri: &str) -> Result<Vec<(String, EntryFact)>, FsError> {
        let dir = path_of(uri)?;
        let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| map_io(uri, &e))?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(|e| map_io(uri, &e))? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hide OS-junk / VCS-internal noise from the tree listing (same gate
            // the search walk and precise-event apply use — keeps all three
            // pipelines consistent).
            if should_hide(&name) {
                continue;
            }
            let child = entry.path();
            let child_uri = canonical::to_file_uri(&child).unwrap_or_else(|_| uri.to_owned());
            let fact = fact_of(&child_uri, &child).await?;
            out.push((name, fact));
        }
        Ok(out)
    }

    async fn stat(&self, uri: &str) -> Result<Option<EntryFact>, FsError> {
        let path = path_of(uri)?;
        match fact_of(uri, &path).await {
            Ok(fact) => Ok(Some(fact)),
            Err(FsError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn read(&self, uri: &str) -> Result<Vec<u8>, FsError> {
        let path = path_of(uri)?;
        tokio::fs::read(&path).await.map_err(|e| map_io(uri, &e))
    }

    async fn write(&self, uri: &str, data: &[u8]) -> Result<(), FsError> {
        let path = path_of(uri)?;
        tokio::fs::write(&path, data).await.map_err(|e| map_io(uri, &e))
    }

    async fn create_file(&self, uri: &str) -> Result<(), FsError> {
        let path = path_of(uri)?;
        // create_new fails with AlreadyExists rather than truncating.
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map(|_| ())
            .map_err(|e| map_io(uri, &e))
    }

    async fn mkdir(&self, uri: &str) -> Result<(), FsError> {
        let path = path_of(uri)?;
        tokio::fs::create_dir(&path).await.map_err(|e| map_io(uri, &e))
    }

    async fn remove(&self, uri: &str, recursive: bool) -> Result<(), FsError> {
        let path = path_of(uri)?;
        let meta = tokio::fs::symlink_metadata(&path).await.map_err(|e| map_io(uri, &e))?;
        let res = if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_dir(&path).await
            }
        } else {
            tokio::fs::remove_file(&path).await
        };
        res.map_err(|e| map_io(uri, &e))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let (src, dst) = (path_of(from)?, path_of(to)?);
        tokio::fs::rename(&src, &dst).await.map_err(|e| map_io(from, &e))
    }

    async fn copy(&self, from: &str, to: &str, recursive: bool) -> Result<(), FsError> {
        let (src, dst) = (path_of(from)?, path_of(to)?);
        let meta = tokio::fs::symlink_metadata(&src).await.map_err(|e| map_io(from, &e))?;
        if meta.is_dir() {
            if !recursive {
                return Err(FsError::Io {
                    uri: from.to_owned(),
                    message: "cannot copy directory without recursive".to_owned(),
                });
            }
            copy_dir_recursive(&src, &dst).await.map_err(|e| map_io(from, &e))
        } else {
            tokio::fs::copy(&src, &dst)
                .await
                .map(|_| ())
                .map_err(|e| map_io(from, &e))
        }
    }
}

#[async_trait]
impl IFsSearchProvider for LocalFsProvider {
    async fn search_names(
        &self,
        root_uri: &str,
        matcher: &NameMatcher,
        sink: &Arc<dyn SearchSink>,
        budget: &Budget,
        cancel: &CancellationToken,
    ) -> Result<(), FsError> {
        let root = path_of(root_uri)?;
        // The `ignore` walk is synchronous and CPU/IO-bound; run it off the async
        // worker so it never blocks the actor's event loop. Shared budget/cancel
        // are cheap Arc handles moved into the blocking task.
        let (matcher, sink, budget, cancel) = (matcher.clone(), Arc::clone(sink), budget.clone(), cancel.clone());
        tokio::task::spawn_blocking(move || walk_names(&root, &matcher, &sink, &budget, &cancel))
            .await
            .map_err(|e| FsError::Io {
                uri: root_uri.to_owned(),
                message: format!("search walk task join failed: {e}"),
            })
    }
}

/// The blocking `ignore` tree walk backing [`LocalFsProvider::search_names`].
/// Honors `.gitignore` / git excludes (same walker ripgrep uses); emits only
/// files whose name matches, stopping on budget exhaustion or cancellation.
fn walk_names(
    root: &Path,
    matcher: &NameMatcher,
    sink: &Arc<dyn SearchSink>,
    budget: &Budget,
    cancel: &CancellationToken,
) {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        // Hide OS-junk / VCS-internal noise, matching the tree listing. On a
        // directory this also prevents descent, so `.git` internals never leak
        // into search results (the `ignore` crate does not skip `.git` itself).
        .filter_entry(|entry| entry.file_name().to_str().map(|n| !should_hide(n)).unwrap_or(true))
        .build();

    for (seen, entry) in walker.enumerate() {
        // Periodic cancel check so a large no-hit subtree still bails promptly.
        if seen.is_multiple_of(CANCEL_CHECK_STRIDE) && cancel.is_cancelled() {
            return;
        }
        let entry = match entry {
            Ok(e) => e,
            // Unreadable entry (permissions, race) — skip, safely handled.
            Err(err) => {
                tracing::debug!(error = %err, "fs search: skipping unreadable entry");
                continue;
            }
        };
        // Filename search returns files only (SearchHit is files-only); the root
        // dir itself and every subdirectory are traversed but never emitted.
        if entry.file_type().is_none_or(|ft| ft.is_dir()) {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if !matcher.matches(&name) {
            continue;
        }
        if cancel.is_cancelled() {
            return;
        }
        // Reserve a hit slot from the shared global budget; failure = cap reached
        // (budget records `limit_reached`) → stop this root's walk.
        if !budget.try_take() {
            return;
        }
        sink.emit(rel_path(root, path), name);
    }
}

/// Root-relative path, forward-slash normalized, no leading slash (wire form).
fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(seg) => Some(seg.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
#[path = "local_provider_test.rs"]
mod local_provider_test;
