//! `GitScmProvider` tests over real temporary repositories.
//!
//! Repositories are built with git2 itself (no `git` CLI dependency) so the
//! suite runs the same way on mac / windows / linux. Paths are always joined,
//! never written as literals, and assertions compare pe-relative paths that git
//! reports with `/` separators on every platform.

use tempfile::TempDir;

use super::super::trash_sink::TrashSink;
use super::*;

/// Committer identity for test commits: repo-local config, so a machine without
/// a global git identity still runs the suite.
fn init_repo(dir: &Path) -> Repository {
    let repo = Repository::init(dir).expect("init repo");
    let mut cfg = repo.config().expect("config");
    cfg.set_str("user.name", "scm test").expect("set name");
    cfg.set_str("user.email", "scm@test.local").expect("set email");
    repo
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write file");
}

/// Commit everything currently in the work tree, returning the new commit.
fn commit_all(repo: &Repository, message: &str) -> git2::Oid {
    let mut index = repo.index().expect("index");
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add all");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = repo.signature().expect("signature");
    let parents = match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(parent) => vec![parent],
        None => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .expect("commit")
}

/// An attached-style resolved root at `dir` (discovery stays one-repo-or-none).
fn root_at(pe_id: &str, dir: &Path, label: &str) -> ResolvedRoot {
    ResolvedRoot {
        pe_id: pe_id.to_owned(),
        absolute_path: dir.to_string_lossy().into_owned(),
        label: label.to_owned(),
        pe_name: None,
        discover_children: false,
    }
}

/// A workspace-style resolved root at `dir` (one-level child discovery).
fn workspace_root_at(pe_id: &str, dir: &Path, label: &str) -> ResolvedRoot {
    ResolvedRoot {
        discover_children: true,
        ..root_at(pe_id, dir, label)
    }
}

/// A discovered repository plus the provider that owns it.
async fn discovered(dir: &Path) -> (GitScmProvider, RepoRef) {
    let provider = GitScmProvider::new();
    let root = root_at("pe1", dir, "fixture");
    let mut repos = provider.discover(&root).await.expect("discover ok");
    assert_eq!(repos.len(), 1, "an attached root surfaces exactly one repo");
    let repo = repos.remove(0);
    (provider, RepoRef { repo_id: repo.repo_id })
}

fn find<'a>(status: &'a ScmStatus, rel: &str, staged: Option<bool>) -> Option<&'a ScmResource> {
    status
        .resources
        .iter()
        .find(|r| r.repo_relative_path == rel && r.staged == staged)
}

#[tokio::test]
async fn discover_reports_none_for_plain_directory() {
    let tmp = TempDir::new().expect("tempdir");
    let provider = GitScmProvider::new();
    let root = root_at("pe1", tmp.path(), "plain");

    // Not a repository is a normal outcome, never a fabricated repo.
    assert!(provider.discover(&root).await.expect("discover ok").is_empty());
}

#[tokio::test]
async fn discover_does_not_walk_up_to_parent_repository() {
    let tmp = TempDir::new().expect("tempdir");
    init_repo(tmp.path());
    let child = tmp.path().join("sub");
    std::fs::create_dir_all(&child).expect("mkdir child");

    let provider = GitScmProvider::new();
    let root = root_at("pe-child", &child, "child");

    // One pe root is at most one repo: a subdirectory of a repo is not itself a
    // repo, and discovery must not surface the parent (decision "1 pe : 1 repo").
    assert!(provider.discover(&root).await.expect("discover ok").is_empty());
}

#[tokio::test]
async fn discover_reports_identity_capabilities_and_head() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "hello\n");
    commit_all(&repo, "base");

    let provider = GitScmProvider::new();
    let root = root_at("pe1", tmp.path(), "fixture");
    let mut repos = provider.discover(&root).await.expect("discover ok");
    assert_eq!(repos.len(), 1, "root that is itself a repo surfaces one");
    let found = repos.remove(0);

    assert_eq!(found.provider_id, "git");
    assert_eq!(found.root.pe_id, "pe1");
    assert_eq!(found.root.relative_path, "", "repo root is the pe root itself");
    assert!(found.repo_id.starts_with("scm:"), "repo_id: {}", found.repo_id);
    assert!(found.capabilities.staging, "git has a staging area");
    assert!(found.capabilities.local_branches);
    assert!(!found.capabilities.history_graph, "stage 2, not advertised yet");
    assert!(!found.capabilities.remote_ops, "stage 2, not advertised yet");
    assert_eq!(found.state, ScmRepositoryState::Idle);
    assert!(!found.is_worktree, "a primary clone is not a worktree");
    assert!(
        found.worktree_of.is_none(),
        "a primary clone has no primary to point at"
    );
}

// ---- one-level workspace discovery -------------------------------------------

/// Init a repository at `dir` with one commit, so it is a normal (non-bare)
/// repository with a work tree — the shape discovery surfaces.
fn init_committed_repo(dir: &Path) -> Repository {
    std::fs::create_dir_all(dir).expect("mkdir repo");
    let repo = init_repo(dir);
    write(dir, "a.txt", "hello\n");
    commit_all(&repo, "base");
    repo
}

/// Find the surfaced repo whose child directory name matches `name`.
fn by_child<'a>(repos: &'a [ScmRepository], name: &str) -> &'a ScmRepository {
    repos
        .iter()
        .find(|r| r.root.relative_path == name)
        .unwrap_or_else(|| panic!("child repo {name} surfaced; got {:?}", child_names(repos)))
}

fn child_names(repos: &[ScmRepository]) -> Vec<String> {
    repos.iter().map(|r| r.root.relative_path.clone()).collect()
}

#[tokio::test]
async fn workspace_root_surfaces_each_child_repository() {
    let tmp = TempDir::new().expect("tempdir");
    init_committed_repo(&tmp.path().join("svc-a"));
    init_committed_repo(&tmp.path().join("svc-b"));
    // A plain (non-repo) directory among them is ignored, not surfaced.
    std::fs::create_dir_all(tmp.path().join("docs")).expect("mkdir docs");
    write(tmp.path(), "docs/readme.md", "not a repo\n");

    let provider = GitScmProvider::new();
    let root = workspace_root_at("ws", tmp.path(), "workspace");
    let repos = provider.discover(&root).await.expect("discover ok");

    let mut names = child_names(&repos);
    names.sort();
    assert_eq!(
        names,
        vec!["svc-a".to_owned(), "svc-b".to_owned()],
        "only child repos surface"
    );

    for name in ["svc-a", "svc-b"] {
        let repo = by_child(&repos, name);
        // Contract: label = child dir name, pe_name None (name belongs to pe
        // root), repo_id folds relative_path in, is_worktree false.
        assert_eq!(repo.label, name);
        assert!(repo.pe_name.is_none(), "child repo carries no pe entry name");
        assert_eq!(repo.repo_id, format!("scm:ws/{name}"));
        assert_eq!(repo.root.pe_id, "ws");
        assert!(!repo.is_worktree);
        assert!(repo.worktree_of.is_none());
    }
}

#[tokio::test]
async fn workspace_root_that_is_itself_a_repository_does_not_scan_children() {
    // Root is a repo *and* has child repos: the root wins, children are not
    // inspected (so a submodule / nested clone is never mis-captured).
    let tmp = TempDir::new().expect("tempdir");
    init_committed_repo(tmp.path());
    init_committed_repo(&tmp.path().join("nested"));

    let provider = GitScmProvider::new();
    let root = workspace_root_at("ws", tmp.path(), "workspace");
    let repos = provider.discover(&root).await.expect("discover ok");

    assert_eq!(repos.len(), 1, "the root repo alone; children not scanned");
    assert_eq!(repos[0].root.relative_path, "", "surfaced as the root itself");
    assert_eq!(repos[0].repo_id, "scm:ws");
}

#[tokio::test]
async fn workspace_discovery_skips_hidden_directories() {
    let tmp = TempDir::new().expect("tempdir");
    init_committed_repo(&tmp.path().join("visible"));
    // A hidden dir that is itself a repo must still be skipped.
    init_committed_repo(&tmp.path().join(".hidden-repo"));

    let provider = GitScmProvider::new();
    let root = workspace_root_at("ws", tmp.path(), "workspace");
    let repos = provider.discover(&root).await.expect("discover ok");

    assert_eq!(
        child_names(&repos),
        vec!["visible".to_owned()],
        "dot-directories are skipped"
    );
}

#[tokio::test]
async fn attached_root_never_scans_children_even_with_child_repos() {
    // An attached pe root keeps one-repo-or-none: a non-repo root with child
    // repos surfaces nothing. Attach behaviour is unchanged.
    let tmp = TempDir::new().expect("tempdir");
    init_committed_repo(&tmp.path().join("svc-a"));

    let provider = GitScmProvider::new();
    let root = root_at("pe1", tmp.path(), "attached");
    let repos = provider.discover(&root).await.expect("discover ok");

    assert!(repos.is_empty(), "attached discovery does not relax by a level");
}

#[tokio::test]
async fn workspace_worktree_points_at_primary_when_primary_in_view() {
    let tmp = TempDir::new().expect("tempdir");
    let primary = init_committed_repo(&tmp.path().join("main"));
    // A linked worktree of `main`, sitting as a sibling child directory.
    let wt_path = tmp.path().join("feature");
    primary.worktree("feature", &wt_path, None).expect("add worktree");

    let provider = GitScmProvider::new();
    let root = workspace_root_at("ws", tmp.path(), "workspace");
    let repos = provider.discover(&root).await.expect("discover ok");

    let main = by_child(&repos, "main");
    let feature = by_child(&repos, "feature");

    assert!(!main.is_worktree, "the primary clone is not a worktree");
    assert!(feature.is_worktree, "the linked worktree is flagged");
    assert_eq!(
        feature.worktree_of.as_deref(),
        Some(main.repo_id.as_str()),
        "worktree points at its primary's repo_id when the primary is in view"
    );
}

