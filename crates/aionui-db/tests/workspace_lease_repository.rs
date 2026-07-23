use std::sync::Arc;

use aionui_db::models::AgentWorkspaceLeaseRow;
use aionui_db::{
    AgentWorkspaceLeaseUpdate, IAgentWorkspaceLeaseRepository, SqliteAgentWorkspaceLeaseRepository,
    init_database_memory,
};

async fn repo() -> (Arc<dyn IAgentWorkspaceLeaseRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone()));
    (repo, db)
}

fn lease(id: &str, team_id: &str, slot_id: &str, path: &str) -> AgentWorkspaceLeaseRow {
    AgentWorkspaceLeaseRow {
        id: id.into(),
        team_id: team_id.into(),
        user_id: "system_default_user".into(),
        slot_id: slot_id.into(),
        workspace_mode: "isolated_worktree".into(),
        repository_path: "/repo".into(),
        worktree_path: path.into(),
        branch_name: format!("aion/team/{team_id}/{slot_id}"),
        base_commit: "abc123".into(),
        allowed_paths: r#"["."]"#.into(),
        lease_status: "active".into(),
        cleanup_status: "none".into(),
        conflict_files: "[]".into(),
        last_error: None,
        created_at: 10,
        updated_at: 10,
        released_at: None,
    }
}

#[tokio::test]
async fn lease_roundtrip_and_status_transition() {
    let (repo, _db) = repo().await;
    repo.create(&lease("l1", "t1", "s1", "/tmp/w1")).await.unwrap();

    let found = repo.get_for_team_slot("t1", "s1").await.unwrap().unwrap();
    assert_eq!(found.branch_name, "aion/team/t1/s1");

    repo.update(
        "l1",
        &AgentWorkspaceLeaseUpdate {
            lease_status: Some("cleanup_pending".into()),
            cleanup_status: Some("dirty_preserved".into()),
            last_error: Some(Some("uncommitted changes".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let updated = repo.get("l1").await.unwrap().unwrap();
    assert_eq!(updated.lease_status, "cleanup_pending");
    assert_eq!(updated.cleanup_status, "dirty_preserved");
    assert_eq!(updated.last_error.as_deref(), Some("uncommitted changes"));
}

#[tokio::test]
async fn team_slot_and_active_worktree_are_unique() {
    let (repo, _db) = repo().await;
    repo.create(&lease("l1", "t1", "s1", "/tmp/w1")).await.unwrap();

    assert!(repo.create(&lease("l2", "t1", "s1", "/tmp/w2")).await.is_err());
    assert!(repo.create(&lease("l3", "t2", "s2", "/tmp/w1")).await.is_err());
}

#[tokio::test]
async fn list_for_team_and_reconcile_candidates_are_deterministic() {
    let (repo, _db) = repo().await;
    repo.create(&lease("l2", "t1", "s2", "/tmp/w2")).await.unwrap();
    repo.create(&lease("l1", "t1", "s1", "/tmp/w1")).await.unwrap();
    repo.create(&lease("l3", "t2", "s3", "/tmp/w3")).await.unwrap();
    repo.update(
        "l3",
        &AgentWorkspaceLeaseUpdate {
            lease_status: Some("released".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let team = repo.list_for_team("t1").await.unwrap();
    assert_eq!(
        team.iter().map(|row| row.slot_id.as_str()).collect::<Vec<_>>(),
        ["s1", "s2"]
    );
    let candidates = repo.list_reconcile_candidates().await.unwrap();
    assert_eq!(candidates.len(), 2);
}
