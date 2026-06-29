mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app_with_mock_agents, get_request, get_with_token, json_with_token, setup_and_login};

fn create_body() -> serde_json::Value {
    json!({
        "type": "acp",
        "name": "Config Options",
        "extra": {}
    })
}

async fn create_and_warmup_conversation(app: &mut axum::Router, token: &str, csrf: &str) -> String {
    let req = json_with_token("POST", "/api/conversations", create_body(), token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["id"].as_str().unwrap().to_owned();

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{id}/warmup"),
        json!(null),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    id
}

#[tokio::test]
async fn config_options_requires_auth() {
    let (app, _services) = build_app_with_mock_agents().await;

    let resp = app
        .oneshot(get_request("/api/conversations/conv-1/config-options"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn config_options_returns_active_agent_snapshot() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_and_warmup_conversation(&mut app, &token, &csrf).await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{id}/config-options"),
            &token,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["config_options"][0]["id"], "model");
    assert_eq!(json["data"]["config_options"][0]["current_value"], "mock-model");
}

#[tokio::test]
async fn set_config_option_requires_csrf() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_and_warmup_conversation(&mut app, &token, &csrf).await;

    let req = Request::builder()
        .method("PUT")
        .uri(format!("/api/conversations/{id}/config-options/model"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&json!({ "value": "mock-model-updated" })).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn set_config_option_returns_observed_confirmation() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_and_warmup_conversation(&mut app, &token, &csrf).await;

    let req = json_with_token(
        "PUT",
        &format!("/api/conversations/{id}/config-options/model"),
        json!({ "value": "mock-model-updated" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["confirmation"], "observed");
    assert_eq!(json["data"]["config_options"][0]["current_value"], "mock-model-updated");
}

#[tokio::test]
async fn old_mode_and_model_routes_are_removed() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_and_warmup_conversation(&mut app, &token, &csrf).await;

    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/conversations/{id}/mode"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let req = json_with_token(
        "PUT",
        &format!("/api/conversations/{id}/model"),
        json!({ "model_id": "mock-model-updated" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
