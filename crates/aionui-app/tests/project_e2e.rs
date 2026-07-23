mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

#[tokio::test]
async fn project_routes_require_authentication() {
    let (app, _services) = build_app().await;
    let request = Request::builder().uri("/api/projects").body(Body::empty()).unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
}

#[tokio::test]
async fn authenticated_user_can_register_and_list_a_project() {
    let temp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "project-admin", "StrongP@ss1").await;

    let create = json_with_token(
        "POST",
        "/api/projects",
        json!({
            "name": "Aion",
            "local_path": temp.path(),
            "project_type": "single"
        }),
        &token,
        &csrf,
    );
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["data"]["name"], "Aion");

    let list = Request::builder()
        .uri("/api/projects")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(list).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn project_create_requires_csrf_for_authenticated_web_mode() {
    let temp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "project-csrf", "StrongP@ss1").await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/projects")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "name": "Blocked",
                "local_path": temp.path(),
                "project_type": "unknown"
            })
            .to_string(),
        ))
        .unwrap();

    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn project_knowledge_refresh_requires_csrf_for_authenticated_web_mode() {
    let temp = tempfile::tempdir().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "project-knowledge-csrf", "StrongP@ss1").await;
    let create = json_with_token(
        "POST",
        "/api/projects",
        json!({
            "name": "Knowledge",
            "local_path": temp.path(),
            "project_type": "single"
        }),
        &token,
        &csrf,
    );
    let created = body_json(app.clone().oneshot(create).await.unwrap()).await;
    let project_id = created["data"]["id"].as_str().unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/projects/{project_id}/knowledge/refresh"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN);
}
