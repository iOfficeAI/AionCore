//! `IFsProvider` — the data-operation half of a filesystem runtime.
//!
//! Single-level, non-recursive operations over a provider scheme. The watch
//! half lives in [`super::watcher`]; the two are composed by an `IFsRuntime`.
//! Lexical canonicalization stays in [`crate::canonical`]; this trait consumes
//! already-canonical URIs.

use async_trait::async_trait;

use super::error::FsError;

/// Kind of a directory entry, as the tree model cares about it (name + kind).
/// Size/mtime are intentionally absent — the tree does not display them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// Backend-internal fact about a single entry. Carries `inode` for same-inode
/// rename synthesis in the tree model; the outward wire `Entry` (protocol.md)
/// exposes only name + kind + symlink_target + excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFact {
    pub kind: Kind,
    /// Filesystem inode (0 when the provider cannot supply one; rename
    /// synthesis then degrades to removed + added).
    pub inode: u64,
    /// Link target when `kind == Symlink`.
    pub symlink_target: Option<String>,
}

/// Data operations for one provider scheme. Non-recursive: `read_dir` lists a
/// single level. Create is split (`create_file` / `mkdir` / `write`-overwrite);
/// remove/rename are kind-agnostic (filesystem generic).
#[async_trait]
pub trait IFsProvider: Send + Sync {
    /// The URI scheme this provider serves (e.g. `"file"`).
    fn scheme(&self) -> &str;

    /// List one directory level: `(name, fact)` per immediate child.
    async fn read_dir(&self, uri: &str) -> Result<Vec<(String, EntryFact)>, FsError>;

    /// Stat a single entry; `None` when it does not exist.
    async fn stat(&self, uri: &str) -> Result<Option<EntryFact>, FsError>;

    /// Read whole file contents.
    async fn read(&self, uri: &str) -> Result<Vec<u8>, FsError>;

    /// Create-or-overwrite a file with `data`.
    async fn write(&self, uri: &str, data: &[u8]) -> Result<(), FsError>;

    /// Create a new empty file; errors if it already exists.
    async fn create_file(&self, uri: &str) -> Result<(), FsError>;

    /// Create a directory.
    async fn mkdir(&self, uri: &str) -> Result<(), FsError>;

    /// Remove a file or directory (recursive for directories when `recursive`).
    async fn remove(&self, uri: &str, recursive: bool) -> Result<(), FsError>;

    /// Rename/move a file or directory.
    async fn rename(&self, from: &str, to: &str) -> Result<(), FsError>;

    /// Copy a file or directory (recursive for directories when `recursive`).
    async fn copy(&self, from: &str, to: &str, recursive: bool) -> Result<(), FsError>;
}