#[tokio::test]
async fn workspace_worktree_without_primary_has_none_owner() {
    // The worktree lives under the workspace but its primary does not: matching
    // is by real git dir, so with no in-view primary the owner is None (the
    // client then renders it at the outer level).
    let outside = TempDir::new().expect("tempdir primary");
    let primary = init_committed_repo(outside.path());

    let tmp = TempDir::new().expect("tempdir workspace");
    let wt_path = tmp.path().join("feature");
    primary.worktree("feature", &wt_path, None).expect("add worktree");

    let provider = GitScmProvider::new();
    let root = workspace_root_at("ws", tmp.path(), "workspace");
    let repos = provider.discover(&root).await.expect("discover ok");

    let feature = by_child(&repos, "feature");
    assert!(feature.is_worktree, "still a worktree even with no in-view primary");
    assert!(
        feature.worktree_of.is_none(),
        "no in-view primary to point at, so the owner is None"
    );
}

#[tokio::test]
async fn status_reports_untracked_as_created_unstaged() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "hello\n");
    commit_all(&repo, "base");
    // Nested so the recursive-untracked setting is actually exercised: without
    // it this collapses to a single directory entry.
    write(tmp.path(), "fresh/new.txt", "new\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    let res = find(&status, "fresh/new.txt", Some(false)).expect("untracked file listed per file");
    assert_eq!(res.state, ScmResourceState::Created);
    assert!(
        res.file.pe_id.is_empty(),
        "a provider does not assemble pe identity — the orchestration layer owns it"
    );
    assert!(!status.truncated);
}

#[tokio::test]
async fn status_is_not_degraded_when_the_index_can_be_written() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "hello\n");
    commit_all(&repo, "base");
    write(tmp.path(), "a.txt", "changed\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    // Guards the degraded flag against being trivially always-true.
    assert!(!status.degraded, "a writable repository takes the normal path");
}

/// Create a branch at head and check it out (updates HEAD + work tree), so the
/// test exercises the exact head move a terminal `git checkout <branch>` makes.
fn checkout_new_branch(repo: &Repository, name: &str) {
    let head_commit = repo.head().expect("head").peel_to_commit().expect("commit");
    repo.branch(name, &head_commit, false).expect("create branch");
    let refname = format!("refs/heads/{name}");
    let obj = repo.revparse_single(&refname).expect("revparse branch");
    repo.checkout_tree(&obj, None).expect("checkout tree");
    repo.set_head(&refname).expect("set head");
}

#[tokio::test]
async fn status_carries_head_branch_name() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "hello\n");
    commit_all(&repo, "base");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    // The status frame is the only frame the refresh path emits; the branch name
    // has to be on it or a client subscribed to status alone never learns head.
    let head = status.head.expect("status carries head");
    let name = head.name.expect("branch name present");
    // git2 defaults to `master` on init; either is acceptable across git versions.
    assert!(name == "master" || name == "main", "branch name: {name}");
    assert!(head.detached.is_none(), "an ordinary branch head is not detached");
}

#[tokio::test]
async fn status_head_name_changes_after_checkout() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "hello\n");
    commit_all(&repo, "base");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let before = provider.status(&repo_ref).await.expect("status ok");
    let before_name = before.head.expect("head before").name.expect("name before");

    // Simulate the terminal `git checkout feature/x` that motivated this frame.
    checkout_new_branch(&repo, "feature/x");
    let after = provider.status(&repo_ref).await.expect("status ok");
    let after_name = after.head.expect("head after").name.expect("name after");

    assert_ne!(before_name, after_name, "checkout must move the reported head");
    assert_eq!(after_name, "feature/x");
}

#[tokio::test]
async fn status_head_reports_detached() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "hello\n");
    let oid = commit_all(&repo, "base");

    // Detach: point HEAD directly at the commit, no branch.
    repo.set_head_detached(oid).expect("detach head");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    let head = status.head.expect("status carries head");
    // `detached` is the load-bearing signal here; `name` mirrors git2's
    // shorthand (the short commit id when detached), same as `ScmRepository.head`.
    assert_eq!(head.detached, Some(true), "detached head is flagged");
    assert_ne!(
        head.name.as_deref(),
        Some("master"),
        "a detached head reports the commit, not the abandoned branch"
    );
}

#[tokio::test]
async fn status_head_reports_unborn() {
    let tmp = TempDir::new().expect("tempdir");
    init_repo(tmp.path());
    // No commit yet: HEAD is unborn. A fresh repo still has a change list.
    write(tmp.path(), "a.txt", "hello\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    // Unborn is a normal state, not an error: head is present but empty.
    let head = status.head.expect("status carries head even when unborn");
    assert!(head.name.is_none(), "unborn head has no branch name yet");
    assert!(head.detached.is_none(), "unborn head is not detached");
}

#[tokio::test]
async fn status_reports_modified_and_deleted() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "m.txt", "one\n");
    write(tmp.path(), "d.txt", "two\n");
    commit_all(&repo, "base");

    write(tmp.path(), "m.txt", "one changed\n");
    std::fs::remove_file(tmp.path().join("d.txt")).expect("remove");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    assert_eq!(
        find(&status, "m.txt", Some(false)).expect("modified listed").state,
        ScmResourceState::Modified
    );
    assert_eq!(
        find(&status, "d.txt", Some(false)).expect("deleted listed").state,
        ScmResourceState::Deleted
    );
}

#[tokio::test]
async fn status_splits_one_file_staged_and_unstaged() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "both.txt", "base\n");
    commit_all(&repo, "base");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "both.txt".to_owned(),
    };

    write(tmp.path(), "both.txt", "staged\n");
    provider
        .staging()
        .expect("git declares staging")
        .stage(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("stage ok");
    // Edit again after staging: the same path now differs on both sides.
    write(tmp.path(), "both.txt", "staged then edited\n");

    let status = provider.status(&repo_ref).await.expect("status ok");
    assert!(
        find(&status, "both.txt", Some(true)).is_some(),
        "staged side present: {:?}",
        status.resources
    );
    assert!(
        find(&status, "both.txt", Some(false)).is_some(),
        "unstaged side present: {:?}",
        status.resources
    );
}

#[tokio::test]
async fn status_reports_conflicted_as_opaque_not_modified() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "c.txt", "base\n");
    commit_all(&repo, "base");
    let base = repo.head().unwrap().peel_to_commit().unwrap();

    // Two divergent branches touching the same line, then merge → conflict.
    repo.branch("other", &base, false).expect("branch");
    write(tmp.path(), "c.txt", "main side\n");
    commit_all(&repo, "main change");

    let other = repo.find_branch("other", git2::BranchType::Local).expect("find branch");
    let other_commit = other.get().peel_to_commit().expect("peel");
    repo.set_head("refs/heads/other").expect("set head");
    repo.reset(other_commit.as_object(), git2::ResetType::Hard, None)
        .expect("reset hard");
    write(tmp.path(), "c.txt", "other side\n");
    commit_all(&repo, "other change");

    let main_commit = repo
        .find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .expect("find default branch")
        .get()
        .peel_to_commit()
        .expect("peel");
    let their = repo.find_annotated_commit(main_commit.id()).expect("annotated");
    // Conflicts are the expected outcome here, so a merge error is not a failure.
    let _ = repo.merge(&[&their], None, None);

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    let conflicted = status
        .resources
        .iter()
        .find(|r| r.repo_relative_path == "c.txt")
        .expect("conflicted file listed");
    assert_eq!(
        conflicted.state,
        ScmResourceState::Conflicted,
        "must not fold into modified: acting on it can destroy a resolution"
    );
    assert!(conflicted.state.is_opaque());
    assert_eq!(conflicted.staged, None, "no staged side for an opaque resource");
}

#[tokio::test]
async fn stage_records_a_deletion() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "gone.txt", "bye\n");
    commit_all(&repo, "base");
    std::fs::remove_file(tmp.path().join("gone.txt")).expect("remove");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "gone.txt".to_owned(),
    };
    provider
        .staging()
        .expect("staging")
        .stage(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("staging a deletion must not error");

    let status = provider.status(&repo_ref).await.expect("status ok");
    assert_eq!(
        find(&status, "gone.txt", Some(true)).expect("deletion staged").state,
        ScmResourceState::Deleted
    );
}

#[tokio::test]
async fn unstage_returns_change_to_the_work_tree_side() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "u.txt", "base\n");
    commit_all(&repo, "base");
    write(tmp.path(), "u.txt", "changed\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let staging = provider.staging().expect("staging");
    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "u.txt".to_owned(),
    };

    staging
        .stage(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("stage ok");
    assert!(
        find(&provider.status(&repo_ref).await.unwrap(), "u.txt", Some(true)).is_some(),
        "staged after stage"
    );

    staging
        .unstage(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("unstage ok");
    let after = provider.status(&repo_ref).await.expect("status ok");
    assert!(find(&after, "u.txt", Some(true)).is_none(), "no longer staged");
    assert_eq!(
        find(&after, "u.txt", Some(false))
            .expect("back on the work-tree side")
            .state,
        ScmResourceState::Modified,
        "unstage keeps the working-tree edit"
    );
}

#[tokio::test]
async fn revert_restores_a_tracked_file() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "r.txt", "original\n");
    commit_all(&repo, "base");
    write(tmp.path(), "r.txt", "scribbled\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "r.txt".to_owned(),
            }],
        )
        .await
        .expect("revert ok");

    let body = std::fs::read_to_string(tmp.path().join("r.txt")).expect("read back");
    assert_eq!(body, "original\n", "tracked file restored to its committed content");
}

#[tokio::test]
async fn revert_refuses_a_conflicted_resource() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "c.txt", "base\n");
    commit_all(&repo, "base");
    let base = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("other", &base, false).expect("branch");
    write(tmp.path(), "c.txt", "main side\n");
    commit_all(&repo, "main change");
    let other_commit = repo
        .find_branch("other", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    repo.set_head("refs/heads/other").unwrap();
    repo.reset(other_commit.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    write(tmp.path(), "c.txt", "other side\n");
    commit_all(&repo, "other change");
    let main_commit = repo
        .find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let their = repo.find_annotated_commit(main_commit.id()).unwrap();
    let _ = repo.merge(&[&their], None, None);

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let err = provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "c.txt".to_owned(),
            }],
        )
        .await
        .expect_err("conflicted resources are opaque in stage 1");

    assert!(
        matches!(err, ScmError::OpaqueResource { .. }),
        "expected an opaque-resource refusal, got {err:?}"
    );
}

