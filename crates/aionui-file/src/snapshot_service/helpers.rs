//! Pure helper functions for the snapshot service.
//!
//! All functions here are synchronous and take no `&self` — they can be
//! called safely inside `spawn_blocking`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use aionui_common::FileChangeOperation;

use crate::error::FileError;
use git2::{IndexAddOption, Repository, Signature, Status, StatusOptions};

use crate::types::{CompareResult, FileChangeInfo, SnapshotInfo, SnapshotMode};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Prefix for temporary snapshot directories under the system temp dir.
pub(super) const SNAPSHOT_DIR_PREFIX: &str = "aionui-snapshot-";

/// Exclude rules written to `<git-dir>/info/exclude` for snapshot mode.
/// These patterns prevent large/generated directories from being tracked.
const SNAPSHOT_EXCLUDE_RULES: &str = "\
node_modules/
dist/
build/
target/
.venv/
__pycache__/
.DS_Store
Thumbs.db
*.pyc
.env
.env.local
.next/
.nuxt/
.output/
";

/// Signature name used for snapshot commits.
const SNAPSHOT_SIG_NAME: &str = "aionui";
/// Signature email used for snapshot commits.
const SNAPSHOT_SIG_EMAIL: &str = "snapshot@aionui.local";
/// Commit message for the initial snapshot baseline.
const SNAPSHOT_INITIAL_MSG: &str = "Initial snapshot";

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Tracked state for an initialized workspace.
#[derive(Clone, Debug)]
pub(super) struct WorkspaceState {
    pub mode: SnapshotMode,
    /// Path to the git directory.
    /// - git-repo mode: the workspace path itself (contains `.git/`).
    /// - snapshot mode: `/tmp/aionui-snapshot-{hash}` (bare-style git dir).
    pub repo_path: PathBuf,
    /// Canonical path to the actual workspace directory.
    pub workspace_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute a deterministic temp directory path for a workspace.
pub(super) fn temp_repo_path(workspace: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    workspace.hash(&mut hasher);
    let hash = hasher.finish();
    std::env::temp_dir().join(format!("{}{:016x}", SNAPSHOT_DIR_PREFIX, hash))
}

/// Open the git repository for a workspace state.
pub(super) fn open_repo(state: &WorkspaceState) -> Result<Repository, FileError> {
    Repository::open(&state.repo_path).map_err(|e| {
        FileError::Internal(format!(
            "Failed to open git repo at {}: {}",
            state.repo_path.display(),
            e
        ))
    })
}

/// Initialize a snapshot-mode temp repository for a non-git workspace.
///
/// 1. Creates the temp directory with a standard `.git` layout.
/// 2. Sets `core.worktree` to point at the real workspace.
/// 3. Writes exclude rules to `.git/info/exclude`.
/// 4. Adds all workspace files and creates an initial commit as the baseline.
pub(super) fn init_snapshot_repo(workspace: &Path, temp_dir: &Path) -> Result<(), FileError> {
    // Clean up any leftover directory from a previous run with the same hash
    if temp_dir.exists() {
        std::fs::remove_dir_all(temp_dir).map_err(|e| {
            FileError::Internal(format!(
                "Failed to clean up existing snapshot dir {}: {}",
                temp_dir.display(),
                e
            ))
        })?;
    }
    std::fs::create_dir_all(temp_dir)
        .map_err(|e| FileError::Internal(format!("Failed to create snapshot dir {}: {}", temp_dir.display(), e)))?;

    // Init a standard repo (creates .git/ inside temp_dir)
    let repo = Repository::init(temp_dir)
        .map_err(|e| FileError::Internal(format!("Failed to init snapshot repo at {}: {}", temp_dir.display(), e)))?;

    // Set workdir to the actual workspace (in-memory)
    repo.set_workdir(workspace, false)
        .map_err(|e| FileError::Internal(format!("Failed to set workdir to {}: {}", workspace.display(), e)))?;

    // Persist core.worktree in config so future opens resolve the workdir
    let mut config = repo
        .config()
        .map_err(|e| FileError::Internal(format!("Failed to open repo config: {}", e)))?;
    let ws_str = workspace.to_string_lossy();
    config
        .set_str("core.worktree", &ws_str)
        .map_err(|e| FileError::Internal(format!("Failed to set core.worktree to {}: {}", ws_str, e)))?;

    // Write exclude rules to .git/info/exclude (avoids polluting the workspace)
    let git_dir = repo.path(); // .git/ directory
    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir)
        .map_err(|e| FileError::Internal(format!("Failed to create info dir {}: {}", info_dir.display(), e)))?;
    std::fs::write(info_dir.join("exclude"), SNAPSHOT_EXCLUDE_RULES)
        .map_err(|e| FileError::Internal(format!("Failed to write exclude rules: {}", e)))?;

