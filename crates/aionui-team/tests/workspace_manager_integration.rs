use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aionui_db::{SqliteAgentWorkspaceLeaseRepository, init_database_memory};
use aionui_team::{GitTeamWorkspaceManager, TeamWorkspaceManager, WorkspaceAgentSpec, WorkspaceCleanupDisposition};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git should start");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn initialized_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "aion@example.test"]);
    git(&repo, &["config", "user.name", "Aion Test"]);
    fs::write(repo.join("README.md"), "baseline\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-m", "baseline"]);
    repo
}

async fn manager(root: &Path) -> (GitTeamWorkspaceManager, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone()));
    (GitTeamWorkspaceManager::new(repo, root.join("managed")), db)
}

#[tokio::test]
async fn creates_integration_and_distinct_agent_worktrees_from_one_baseline() {
    let temp = tempfile::tempdir().unwrap();
    let source = initialized_repo(temp.path());
    let baseline = git(&source, &["rev-parse", "HEAD"]);
    let (manager, _db) = manager(temp.path()).await;

    let plan = manager
        .prepare_team(
            "user-1",
            "team-123456789",
            source.to_str().unwrap(),
            &[
                WorkspaceAgentSpec::new("slot-111111", "Lead Agent"),
                WorkspaceAgentSpec::new("slot-222222", "Worker Agent"),
            ],
        )
        .await
        .unwrap();

    assert_eq!(plan.base_commit, baseline);
    assert_eq!(plan.agent_leases.len(), 2);
    assert_ne!(plan.agent_leases[0].worktree_path, plan.agent_leases[1].worktree_path);
    assert_ne!(plan.integration.worktree_path, plan.agent_leases[0].worktree_path);
    assert!(Path::new(&plan.integration.worktree_path).is_dir());
    assert!(Path::new(&plan.agent_leases[0].worktree_path).is_dir());
    assert_eq!(
        plan.integration.branch_name,
        GitTeamWorkspaceManager::integration_branch_name("team-123456789")
    );
    assert!(plan.agent_leases[0].branch_name.contains("slot-111"));
    assert!(git(&source, &["status", "--porcelain"]).is_empty());

    let persisted = manager.list_team_leases("team-123456789").await.unwrap();
    assert_eq!(persisted.len(), 3);
    assert!(persisted.iter().all(|lease| lease.lease_status == "active"));
}

#[tokio::test]
async fn rejects_dirty_repository_and_never_overwrites_existing_branch() {
    let temp = tempfile::tempdir().unwrap();
    let source = initialized_repo(temp.path());
    let (manager, _db) = manager(temp.path()).await;
    fs::write(source.join("README.md"), "dirty\n").unwrap();

    let error = manager
        .prepare_team(
            "user-1",
            "team-dirty",
            source.to_str().unwrap(),
            &[WorkspaceAgentSpec::new("slot-1", "Lead")],
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("dirty"));
    assert!(manager.list_team_leases("team-dirty").await.unwrap().is_empty());

    git(&source, &["restore", "README.md"]);
    let collision_branch = GitTeamWorkspaceManager::integration_branch_name("team-collision");
    git(&source, &["branch", &collision_branch]);
    let error = manager
        .prepare_team(
            "user-1",
            "team-collision",
            source.to_str().unwrap(),
            &[WorkspaceAgentSpec::new("slot-1", "Lead")],
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already exists"));
}

#[tokio::test]
async fn cleanup_preserves_dirty_worktree_and_reconcile_marks_missing_path() {
    let temp = tempfile::tempdir().unwrap();
    let source = initialized_repo(temp.path());
    let (manager, _db) = manager(temp.path()).await;
    let plan = manager
        .prepare_team(
            "user-1",
            "team-cleanup",
            source.to_str().unwrap(),
            &[WorkspaceAgentSpec::new("slot-1", "Lead")],
        )
        .await
        .unwrap();
    let agent = &plan.agent_leases[0];
    fs::write(Path::new(&agent.worktree_path).join("dirty.txt"), "keep me").unwrap();

    let cleanup = manager.release_slot("team-cleanup", "slot-1").await.unwrap();
    assert_eq!(cleanup.disposition, WorkspaceCleanupDisposition::DirtyPreserved);
    assert!(Path::new(&agent.worktree_path).exists());

    fs::remove_dir_all(&plan.integration.worktree_path).unwrap();
    manager.reconcile_all().await.unwrap();
    let leases = manager.list_team_leases("team-cleanup").await.unwrap();
    let integration = leases.iter().find(|lease| lease.slot_id == "__integration__").unwrap();
    assert_eq!(integration.lease_status, "conflict");
    assert_eq!(integration.cleanup_status, "missing_worktree");
}

#[tokio::test]
async fn owned_path_validation_rejects_traversal_and_sibling_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let source = initialized_repo(temp.path());
    let (manager, _db) = manager(temp.path()).await;
    let plan = manager
        .prepare_team(
            "user-1",
            "team-paths",
            source.to_str().unwrap(),
            &[
                WorkspaceAgentSpec::new("slot-a", "A"),
                WorkspaceAgentSpec::new("slot-b", "B"),
            ],
        )
        .await
        .unwrap();

    let accepted = manager
        .validate_owned_path("team-paths", "slot-a", Path::new("src/lib.rs"))
        .await
        .unwrap();
    assert!(accepted.starts_with(&plan.agent_leases[0].worktree_path));
    assert!(
        manager
            .validate_owned_path("team-paths", "slot-a", Path::new("../slot-b/README.md"))
            .await
            .is_err()
    );
    assert!(
        manager
            .validate_owned_path("team-paths", "slot-a", Path::new(&plan.agent_leases[1].worktree_path))
            .await
            .is_err()
    );
}