#[tokio::test]
async fn original_reads_each_anchor() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "committed\n");
    commit_all(&repo, "base");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "a.txt".to_owned(),
    };

    write(tmp.path(), "a.txt", "staged\n");
    provider
        .staging()
        .expect("staging")
        .stage(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("stage ok");
    write(tmp.path(), "a.txt", "working\n");

    let at = |anchor| {
        let provider = &provider;
        let repo_ref = &repo_ref;
        let file = &file;
        async move {
            String::from_utf8(
                provider
                    .original(repo_ref, file, anchor)
                    .await
                    .expect("original ok")
                    .expect("content present"),
            )
            .expect("utf-8")
        }
    };

    assert_eq!(at(ContentRef::Working).await, "working\n");
    assert_eq!(at(ContentRef::Staged).await, "staged\n");
    assert_eq!(at(ContentRef::Committed).await, "committed\n");
}

#[tokio::test]
async fn original_is_none_for_a_file_absent_at_that_anchor() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "seed.txt", "seed\n");
    commit_all(&repo, "base");
    write(tmp.path(), "brand-new.txt", "fresh\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "brand-new.txt".to_owned(),
    };

    // An added file has no committed version — `None`, not an error.
    assert!(
        provider
            .original(&repo_ref, &file, ContentRef::Committed)
            .await
            .expect("original ok")
            .is_none()
    );
}

#[tokio::test]
async fn diff_between_anchors_reports_both_sides() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "d.txt", "before\n");
    commit_all(&repo, "base");
    write(tmp.path(), "d.txt", "after\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let diff = provider
        .diff(
            &repo_ref,
            &FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "d.txt".to_owned(),
            },
            ContentRef::Working,
            ContentRef::Committed,
        )
        .await
        .expect("diff ok");

    let patch = diff.patch.expect("patch present");
    assert!(patch.contains("-after") || patch.contains("+after"), "patch: {patch}");
    assert!(patch.contains("before"), "patch mentions the other side: {patch}");
    assert!(!diff.binary);
}

#[tokio::test]
async fn diff_flags_binary_content() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    std::fs::write(tmp.path().join("b.bin"), [0x00, 0x01, 0x02]).expect("write binary");
    commit_all(&repo, "base");
    std::fs::write(tmp.path().join("b.bin"), [0x00, 0x03]).expect("rewrite binary");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let diff = provider
        .diff(
            &repo_ref,
            &FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "b.bin".to_owned(),
            },
            ContentRef::Working,
            ContentRef::Committed,
        )
        .await
        .expect("diff ok");

    assert!(diff.binary, "binary content is flagged, not inlined");
    assert!(diff.patch.is_none());
}

#[tokio::test]
async fn unknown_repository_is_rejected() {
    let provider = GitScmProvider::new();
    let err = provider
        .status(&RepoRef {
            repo_id: "scm:never-discovered".to_owned(),
        })
        .await
        .expect_err("unknown repo must not be served");

    assert!(matches!(err, ScmError::UnknownRepository { .. }), "got {err:?}");
}

/// A provider without a staging area must refuse the staged anchor rather than
/// serve a meaningless answer. Exercised through the shared gate, since the git
/// provider itself always has staging.
#[test]
fn staged_anchor_is_refused_without_the_staging_capability() {
    let caps = ScmCapabilities {
        staging: false,
        local_branches: false,
        history_graph: false,
        remote_ops: false,
    };

    let err = check_anchor(caps, ContentRef::Staged).expect_err("staged anchor needs staging");
    assert!(
        matches!(err, ScmError::CapabilityUnsupported { capability: "staging" }),
        "got {err:?}"
    );

    // The anchors every provider has must stay available.
    check_anchor(caps, ContentRef::Working).expect("working is universal");
    check_anchor(caps, ContentRef::Committed).expect("committed is universal");
}

/// The stale-index fallback: with a read-only `.git` and a stale index, index
/// writeback fails and the whole `statuses()` call errors out. The provider must
/// retry without writeback and still return the full list, degraded but usable.
///
/// Unix-only: the test needs POSIX mode bits to make `.git` unwritable.
#[cfg(unix)]
#[tokio::test]
async fn status_falls_back_when_index_writeback_is_impossible() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    for i in 0..40 {
        write(tmp.path(), &format!("f{i}.txt"), "body\n");
    }
    commit_all(&repo, "base");
    write(tmp.path(), "f0.txt", "edited\n");

    // Staleness: every mtime changes while contents mostly do not, which is what
    // an external touch / build / unpack does.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    for i in 0..40 {
        let f = std::fs::File::options()
            .write(true)
            .open(tmp.path().join(format!("f{i}.txt")))
            .expect("open for touch");
        f.set_modified(later).expect("set mtime");
    }

    let (provider, repo_ref) = discovered(tmp.path()).await;

    let git_dir = tmp.path().join(".git");
    let original = std::fs::metadata(&git_dir).expect("stat .git").permissions();
    let mut readonly = original.clone();
    readonly.set_mode(0o500); // r-x: traversable, not writable
    std::fs::set_permissions(&git_dir, readonly).expect("make .git read-only");

    let status = provider.status(&repo_ref).await;

    // Restore before asserting so a failure cannot leave an unwritable tempdir.
    std::fs::set_permissions(&git_dir, original).expect("restore .git permissions");

    let status = status.expect("status must succeed via fallback, never surface as an SCM error");
    assert!(
        status.degraded,
        "the fallback must actually have fired, otherwise this test proves nothing"
    );
    assert_eq!(
        find(&status, "f0.txt", Some(false))
            .expect("edited file still listed")
            .state,
        ScmResourceState::Modified,
        "the fallback result is complete, only slower"
    );
}

/// A body long enough for git's similarity scoring to match the two sides.
fn renameable_body(tag: &str) -> String {
    (0..40).map(|i| format!("line {i} of {tag}\n")).collect()
}

#[tokio::test]
async fn status_reports_a_worktree_rename_with_both_paths() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "old-name.txt", &renameable_body("subject"));
    commit_all(&repo, "base");
    std::fs::rename(tmp.path().join("old-name.txt"), tmp.path().join("new-name.txt")).expect("rename");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    // Rename detection is off by default in git2: without it this shows up as a
    // delete plus a create, which the neutral model must not report as a rename.
    let renamed = status
        .resources
        .iter()
        .find(|r| r.state == ScmResourceState::Renamed)
        .unwrap_or_else(|| panic!("expected a renamed resource, got {:?}", status.resources));

    // The resource must be identified by the path that exists now; the old path
    // belongs in `rename_from`.
    assert_eq!(
        renamed.repo_relative_path, "new-name.txt",
        "identity is the current path, not the vanished one"
    );
    assert_eq!(renamed.file.relative_path, "new-name.txt");
    assert_eq!(renamed.rename_from.as_deref(), Some("old-name.txt"));
    assert_eq!(
        status.resources.len(),
        1,
        "one rename, not a delete + create pair: {:?}",
        status.resources
    );
}

#[tokio::test]
async fn status_reports_a_staged_rename() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "before.txt", &renameable_body("subject"));
    commit_all(&repo, "base");

    std::fs::rename(tmp.path().join("before.txt"), tmp.path().join("after.txt")).expect("rename");
    let mut index = repo.index().expect("index");
    index.remove_path(Path::new("before.txt")).expect("remove old");
    index.add_path(Path::new("after.txt")).expect("add new");
    index.write().expect("write index");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    let renamed = status
        .resources
        .iter()
        .find(|r| r.state == ScmResourceState::Renamed)
        .unwrap_or_else(|| panic!("expected a renamed resource, got {:?}", status.resources));
    assert_eq!(renamed.repo_relative_path, "after.txt");
    assert_eq!(renamed.rename_from.as_deref(), Some("before.txt"));
    assert_eq!(renamed.staged, Some(true), "the rename is staged");
}

/// Past the candidate limit, rename detection is skipped for cost reasons and
/// the moves degrade to delete + create. Accuracy is preserved; only the rename
/// label is lost.
#[tokio::test]
async fn bulk_moves_skip_rename_detection_and_stay_accurate() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    let count = RENAME_DETECTION_CANDIDATE_LIMIT + 20;
    for i in 0..count {
        write(tmp.path(), &format!("f{i}.txt"), &renameable_body(&format!("body {i}")));
    }
    commit_all(&repo, "base");

    std::fs::create_dir_all(tmp.path().join("moved")).expect("mkdir");
    for i in 0..count {
        std::fs::rename(
            tmp.path().join(format!("f{i}.txt")),
            tmp.path().join("moved").join(format!("f{i}.txt")),
        )
        .expect("rename");
    }

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    assert!(
        !status.resources.iter().any(|r| r.state == ScmResourceState::Renamed),
        "detection is skipped past the limit"
    );
    let created = status
        .resources
        .iter()
        .filter(|r| r.state == ScmResourceState::Created)
        .count();
    let deleted = status
        .resources
        .iter()
        .filter(|r| r.state == ScmResourceState::Deleted)
        .count();
    assert_eq!(created, count, "every new path is reported");
    assert_eq!(deleted, count, "every vanished path is reported");
}

/// A sink that always refuses, standing in for "the trash move cannot be
/// performed" (read-only or missing trash location, a shell that rejects the
/// item, a vanished parent directory).
struct RefusingTrash;

impl TrashSink for RefusingTrash {
    fn trash(&self, _path: &Path) -> Result<(), String> {
        Err("refused by test double".to_owned())
    }
}

