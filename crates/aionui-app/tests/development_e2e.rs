mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

#[tokio::test]
async fn development_routes_require_authentication() {
    let (app, _services) = build_app().await;
    let request = Request::builder()
        .uri("/api/development-runs")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
}

#[tokio::test]
async fn authenticated_user_can_create_an_evidence_backed_development_board() {
    let project = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "development-admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/projects",
            json!({
                "name": "Aion",
                "local_path": project.path(),
                "project_type": "single"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let project_id = body_json(response).await["data"]["id"].as_str().unwrap().to_owned();

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/development-runs",
            json!({
                "project_id": project_id,
                "execution_mode": "single",
                "request_summary": "Implement quality gates",
                "acceptance_criteria": ["unit tests pass"]
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let run_id = body_json(response).await["data"]["id"].as_str().unwrap().to_owned();

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/development-runs/{run_id}/tasks"),
            json!({
                "subject": "Implement executor",
                "acceptance_criteria": ["unit tests pass"],
                "risk_level": "high",
                "task_type": "implementation",
                "blocked_by": []
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/development-runs/{run_id}/tasks"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"].as_array().unwrap().len(), 1);
}