    // Stage all workspace files
    let mut index = repo
        .index()
        .map_err(|e| FileError::Internal(format!("Failed to get index: {}", e)))?;
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|e| FileError::Internal(format!("Failed to add files to index: {}", e)))?;
    index
        .write()
        .map_err(|e| FileError::Internal(format!("Failed to write index: {}", e)))?;

    // Create initial commit
    let tree_oid = index
        .write_tree()
        .map_err(|e| FileError::Internal(format!("Failed to write tree: {}", e)))?;
    let tree = repo
        .find_tree(tree_oid)
        .map_err(|e| FileError::Internal(format!("Failed to find tree: {}", e)))?;
    let sig = Signature::now(SNAPSHOT_SIG_NAME, SNAPSHOT_SIG_EMAIL)
        .map_err(|e| FileError::Internal(format!("Failed to create signature: {}", e)))?;
    repo.commit(Some("HEAD"), &sig, &sig, SNAPSHOT_INITIAL_MSG, &tree, &[])
        .map_err(|e| FileError::Internal(format!("Failed to create initial commit: {}", e)))?;

    Ok(())
}

/// Get the current branch name from a repository.
/// Returns `None` if HEAD is detached or the repo has no commits.
pub(super) fn current_branch(repo: &Repository) -> Option<String> {
    repo.head().ok().and_then(|head| head.shorthand().map(String::from))
}

/// Build a `SnapshotInfo` from mode and repository.
pub(super) fn build_info(mode: SnapshotMode, repo: &Repository) -> SnapshotInfo {
    let branch = match mode {
        SnapshotMode::GitRepo => current_branch(repo),
        SnapshotMode::Snapshot => None,
    };
    SnapshotInfo { mode, branch }
}

/// Map git2 index (staging area) status flags to `FileChangeOperation`.
pub(super) fn index_operation(status: Status) -> Option<FileChangeOperation> {
    if status.intersects(Status::INDEX_NEW) {
        Some(FileChangeOperation::Create)
    } else if status.intersects(Status::INDEX_MODIFIED) {
        Some(FileChangeOperation::Modify)
    } else if status.intersects(Status::INDEX_DELETED) {
        Some(FileChangeOperation::Delete)
    } else {
        None
    }
}

/// Map git2 working-tree status flags to `FileChangeOperation`.
pub(super) fn worktree_operation(status: Status) -> Option<FileChangeOperation> {
    if status.intersects(Status::WT_NEW) {
        Some(FileChangeOperation::Create)
    } else if status.intersects(Status::WT_MODIFIED) {
        Some(FileChangeOperation::Modify)
    } else if status.intersects(Status::WT_DELETED) {
        Some(FileChangeOperation::Delete)
    } else {
        None
    }
}

/// Parse git2 statuses into staged and unstaged change lists.
pub(super) fn parse_statuses(repo: &Repository, workspace: &Path) -> Result<CompareResult, FileError> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| FileError::Internal(format!("Failed to get git status: {}", e)))?;

    let ws_str = workspace.to_string_lossy();
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();

    for entry in statuses.iter() {
        let status = entry.status();
        let rel_path = match entry.path() {
            Some(p) => p.to_string(),
            None => continue,
        };
        let full_path = format!("{}/{}", ws_str.trim_end_matches('/'), &rel_path);

        if let Some(op) = index_operation(status) {
            staged.push(FileChangeInfo {
                file_path: full_path.clone(),
                relative_path: rel_path.clone(),
                operation: op,
            });
        }
        if let Some(op) = worktree_operation(status) {
            unstaged.push(FileChangeInfo {
                file_path: full_path,
                relative_path: rel_path,
                operation: op,
            });
        }
    }

    Ok(CompareResult { staged, unstaged })
}

