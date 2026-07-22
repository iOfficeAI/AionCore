//! App Operations model API tests with authentication and CSRF coverage.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_request, get_with_token, json_with_token, setup_and_login};

const APP_OPERATIONS_MODEL_PATH: &str = "/api/app-operations/model";

#[tokio::test]
async fn app_operations_get_requires_authentication() {
    let (app, _) = build_app().await;

    let resp = app.oneshot(get_request(APP_OPERATIONS_MODEL_PATH)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn app_operations_put_requires_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let req = Request::builder()
        .method("PUT")
        .uri(APP_OPERATIONS_MODEL_PATH)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(r#"{"mode":"auto"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn app_operations_defaults_to_auto_setup_required_without_providers() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token(APP_OPERATIONS_MODEL_PATH, &token))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["setting"], json!({ "mode": "auto" }));
    assert_eq!(json["data"]["health"], "setup_required");
    assert_eq!(json["data"]["reason_code"], "no_eligible_model");
    assert!(json["data"].get("resolved_model").is_none());
}

#[tokio::test]
async fn app_operations_fixed_setting_persists_across_get() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_request = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "id": "app-operations-provider",
            "platform": "openai",
            "name": "App Operations Provider",
            "base_url": "https://api.example.com",
            "api_key": "test-key",
            "models": ["model-a"]
        }),
        &token,
        &csrf,
    );
    let provider_response = app.clone().oneshot(provider_request).await.unwrap();
    assert_eq!(provider_response.status(), StatusCode::CREATED);

    let update_request = json_with_token(
        "PUT",
        APP_OPERATIONS_MODEL_PATH,
        json!({
            "mode": "fixed",
            "provider_id": "app-operations-provider",
            "model_id": "model-a"
        }),
        &token,
        &csrf,
    );
    let update_response = app.clone().oneshot(update_request).await.unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);
    let update_json = body_json(update_response).await;
    assert_eq!(
        update_json["data"]["setting"],
        json!({
            "mode": "fixed",
            "provider_id": "app-operations-provider",
            "model_id": "model-a"
        })
    );
    assert_eq!(
        update_json["data"]["resolved_model"],
        json!({ "provider_id": "app-operations-provider", "model_id": "model-a" })
    );
    assert_eq!(update_json["data"]["health"], "ready");

    let get_response = app
        .oneshot(get_with_token(APP_OPERATIONS_MODEL_PATH, &token))
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    let get_json = body_json(get_response).await;
    assert_eq!(get_json["data"], update_json["data"]);
}

#[tokio::test]
async fn app_operations_fixed_rejects_unknown_model() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_request = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "id": "app-operations-provider",
            "platform": "openai",
            "name": "App Operations Provider",
            "base_url": "https://api.example.com",
            "api_key": "test-key",
            "models": ["model-a"]
        }),
        &token,
        &csrf,
    );
    let provider_response = app.clone().oneshot(provider_request).await.unwrap();
    assert_eq!(provider_response.status(), StatusCode::CREATED);

    let known_model_request = json_with_token(
        "PUT",
        APP_OPERATIONS_MODEL_PATH,
        json!({
            "mode": "fixed",
            "provider_id": "app-operations-provider",
            "model_id": "model-a"
        }),
        &token,
        &csrf,
    );
    let known_model_response = app.clone().oneshot(known_model_request).await.unwrap();
    assert_eq!(known_model_response.status(), StatusCode::OK);

    let unknown_model_request = json_with_token(
        "PUT",
        APP_OPERATIONS_MODEL_PATH,
        json!({
            "mode": "fixed",
            "provider_id": "app-operations-provider",
            "model_id": "unknown-model"
        }),
        &token,
        &csrf,
    );
    let unknown_model_response = app.clone().oneshot(unknown_model_request).await.unwrap();
    assert_eq!(unknown_model_response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let unknown_model_json = body_json(unknown_model_response).await;
    assert_eq!(unknown_model_json["code"], "UNPROCESSABLE_ENTITY");

    let get_response = app
        .oneshot(get_with_token(APP_OPERATIONS_MODEL_PATH, &token))
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    let get_json = body_json(get_response).await;
    assert_eq!(
        get_json["data"]["setting"],
        json!({
            "mode": "fixed",
            "provider_id": "app-operations-provider",
            "model_id": "model-a"
        })
    );
    assert_eq!(get_json["data"]["health"], "ready");
}
