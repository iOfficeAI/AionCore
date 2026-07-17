use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aionui_auth::CurrentUser;
use aionui_db::models::ProjectRow;
use aionui_db::{
    IDevelopmentRepository, IProjectRepository, SqliteAgentWorkspaceLeaseRepository, SqliteDevelopmentRepository,
    SqliteProjectRepository, init_database_memory,
};
use aionui_development::{
    DeliveryProvider, DeliveryProviderSnapshot, DeliveryService, DevelopmentRouterState, DevelopmentService,
    ProviderPullRequest, development_routes,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[derive(Default)]
struct FakeDeliveryProvider {
    pushes: AtomicUsize,
}

#[async_trait]
impl DeliveryProvider for FakeDeliveryProvider {
    async fn preflight(&self, _repository: &Path) -> Result<(), String> {
        Ok(())
    }

    async fn push(&self, _repository: &Path, _branch: &str) -> Result<(), String> {
        self.pushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn ensure_pull_request(
        &self,
        _repository: &Path,
        _head: &str,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<ProviderPullRequest, String> {
        unreachable!()
    }

    async fn synchronize(&self, _repository: &Path, _number: i64) -> Result<DeliveryProviderSnapshot, String> {
        unreachable!()
    }

    async fn merge(&self, _repository: &Path, _number: i64) -> Result<(), String> {
        unreachable!()
    }
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

async fn app() -> (
    Router,
    tempfile::TempDir,
    aionui_db::Database,
    Arc<FakeDeliveryProvider>,
) {
    let db = init_database_memory().await.unwrap();
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init", "-b", "main"]);
    git(project.path(), &["config", "user.email", "routes@example.com"]);
    git(project.path(), &["config", "user.name", "Route Test"]);
    std::fs::write(project.path().join("README.md"), "baseline\n").unwrap();
    git(project.path(), &["add", "."]);
    git(project.path(), &["commit", "-m", "baseline"]);
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "project-1".into(),
            user_id: "system_default_user".into(),
            name: "Project".into(),
            local_path: project.path().to_string_lossy().into_owned(),
            repository_url: Some("https://github.com/example/project.git".into()),
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    let service = Arc::new(DevelopmentService::new(
        development_repo.clone(),
        project_repo.clone(),
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
        project.path().join("artifacts"),
    ));
    let provider = Arc::new(FakeDeliveryProvider::default());
    let delivery_service = Arc::new(DeliveryService::new(development_repo, project_repo, provider.clone()));
    let router = development_routes(DevelopmentRouterState {
        service,
        delivery_service,
    })
    .layer(Extension(CurrentUser {
        id: "system_default_user".into(),
        username: "system_default_user".into(),
    }));
    (router, project, db, provider)
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn routes_create_and_read_a_development_board() {
    let (app, _project, _db, _provider) = app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-runs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project_id": "project-1",
                        "execution_mode": "single",
                        "request_summary": "Implement feature",
                        "acceptance_criteria": ["tests pass"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let run_id = json(response).await["data"]["id"].as_str().unwrap().to_owned();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/development-runs/{run_id}/tasks"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "subject": "Implement",
                        "acceptance_criteria": ["tests pass"],
                        "task_type": "implementation",
                        "risk_level": "medium"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/development-runs/{run_id}/tasks"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_run_payload_is_rejected() {
    let (app, _project, _db, _provider) = app().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-runs")
                .header("content-type", "application/json")
                .body(Body::from("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delivery_routes_prepare_and_require_confirmation_for_push() {
    let (app, project, db, provider) = app().await;
    let repo = SqliteDevelopmentRepository::new(db.pool().clone());
    repo.create_run(&aionui_db::models::DevelopmentRunRow {
        id: "run-route-delivery".into(),
        user_id: "system_default_user".into(),
        project_id: "project-1".into(),
        team_id: None,
        source_channel: Some("webui".into()),
        source_user_id: None,
        execution_mode: "single".into(),
        status: "reviewing".into(),
        request_summary: "Ship route delivery".into(),
        acceptance_criteria: r#"["tests pass"]"#.into(),
        baseline_commit: Some(git(project.path(), &["rev-parse", "HEAD"])),
        integration_branch: None,
        started_at: Some(1),
        finished_at: None,
        created_at: 1,
        updated_at: 1,
    })
    .await
    .unwrap();
    repo.create_task(&aionui_db::models::DevelopmentTaskRow {
        id: "route-task".into(),
        team_id: "run:run-route-delivery".into(),
        run_id: Some("run-route-delivery".into()),
        subject: "Implement".into(),
        description: None,
        status: "completed".into(),
        owner: Some("agent".into()),
        blocked_by: "[]".into(),
        blocks: "[]".into(),
        metadata: None,
        acceptance_criteria: r#"["tests pass"]"#.into(),
        task_type: "implementation".into(),
        risk_level: "medium".into(),
        assigned_workspace_lease_id: None,
        review_status: "approved".into(),
        verification_status: "passed".into(),
        created_at: 1,
        updated_at: 1,
    })
    .await
    .unwrap();
    repo.create_gate(&aionui_db::models::QualityGateRunRow {
        id: "route-gate".into(),
        run_id: "run-route-delivery".into(),
        task_id: None,
        gate_type: "unit_test".into(),
        command: "cargo test".into(),
        working_directory: project.path().to_string_lossy().into_owned(),
        exit_code: Some(0),
        status: "passed".into(),
        stdout_artifact_id: None,
        stderr_artifact_id: None,
        duration_ms: Some(1),
        required: true,
        started_at: Some(1),
        finished_at: Some(2),
        created_at: 1,
    })
    .await
    .unwrap();
    std::fs::write(project.path().join("README.md"), "delivered\n").unwrap();

    let prepared = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-runs/run-route-delivery/delivery/prepare")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"message":"feat: deliver route"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prepared.status(), StatusCode::OK);
    assert_ne!(json(prepared).await["data"]["branch"], "main");

    let rejected = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-runs/run-route-delivery/delivery/push")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"confirmed":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(provider.pushes.load(Ordering::SeqCst), 0);

    let pushed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-runs/run-route-delivery/delivery/push")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"confirmed":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pushed.status(), StatusCode::OK);
    assert_eq!(provider.pushes.load(Ordering::SeqCst), 1);

    let fetched = app
        .oneshot(
            Request::builder()
                .uri("/api/development-runs/run-route-delivery/delivery")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(json(fetched).await["data"]["push_status"], "pushed");
}
