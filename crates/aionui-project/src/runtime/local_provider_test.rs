use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::tempdir;

use crate::canonical::{self, Canonical};
use crate::runtime::error::FsError;
use crate::runtime::provider::{IFsProvider, Kind};
use crate::runtime::search::{Budget, CancellationToken, IFsSearchProvider, MatchMode, NameMatcher, SearchSink};

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

// ── noise filtering (read_dir) ───────────────────────────────────────────────

#[tokio::test]
async fn read_dir_hides_os_junk_and_vcs_but_keeps_real_dotfiles() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Noise — must be hidden.
    std::fs::write(root.join(".DS_Store"), b"x").unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join("Thumbs.db"), b"x").unwrap();
    std::fs::write(root.join("._resource"), b"x").unwrap();
    // Kept — real files including user-meaningful dotfiles.
    std::fs::write(root.join("README.md"), b"x").unwrap();
    std::fs::write(root.join(".env"), b"x").unwrap();
    std::fs::create_dir(root.join("src")).unwrap();

    let provider = LocalFsProvider::new();
    let mut entries = provider.read_dir(canon(root).as_str()).await.unwrap();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec![".env", "README.md", "src"]);
}

// ── filename search (IFsSearchProvider) ──────────────────────────────────────

/// Test sink: collects `(relative_path, name)` hits under a mutex.
#[derive(Default)]
struct CollectSink(Mutex<Vec<(String, String)>>);

impl SearchSink for CollectSink {
    fn emit(&self, relative_path: String, name: String) {
        self.0.lock().unwrap().push((relative_path, name));
    }
}

impl CollectSink {
    fn hits(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

/// Search helper that keeps the concrete sink so hits can be read back.
async fn search_collect(root: &Path, query: &str, mode: MatchMode, limit: usize) -> (Vec<(String, String)>, bool) {
    let provider = LocalFsProvider::new();
    let collect = Arc::new(CollectSink::default());
    let sink: Arc<dyn SearchSink> = collect.clone();
    let matcher = NameMatcher::new(query, mode);
    let budget = Budget::new(limit);
    let cancel = CancellationToken::new();
    provider
        .search_names(canon(root).as_str(), &matcher, &sink, &budget, &cancel)
        .await
        .unwrap();
    let mut hits = collect.hits();
    hits.sort();
    (hits, budget.limit_reached())
}

#[tokio::test]
async fn search_matches_files_by_name_substring() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("Button.tsx"), b"x").unwrap();
    std::fs::write(root.join("Widget.tsx"), b"x").unwrap();
    std::fs::write(root.join("iconButton.ts"), b"x").unwrap();

    let (hits, capped) = search_collect(root, "button", MatchMode::Substring, 100).await;
    let mut names: Vec<&str> = hits.iter().map(|(_, n)| n.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Button.tsx", "iconButton.ts"]);
    // Relative paths are forward-slash, root-relative.
    assert!(hits.iter().any(|(rel, _)| rel == "src/Button.tsx"));
    assert!(!capped);
}

#[tokio::test]
async fn search_empty_query_returns_all_files_not_dirs() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("a.txt"), b"x").unwrap();
    std::fs::write(root.join("b.txt"), b"x").unwrap();

    let (hits, _) = search_collect(root, "", MatchMode::Substring, 100).await;
    let mut names: Vec<&str> = hits.iter().map(|(_, n)| n.as_str()).collect();
    names.sort_unstable();
    // Files only — directories ("sub") are traversed but never emitted.
    assert_eq!(names, vec!["a.txt", "b.txt"]);
}

#[tokio::test]
async fn search_respects_gitignore() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), b"x").unwrap();
    std::fs::write(root.join("kept.txt"), b"x").unwrap();

    let (hits, _) = search_collect(root, "", MatchMode::Substring, 100).await;
    let names: Vec<&str> = hits.iter().map(|(_, n)| n.as_str()).collect();
    assert!(names.contains(&"kept.txt"));
    assert!(names.contains(&".gitignore"));
    // The gitignored file is excluded by the walker.
    assert!(!names.contains(&"ignored.txt"), "got {names:?}");
}

#[tokio::test]
async fn search_stops_at_budget_and_reports_limit_reached() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    for i in 0..10 {
        std::fs::write(root.join(format!("f{i}.txt")), b"x").unwrap();
    }

    let (hits, capped) = search_collect(root, "", MatchMode::Substring, 3).await;
    // Budget caps total emitted hits; the cap is reported.
    assert_eq!(hits.len(), 3);
    assert!(capped);
}

#[tokio::test]
async fn search_hides_git_internals_and_os_junk() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git").join("HEAD"), b"ref: x").unwrap();
    std::fs::write(root.join(".git").join("config"), b"x").unwrap();
    std::fs::write(root.join(".DS_Store"), b"x").unwrap();
    std::fs::write(root.join("main.rs"), b"x").unwrap();

    let (hits, _) = search_collect(root, "", MatchMode::Substring, 100).await;
    let names: Vec<&str> = hits.iter().map(|(_, n)| n.as_str()).collect();
    // Only the real file — no `.git` internals, no `.DS_Store`.
    assert_eq!(names, vec!["main.rs"]);
    assert!(
        !hits.iter().any(|(rel, _)| rel.contains(".git")),
        "search must not descend into .git: {hits:?}"
    );
}

#[tokio::test]
async fn search_cancelled_before_start_emits_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    for i in 0..300 {
        std::fs::write(root.join(format!("f{i}.txt")), b"x").unwrap();
    }
    let provider = LocalFsProvider::new();
    let collect = Arc::new(CollectSink::default());
    let sink: Arc<dyn SearchSink> = collect.clone();
    let matcher = NameMatcher::new("", MatchMode::Substring);
    let budget = Budget::new(10_000);
    let cancel = CancellationToken::new();
    cancel.cancel(); // cancelled up front
    provider
        .search_names(canon(root).as_str(), &matcher, &sink, &budget, &cancel)
        .await
        .unwrap();
    // The very first stride check (index 0) sees the cancel and returns.
    assert!(collect.hits().is_empty());
}