/// A sink that records what it was asked to trash and then removes the file, so
/// a test can tell "went through the trash" apart from "was unlinked directly" —
/// both of which leave the work tree looking identical.
#[derive(Default)]
struct RecordingTrash {
    seen: std::sync::Mutex<Vec<std::path::PathBuf>>,
}

impl TrashSink for RecordingTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        self.seen
            .lock()
            .expect("recording sink poisoned")
            .push(path.to_path_buf());
        std::fs::remove_file(path).map_err(|err| err.to_string())
    }
}

/// Build a provider with an injected sink and discover the fixture repo through it.
async fn discovered_with_trash(dir: &Path, trash: Arc<dyn TrashSink>) -> (GitScmProvider, RepoRef) {
    let provider = GitScmProvider::with_trash(trash);
    let root = root_at("pe1", dir, "fixture");
    let mut repos = provider.discover(&root).await.expect("discover ok");
    assert_eq!(repos.len(), 1, "an attached root surfaces exactly one repo");
    let repo = repos.remove(0);
    (provider, RepoRef { repo_id: repo.repo_id })
}

/// Fixture: a committed seed plus one untracked file, which is the discard case
/// that routes through the trash.
fn repo_with_untracked_victim(tmp: &TempDir) -> Repository {
    let repo = init_repo(tmp.path());
    write(tmp.path(), "seed.txt", "seed\n");
    commit_all(&repo, "base");
    write(tmp.path(), "fresh/victim.txt", "irreplaceable\n");
    repo
}

/// **Data-safety floor.** When the trash move fails, discard must report that
/// file as failed and leave it exactly where it was. Falling through to a delete
/// would turn a recoverable failure into permanent data loss — the
/// version-control system has no copy of an untracked file.
#[tokio::test]
async fn discard_keeps_the_file_when_the_trash_move_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = repo_with_untracked_victim(&tmp);
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::new(RefusingTrash)).await;

    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "fresh/victim.txt".to_owned(),
    };
    // A per-file IO failure is reported in the outcome, not as a whole-request
    // error: the request itself was valid and other files would have been done.
    let outcome = provider
        .revert(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("the request is valid, so it is not a whole-request error");
    assert!(!outcome.is_complete(), "the failure is reported");
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].file, file, "reported against the identity passed in");
    assert!(
        outcome.failed[0].reason.contains("trash"),
        "the reason names what went wrong, got {:?}",
        outcome.failed[0].reason
    );

    let victim = tmp.path().join("fresh").join("victim.txt");
    assert!(victim.exists(), "the file must survive a failed discard");
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read survivor"),
        "irreplaceable\n",
        "content must be untouched"
    );
}

/// **Data-safety floor.** Discarding an untracked file must go *through the
/// trash*, not unlink it. The work tree looks the same either way, so the sink
/// itself is the only place the difference is observable.
#[tokio::test]
async fn discard_of_an_untracked_file_goes_through_the_trash() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = repo_with_untracked_victim(&tmp);
    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "fresh/victim.txt".to_owned(),
            }],
        )
        .await
        .expect("discard succeeds");

    let seen = recorder.seen.lock().expect("recording sink poisoned").clone();
    assert_eq!(seen.len(), 1, "exactly one file was handed to the trash, got {seen:?}");
    assert!(
        seen[0].ends_with(Path::new("fresh").join("victim.txt")),
        "the discarded file is the one handed over, got {:?}",
        seen[0]
    );
    assert!(
        !tmp.path().join("fresh").join("victim.txt").exists(),
        "and it did leave the work tree"
    );
}

/// Tracked files must not reach the trash at all: they are restored from the
/// committed version in place.
#[tokio::test]
async fn discard_of_a_tracked_file_does_not_touch_the_trash() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "tracked.txt", "original\n");
    commit_all(&repo, "base");
    write(tmp.path(), "tracked.txt", "scribbled\n");

    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "tracked.txt".to_owned(),
            }],
        )
        .await
        .expect("discard succeeds");

    assert!(
        recorder.seen.lock().expect("recording sink poisoned").is_empty(),
        "a tracked file is restored, never trashed"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("tracked.txt")).expect("read restored"),
        "original\n",
    );
}

/// A sink that refuses only the paths it was told to, so a batch can be made to
/// fail partway without failing entirely.
struct SelectiveTrash {
    refuse: Vec<String>,
    seen: std::sync::Mutex<Vec<String>>,
}

impl SelectiveTrash {
    fn refusing(names: &[&str]) -> Self {
        Self {
            refuse: names.iter().map(|n| (*n).to_owned()).collect(),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl TrashSink for SelectiveTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.seen.lock().expect("sink poisoned").push(name.clone());
        if self.refuse.contains(&name) {
            return Err(format!("refusing {name}"));
        }
        std::fs::remove_file(path).map_err(|err| err.to_string())
    }
}

/// Two tracked edits plus three untracked files — the mixed batch a user gets
/// when selecting "discard all".
fn mixed_batch(tmp: &TempDir) -> (Repository, Vec<FileRef>) {
    let repo = init_repo(tmp.path());
    write(tmp.path(), "t1.txt", "committed-1\n");
    write(tmp.path(), "t2.txt", "committed-2\n");
    commit_all(&repo, "base");
    write(tmp.path(), "t1.txt", "edited-1\n");
    write(tmp.path(), "t2.txt", "edited-2\n");
    write(tmp.path(), "u1.txt", "new-1\n");
    write(tmp.path(), "u2.txt", "new-2\n");
    write(tmp.path(), "u3.txt", "new-3\n");

    let files = ["t1.txt", "t2.txt", "u1.txt", "u2.txt", "u3.txt"]
        .iter()
        .map(|p| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: (*p).to_owned(),
        })
        .collect();
    (repo, files)
}

/// One file failing must not abandon the rest. Before this was best effort, a
/// mid-batch failure returned "failed" while leaving earlier files already
/// changed and later ones untouched — the caller could not tell which.
#[tokio::test]
async fn a_partial_failure_still_processes_every_other_file() {
    let tmp = TempDir::new().expect("tempdir");
    let (_repo, files) = mixed_batch(&tmp);
    let sink = Arc::new(SelectiveTrash::refusing(&["u2.txt"]));
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&sink) as Arc<dyn TrashSink>).await;

    let outcome = provider.revert(&repo_ref, &files).await.expect("request is valid");

    // Exactly the refused file is reported.
    assert_eq!(outcome.failed.len(), 1, "one failure, got {:?}", outcome.failed);
    assert_eq!(outcome.failed[0].file.relative_path, "u2.txt");

    // Everything else really happened: tracked files restored, other untracked
    // files gone, and the failed one left untouched.
    for tracked in ["t1.txt", "t2.txt"] {
        assert!(
            std::fs::read_to_string(tmp.path().join(tracked))
                .expect("read tracked")
                .starts_with("committed"),
            "{tracked} was restored"
        );
    }
    assert!(!tmp.path().join("u1.txt").exists(), "u1 was discarded");
    assert!(
        !tmp.path().join("u3.txt").exists(),
        "u3 was still attempted after the failure — this is the whole point of best effort"
    );
    assert!(tmp.path().join("u2.txt").exists(), "the failed file survives");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("u2.txt")).expect("read survivor"),
        "new-2\n"
    );
}

#[tokio::test]
async fn every_file_failing_is_reported_per_file() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "seed.txt", "seed\n");
    commit_all(&repo, "base");
    write(tmp.path(), "a.txt", "a\n");
    write(tmp.path(), "b.txt", "b\n");

    let files: Vec<FileRef> = ["a.txt", "b.txt"]
        .iter()
        .map(|p| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: (*p).to_owned(),
        })
        .collect();
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::new(RefusingTrash)).await;

    let outcome = provider.revert(&repo_ref, &files).await.expect("request is valid");

    // All failing is still not a whole-request error: the request was valid, and
    // the caller needs the per-file reasons.
    assert_eq!(outcome.failed.len(), 2, "both reported, got {:?}", outcome.failed);
    assert!(
        tmp.path().join("a.txt").exists() && tmp.path().join("b.txt").exists(),
        "nothing lost"
    );
}

/// The conflicted pre-check runs over the whole selection **before** anything is
/// touched, so selecting a conflicted file leaves the batch untouched. Without
/// that ordering, a user who picked one conflicted file among many would find the
/// others already discarded.
#[tokio::test]
async fn a_conflicted_file_in_the_batch_makes_the_whole_request_a_no_op() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "c.txt", "base\n");
    write(tmp.path(), "other.txt", "committed\n");
    commit_all(&repo, "base");
    let base = repo.head().unwrap().peel_to_commit().unwrap();

    // Produce a real conflict on c.txt.
    repo.branch("other", &base, false).expect("branch");
    write(tmp.path(), "c.txt", "main side\n");
    commit_all(&repo, "main change");
    let other_commit = repo
        .find_branch("other", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    repo.set_head("refs/heads/other").unwrap();
    repo.reset(other_commit.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    write(tmp.path(), "c.txt", "other side\n");
    commit_all(&repo, "other change");
    let main_commit = repo
        .find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let their = repo.find_annotated_commit(main_commit.id()).unwrap();
    let _ = repo.merge(&[&their], None, None);

    // An untracked file alongside it, which must survive untouched.
    write(tmp.path(), "bystander.txt", "keep me\n");
    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    let files: Vec<FileRef> = ["bystander.txt", "c.txt"]
        .iter()
        .map(|p| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: (*p).to_owned(),
        })
        .collect();
    let err = provider
        .revert(&repo_ref, &files)
        .await
        .expect_err("a conflicted selection is refused outright");
    assert!(
        matches!(err, ScmError::OpaqueResource { .. }),
        "refused as opaque, got {err:?}"
    );

    // Whole-request refusal means untouched — not "some already discarded".
    assert!(
        tmp.path().join("bystander.txt").exists(),
        "the pre-check runs before any file is touched"
    );
    assert!(
        recorder.seen.lock().expect("sink").is_empty(),
        "and nothing reached the trash at all"
    );
}

