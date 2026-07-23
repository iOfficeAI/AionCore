use std::sync::Arc;

use aionui_db::models::ProjectRow;
use aionui_db::{
    IProjectRepository, SqliteAgentWorkspaceLeaseRepository, SqliteDevelopmentRepository, SqliteProjectRepository,
    init_database_memory,
};
use aionui_development::{
    AppendPlanRevisionInput, CompletionEvidenceInput, CreateDevelopmentRunInput, CreateDevelopmentTaskInput,
    DevelopmentService,
};

async fn setup() -> DevelopmentService {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('user-1', 'dev', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let project_dir = tempfile::tempdir().unwrap().keep();
    let repo = git2::Repository::init(&project_dir).unwrap();
    std::fs::write(project_dir.join("README.md"), "baseline\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Aion", "aion@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])
        .unwrap();

    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "project-1".into(),
            user_id: "user-1".into(),
            name: "Project".into(),
            local_path: project_dir.to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let artifact_root = tempfile::tempdir().unwrap().keep();
    DevelopmentService::new(
        Arc::new(SqliteDevelopmentRepository::new(db.pool().clone())),
        project_repo,
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
        artifact_root,
    )
}

async fn create_run(service: &DevelopmentService) -> aionui_db::models::DevelopmentRunRow {
    service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: Some("webui".into()),
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Implement immutable requirements".into(),
                acceptance_criteria: vec!["requirements are immutable".into(), "tests prove completion".into()],
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn requirement_versions_are_append_only_and_plan_revisions_are_monotonic() {
    let service = setup().await;
    let run = create_run(&service).await;
    let original = service.requirements_snapshot("user-1", &run.id).await.unwrap();

    service
        .append_requirement_revision(
            "user-1",
            &run.id,
            "Requirements and evidence are append-only",
            "Clarify acceptance evidence",
            vec![
                "requirements are immutable".into(),
                "every criterion has accepted evidence".into(),
            ],
        )
        .await
        .unwrap();
    let first_plan = service
        .append_plan_revision(
            "user-1",
            &run.id,
            AppendPlanRevisionInput {
                summary: "Initial plan".into(),
                content: "1. persist versions\n2. verify evidence".into(),
            },
        )
        .await
        .unwrap();
    let second_plan = service
        .append_plan_revision(
            "user-1",
            &run.id,
            AppendPlanRevisionInput {
                summary: "Refined plan".into(),
                content: "1. persist\n2. map\n3. verify".into(),
            },
        )
        .await
        .unwrap();

    let snapshot = service.requirements_snapshot("user-1", &run.id).await.unwrap();
    assert_eq!(snapshot.original_requirement, original.original_requirement);
    assert_eq!(snapshot.requirement_versions.len(), 2);
    assert_eq!(first_plan.revision, 1);
    assert_eq!(second_plan.revision, 2);
    assert_eq!(snapshot.plan_revisions.len(), 2);
}

#[tokio::test]
async fn tasks_must_map_owned_criteria_and_run_completion_requires_accepted_evidence() {
    let service = setup().await;
    let run = create_run(&service).await;
    let snapshot = service.requirements_snapshot("user-1", &run.id).await.unwrap();
    let first = snapshot.active_criteria[0].clone();
    let second = snapshot.active_criteria[1].clone();

    let unmapped = service
        .create_task(
            "user-1",
            &run.id,
            CreateDevelopmentTaskInput {
                subject: "Unmapped".into(),
                description: None,
                owner: Some("codex".into()),
                blocked_by: vec![],
                acceptance_criteria: vec!["not owned by this run".into()],
                task_type: "implementation".into(),
                risk_level: "medium".into(),
                assigned_workspace_lease_id: None,
            },
        )
        .await;
    assert!(unmapped.is_err());

    let task = service
        .create_task(
            "user-1",
            &run.id,
            CreateDevelopmentTaskInput {
                subject: "Implement".into(),
                description: None,
                owner: Some("codex".into()),
                blocked_by: vec![],
                acceptance_criteria: vec![first.statement.clone(), second.statement.clone()],
                task_type: "implementation".into(),
                risk_level: "medium".into(),
                assigned_workspace_lease_id: None,
            },
        )
        .await
        .unwrap();
    assert!(service.complete_run("user-1", &run.id).await.is_err());

    service
        .record_completion_evidence(
            "user-1",
            &run.id,
            &task.id,
            CompletionEvidenceInput {
                criterion_id: first.id.clone(),
                evidence_type: "code".into(),
                artifact_id: None,
                reference: "commit:abc1234".into(),
                accepted: true,
                reviewer_id: Some("reviewer".into()),
            },
        )
        .await
        .unwrap();
    assert!(service.complete_run("user-1", &run.id).await.is_err());

    service
        .record_completion_evidence(
            "user-1",
            &run.id,
            &task.id,
            CompletionEvidenceInput {
                criterion_id: second.id,
                evidence_type: "test".into(),
                artifact_id: None,
                reference: "gate:unit_test".into(),
                accepted: true,
                reviewer_id: Some("reviewer".into()),
            },
        )
        .await
        .unwrap();
    let completed = service.complete_run("user-1", &run.id).await.unwrap();
    assert_eq!(completed.status, "succeeded");
    let final_snapshot = service.requirements_snapshot("user-1", &run.id).await.unwrap();
    assert!(final_snapshot.coverage.iter().all(|item| item.accepted));
}
