use std::sync::{Arc, Mutex};

use aionui_db::models::ProjectRow;
use aionui_db::{
    IDevelopmentRepository, IProjectRepository, SqliteAgentWorkspaceLeaseRepository, SqliteDevelopmentRepository,
    SqliteProjectRepository, init_database_memory,
};
use aionui_development::{
    CreateDevelopmentRunInput, DevelopmentService, DevelopmentWorkspacePort, PrepareDevelopmentWorkspace,
    PreparedDevelopmentWorkspace,
};

#[derive(Default)]
struct RecordingWorkspacePort {
    prepared: Mutex<Vec<PrepareDevelopmentWorkspace>>,
    restored: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl DevelopmentWorkspacePort for RecordingWorkspacePort {
    async fn prepare(&self, input: PrepareDevelopmentWorkspace) -> Result<PreparedDevelopmentWorkspace, String> {
        self.prepared.lock().unwrap().push(input.clone());
        Ok(PreparedDevelopmentWorkspace {
            lease_id: "lease-1".into(),
            workspace_path: "/managed/run-1".into(),
            branch: "aion/run/run-1/agent".into(),
            safe_point: input.baseline_commit,
        })
    }

    async fn restore(&self, lease_id: &str, safe_point: &str) -> Result<String, String> {
        self.restored.lock().unwrap().push((lease_id.into(), safe_point.into()));
        Ok("restored_and_released".into())
    }
}

async fn setup() -> (
    DevelopmentService,
    Arc<RecordingWorkspacePort>,
    Arc<SqliteDevelopmentRepository>,
    tempfile::TempDir,
) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('user-1', 'dev', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(project_dir.path()).unwrap();
    std::fs::write(project_dir.path().join("tracked.txt"), "baseline\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("tracked.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Aion", "aion@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])
        .unwrap();
    std::fs::write(project_dir.path().join("tracked.txt"), "user change\n").unwrap();
    std::fs::write(project_dir.path().join("untracked.txt"), "private draft\n").unwrap();

    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "project-1".into(),
            user_id: "user-1".into(),
            name: "Project".into(),
            local_path: project_dir.path().to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let workspace = Arc::new(RecordingWorkspacePort::default());
    let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    let service = DevelopmentService::new(
        development_repo.clone(),
        project_repo,
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
        tempfile::tempdir().unwrap().keep(),
    )
    .with_workspace(workspace.clone());
    (service, workspace, development_repo, project_dir)
}

#[tokio::test]
async fn single_run_captures_initial_user_diff_and_uses_an_isolated_workspace() {
    let (service, port, _development_repo, project_dir) = setup().await;
    let run = service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: None,
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Implement safely".into(),
                acceptance_criteria: vec!["user changes survive".into()],
            },
        )
        .await
        .unwrap();

    let workspace = service.prepare_single_workspace("user-1", &run.id).await.unwrap();
    assert_eq!(workspace.workspace_lease_id.as_deref(), Some("lease-1"));
    assert!(workspace.initial_diff_checksum.starts_with("sha256:"));
    assert!(std::path::Path::new(&workspace.initial_diff_path).is_file());
    assert_eq!(workspace.workspace_path.as_deref(), Some("/managed/run-1"));
    assert_eq!(
        std::fs::read_to_string(project_dir.path().join("tracked.txt")).unwrap(),
        "user change\n"
    );
    assert!(project_dir.path().join("untracked.txt").exists());
    assert_eq!(port.prepared.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_restores_the_recorded_safe_point_and_persists_cleanup() {
    let (service, port, _development_repo, _project_dir) = setup().await;
    let run = service
        .create_run(
            "user-1",
            CreateDevelopmentRunInput {
                project_id: "project-1".into(),
                team_id: None,
                source_channel: None,
                source_user_id: None,
                execution_mode: "single".into(),
                request_summary: "Cancel safely".into(),
                acceptance_criteria: vec!["safe point restored".into()],
            },
        )
        .await
        .unwrap();
    let prepared = service.prepare_single_workspace("user-1", &run.id).await.unwrap();
    let cancelled = service.cancel_single_workspace("user-1", &run.id).await.unwrap();

    assert_eq!(cancelled.cleanup_status, "restored_and_released");
    assert_eq!(cancelled.safe_point, prepared.safe_point);
    assert_eq!(
        port.restored.lock().unwrap().as_slice(),
        &[("lease-1".into(), prepared.safe_point)]
    );
    assert!(service.cancel_single_workspace("user-1", &run.id).await.is_err());
    assert_eq!(port.restored.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_rejects_integrating_and_terminal_runs_before_restore() {
    for status in ["integrating", "succeeded", "failed"] {
        let (service, port, development_repo, _project_dir) = setup().await;
        let run = service
            .create_run(
                "user-1",
                CreateDevelopmentRunInput {
                    project_id: "project-1".into(),
                    team_id: None,
                    source_channel: None,
                    source_user_id: None,
                    execution_mode: "single".into(),
                    request_summary: format!("Reject cancellation from {status}"),
                    acceptance_criteria: vec!["workspace is untouched".into()],
                },
            )
            .await
            .unwrap();
        service.prepare_single_workspace("user-1", &run.id).await.unwrap();
        development_repo
            .update_run_status(&run.id, "user-1", status, None)
            .await
            .unwrap();

        assert!(service.cancel_single_workspace("user-1", &run.id).await.is_err());
        assert!(service.cancel_run("user-1", &run.id).await.is_err());
        assert!(port.restored.lock().unwrap().is_empty());
        assert_eq!(
            development_repo
                .get_run(&run.id, "user-1")
                .await
                .unwrap()
                .unwrap()
                .status,
            status
        );
    }
}