/// All three multi-file actions must offer the same guarantee for blocked
/// resources: a conflicted file anywhere in the selection refuses the whole
/// request before anything is touched. Without the pre-check, `unstage` would
/// reach a conflicted entry only partway through and leave earlier files already
/// unstaged.
#[tokio::test]
async fn stage_and_unstage_refuse_a_conflicted_selection_atomically() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "c.txt", "base\n");
    write(tmp.path(), "clean.txt", "committed\n");
    commit_all(&repo, "base");
    let base = repo.head().unwrap().peel_to_commit().unwrap();

    repo.branch("other", &base, false).expect("branch");
    write(tmp.path(), "c.txt", "main side\n");
    commit_all(&repo, "main change");
    let other_commit = repo
        .find_branch("other", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    repo.set_head("refs/heads/other").unwrap();
    repo.reset(other_commit.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    write(tmp.path(), "c.txt", "other side\n");
    commit_all(&repo, "other change");
    let main_commit = repo
        .find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let their = repo.find_annotated_commit(main_commit.id()).unwrap();
    let _ = repo.merge(&[&their], None, None);

    // A clean edit alongside the conflict, which must stay untouched.
    write(tmp.path(), "clean.txt", "edited\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let staging = provider.staging().expect("git declares staging");
    let files: Vec<FileRef> = ["clean.txt", "c.txt"]
        .iter()
        .map(|p| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: (*p).to_owned(),
        })
        .collect();

    for (label, result) in [
        ("stage", staging.stage(&repo_ref, &files).await),
        ("unstage", staging.unstage(&repo_ref, &files).await),
    ] {
        let err = result.unwrap_err_or_else_msg(label);
        assert!(
            matches!(err, ScmError::OpaqueResource { .. }),
            "{label} must refuse a conflicted selection, got {err:?}"
        );
    }

    // Refused means untouched: the clean file was never staged.
    let status = provider.status(&repo_ref).await.expect("status ok");
    assert!(
        find(&status, "clean.txt", Some(true)).is_none(),
        "the clean file must not have been staged by a refused request: {:?}",
        status.resources
    );
}

/// Small helper so the loop above reads clearly.
trait UnwrapErrMsg<T> {
    fn unwrap_err_or_else_msg(self, label: &str) -> ScmError;
}

impl<T: std::fmt::Debug> UnwrapErrMsg<T> for Result<T, ScmError> {
    fn unwrap_err_or_else_msg(self, label: &str) -> ScmError {
        match self {
            Ok(value) => panic!("{label} should have been refused, got Ok({value:?})"),
            Err(err) => err,
        }
    }
}

/// A failure must come back carrying **the exact identity the caller sent**, so a
/// client can match it to the row it rendered. Reporting a blank or re-derived
/// `pe_id` would match nothing and silently degrade "this file failed" into
/// "something failed".
#[tokio::test]
async fn a_failure_carries_back_the_identity_that_was_requested() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = repo_with_untracked_victim(&tmp);
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::new(RefusingTrash)).await;

    // A pe id distinct from the fixtures' usual one: if anything re-derived the
    // identity instead of carrying it, this is what would go missing.
    let requested = FileRef {
        pe_id: "pe-distinctive-42".to_owned(),
        relative_path: "fresh/victim.txt".to_owned(),
    };

    let outcome = provider
        .revert(&repo_ref, std::slice::from_ref(&requested))
        .await
        .expect("request is valid");

    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(
        outcome.failed[0].file, requested,
        "the failure carries the requested identity verbatim"
    );
    assert!(
        !outcome.failed[0].file.pe_id.is_empty(),
        "never a blank identity: it would match no row on the client"
    );
}

/// Duplicate entries in one request each keep their own identity, which a
/// path-based lookup could not guarantee.
#[tokio::test]
async fn duplicate_paths_in_a_request_each_report_their_own_identity() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = repo_with_untracked_victim(&tmp);
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::new(RefusingTrash)).await;

    let files = vec![
        FileRef {
            pe_id: "pe-first".to_owned(),
            relative_path: "fresh/victim.txt".to_owned(),
        },
        FileRef {
            pe_id: "pe-second".to_owned(),
            relative_path: "fresh/victim.txt".to_owned(),
        },
    ];

    let outcome = provider.revert(&repo_ref, &files).await.expect("request is valid");

    assert_eq!(outcome.failed.len(), 2, "each request item is reported");
    let reported: Vec<&str> = outcome.failed.iter().map(|f| f.file.pe_id.as_str()).collect();
    assert_eq!(
        reported,
        vec!["pe-first", "pe-second"],
        "each keeps the identity it was sent with, in request order"
    );
}

/// Discarding a working-tree change restores the **staged** version, not the
/// committed one.
///
/// This is the behaviour the version-control system itself has, and getting it
/// wrong is user-visible: restoring from the last commit while leaving the index
/// alone produces a state the engine never creates — work tree at the commit,
/// index still holding the staged version — so the file keeps showing changes on
/// both sides and the change count does not drop even though the user asked to
/// discard.
#[tokio::test]
async fn discard_restores_the_staged_version_and_drops_only_the_unstaged_change() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "f.txt", "committed\n");
    commit_all(&repo, "base");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "f.txt".to_owned(),
    };

    // Stage one version, then edit again: both sides now differ.
    write(tmp.path(), "f.txt", "staged-version\n");
    provider
        .staging()
        .expect("staging")
        .stage(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("stage ok");
    write(tmp.path(), "f.txt", "worktree-version\n");

    let before = provider.status(&repo_ref).await.expect("status ok");
    assert_eq!(before.resources.len(), 2, "both sides present: {:?}", before.resources);

    provider
        .revert(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("discard ok");

    // The staged version survives in the work tree — the staged edit was not asked
    // to be discarded.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("f.txt")).expect("read back"),
        "staged-version\n",
        "restored from the index, not from the last commit"
    );

    let after = provider.status(&repo_ref).await.expect("status ok");
    assert_eq!(
        after.resources.len(),
        1,
        "the change count drops: only the staged side remains, got {:?}",
        after.resources
    );
    assert!(
        find(&after, "f.txt", Some(true)).is_some(),
        "the staged side is untouched: {:?}",
        after.resources
    );
    assert!(
        find(&after, "f.txt", Some(false)).is_none(),
        "the unstaged side is gone: {:?}",
        after.resources
    );
}

/// The common case: nothing staged, so the index version *is* the committed one
/// and discarding restores the committed content. No special casing needed.
#[tokio::test]
async fn discard_restores_committed_content_when_nothing_is_staged() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "f.txt", "committed\n");
    commit_all(&repo, "base");
    write(tmp.path(), "f.txt", "scribbled\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "f.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("f.txt")).expect("read back"),
        "committed\n",
    );
    assert!(
        provider
            .status(&repo_ref)
            .await
            .expect("status ok")
            .resources
            .is_empty(),
        "the file is clean again"
    );
}

/// In a repository with no commits yet there is no committed version, but the
/// index may still hold a staged one — and the same rule applies, so discard
/// restores that staged version rather than doing nothing.
#[tokio::test]
async fn discard_in_a_repository_without_commits_restores_the_staged_version() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    // Never committed; staged addition, then edited again.
    write(tmp.path(), "s.txt", "staged-content\n");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("s.txt")).expect("add");
    index.write().expect("write index");
    write(tmp.path(), "s.txt", "worktree-content\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "s.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("s.txt")).expect("read back"),
        "staged-content\n",
        "restored from the index even with no commits to fall back on"
    );
}

/// A file can carry a non-rename change on the index side **and** a rename on the
/// work-tree side: stage an edit, then move the file. Both deltas exist, and the
/// rename is the one that determines where the file actually lives now.
///
/// Getting this wrong is not a cosmetic mislabel. The resource's identity would be
/// the vanished old path, so every follow-up acts on a file that is not there:
/// reading its content yields nothing, and discarding it reports success while
/// restoring the old path instead — a claim of "done" with the work not done,
/// which is exactly what the per-file failure reporting exists to prevent.
#[tokio::test]
async fn a_rename_is_detected_even_when_the_index_side_has_a_different_change() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", &renameable_body("subject"));
    commit_all(&repo, "base");

    // Index side: a staged edit (a *non-rename* delta).
    write(
        tmp.path(),
        "a.txt",
        &format!("{}extra line\n", renameable_body("subject")),
    );
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("a.txt")).expect("stage the edit");
    index.write().expect("write index");

    // Work-tree side: the same file is then moved.
    std::fs::rename(tmp.path().join("a.txt"), tmp.path().join("b.txt")).expect("rename");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    let renamed = status
        .resources
        .iter()
        .find(|r| r.state == ScmResourceState::Renamed)
        .unwrap_or_else(|| {
            panic!(
                "the work-tree rename must be seen despite the index-side edit, got {:?}",
                status.resources
            )
        });

    // Identity must be where the file now is, with the previous path recorded.
    assert_eq!(renamed.repo_relative_path, "b.txt", "identity is the current path");
    assert_eq!(
        renamed.rename_from.as_deref(),
        Some("a.txt"),
        "and the old path is kept"
    );

    // The identity has to be usable: content readable, and no resource points at a
    // path that no longer exists on disk.
    let content = provider
        .original(
            &repo_ref,
            &FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: renamed.repo_relative_path.clone(),
            },
            ContentRef::Working,
        )
        .await
        .expect("original ok");
    assert!(
        content.is_some(),
        "the reported identity must resolve to real content, not a vanished path"
    );
    assert!(
        !tmp.path().join("a.txt").exists(),
        "the old path is gone from the work tree — nothing may still report it"
    );
}

