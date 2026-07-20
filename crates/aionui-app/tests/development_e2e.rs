mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

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

    let (app, _services) = build_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/development-projects/project/operations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
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
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/development-projects/{project_id}/operations/policy"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({
                        "isolation_mode": "host",
                        "container_cpu_millis": 1000,
                        "container_memory_mb": 2048,
                        "container_pids_limit": 256,
                        "network_mode": "none",
                        "allowed_secret_keys": [],
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
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/development-projects/{project_id}/operations/policy"),
            json!({
                "isolation_mode": "host",
                "container_cpu_millis": 1000,
                "container_memory_mb": 2048,
                "container_pids_limit": 256,
                "network_mode": "none",
                "allowed_secret_keys": [],
                "max_duration_ms": 14400000,
                "max_parallel_agents": 4,
                "max_retries": 3,
                "max_cost_microunits": 0,
                "alert_percent": 80,
                "over_limit_action": "pause"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let secret_path = format!("/api/development-projects/{project_id}/secrets");
    let without_csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&secret_path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({"name": "CI token", "value": "ghp_e2e_never_return", "expires_at": null}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(without_csrf.status(), StatusCode::FORBIDDEN);

    let created = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &secret_path,
            json!({"name": "CI token", "value": "ghp_e2e_never_return", "expires_at": null}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = body_json(created).await;
    assert!(!created.to_string().contains("ghp_e2e_never_return"));

    let (other_token, _other_csrf) = setup_and_login(&mut app, &services, "development-other", "StrongP@ss2").await;
    let isolated = app
        .clone()
        .oneshot(get_with_token(&secret_path, &other_token))
        .await
        .unwrap();
    assert_eq!(isolated.status(), StatusCode::NOT_FOUND);

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
        .oneshot(
            Request::builder()
                .uri(format!("/api/development-runs/{run_id}/requirements"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/development-runs/{run_id}/plans"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({"summary": "Plan", "content": "Implement"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let (other_token, _other_csrf) = setup_and_login(&mut app, &services, "other-developer", "StrongP@ss2").await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/development-runs/{run_id}/requirements"))
                .header("authorization", format!("Bearer {other_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/development-runs/{run_id}/requirements"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requirements = body_json(response).await;
    assert_eq!(requirements["data"]["original_requirement"], "Implement quality gates");
    assert_eq!(requirements["data"]["active_criteria"].as_array().unwrap().len(), 1);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/development-runs/{run_id}/plans"),
            json!({"summary": "Plan", "content": "Implement and verify"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

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
