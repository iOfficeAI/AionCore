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
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::canonical;

use super::error::FsError;
use super::provider::{EntryFact, IFsProvider, Kind};

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

#[cfg(test)]
#[path = "local_provider_test.rs"]
mod local_provider_test;
