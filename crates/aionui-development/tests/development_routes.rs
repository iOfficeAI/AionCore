use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aionui_auth::CurrentUser;
use aionui_db::models::ProjectRow;
use aionui_db::{
    IDevelopmentRepository, IProjectRepository, SqliteAgentWorkspaceLeaseRepository,
    SqliteDevelopmentOperationsRepository, SqliteDevelopmentRepository, SqliteProjectRepository, init_database_memory,
};
use aionui_development::{
    DeliveryProvider, DeliveryProviderSnapshot, DeliveryService, DeploymentService, DevelopmentOperationsService,
    DevelopmentRouterState, DevelopmentService, PortabilityService, PricingService, ProviderPullRequest,
    RetentionService, SecretService, UnconfiguredDeploymentProvider, development_routes,
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

async fn app_for(
    current_user_id: &str,
) -> (
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
    let lease_repo = Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone()));
    let operations_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    let operations_service = Arc::new(DevelopmentOperationsService::new(
        operations_repo.clone(),
        development_repo.clone(),
        project_repo.clone(),
        lease_repo.clone(),
    ));
    let service = Arc::new(
        DevelopmentService::new(
            development_repo.clone(),
            project_repo.clone(),
            lease_repo,
            project.path().join("artifacts"),
        )
        .with_operations(operations_service.clone()),
    );
    let provider = Arc::new(FakeDeliveryProvider::default());
    let delivery_service = Arc::new(
        DeliveryService::new(development_repo.clone(), project_repo.clone(), provider.clone())
            .with_operations(operations_service.clone()),
    );
    let router = development_routes(DevelopmentRouterState {
        service,
        delivery_service,
        deployment_service: Arc::new(DeploymentService::new(
            development_repo.clone(),
            project_repo,
            Arc::new(UnconfiguredDeploymentProvider),
        )),
        operations_service,
        secret_service: Arc::new(SecretService::new(
            operations_repo.clone(),
            Arc::new(SqliteProjectRepository::new(db.pool().clone())),
            Arc::new([9_u8; 32]),
        )),
        pricing_service: Arc::new(PricingService::new(operations_repo.clone())),
        development_repo,
        operations_repo,
        approval_repo: Arc::new(aionui_db::SqliteApprovalRepository::new(db.pool().clone())),
        portability_service: Arc::new(PortabilityService::new(
            db.pool().clone(),
            b"development-route-test",
            "route-test-instance",
        )),
        retention_service: Arc::new(RetentionService::new(db.pool().clone())),
    })
    .layer(Extension(CurrentUser {
        id: current_user_id.into(),
        username: current_user_id.into(),
    }));
    (router, project, db, provider)
}

