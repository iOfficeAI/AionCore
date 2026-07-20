use std::sync::{Arc, Mutex};

use aionui_db::{
    IDevelopmentRepository, IProjectRepository, SqliteAgentWorkspaceLeaseRepository,
    SqliteDevelopmentOperationsRepository, SqliteDevelopmentRepository, SqliteProjectRepository,
};
use aionui_development::{
    DeploymentExecution, DeploymentProvider, DeploymentRequestInput, DeploymentService, DevelopmentOperationsService,
};
use async_trait::async_trait;

#[derive(Default)]
struct RecordingDeploymentProvider {
    executions: Mutex<Vec<DeploymentExecution>>,
}

#[async_trait]
impl DeploymentProvider for RecordingDeploymentProvider {
    async fn deploy(&self, execution: &DeploymentExecution) -> Result<Option<String>, String> {
        self.executions.lock().unwrap().push(execution.clone());
        Ok(Some(format!("remote:{}", execution.deployment_key)))
    }

    async fn cancel(&self, _remote_id: &str) -> Result<(), String> {
        Ok(())
    }
}

async fn setup() -> (
    DeploymentService,
    Arc<RecordingDeploymentProvider>,
    Arc<SqliteDevelopmentRepository>,
    aionui_db::Database,
) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let projects = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    projects
        .create(&aionui_db::models::ProjectRow {
            id: "project-deploy".into(),
            user_id: "system_default_user".into(),
            name: "Deploy".into(),
            local_path: "/tmp/deploy".into(),
            repository_url: Some("https://gitlab.example/acme/app.git".into()),
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    let now = aionui_common::now_ms();
    repo.create_run(&aionui_db::models::DevelopmentRunRow {
        id: "run-deploy".into(),
        user_id: "system_default_user".into(),
        project_id: "project-deploy".into(),
        team_id: None,
        source_channel: Some("webui".into()),
        source_user_id: None,
        execution_mode: "single".into(),
        status: "succeeded".into(),
        request_summary: "Deploy release".into(),
        acceptance_criteria: "[]".into(),
        baseline_commit: Some("base123".into()),
        integration_branch: Some("aion/run/deploy".into()),
        started_at: Some(now),
        finished_at: Some(now),
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
    repo.upsert_delivery(&aionui_db::models::DevelopmentDeliveryRow {
        id: "delivery-deploy".into(),
        run_id: "run-deploy".into(),
        project_id: "project-deploy".into(),
        user_id: "system_default_user".into(),
        provider: "gitlab".into(),
        repository: Some("https://gitlab.example/acme/app.git".into()),
        branch: "aion/run/deploy".into(),
        base_branch: "main".into(),
        commit_sha: Some("abc1234".into()),
        status: "merged".into(),
        push_status: "pushed".into(),
        pr_number: Some(42),
        pr_url: Some("https://gitlab.example/acme/app/-/merge_requests/42".into()),
        pr_status: "merged".into(),
        ci_status: "passed".into(),
        review_status: "approved".into(),
        merge_status: "merged".into(),
        report_json: "{}".into(),
        last_error: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
    let provider = Arc::new(RecordingDeploymentProvider::default());
    let operations = Arc::new(DevelopmentOperationsService::new(
        Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone())),
        repo.clone(),
        projects.clone(),
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
    ));
    let service = DeploymentService::new(repo.clone(), projects, provider.clone()).with_operations(operations);
    (service, provider, repo, db)
}

fn request() -> DeploymentRequestInput {
    DeploymentRequestInput {
        environment: "production".into(),
        deployment_key: "release-2026-07-20".into(),
        commit_sha: "abc1234".into(),
    }
}

#[tokio::test]
async fn deployment_requires_unexpired_human_approval_tied_to_the_exact_request() {
    let (service, provider, _repo, db) = setup().await;
    let pending = service
        .request("system_default_user", "run-deploy", request())
        .await
        .unwrap();
    assert!(service.list("other-user", "run-deploy").await.is_err());
    assert!(service.get("other-user", &pending.id).await.is_err());
    assert_eq!(pending.status, "pending_approval");
    assert!(
        service
            .execute("system_default_user", "run-deploy", &pending.id)
            .await
            .is_err()
    );

    assert!(
        service
            .approve("system_default_user", "run-deploy", &pending.id, 1)
            .await
            .is_err()
    );
    let approved = service
        .approve("system_default_user", "run-deploy", &pending.id, 2)
        .await
        .unwrap();
    assert_eq!(approved.status, "approved");

    sqlx::query("UPDATE development_deployments SET approval_commit_sha = 'different' WHERE id = ?")
        .bind(&pending.id)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        service
            .execute("system_default_user", "run-deploy", &pending.id)
            .await
            .is_err()
    );
    assert!(provider.executions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_requests_and_callbacks_never_deploy_twice() {
    let (service, provider, _repo, db) = setup().await;
    let first = service
        .request("system_default_user", "run-deploy", request())
        .await
        .unwrap();
    let duplicate = service
        .request("system_default_user", "run-deploy", request())
        .await
        .unwrap();
    assert_eq!(first.id, duplicate.id);
    service
        .approve("system_default_user", "run-deploy", &first.id, 2)
        .await
        .unwrap();

    let completed = service
        .execute("system_default_user", "run-deploy", &first.id)
        .await
        .unwrap();
    let callback = service
        .execute("system_default_user", "run-deploy", &first.id)
        .await
        .unwrap();
    assert_eq!(completed.status, "succeeded");
    assert_eq!(callback.status, "succeeded");
    assert_eq!(provider.executions.lock().unwrap().len(), 1);
    let actions = sqlx::query_scalar::<_, String>(
        "SELECT action FROM development_audit_events WHERE target_id = ? ORDER BY created_at, id",
    )
    .bind(&first.id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(actions.contains(&"deployment.request".to_owned()));
    assert!(actions.contains(&"deployment.approve".to_owned()));
    assert!(actions.contains(&"deployment.execute".to_owned()));
}

#[tokio::test]
async fn expired_or_rejected_production_approval_fails_closed() {
    let (service, provider, _repo, db) = setup().await;
    let pending = service
        .request("system_default_user", "run-deploy", request())
        .await
        .unwrap();
    sqlx::query("UPDATE development_deployments SET approval_expires_at = 1 WHERE id = ?")
        .bind(&pending.id)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        service
            .approve("system_default_user", "run-deploy", &pending.id, 2)
            .await
            .is_err()
    );
    assert!(
        service
            .execute("system_default_user", "run-deploy", &pending.id)
            .await
            .is_err()
    );
    assert!(provider.executions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn deployment_actions_are_scoped_to_the_run_in_the_route_context() {
    let (service, provider, _repo, _db) = setup().await;
    let pending = service
        .request("system_default_user", "run-deploy", request())
        .await
        .unwrap();

    assert!(
        service
            .approve("system_default_user", "another-run", &pending.id, 2)
            .await
            .is_err()
    );
    assert!(provider.executions.lock().unwrap().is_empty());
}
