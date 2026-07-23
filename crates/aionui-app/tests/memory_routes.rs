//! Memory route composition, security, isolation, and compatibility coverage.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, build_app, build_app_with_mock_agents, get_request, get_with_token, json_with_token, setup_and_login,
};

const SETTINGS_PATH: &str = "/api/memory/settings";

#[tokio::test]
async fn memory_routes_are_registered_and_require_authentication() {
    let (app, _) = build_app().await;

    let response = app.oneshot(get_request(SETTINGS_PATH)).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn memory_mutations_require_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let request = Request::builder()
        .method("PUT")
        .uri(SETTINGS_PATH)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(r#"{"enabled":true}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn memory_settings_are_isolated_by_authenticated_user() {
    let (mut app, services) = build_app().await;
    let (admin_token, admin_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;

    let admin_update = json_with_token(
        "PUT",
        SETTINGS_PATH,
        json!({
            "enabled": true,
            "default_capture": true,
            "default_recall": false,
            "consent_version": 1
        }),
        &admin_token,
        &admin_csrf,
    );
    assert_eq!(
        app.clone().oneshot(admin_update).await.unwrap().status(),
        StatusCode::OK
    );

    let other_update = json_with_token(
        "PUT",
        SETTINGS_PATH,
        json!({
            "enabled": false,
            "default_capture": false,
            "default_recall": true
        }),
        &other_token,
        &other_csrf,
    );
    assert_eq!(
        app.clone().oneshot(other_update).await.unwrap().status(),
        StatusCode::OK
    );

    let admin = body_json(
        app.clone()
            .oneshot(get_with_token(SETTINGS_PATH, &admin_token))
            .await
            .unwrap(),
    )
    .await;
    let other = body_json(app.oneshot(get_with_token(SETTINGS_PATH, &other_token)).await.unwrap()).await;

    assert_eq!(admin["data"]["enabled"], true);
    assert_eq!(admin["data"]["default_capture"], true);
    assert_eq!(admin["data"]["default_recall"], false);
    assert_eq!(other["data"]["enabled"], false);
    assert_eq!(other["data"]["default_capture"], false);
    assert_eq!(other["data"]["default_recall"], true);
}

#[tokio::test]
async fn module_state_reuses_the_single_application_memory_service() {
    let database = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(database, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let (states, _) = aionui_app::build_module_states(&services).await.unwrap();

    assert!(std::sync::Arc::ptr_eq(&services.memory_service, &states.memory.service));
}

#[tokio::test]
async fn legacy_send_payload_without_memory_fields_remains_accepted() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let create = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Legacy client",
            "extra": {}
        }),
        &token,
        &csrf,
    );
    let created = app.clone().oneshot(create).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let conversation_id = body_json(created).await["data"]["id"].as_str().unwrap().to_owned();

    let request = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/messages"),
        json!({ "content": "Hello from an older client" }),
        &token,
        &csrf,
    );
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}
