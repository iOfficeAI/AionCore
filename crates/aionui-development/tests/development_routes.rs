use std::sync::Arc;

use aionui_auth::CurrentUser;
use aionui_db::models::ProjectRow;
use aionui_db::{
    IProjectRepository, SqliteAgentWorkspaceLeaseRepository, SqliteDevelopmentRepository, SqliteProjectRepository,
    init_database_memory,
};
use aionui_development::{DevelopmentRouterState, DevelopmentService, development_routes};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn app() -> (Router, tempfile::TempDir, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let project = tempfile::tempdir().unwrap();
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "project-1".into(),
            user_id: "system_default_user".into(),
            name: "Project".into(),
            local_path: project.path().to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: None,
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let service = Arc::new(DevelopmentService::new(
        Arc::new(SqliteDevelopmentRepository::new(db.pool().clone())),
        project_repo,
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
        project.path().join("artifacts"),
    ));
    let router = development_routes(DevelopmentRouterState { service }).layer(Extension(CurrentUser {
        id: "system_default_user".into(),
        username: "system_default_user".into(),
    }));
    (router, project, db)
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn routes_create_and_read_a_development_board() {
    let (app, _project, _db) = app().await;
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
    let (app, _project, _db) = app().await;
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
