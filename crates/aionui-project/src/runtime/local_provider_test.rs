use std::path::Path;

use tempfile::tempdir;

use crate::canonical::{self, Canonical};
use crate::runtime::error::FsError;
use crate::runtime::provider::{IFsProvider, Kind};

use super::LocalFsProvider;

/// Canonical `file:` URI for a filesystem path (test helper).
fn canon(path: &Path) -> Canonical {
    let uri = canonical::to_file_uri(path).expect("to_file_uri");
    canonical::canonicalize(&uri).expect("canonicalize")
}

/// `file:` URI for a path joined under `root` (child, not necessarily folded).
fn uri(path: &Path) -> String {
    canonical::to_file_uri(path).expect("to_file_uri")
}

#[tokio::test]
async fn read_dir_lists_immediate_children_with_kind() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("README.md"), b"hi").unwrap();

    let provider = LocalFsProvider::new();
    let mut entries = provider.read_dir(canon(root).as_str()).await.unwrap();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["README.md", "src"]);
    assert_eq!(entries[0].1.kind, Kind::File);
    assert_eq!(entries[1].1.kind, Kind::Dir);
}

#[tokio::test]
async fn read_dir_missing_dir_errors_not_found() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("nope");
    let provider = LocalFsProvider::new();
    let err = provider.read_dir(&uri(&missing)).await.unwrap_err();
    assert!(matches!(err, FsError::NotFound { .. }), "got {err:?}");
}

#[tokio::test]
async fn stat_returns_fact_for_file_and_none_for_missing() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, b"x").unwrap();
    let provider = LocalFsProvider::new();

    let fact = provider.stat(&uri(&f)).await.unwrap().expect("some");
    assert_eq!(fact.kind, Kind::File);

    let missing = dir.path().join("gone.txt");
    assert!(provider.stat(&uri(&missing)).await.unwrap().is_none());
}

#[tokio::test]
async fn read_missing_file_errors_not_found() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("nope.txt");
    let provider = LocalFsProvider::new();
    let err = provider.read(&uri(&missing)).await.unwrap_err();
    assert!(matches!(err, FsError::NotFound { .. }), "got {err:?}");
}

#[tokio::test]
async fn write_then_read_roundtrip_and_overwrite() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("data.bin");
    let provider = LocalFsProvider::new();

    provider.write(&uri(&f), b"hello").await.unwrap();
    assert_eq!(provider.read(&uri(&f)).await.unwrap(), b"hello");

    provider.write(&uri(&f), b"world!").await.unwrap();
    assert_eq!(provider.read(&uri(&f)).await.unwrap(), b"world!");
}

#[tokio::test]
async fn create_file_new_then_already_exists_errors() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("new.txt");
    let provider = LocalFsProvider::new();

    provider.create_file(&uri(&f)).await.unwrap();
    assert!(f.exists());

    let err = provider.create_file(&uri(&f)).await.unwrap_err();
    assert!(matches!(err, FsError::AlreadyExists { .. }), "got {err:?}");
}

#[tokio::test]
async fn mkdir_creates_directory() {
    let dir = tempdir().unwrap();
    let d = dir.path().join("sub");
    let provider = LocalFsProvider::new();
    provider.mkdir(&uri(&d)).await.unwrap();
    assert!(d.is_dir());
}

#[tokio::test]
async fn remove_file_and_recursive_dir() {
    let dir = tempdir().unwrap();
    let provider = LocalFsProvider::new();

    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"x").unwrap();
    provider.remove(&uri(&f), false).await.unwrap();
    assert!(!f.exists());

    let d = dir.path().join("tree");
    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("inner.txt"), b"y").unwrap();
    provider.remove(&uri(&d), true).await.unwrap();
    assert!(!d.exists());
}

#[tokio::test]
async fn remove_nonempty_dir_without_recursive_errors() {
    let dir = tempdir().unwrap();
    let d = dir.path().join("tree");
    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("inner.txt"), b"y").unwrap();
    let provider = LocalFsProvider::new();
    assert!(provider.remove(&uri(&d), false).await.is_err());
    assert!(d.exists());
}

#[tokio::test]
async fn rename_moves_entry() {
    let dir = tempdir().unwrap();
    let from = dir.path().join("old.txt");
    let to = dir.path().join("renamed.txt");
    std::fs::write(&from, b"x").unwrap();
    let provider = LocalFsProvider::new();
    provider.rename(&uri(&from), &uri(&to)).await.unwrap();
    assert!(!from.exists() && to.exists());
}

#[tokio::test]
async fn copy_file_duplicates_content() {
    let dir = tempdir().unwrap();
    let from = dir.path().join("src.txt");
    let to = dir.path().join("dst.txt");
    std::fs::write(&from, b"payload").unwrap();
    let provider = LocalFsProvider::new();
    provider.copy(&uri(&from), &uri(&to), false).await.unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), b"payload");
    assert!(from.exists());
}

#[tokio::test]
async fn copy_dir_recursive_duplicates_tree() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("d");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"top").unwrap();
    std::fs::create_dir(src.join("sub")).unwrap();
    std::fs::write(src.join("sub").join("b.txt"), b"nested").unwrap();
    let dst = dir.path().join("d2");

    let provider = LocalFsProvider::new();
    provider.copy(&uri(&src), &uri(&dst), true).await.unwrap();

    // Whole tree duplicated, contents intact.
    assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"top");
    assert_eq!(std::fs::read(dst.join("sub").join("b.txt")).unwrap(), b"nested");
    // Source left in place.
    assert!(src.join("a.txt").exists());
    assert!(src.join("sub").join("b.txt").exists());
}

#[tokio::test]
async fn copy_dir_without_recursive_errors() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("d");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"x").unwrap();
    let dst = dir.path().join("d2");

    let provider = LocalFsProvider::new();
    let err = provider.copy(&uri(&src), &uri(&dst), false).await.unwrap_err();
    // Directory copy without recursive is rejected before any IO.
    assert!(
        matches!(&err, FsError::Io { message, .. } if message.contains("recursive")),
        "got {err:?}"
    );
    assert!(!dst.exists(), "nothing copied");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_reports_kind_and_target() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    std::fs::write(&target, b"x").unwrap();
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let provider = LocalFsProvider::new();
    let fact = provider.stat(&uri(&link)).await.unwrap().expect("some");
    assert_eq!(fact.kind, Kind::Symlink);
    assert!(fact.symlink_target.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn read_dir_populates_inode() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
    let provider = LocalFsProvider::new();
    let entries = provider.read_dir(canon(dir.path()).as_str()).await.unwrap();
    assert!(entries[0].1.inode != 0);
}