/// Read a file's content from HEAD.
/// Returns `None` if the file is not tracked or the repo has no commits.
pub(super) fn read_baseline(repo: &Repository, rel_path: &str) -> Result<Option<String>, FileError> {
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    let commit = head
        .peel_to_commit()
        .map_err(|e| FileError::Internal(format!("Failed to peel HEAD to commit: {}", e)))?;
    let tree = commit
        .tree()
        .map_err(|e| FileError::Internal(format!("Failed to get commit tree: {}", e)))?;

    let entry = match tree.get_path(Path::new(rel_path)) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    let blob = repo
        .find_blob(entry.id())
        .map_err(|e| FileError::Internal(format!("Failed to read blob: {}", e)))?;

    match std::str::from_utf8(blob.content()) {
        Ok(s) => Ok(Some(s.to_string())),
        Err(_) => Ok(None), // Binary file -- no text baseline
    }
}

/// Canonicalize a workspace path and validate it exists.
pub(super) fn resolve_workspace(workspace: &str) -> Result<PathBuf, FileError> {
    let path = Path::new(workspace);
    if !path.exists() {
        return Err(FileError::NotFound(format!("Workspace not found: {}", workspace)));
    }
    std::fs::canonicalize(path)
        .map_err(|e| FileError::Internal(format!("Failed to canonicalize workspace path {}: {}", workspace, e)))
}

/// Stage all changes including deletions.
///
/// `index.add_all` with `DEFAULT` only handles new/modified files.
/// Deleted files must be explicitly removed from the index.
pub(super) fn stage_all_with_deletions(repo: &Repository) -> Result<(), FileError> {
    let mut index = repo
        .index()
        .map_err(|e| FileError::Internal(format!("Failed to get index: {}", e)))?;

    // Stage new and modified files
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|e| FileError::Internal(format!("Failed to stage all files: {}", e)))?;

    // Find and remove deleted files from the index
    let mut opts = StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| FileError::Internal(format!("Failed to get status: {}", e)))?;
    for entry in statuses.iter() {
        if entry.status().intersects(Status::WT_DELETED)
            && let Some(path) = entry.path()
        {
            index.remove_path(Path::new(path)).map_err(|e| {
                FileError::Internal(format!("Failed to remove deleted file {} from index: {}", path, e))
            })?;
        }
    }

    index
        .write()
        .map_err(|e| FileError::Internal(format!("Failed to write index: {}", e)))?;
    Ok(())
}

/// Stage a single file, handling both existing and deleted files.
///
/// For existing files, adds to the index. For deleted files, removes from
/// the index (equivalent to `git add <deleted-file>`).
pub(super) fn stage_single_file(repo: &Repository, rel_path: &str) -> Result<(), FileError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| FileError::Internal("Repository has no workdir".into()))?;
    let abs_path = workdir.join(rel_path);

    let mut index = repo
        .index()
        .map_err(|e| FileError::Internal(format!("Failed to get index: {}", e)))?;

    if abs_path.exists() {
        index
            .add_path(Path::new(rel_path))
            .map_err(|e| FileError::Internal(format!("Failed to stage file {}: {}", rel_path, e)))?;
    } else {
        // File was deleted from disk; remove from index
        index
            .remove_path(Path::new(rel_path))
            .map_err(|e| FileError::Internal(format!("Failed to stage deleted file {}: {}", rel_path, e)))?;
    }

    index
        .write()
        .map_err(|e| FileError::Internal(format!("Failed to write index: {}", e)))?;
    Ok(())
}