/// Unstaging a batch takes a single engine call, falling back to one call per file
/// only if that fails. Both routes must agree, so the shortcut is verified against
/// the outcome it is supposed to be equivalent to.
#[tokio::test]
async fn unstaging_a_batch_matches_unstaging_one_by_one() {
    let build = |tmp: &TempDir| {
        let repo = init_repo(tmp.path());
        for i in 0..5 {
            write(tmp.path(), &format!("f{i}.txt"), "committed\n");
        }
        commit_all(&repo, "base");
        for i in 0..5 {
            write(tmp.path(), &format!("f{i}.txt"), "edited\n");
        }
    };
    let refs: Vec<FileRef> = (0..5)
        .map(|i| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: format!("f{i}.txt"),
        })
        .collect();

    // Batch: one call for all five.
    let batched = TempDir::new().expect("tempdir");
    build(&batched);
    let (provider, repo_ref) = discovered(batched.path()).await;
    let staging = provider.staging().expect("staging");
    staging.stage(&repo_ref, &refs).await.expect("stage ok");
    let outcome = staging.unstage(&repo_ref, &refs).await.expect("unstage ok");
    let after_batch = provider.status(&repo_ref).await.expect("status ok");

    // One at a time: the fallback route.
    let single = TempDir::new().expect("tempdir");
    build(&single);
    let (provider2, repo_ref2) = discovered(single.path()).await;
    let staging2 = provider2.staging().expect("staging");
    staging2.stage(&repo_ref2, &refs).await.expect("stage ok");
    for one in &refs {
        staging2
            .unstage(&repo_ref2, std::slice::from_ref(one))
            .await
            .expect("unstage ok");
    }
    let after_single = provider2.status(&repo_ref2).await.expect("status ok");

    assert!(outcome.is_complete(), "the batch reported no failures");
    let sides = |s: &ScmStatus| {
        let mut v: Vec<(String, Option<bool>)> = s
            .resources
            .iter()
            .map(|r| (r.repo_relative_path.clone(), r.staged))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        sides(&after_batch),
        sides(&after_single),
        "the shortcut must land in the same state as unstaging individually"
    );
    assert!(
        after_batch.resources.iter().all(|r| r.staged == Some(false)),
        "everything is back on the work-tree side: {:?}",
        after_batch.resources
    );
}

/// An empty selection must do nothing at all. The batch call receives an empty
/// pathspec, and a pathspec-less reset would mean "reset everything" — which would
/// silently unstage work the user never selected.
#[tokio::test]
async fn an_empty_selection_leaves_the_staging_area_untouched() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "committed\n");
    write(tmp.path(), "b.txt", "committed\n");
    commit_all(&repo, "base");
    write(tmp.path(), "a.txt", "edited\n");
    write(tmp.path(), "b.txt", "edited\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let staging = provider.staging().expect("staging");
    let all: Vec<FileRef> = ["a.txt", "b.txt"]
        .iter()
        .map(|p| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: (*p).to_owned(),
        })
        .collect();
    staging.stage(&repo_ref, &all).await.expect("stage ok");
    let staged_before = provider
        .status(&repo_ref)
        .await
        .expect("status ok")
        .resources
        .iter()
        .filter(|r| r.staged == Some(true))
        .count();
    assert_eq!(staged_before, 2);

    for outcome in [
        staging.unstage(&repo_ref, &[]).await.expect("empty unstage ok"),
        staging.stage(&repo_ref, &[]).await.expect("empty stage ok"),
        provider.revert(&repo_ref, &[]).await.expect("empty discard ok"),
    ] {
        assert!(outcome.is_complete(), "nothing to do is not a failure");
    }

    let staged_after = provider
        .status(&repo_ref)
        .await
        .expect("status ok")
        .resources
        .iter()
        .filter(|r| r.staged == Some(true))
        .count();
    assert_eq!(
        staged_after, staged_before,
        "an empty selection must not touch anything, least of all everything"
    );
}

/// Build a real merge conflict on `c.txt`, leaving `other.txt` clean.
fn conflicted_fixture(tmp: &TempDir) {
    let repo = init_repo(tmp.path());
    write(tmp.path(), "c.txt", "base\n");
    write(tmp.path(), "other.txt", "committed\n");
    commit_all(&repo, "base");
    let base = repo.head().unwrap().peel_to_commit().unwrap();

    repo.branch("other", &base, false).expect("branch");
    write(tmp.path(), "c.txt", "main side\n");
    commit_all(&repo, "main change");
    let other_commit = repo
        .find_branch("other", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    repo.set_head("refs/heads/other").unwrap();
    repo.reset(other_commit.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    write(tmp.path(), "c.txt", "other side\n");
    commit_all(&repo, "other change");
    let main_commit = repo
        .find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let their = repo.find_annotated_commit(main_commit.id()).unwrap();
    let _ = repo.merge(&[&their], None, None);
}

/// A conflicted file has no single staged version — the index holds the three
/// sides of the conflict instead. Asking for its staged content must be refused,
/// not answered with "nothing".
///
/// Answering "nothing" is what makes this dangerous: a diff against an empty side
/// reads as **the entire file was deleted**, while the file sits intact on disk.
/// That is a fabricated result, not a missing one, and no client-side workaround
/// can fix it — every other consumer would get the same fiction.
#[tokio::test]
async fn the_staged_anchor_is_refused_for_a_conflicted_file() {
    let tmp = TempDir::new().expect("tempdir");
    conflicted_fixture(&tmp);
    let (provider, repo_ref) = discovered(tmp.path()).await;
    let conflicted = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "c.txt".to_owned(),
    };

    let err = provider
        .original(&repo_ref, &conflicted, ContentRef::Staged)
        .await
        .expect_err("a conflicted file has no staged version to return");
    assert!(
        matches!(err, ScmError::OpaqueResource { .. }),
        "refused the way conflicted resources are refused elsewhere, got {err:?}"
    );

    // A diff involving that anchor must fail too, rather than fabricate a patch
    // that claims the file was emptied.
    let diff_err = provider
        .diff(&repo_ref, &conflicted, ContentRef::Working, ContentRef::Staged)
        .await
        .expect_err("no patch can be produced against a version that does not exist");
    assert!(matches!(diff_err, ScmError::OpaqueResource { .. }), "got {diff_err:?}");
}

/// The refusal must be specific to conflicted files: the universal anchors still
/// work, and an unstaged-but-unconflicted file still reports "nothing staged"
/// rather than being refused.
#[tokio::test]
async fn the_other_anchors_still_work_for_a_conflicted_file() {
    let tmp = TempDir::new().expect("tempdir");
    conflicted_fixture(&tmp);
    let (provider, repo_ref) = discovered(tmp.path()).await;
    let conflicted = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "c.txt".to_owned(),
    };

    // Working shows the conflict markers; committed shows the last commit. Both are
    // meaningful and must remain available — the client needs them to render the
    // conflict at all.
    let working = provider
        .original(&repo_ref, &conflicted, ContentRef::Working)
        .await
        .expect("working anchor ok")
        .expect("working content present");
    assert!(
        String::from_utf8_lossy(&working).contains("<<<<<<<"),
        "the work tree carries the conflict markers"
    );
    assert!(
        provider
            .original(&repo_ref, &conflicted, ContentRef::Committed)
            .await
            .expect("committed anchor ok")
            .is_some(),
        "the committed side is still readable"
    );

    // A merely-untracked file is absent from the index, not conflicted: that is
    // "nothing staged", which is an answer rather than a refusal.
    write(tmp.path(), "fresh.txt", "new\n");
    let fresh = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "fresh.txt".to_owned(),
    };
    assert!(
        provider
            .original(&repo_ref, &fresh, ContentRef::Staged)
            .await
            .expect("an unstaged file is not an error")
            .is_none(),
        "absent from the index means no content, not a refusal"
    );
}

/// The resource cap counts untracked entries too. Scan cost is driven by the
/// number of non-ignored files, and creating files in bulk goes down exactly that
/// path — so exempting untracked entries would leave the real growth uncapped
/// while still reporting a complete list.
#[tokio::test]
async fn the_resource_cap_counts_untracked_entries() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "seed.txt", "seed\n");
    commit_all(&repo, "base");

    // Only untracked files, more than the cap allows.
    let count = STATUS_RESOURCE_LIMIT + 25;
    for i in 0..count {
        write(tmp.path(), &format!("gen/f{i}.txt"), "x\n");
    }

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    assert!(
        status.truncated,
        "a list of only untracked entries must still be truncated at the cap"
    );
    assert_eq!(
        status.resources.len(),
        STATUS_RESOURCE_LIMIT,
        "and it is capped at the limit, not returned in full"
    );
}

/// A file staged as deleted and then recreated in the work tree shows up as two
/// resources: the staged deletion, and the recreated file as an untracked
/// creation. That matches what the version-control system itself reports.
///
/// Pinned deliberately, because the routing this exercises is shared with the
/// rename case: discarding the recreated file treats it as the untracked file it
/// is and sends it to the trash, leaving the staged deletion alone. A change to
/// rename handling must not quietly alter this.
#[tokio::test]
async fn a_staged_deletion_with_a_recreated_file_reports_both_sides() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "f.txt", "committed\n");
    commit_all(&repo, "base");
    std::fs::remove_file(tmp.path().join("f.txt")).expect("remove");
    let mut index = repo.index().expect("index");
    index.remove_path(Path::new("f.txt")).expect("stage the deletion");
    index.write().expect("write index");
    write(tmp.path(), "f.txt", "recreated\n");

    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    let status = provider.status(&repo_ref).await.expect("status ok");
    assert_eq!(
        status.resources.len(),
        2,
        "both sides are reported: {:?}",
        status.resources
    );
    assert_eq!(
        find(&status, "f.txt", Some(true)).expect("staged side").state,
        ScmResourceState::Deleted,
        "the staged deletion is one resource"
    );
    assert_eq!(
        find(&status, "f.txt", Some(false)).expect("work-tree side").state,
        ScmResourceState::Created,
        "and the recreated file is the other"
    );

    // Discarding it routes through the trash, because in the work tree it really is
    // an untracked file.
    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "f.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");
    assert_eq!(
        recorder.seen.lock().expect("sink").len(),
        1,
        "the recreated file went to the trash rather than being unlinked"
    );
    assert!(!tmp.path().join("f.txt").exists(), "and it left the work tree");
}