async fn app() -> (
    Router,
    tempfile::TempDir,
    aionui_db::Database,
    Arc<FakeDeliveryProvider>,
) {
    app_for("system_default_user").await
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn retention_routes_are_owner_scoped_and_require_confirmation_for_cleanup() {
    let (app, _project, _db, _provider) = app().await;
    let updated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/development-projects/project-1/retention")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"conversation_history_days":30,"artifact_days":60,"evaluation_days":90}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(json(updated).await["data"]["artifact_days"], 60);

    let preview = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-projects/project-1/retention/cleanup")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"dry_run":true,"confirmation_count":0}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preview.status(), StatusCode::OK);
    assert_eq!(json(preview).await["data"]["dry_run"], true);

    let rejected = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-projects/project-1/retention/cleanup")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"dry_run":false,"confirmation_count":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

    let (other_app, _project, _db, _provider) = app_for("other-user").await;
    let forbidden = other_app
        .oneshot(
            Request::builder()
                .uri("/api/development-projects/project-1/retention")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn secret_routes_return_only_metadata_and_enforce_project_ownership() {
    let (app, _project, _db, _provider) = app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-projects/project-1/secrets")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "GitHub token",
                        "value": "ghp_route_secret_must_not_escape",
                        "expires_at": null
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json(response).await;
    let secret_id = body["data"]["id"].as_str().unwrap().to_owned();
    assert!(!body.to_string().contains("ghp_route_secret_must_not_escape"));
    assert!(body["data"].get("encrypted_value").is_none());

    let grant = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/development-projects/project-1/secrets/{secret_id}/grants"
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "secret_id": &secret_id,
                        "scope_type": "project",
                        "scope_id": "project-1",
                        "environment_key": "GITHUB_TOKEN",
                        "expires_at": null
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(grant.status(), StatusCode::OK);

    let (other_app, _other_project, _other_db, _other_provider) = app_for("other-user").await;
    let unauthorized = other_app
        .oneshot(
            Request::builder()
                .uri("/api/development-projects/project-1/secrets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::NOT_FOUND);

    let revoked = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/development-projects/project-1/secrets/{secret_id}/revoke"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
}

#[tokio::test]
async fn operations_routes_manage_policy_snapshot_and_recovery() {
    let (app, _project, db, _provider) = app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/development-projects/project-1/operations/policy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["data"]["isolation_mode"], "host");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/development-projects/project-1/operations/policy")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "isolation_mode": "docker",
                        "container_image": "node:24-alpine",
                        "container_cpu_millis": 1000,
                        "container_memory_mb": 2048,
                        "container_pids_limit": 256,
                        "network_mode": "none",
                        "allowed_secret_keys": ["NPM_TOKEN"],
                        "max_duration_ms": 14400000,
                        "max_parallel_agents": 4,
                        "max_retries": 3,
                        "max_cost_microunits": 0,
                        "alert_percent": 80,
                        "over_limit_action": "pause"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["data"]["isolation_mode"], "docker");
    assert_eq!(body["data"]["allowed_secret_keys_json"], "[\"NPM_TOKEN\"]");

    let run = app
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
                        "request_summary": "recover",
                        "acceptance_criteria": ["safe"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(run.status(), StatusCode::CREATED);
    let run_id = json(run).await["data"]["id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE development_runs SET updated_at = 1, started_at = 1 WHERE id = ?")
        .bind(&run_id)
        .execute(db.pool())
        .await
        .unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/development-operations/reconcile")
                .header("content-type", "application/json")
                .body(Body::from("{\"stale_after_ms\":10}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["data"].as_array().unwrap().len(), 1);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/development-projects/project-1/operations?run_id={run_id}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["data"]["recovery"].as_array().unwrap().len(), 1);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/development-runs/{run_id}/recovery"))
                .header("content-type", "application/json")
                .body(Body::from("{\"action\":\"terminate\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["data"]["status_after"], "cancelled");
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
async fn timeline_merges_run_and_task_events_and_controls_are_server_validated() {
    let (app, _project, _db, _provider) = app().await;
    let run = app
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
                        "request_summary": "Timeline fixture",
                        "acceptance_criteria": ["events are correlated"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let run_id = json(run).await["data"]["id"].as_str().unwrap().to_owned();
    let task = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/development-runs/{run_id}/tasks"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "subject": "Timeline task",
                        "blocked_by": [],
                        "acceptance_criteria": ["events are correlated"],
                        "task_type": "implementation",
                        "risk_level": "medium"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(task.status(), StatusCode::CREATED);

    let timeline = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/development-runs/{run_id}/timeline"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(timeline.status(), StatusCode::OK);
    let body = json(timeline).await;
    let events = body["data"]["events"].as_array().unwrap();
    assert!(events.iter().any(|event| event["kind"] == "run"));
    assert!(events.iter().any(|event| event["kind"] == "task"));
    assert_eq!(body["data"]["controls"]["allowed_run_actions"][0], "pause");

    let paused = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/development-runs/{run_id}/control"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"pause","task_id":null,"target_slot_id":null}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(json(paused).await["data"]["run_status"], "paused");

    let rejected = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/development-runs/{run_id}/control"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"pause","task_id":null,"target_slot_id":null}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
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
    let now = aionui_common::now_ms();
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
        started_at: Some(now),
        finished_at: None,
        created_at: now,
        updated_at: now,
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
        isolation_mode: "host".into(),
        execution_id: Some("gate-route".into()),
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
                .body(Body::from(r#"{"confirmed":false,"confirmation_count":0}"#))
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
                .body(Body::from(r#"{"confirmed":true,"confirmation_count":2}"#))
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