/// Unstage a single file (reset it in the index to match HEAD).
pub(super) fn unstage_single_file(repo: &Repository, rel_path: &str) -> Result<(), FileError> {
    let head = repo
        .head()
        .map_err(|e| FileError::Internal(format!("Failed to get HEAD: {}", e)))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| FileError::Internal(format!("Failed to peel HEAD: {}", e)))?;
    // reset_default expects a commit-ish object, not a tree
    repo.reset_default(Some(commit.as_object()), [rel_path])
        .map_err(|e| FileError::Internal(format!("Failed to unstage file {}: {}", rel_path, e)))?;
    Ok(())
}

/// Unstage all staged changes (mixed reset to HEAD).
pub(super) fn unstage_all_files(repo: &Repository) -> Result<(), FileError> {
    let head = repo
        .head()
        .map_err(|e| FileError::Internal(format!("Failed to get HEAD: {}", e)))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| FileError::Internal(format!("Failed to peel HEAD: {}", e)))?;
    repo.reset(commit.as_object(), git2::ResetType::Mixed, None)
        .map_err(|e| FileError::Internal(format!("Failed to unstage all: {}", e)))?;
    Ok(())
}

/// Discard working-tree changes for a single file.
///
/// - `Create`: delete the new file from disk.
/// - `Modify`: restore file content from HEAD.
/// - `Delete`: restore the deleted file from HEAD.
pub(super) fn discard_single_file(
    repo: &Repository,
    workspace: &Path,
    rel_path: &str,
    operation: FileChangeOperation,
) -> Result<(), FileError> {
    match operation {
        FileChangeOperation::Create => {
            // New/untracked file: just delete it
            let abs_path = workspace.join(rel_path);
            if abs_path.exists() {
                std::fs::remove_file(&abs_path)
                    .map_err(|e| FileError::Internal(format!("Failed to delete file {}: {}", abs_path.display(), e)))?;
            }
            Ok(())
        }
        FileChangeOperation::Modify | FileChangeOperation::Delete => {
            // Restore file from HEAD using checkout
            checkout_path_from_head(repo, rel_path)
        }
    }
}

/// Reset a file completely: unstage (if staged) and restore working tree.
///
/// - `Create`: unstage + delete file.
/// - `Modify`: unstage + restore from HEAD.
/// - `Delete`: unstage + restore from HEAD.
pub(super) fn reset_single_file(
    repo: &Repository,
    workspace: &Path,
    rel_path: &str,
    operation: FileChangeOperation,
) -> Result<(), FileError> {
    // Step 1: unstage (ignore errors for files not in index)
    let _ = unstage_single_file(repo, rel_path);

    // Step 2: restore working tree
    discard_single_file(repo, workspace, rel_path, operation)
}

/// Checkout a single file from HEAD, restoring it in the working tree.
fn checkout_path_from_head(repo: &Repository, rel_path: &str) -> Result<(), FileError> {
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.force().path(rel_path);

    repo.checkout_head(Some(&mut cb))
        .map_err(|e| FileError::Internal(format!("Failed to checkout {} from HEAD: {}", rel_path, e)))?;
    Ok(())
}