/// Move a committed file, leaving the rename unstaged.
fn worktree_rename(tmp: &TempDir) -> Repository {
    let repo = init_repo(tmp.path());
    write(tmp.path(), "old.txt", &renameable_body("subject"));
    commit_all(&repo, "base");
    std::fs::rename(tmp.path().join("old.txt"), tmp.path().join("new.txt")).expect("rename");
    repo
}

/// Discarding a rename undoes the whole move: the file comes back where it was,
/// and the path it had moved to goes through the trash.
///
/// A rename is presented to the client as **one** resource, so one discard has to
/// undo one move. Handling only the new path — which on its own looks like an
/// untracked file — would trash it and never restore the source, leaving the file
/// present at neither path while the call reported success.
#[tokio::test]
async fn discarding_a_worktree_rename_restores_the_old_path_and_trashes_the_new_one() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = worktree_rename(&tmp);
    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    // Precondition: one resource, identified by the new path, carrying the old one.
    let status = provider.status(&repo_ref).await.expect("status ok");
    let renamed = status
        .resources
        .iter()
        .find(|r| r.state == ScmResourceState::Renamed)
        .unwrap_or_else(|| panic!("expected a rename, got {:?}", status.resources));
    assert_eq!(renamed.repo_relative_path, "new.txt");
    assert_eq!(renamed.rename_from.as_deref(), Some("old.txt"));

    let outcome = provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "new.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");
    assert!(outcome.is_complete(), "reported no failures: {:?}", outcome.failed);

    assert!(
        tmp.path().join("old.txt").exists(),
        "the file is back where it came from — losing it from both paths is the bug this prevents"
    );
    assert!(
        !tmp.path().join("new.txt").exists(),
        "and it is gone from where it moved to"
    );
    assert_eq!(
        recorder.seen.lock().expect("sink").len(),
        1,
        "the destination went through the trash, not an unlink"
    );

    // And the change list is actually clean again, rather than showing a leftover.
    assert!(
        provider
            .status(&repo_ref)
            .await
            .expect("status ok")
            .resources
            .is_empty(),
        "the repository is clean after undoing the move"
    );
}

/// The same for a staged rename. Its source is no longer in the index, so the
/// restore has to come from the last commit — restoring "from the index" would find
/// nothing and silently do nothing while reporting success.
#[tokio::test]
async fn discarding_a_staged_rename_also_undoes_the_whole_move() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "old.txt", &renameable_body("subject"));
    commit_all(&repo, "base");
    std::fs::rename(tmp.path().join("old.txt"), tmp.path().join("new.txt")).expect("rename");
    let mut index = repo.index().expect("index");
    index.remove_path(Path::new("old.txt")).expect("remove source");
    index.add_path(Path::new("new.txt")).expect("add destination");
    index.write().expect("write index");

    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;
    let outcome = provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "new.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");

    assert!(outcome.is_complete(), "reported no failures: {:?}", outcome.failed);
    assert!(
        tmp.path().join("old.txt").exists(),
        "the source is restored from the commit"
    );
    assert!(!tmp.path().join("new.txt").exists(), "the destination is gone");
    assert_eq!(recorder.seen.lock().expect("sink").len(), 1, "through the trash");
}

/// If the trash cannot take the destination, the file must be reported as failed —
/// not reported as a success that silently did half the work.
#[tokio::test]
async fn a_rename_discard_reports_failure_when_the_trash_refuses() {
    let tmp = TempDir::new().expect("tempdir");
    let _repo = worktree_rename(&tmp);
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::new(RefusingTrash)).await;

    let file = FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: "new.txt".to_owned(),
    };
    let outcome = provider
        .revert(&repo_ref, std::slice::from_ref(&file))
        .await
        .expect("the request itself is valid");

    assert!(!outcome.is_complete(), "the failure is reported rather than swallowed");
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].file, file, "against the identity that was requested");
    assert!(
        tmp.path().join("new.txt").exists(),
        "and the file the trash refused is still there"
    );
}

/// A rename never appears alone in a real repository — there is always other work
/// in progress alongside it. This is the case the earlier tests missed: they each
/// built a repository containing *only* the rename, so a scan that gave up on the
/// first non-rename entry still looked correct.
#[tokio::test]
async fn a_rename_is_undone_even_when_other_changes_coexist() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "old.txt", &renameable_body("subject"));
    // ⚠️ These names are load-bearing — do not "tidy" them into matching names.
    //
    // The rename is reported under `new.txt`, and coexisting entries are placed
    // deliberately on **both** sides of that in sort order (`aaa_*` before,
    // `zzz_*` after). An implementation that walks the repository's entries and
    // stops early on the first non-rename is only caught when a non-rename comes
    // *first*, so a test with only later-sorting neighbours passes while the defect
    // is live. Covering both sides makes the test independent of iteration order —
    // which matters because that order is not a guarantee we should rely on, and
    // has only ever been observed on one platform.
    // The neighbours must straddle the target in sort order, and "the target" is two
    // different paths depending on what is being checked: the repository reports a
    // rename under the path it came *from*, so that is what the scan order follows,
    // while the lookup it builds is keyed by the path it moved *to*. Both
    // relationships are asserted below rather than left to the reader to verify.
    const BEFORE: &str = "aaa_modified.txt";
    const AFTER: &str = "zzz_modified.txt";
    const OLD_PATH: &str = "old.txt";
    const NEW_PATH: &str = "new.txt";
    assert!(
        BEFORE < OLD_PATH && OLD_PATH < AFTER,
        "neighbours must straddle the *source* path — entries are visited in that order"
    );
    assert!(
        BEFORE < NEW_PATH && NEW_PATH < AFTER,
        "and straddle the *destination* path — that is what the rename lookup is keyed by"
    );

    write(tmp.path(), BEFORE, "committed\n");
    write(tmp.path(), AFTER, "committed\n");
    commit_all(&repo, "base");

    std::fs::rename(tmp.path().join(OLD_PATH), tmp.path().join(NEW_PATH)).expect("rename");
    write(tmp.path(), BEFORE, "edited\n"); // coexisting, sorts before both paths
    write(tmp.path(), AFTER, "edited\n"); // coexisting, sorts after both paths
    write(tmp.path(), "aaa_untracked.txt", "fresh\n"); // untracked, sorts before

    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    let outcome = provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "new.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");
    assert!(outcome.is_complete(), "no failures: {:?}", outcome.failed);

    assert!(
        tmp.path().join("old.txt").exists(),
        "the rename is undone despite unrelated changes being scanned first"
    );
    assert!(!tmp.path().join("new.txt").exists(), "and the destination is gone");

    // The unrelated changes are untouched — discarding one resource must not reach
    // the others.
    for neighbour in ["aaa_modified.txt", "zzz_modified.txt"] {
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(neighbour)).expect("read"),
            "edited\n",
            "{neighbour} is left alone"
        );
    }
    assert!(
        tmp.path().join("aaa_untracked.txt").exists(),
        "and so is the coexisting untracked file"
    );
    assert_eq!(
        recorder.seen.lock().expect("sink").len(),
        1,
        "only the rename destination went to the trash"
    );
}

/// Two renames at once: the lookup must hold every pair, not just the first one it
/// happened to see.
#[tokio::test]
async fn two_simultaneous_renames_are_each_undone_correctly() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "first.txt", &renameable_body("first subject"));
    write(tmp.path(), "second.txt", &renameable_body("second subject"));
    commit_all(&repo, "base");
    std::fs::rename(tmp.path().join("first.txt"), tmp.path().join("first-moved.txt")).expect("rename");
    std::fs::rename(tmp.path().join("second.txt"), tmp.path().join("second-moved.txt")).expect("rename");

    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    // Discard the *second* one only: if the lookup kept just one pair, this is the
    // one that would be missing.
    provider
        .revert(
            &repo_ref,
            &[FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "second-moved.txt".to_owned(),
            }],
        )
        .await
        .expect("discard ok");

    assert!(tmp.path().join("second.txt").exists(), "the second rename is undone");
    assert!(!tmp.path().join("second-moved.txt").exists());
    // The first rename is still pending, untouched.
    assert!(
        tmp.path().join("first-moved.txt").exists() && !tmp.path().join("first.txt").exists(),
        "the other rename is left as it was"
    );
}

/// Both renames in one request, alongside unrelated changes.
#[tokio::test]
async fn a_batch_of_renames_is_undone_in_one_request() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    for i in 0..6 {
        write(tmp.path(), &format!("f{i}.txt"), &renameable_body(&format!("body {i}")));
    }
    write(tmp.path(), "a_noise.txt", "committed\n");
    commit_all(&repo, "base");

    for i in 0..6 {
        std::fs::rename(
            tmp.path().join(format!("f{i}.txt")),
            tmp.path().join(format!("moved{i}.txt")),
        )
        .expect("rename");
    }
    write(tmp.path(), "a_noise.txt", "edited\n");

    let recorder = Arc::new(RecordingTrash::default());
    let (provider, repo_ref) = discovered_with_trash(tmp.path(), Arc::clone(&recorder) as Arc<dyn TrashSink>).await;

    let files: Vec<FileRef> = (0..6)
        .map(|i| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: format!("moved{i}.txt"),
        })
        .collect();
    let outcome = provider.revert(&repo_ref, &files).await.expect("discard ok");
    assert!(outcome.is_complete(), "no failures: {:?}", outcome.failed);

    for i in 0..6 {
        assert!(tmp.path().join(format!("f{i}.txt")).exists(), "f{i}.txt is restored");
        assert!(!tmp.path().join(format!("moved{i}.txt")).exists());
    }
    assert_eq!(
        recorder.seen.lock().expect("sink").len(),
        6,
        "each destination trashed once"
    );
}

