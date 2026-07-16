use aionui_db::models::{
    DevelopmentRunRow, DevelopmentTaskRow, ProjectRow, QualityGateRunRow, ReviewFindingRow, TaskArtifactRow,
};
use aionui_db::{
    IDevelopmentRepository, IProjectRepository, SqliteDevelopmentRepository, SqliteProjectRepository,
    init_database_memory,
};

async fn setup() -> (SqliteDevelopmentRepository, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('user-1', 'developer', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    SqliteProjectRepository::new(db.pool().clone())
        .create(&ProjectRow {
            id: "project-1".into(),
            user_id: "user-1".into(),
            name: "Project".into(),
            local_path: "/tmp/project".into(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    (SqliteDevelopmentRepository::new(db.pool().clone()), db)
}

fn run() -> DevelopmentRunRow {
    DevelopmentRunRow {
        id: "run-1".into(),
        user_id: "user-1".into(),
        project_id: "project-1".into(),
        team_id: Some("team-1".into()),
        source_channel: Some("webui".into()),
        source_user_id: None,
        execution_mode: "team".into(),
        status: "running".into(),
        request_summary: "Implement feature".into(),
        acceptance_criteria: r#"["tests pass"]"#.into(),
        baseline_commit: Some("abc123".into()),
        integration_branch: Some("aion/run/run-1/integration".into()),
        started_at: Some(2),
        finished_at: None,
        created_at: 1,
        updated_at: 2,
    }
}

#[tokio::test]
async fn run_is_scoped_to_owner_and_updates_status() {
    let (repo, _db) = setup().await;
    repo.create_run(&run()).await.unwrap();
    assert!(repo.get_run("run-1", "other").await.unwrap().is_none());
    assert_eq!(repo.list_runs("user-1", Some("project-1")).await.unwrap().len(), 1);

    repo.update_run_status("run-1", "user-1", "verifying", None)
        .await
        .unwrap();
    assert_eq!(
        repo.get_run("run-1", "user-1").await.unwrap().unwrap().status,
        "verifying"
    );
}

#[tokio::test]
async fn artifacts_gates_and_findings_roundtrip_by_task() {
    let (repo, db) = setup().await;
    repo.create_run(&run()).await.unwrap();
    sqlx::query(
        "INSERT INTO team_tasks (id, team_id, run_id, subject, blocked_by, blocks, created_at, updated_at) \
         VALUES ('task-1', 'team-1', 'run-1', 'Task', '[]', '[]', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    repo.create_artifact(&TaskArtifactRow {
        id: "artifact-1".into(),
        run_id: "run-1".into(),
        task_id: Some("task-1".into()),
        artifact_type: "test".into(),
        path_or_uri: "artifacts/test.log".into(),
        checksum: "sha256:1".into(),
        producer_agent_id: Some("slot-1".into()),
        metadata: None,
        created_at: 3,
    })
    .await
    .unwrap();
    repo.create_gate(&QualityGateRunRow {
        id: "gate-1".into(),
        run_id: "run-1".into(),
        task_id: Some("task-1".into()),
        gate_type: "unit_test".into(),
        command: "cargo test".into(),
        working_directory: "/tmp/worktree".into(),
        exit_code: Some(0),
        status: "passed".into(),
        stdout_artifact_id: Some("artifact-1".into()),
        stderr_artifact_id: None,
        duration_ms: Some(10),
        required: true,
        started_at: Some(3),
        finished_at: Some(4),
        created_at: 3,
    })
    .await
    .unwrap();
    repo.create_finding(&ReviewFindingRow {
        id: "finding-1".into(),
        run_id: "run-1".into(),
        task_id: "task-1".into(),
        reviewer_agent_id: "slot-review".into(),
        producer_agent_id: Some("slot-1".into()),
        severity: "major".into(),
        file_path: Some("src/lib.rs".into()),
        line_number: Some(10),
        reason: "Bug".into(),
        suggestion: Some("Fix".into()),
        status: "open".into(),
        created_at: 4,
        updated_at: 4,
    })
    .await
    .unwrap();

    assert_eq!(repo.list_artifacts("run-1", Some("task-1")).await.unwrap().len(), 1);
    assert_eq!(
        repo.list_gates("run-1", Some("task-1")).await.unwrap()[0].status,
        "passed"
    );
    assert_eq!(
        repo.list_findings("run-1", "task-1").await.unwrap()[0].severity,
        "major"
    );
    repo.update_finding_status("run-1", "finding-1", "resolved")
        .await
        .unwrap();
    assert_eq!(
        repo.list_findings("run-1", "task-1").await.unwrap()[0].status,
        "resolved"
    );
}

#[tokio::test]
async fn development_tasks_roundtrip_with_quality_state() {
    let (repo, _db) = setup().await;
    repo.create_run(&run()).await.unwrap();
    repo.create_task(&DevelopmentTaskRow {
        id: "task-2".into(),
        team_id: "team-1".into(),
        run_id: Some("run-1".into()),
        subject: "Implement".into(),
        description: None,
        status: "ready".into(),
        owner: Some("slot-1".into()),
        blocked_by: "[]".into(),
        blocks: "[]".into(),
        metadata: None,
        acceptance_criteria: r#"["tests pass"]"#.into(),
        task_type: "implementation".into(),
        risk_level: "high".into(),
        assigned_workspace_lease_id: None,
        review_status: "pending".into(),
        verification_status: "pending".into(),
        created_at: 2,
        updated_at: 2,
    })
    .await
    .unwrap();

    assert_eq!(repo.list_tasks("run-1").await.unwrap().len(), 1);
    repo.update_task_state("run-1", "task-2", "review", "in_review", "passed")
        .await
        .unwrap();
    let task = repo.get_task("run-1", "task-2").await.unwrap().unwrap();
    assert_eq!(task.status, "review");
    assert_eq!(task.review_status, "in_review");
    assert_eq!(task.verification_status, "passed");
}
