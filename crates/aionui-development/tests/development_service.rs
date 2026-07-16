use std::sync::Arc;

use aionui_db::models::{ProjectCommandProfileRow, ProjectRow};
use aionui_db::{
    IProjectRepository, SqliteAgentWorkspaceLeaseRepository, SqliteDevelopmentRepository, SqliteProjectRepository,
    init_database_memory,
};
use aionui_development::{
    CreateArtifactInput, CreateDevelopmentRunInput, CreateDevelopmentTaskInput, DevelopmentService, SubmitReviewInput,
};
use sha2::{Digest, Sha256};

async fn setup(project_path: &str) -> (DevelopmentService, tempfile::TempDir) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('user-1', 'developer', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "project-1".into(),
            user_id: "user-1".into(),
            name: "Project".into(),
            local_path: project_path.into(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    project_repo
        .upsert_command_profile(&ProjectCommandProfileRow {
            project_id: "project-1".into(),
            install_command: None,
            format_command: None,
            lint_command: None,
            typecheck_command: None,
            unit_test_command: Some("printf gate-ok".into()),
            integration_test_command: None,
            e2e_command: None,
            build_command: None,
            security_scan_command: None,
            command_timeout_seconds: 10,
            updated_at: 1,
        })
        .await
        .unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let service = DevelopmentService::new(
        Arc::new(SqliteDevelopmentRepository::new(db.pool().clone())),
        project_repo,
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
        artifacts.path().to_path_buf(),
    );
    (service, artifacts)
}

#[tokio::test]
async fn create_run_and_task_are_owner_scoped() {
    let project = tempfile::tempdir().unwrap();
    let (service, _artifacts) = setup(project.path().to_str().unwrap()).await;
    let run = service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: Some("webui".into()),
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Fix issue".into(),
                acceptance_criteria: vec!["unit tests pass".into()],
            },
        )
        .await
        .unwrap();
    assert!(service.get_run("other", &run.id).await.is_err());

    let task = service
        .create_task(
            "user-1",
            &run.id,
            CreateDevelopmentTaskInput {
                subject: "Implement fix".into(),
                description: None,
                owner: Some("implementer".into()),
                blocked_by: vec![],
                acceptance_criteria: vec!["unit tests pass".into()],
                task_type: "implementation".into(),
                risk_level: "medium".into(),
                assigned_workspace_lease_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(service.list_tasks("user-1", &run.id).await.unwrap()[0].id, task.id);
}

#[tokio::test]
async fn configured_gate_runs_and_records_bounded_artifact() {
    let project = tempfile::tempdir().unwrap();
    let (service, _artifacts) = setup(project.path().to_str().unwrap()).await;
    let run = service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: None,
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Run test".into(),
                acceptance_criteria: vec!["tests pass".into()],
            },
        )
        .await
        .unwrap();
    let gate = service
        .execute_gate("user-1", &run.id, None, "unit_test", None, true)
        .await
        .unwrap();
    assert_eq!(gate.status, "passed");
    assert_eq!(gate.exit_code, Some(0));
    assert!(gate.stdout_artifact_id.is_some());
}

#[tokio::test]
async fn completion_is_rejected_without_required_evidence() {
    let project = tempfile::tempdir().unwrap();
    let (service, _artifacts) = setup(project.path().to_str().unwrap()).await;
    let run = service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: None,
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Implement".into(),
                acceptance_criteria: vec!["works".into()],
            },
        )
        .await
        .unwrap();
    let task = service
        .create_task(
            "user-1",
            &run.id,
            CreateDevelopmentTaskInput {
                subject: "Implement".into(),
                description: None,
                owner: None,
                blocked_by: vec![],
                acceptance_criteria: vec!["works".into()],
                task_type: "implementation".into(),
                risk_level: "high".into(),
                assigned_workspace_lease_id: None,
            },
        )
        .await
        .unwrap();

    let evaluation = service.evaluate_completion("user-1", &run.id, &task.id).await.unwrap();
    assert!(!evaluation.allowed);
    assert!(evaluation.reasons.iter().any(|reason| reason.contains("gate")));
    assert!(service.complete_task("user-1", &run.id, &task.id).await.is_err());
}

#[tokio::test]
async fn completion_succeeds_with_gate_review_acceptance_and_commit_evidence() {
    let project = tempfile::tempdir().unwrap();
    let (service, _artifacts) = setup(project.path().to_str().unwrap()).await;
    let run = service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: None,
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Implement".into(),
                acceptance_criteria: vec!["works".into()],
            },
        )
        .await
        .unwrap();
    let task = service
        .create_task(
            "user-1",
            &run.id,
            CreateDevelopmentTaskInput {
                subject: "Implement".into(),
                description: None,
                owner: Some("implementer".into()),
                blocked_by: vec![],
                acceptance_criteria: vec!["works".into()],
                task_type: "implementation".into(),
                risk_level: "high".into(),
                assigned_workspace_lease_id: None,
            },
        )
        .await
        .unwrap();
    service
        .execute_gate("user-1", &run.id, Some(&task.id), "unit_test", None, true)
        .await
        .unwrap();
    let report_path = project.path().join("test-report.txt");
    std::fs::write(&report_path, b"tests passed").unwrap();
    let report_checksum = format!("sha256:{:x}", Sha256::digest(b"tests passed"));
    for (artifact_type, value, checksum) in [
        ("test", report_path.to_string_lossy().into_owned(), report_checksum),
        ("commit", "abc1234".into(), "sha256:verified".into()),
    ] {
        service
            .create_artifact(
                "user-1",
                &run.id,
                CreateArtifactInput {
                    task_id: Some(task.id.clone()),
                    artifact_type: artifact_type.into(),
                    path_or_uri: value.into(),
                    checksum,
                    producer_agent_id: Some("implementer".into()),
                    metadata: None,
                },
            )
            .await
            .unwrap();
    }
    service
        .submit_review(
            "user-1",
            &run.id,
            SubmitReviewInput {
                task_id: task.id.clone(),
                reviewer_agent_id: "reviewer".into(),
                producer_agent_id: Some("implementer".into()),
                findings: vec![],
                approved: true,
            },
        )
        .await
        .unwrap();

    assert!(
        service
            .evaluate_completion("user-1", &run.id, &task.id)
            .await
            .unwrap()
            .allowed
    );
    assert_eq!(
        service.complete_task("user-1", &run.id, &task.id).await.unwrap().status,
        "completed"
    );
}