/// The reporting path must recognise a rename regardless of what else the
/// repository contains, and regardless of the order entries are visited in.
///
/// Same file naming rationale as the discard-side test: neighbours are placed on
/// both sides of `new.txt` in sort order, so the test cannot pass merely because a
/// non-rename happened to be visited last.
#[tokio::test]
async fn status_reports_a_rename_regardless_of_neighbouring_changes() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "old.txt", &renameable_body("subject"));
    write(tmp.path(), "aaa_modified.txt", "committed\n");
    write(tmp.path(), "zzz_modified.txt", "committed\n");
    commit_all(&repo, "base");
    std::fs::rename(tmp.path().join("old.txt"), tmp.path().join("new.txt")).expect("rename");
    write(tmp.path(), "aaa_modified.txt", "edited\n");
    write(tmp.path(), "zzz_modified.txt", "edited\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let status = provider.status(&repo_ref).await.expect("status ok");

    let renamed = status
        .resources
        .iter()
        .find(|r| r.state == ScmResourceState::Renamed)
        .unwrap_or_else(|| panic!("the rename must be reported, got {:?}", status.resources));
    assert_eq!(renamed.repo_relative_path, "new.txt");
    assert_eq!(renamed.rename_from.as_deref(), Some("old.txt"));
    // And the neighbours are still reported as their own resources.
    assert!(
        find(&status, "aaa_modified.txt", Some(false)).is_some()
            && find(&status, "zzz_modified.txt", Some(false)).is_some(),
        "neighbouring changes are reported too: {:?}",
        status.resources
    );
}

/// Selecting the same file more than once in a single request is idempotent, and
/// leaves everything else alone.
///
/// Duplicates arise naturally: a file with both a staged and an unstaged change is
/// two rows in the UI, and selecting both sends the same path twice. The request
/// may also carry the same path under different pe identities.
///
/// ⚠️ Neighbour names are load-bearing (`aaa_*` / `zzz_*`): they sit on both sides
/// of the target in sort order, so this cannot pass merely because the target
/// happened to be visited before anything else. Do not rename them to match.
#[tokio::test]
async fn staging_the_same_path_twice_is_idempotent_and_leaves_neighbours_alone() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "dup.txt", "committed\n");
    write(tmp.path(), "aaa_other.txt", "committed\n");
    write(tmp.path(), "zzz_other.txt", "committed\n");
    commit_all(&repo, "base");
    for name in ["dup.txt", "aaa_other.txt", "zzz_other.txt"] {
        write(tmp.path(), name, "edited\n");
    }

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let staging = provider.staging().expect("staging");
    let duplicated = |pe: &str| FileRef {
        pe_id: pe.to_owned(),
        relative_path: "dup.txt".to_owned(),
    };
    // The same path three times, including under a second identity.
    let files = vec![duplicated("pe1"), duplicated("pe1"), duplicated("pe-other")];

    let staged_outcome = staging.stage(&repo_ref, &files).await.expect("stage ok");
    assert!(
        staged_outcome.is_complete(),
        "repeating a path is not a failure: {:?}",
        staged_outcome.failed
    );
    let after_stage = provider.status(&repo_ref).await.expect("status ok");
    assert!(
        find(&after_stage, "dup.txt", Some(true)).is_some(),
        "the file is staged once: {:?}",
        after_stage.resources
    );
    assert_eq!(
        after_stage
            .resources
            .iter()
            .filter(|r| r.repo_relative_path == "dup.txt" && r.staged == Some(true))
            .count(),
        1,
        "and appears once, not once per duplicate: {:?}",
        after_stage.resources
    );

    let unstaged_outcome = staging.unstage(&repo_ref, &files).await.expect("unstage ok");
    assert!(unstaged_outcome.is_complete(), "{:?}", unstaged_outcome.failed);
    let after_unstage = provider.status(&repo_ref).await.expect("status ok");
    assert!(
        find(&after_unstage, "dup.txt", Some(true)).is_none(),
        "and unstaging it once is enough: {:?}",
        after_unstage.resources
    );

    // The neighbours on both sides were never selected, so they must be untouched.
    for neighbour in ["aaa_other.txt", "zzz_other.txt"] {
        assert_eq!(
            find(&after_unstage, neighbour, Some(false)).map(|r| r.state),
            Some(ScmResourceState::Modified),
            "{neighbour} is still an unstaged modification"
        );
        assert!(
            find(&after_unstage, neighbour, Some(true)).is_none(),
            "{neighbour} was never staged"
        );
    }
}

/// In a repository with no commits, the committed anchor has nothing to offer and
/// says so, while the other two keep working.
///
/// "Nothing committed" is a truthful answer here, unlike the conflicted case where
/// the same shape would have been a fabrication — the distinction that decides
/// whether absence may be reported as absence.
#[tokio::test]
async fn anchors_in_a_repository_without_commits_report_absence_truthfully() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "staged.txt", "staged-body\n");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("staged.txt")).expect("add");
    index.write().expect("write index");
    write(tmp.path(), "untracked.txt", "untracked-body\n");

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let read = |rel: &'static str, anchor| {
        let provider = &provider;
        let repo_ref = &repo_ref;
        async move {
            provider
                .original(
                    repo_ref,
                    &FileRef {
                        pe_id: "pe1".to_owned(),
                        relative_path: rel.to_owned(),
                    },
                    anchor,
                )
                .await
                .expect("anchor read is not an error")
        }
    };

    // A staged addition: readable in the work tree and in the index, absent from a
    // history that does not exist yet.
    assert!(read("staged.txt", ContentRef::Working).await.is_some());
    assert!(read("staged.txt", ContentRef::Staged).await.is_some());
    assert!(
        read("staged.txt", ContentRef::Committed).await.is_none(),
        "no commit exists, so there is no committed version — reported as absent, not as an error"
    );

    // An untracked file: only the work tree has it.
    assert!(read("untracked.txt", ContentRef::Working).await.is_some());
    assert!(read("untracked.txt", ContentRef::Staged).await.is_none());
    assert!(read("untracked.txt", ContentRef::Committed).await.is_none());

    // And a diff against the missing side renders as an addition rather than failing.
    let diff = provider
        .diff(
            &repo_ref,
            &FileRef {
                pe_id: "pe1".to_owned(),
                relative_path: "staged.txt".to_owned(),
            },
            ContentRef::Working,
            ContentRef::Committed,
        )
        .await
        .expect("diff against an unborn history still produces a patch");
    assert!(diff.patch.is_some(), "a patch is produced");
    assert!(!diff.binary);
}

/// Staging several *distinct* files must stage all of them.
///
/// The duplicate-path test cannot show this: when every entry names the same file,
/// processing only the first one is indistinguishable from processing all three.
/// This one uses distinct paths so that "stops after the first" is visible, and
/// asserts against the repository rather than the returned outcome — an
/// implementation that skipped work could still report no failures.
#[tokio::test]
async fn staging_several_distinct_files_stages_every_one_of_them() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    let names = ["a_first.txt", "m_middle.txt", "z_last.txt"];
    for name in names {
        write(tmp.path(), name, "committed\n");
    }
    commit_all(&repo, "base");
    for name in names {
        write(tmp.path(), name, "edited\n");
    }

    let (provider, repo_ref) = discovered(tmp.path()).await;
    let staging = provider.staging().expect("staging");
    let files: Vec<FileRef> = names
        .iter()
        .map(|name| FileRef {
            pe_id: "pe1".to_owned(),
            relative_path: (*name).to_owned(),
        })
        .collect();

    let outcome = staging.stage(&repo_ref, &files).await.expect("stage ok");
    assert!(outcome.is_complete(), "no failures: {:?}", outcome.failed);

    let status = provider.status(&repo_ref).await.expect("status ok");
    for name in names {
        assert!(
            find(&status, name, Some(true)).is_some(),
            "{name} is staged — not just the first entry: {:?}",
            status.resources
        );
    }

    // And the mirror direction: unstaging the batch clears every one of them.
    let outcome = staging.unstage(&repo_ref, &files).await.expect("unstage ok");
    assert!(outcome.is_complete(), "no failures: {:?}", outcome.failed);
    let after = provider.status(&repo_ref).await.expect("status ok");
    for name in names {
        assert!(
            find(&after, name, Some(true)).is_none(),
            "{name} is no longer staged: {:?}",
            after.resources
        );
    }
}

/// `repo_id` is a pure `scm:{pe_id}` mapping: deterministic, and derived from the
/// pe identity rather than a path. This is the premise the roots diff relies on —
/// it compares repositories by `repo_id`, not by path. Because the project layer
/// canonicalizes a folder's path (folding case on a case-insensitive filesystem)
/// before it ever becomes a `pe_id`, one physical directory resolves to exactly
/// one `pe_id`, hence one `repo_id`; two case-different spellings can never split
/// it into two. (The fold itself is covered by `canonical_test::casing_folds_per_platform`
/// and the identity-level dedup by the project service suite.)
#[test]
fn repo_id_is_a_deterministic_scm_prefix_over_the_pe_id() {
    // The pe root itself (empty relative path) keeps the historical form.
    assert_eq!(GitScmProvider::repo_id_for("pe1", ""), "scm:pe1");
    assert_eq!(
        GitScmProvider::repo_id_for("pe1", ""),
        GitScmProvider::repo_id_for("pe1", ""),
        "the same pe id always yields the same repo id"
    );
    assert_ne!(
        GitScmProvider::repo_id_for("pe1", ""),
        GitScmProvider::repo_id_for("pe2", ""),
        "distinct pe ids stay distinct"
    );
}

/// A child repository under a workspace root folds its relative path into the id,
/// so children of one pe (which share `pe_id`) stay unique and stable.
#[test]
fn repo_id_folds_relative_path_for_child_repositories() {
    assert_eq!(GitScmProvider::repo_id_for("pe1", "svc-a"), "scm:pe1/svc-a");
    assert_ne!(
        GitScmProvider::repo_id_for("pe1", "svc-a"),
        GitScmProvider::repo_id_for("pe1", "svc-b"),
        "sibling children of one pe stay distinct"
    );
    assert_ne!(
        GitScmProvider::repo_id_for("pe1", "svc-a"),
        GitScmProvider::repo_id_for("pe1", ""),
        "a child never collides with its workspace root"
    );
    assert_eq!(
        GitScmProvider::repo_id_for("pe1", "svc-a"),
        GitScmProvider::repo_id_for("pe1", "svc-a"),
        "the same child always yields the same repo id"
    );
}