/// List all branch names in the repository.
pub(super) fn list_branches(repo: &Repository) -> Result<Vec<String>, FileError> {
    let branches = repo
        .branches(Some(git2::BranchType::Local))
        .map_err(|e| FileError::Internal(format!("Failed to list branches: {}", e)))?;

    let mut names = Vec::new();
    for branch_result in branches {
        let (branch, _) = branch_result.map_err(|e| FileError::Internal(format!("Failed to read branch: {}", e)))?;
        if let Some(name) = branch
            .name()
            .map_err(|e| FileError::Internal(format!("Failed to get branch name: {}", e)))?
        {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- temp_repo_path --

    #[test]
    fn temp_repo_path_deterministic() {
        let a = temp_repo_path("/home/user/project");
        let b = temp_repo_path("/home/user/project");
        assert_eq!(a, b);
    }

    #[test]
    fn temp_repo_path_different_for_different_workspaces() {
        let a = temp_repo_path("/home/user/project-a");
        let b = temp_repo_path("/home/user/project-b");
        assert_ne!(a, b);
    }

    #[test]
    fn temp_repo_path_has_prefix() {
        let p = temp_repo_path("/ws");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(SNAPSHOT_DIR_PREFIX));
    }

    // -- index_operation / worktree_operation --

    #[test]
    fn index_operation_new() {
        assert_eq!(index_operation(Status::INDEX_NEW), Some(FileChangeOperation::Create));
    }

    #[test]
    fn index_operation_modified() {
        assert_eq!(
            index_operation(Status::INDEX_MODIFIED),
            Some(FileChangeOperation::Modify)
        );
    }

    #[test]
    fn index_operation_deleted() {
        assert_eq!(
            index_operation(Status::INDEX_DELETED),
            Some(FileChangeOperation::Delete)
        );
    }

    #[test]
    fn index_operation_none_for_wt() {
        assert_eq!(index_operation(Status::WT_NEW), None);
    }

    #[test]
    fn worktree_operation_new() {
        assert_eq!(worktree_operation(Status::WT_NEW), Some(FileChangeOperation::Create));
    }

    #[test]
    fn worktree_operation_modified() {
        assert_eq!(
            worktree_operation(Status::WT_MODIFIED),
            Some(FileChangeOperation::Modify)
        );
    }

    #[test]
    fn worktree_operation_deleted() {
        assert_eq!(
            worktree_operation(Status::WT_DELETED),
            Some(FileChangeOperation::Delete)
        );
    }

    #[test]
    fn worktree_operation_none_for_index() {
        assert_eq!(worktree_operation(Status::INDEX_NEW), None);
    }

    // -- resolve_workspace --

    #[test]
    fn resolve_workspace_not_found() {
        let err = resolve_workspace("/nonexistent/path/xyz123").unwrap_err();
        assert!(matches!(err, FileError::NotFound(_)));
    }

    #[test]
    fn resolve_workspace_success() {
        let tmp = std::env::temp_dir();
        let result = resolve_workspace(tmp.to_str().unwrap());
        assert!(result.is_ok());
    }

    // -- current_branch --

    #[test]
    fn current_branch_of_fresh_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        // Fresh repo with no commits -- HEAD is unborn
        assert!(current_branch(&repo).is_none());

        // Create an initial commit so HEAD points to a branch
        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let branch = current_branch(&repo);
        assert!(branch.is_some());
        assert!(!branch.unwrap().is_empty());
    }

    // -- build_info --

    #[test]
    fn build_info_git_repo_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let info = build_info(SnapshotMode::GitRepo, &repo);
        assert_eq!(info.mode, SnapshotMode::GitRepo);
        assert!(info.branch.is_some());
    }

    #[test]
    fn build_info_snapshot_mode_returns_no_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let info = build_info(SnapshotMode::Snapshot, &repo);
        assert_eq!(info.mode, SnapshotMode::Snapshot);
        assert!(info.branch.is_none());
    }

    // -- read_baseline --

    #[test]
    fn read_baseline_no_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let result = read_baseline(&repo, "any.txt").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_baseline_tracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("hello.txt"), "Hello, world!").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("hello.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "add hello", &tree, &[]).unwrap();

        let content = read_baseline(&repo, "hello.txt").unwrap();
        assert_eq!(content.as_deref(), Some("Hello, world!"));
    }

    #[test]
    fn read_baseline_untracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let content = read_baseline(&repo, "missing.txt").unwrap();
        assert!(content.is_none());
    }

    // -- parse_statuses --

    #[test]
    fn parse_statuses_clean_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert!(result.staged.is_empty());
        assert!(result.unstaged.is_empty());
    }

    #[test]
    fn parse_statuses_new_untracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::write(tmp.path().join("b.txt"), "b").unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert!(result.staged.is_empty());
        assert_eq!(result.unstaged.len(), 1);
        assert_eq!(result.unstaged[0].relative_path, "b.txt");
        assert_eq!(result.unstaged[0].operation, FileChangeOperation::Create);
    }

    #[test]
    fn parse_statuses_modified_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "original").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "modified").unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert!(result.staged.is_empty());
        assert_eq!(result.unstaged.len(), 1);
        assert_eq!(result.unstaged[0].operation, FileChangeOperation::Modify);
    }

    #[test]
    fn parse_statuses_deleted_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert!(result.staged.is_empty());
        assert_eq!(result.unstaged.len(), 1);
        assert_eq!(result.unstaged[0].operation, FileChangeOperation::Delete);
    }

    #[test]
    fn parse_statuses_staged_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::write(tmp.path().join("new.txt"), "new content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert_eq!(result.staged.len(), 1);
        assert_eq!(result.staged[0].relative_path, "new.txt");
        assert_eq!(result.staged[0].operation, FileChangeOperation::Create);
        assert!(result.unstaged.is_empty());
    }

    #[test]
    fn parse_statuses_staged_and_unstaged_mixed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "original").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "staged change").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();

        std::fs::write(tmp.path().join("a.txt"), "unstaged change").unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert_eq!(result.staged.len(), 1);
        assert_eq!(result.staged[0].operation, FileChangeOperation::Modify);
        assert_eq!(result.unstaged.len(), 1);
        assert_eq!(result.unstaged[0].operation, FileChangeOperation::Modify);
    }

    // -- stage_all_with_deletions --

    #[test]
    fn stage_all_handles_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        // Commit two files
        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "b").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.add_path(Path::new("b.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        // Delete b.txt and modify a.txt
        std::fs::remove_file(tmp.path().join("b.txt")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "modified").unwrap();

        stage_all_with_deletions(&repo).unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        // Both changes should be staged now
        assert_eq!(result.staged.len(), 2);
        assert!(result.unstaged.is_empty());

        let delete_entry = result
            .staged
            .iter()
            .find(|e| e.relative_path == "b.txt")
            .expect("b.txt should be staged");
        assert_eq!(delete_entry.operation, FileChangeOperation::Delete);
    }

    // -- stage_single_file --

    #[test]
    fn stage_single_file_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        stage_single_file(&repo, "a.txt").unwrap();

        let result = parse_statuses(&repo, tmp.path()).unwrap();
        assert_eq!(result.staged.len(), 1);
        assert_eq!(result.staged[0].operation, FileChangeOperation::Delete);
        assert!(result.unstaged.is_empty());
    }

    // -- discard_single_file --

    #[test]
    fn discard_created_file_deletes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::write(tmp.path().join("new.txt"), "new").unwrap();
        assert!(tmp.path().join("new.txt").exists());

        discard_single_file(&repo, tmp.path(), "new.txt", FileChangeOperation::Create).unwrap();

        assert!(!tmp.path().join("new.txt").exists());
    }

    #[test]
    fn discard_modified_file_restores_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "original").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "modified").unwrap();

        discard_single_file(&repo, tmp.path(), "a.txt", FileChangeOperation::Modify).unwrap();

        let content = std::fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(content, "original");
    }

    #[test]
    fn discard_deleted_file_restores_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.txt"), "content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        assert!(!tmp.path().join("a.txt").exists());

        discard_single_file(&repo, tmp.path(), "a.txt", FileChangeOperation::Delete).unwrap();

        assert!(tmp.path().join("a.txt").exists());
        let content = std::fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(content, "content");
    }

    // -- list_branches --

    #[test]
    fn list_branches_returns_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let branches = list_branches(&repo).unwrap();
        assert_eq!(branches.len(), 1);
    }

    #[test]
    fn list_branches_includes_created_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let mut index = repo.index().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@test.com").unwrap();
        let commit_oid = repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        let commit = repo.find_commit(commit_oid).unwrap();

        repo.branch("feature-a", &commit, false).unwrap();
        repo.branch("feature-b", &commit, false).unwrap();

        let branches = list_branches(&repo).unwrap();
        assert_eq!(branches.len(), 3); // default + feature-a + feature-b
        assert!(branches.contains(&"feature-a".to_string()));
        assert!(branches.contains(&"feature-b".to_string()));
    }
}
