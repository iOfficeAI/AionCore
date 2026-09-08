//! The agent-facing create route is reachable through the REAL app router —
//! mounted outside auth/CSRF, wired to the shared `ConversationService` (so it
//! sees the provider/assistant repos `AppServices` injects).

mod common;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, build_app};
use tower::ServiceExt;

const USER: &str = "system_default_user";
const CALLER: &str = "conv-agent-create-caller";

async fn insert_caller(services: &aionui_app::AppServices, id: &str, extra: &str) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES (?, ?, 'caller', 'acp', ?, 'finished', 1, 1)",
    )
    .bind(id)
    .bind(USER)
    .bind(extra)
    .execute(services.database.pool())
    .await
    .unwrap();
}

fn create_request(conversation_id: &str, token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/runtime/conversations/create")
        .header("content-type", "application/json")
        .header("x-aionui-user-id", USER)
        .header("x-aionui-conversation-id", conversation_id)
        .header("x-aionui-runtime-token", token)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn runtime_create_is_mounted_and_creates_without_a_csrf_token() {
    let (app, services) = build_app().await;
    let workspace = std::env::temp_dir().join("aionui-app-runtime-create-e2e");
    std::fs::create_dir_all(&workspace).unwrap();
    insert_caller(
        &services,
        CALLER,
        &serde_json::json!({ "workspace": workspace, "backend": "claude" }).to_string(),
    )
    .await;
    let token = services
        .runtime_token_service
        .issue(
            USER,
            CALLER,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;

    let response = app
        .clone()
        .oneshot(create_request(CALLER, &token, r#"{"name":"子任务"}"#))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let envelope = body_json(response).await;
    assert_eq!(envelope["success"], serde_json::json!(true));
    assert_eq!(
        envelope["data"]["workspace"],
        serde_json::json!(workspace.to_string_lossy())
    );
    let new_id = envelope["data"]["id"].as_str().unwrap().to_owned();
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM conversations WHERE id = ? AND user_id = ?")
        .bind(&new_id)
        .bind(USER)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    services.database.close().await;
}

#[tokio::test]
async fn runtime_create_refuses_a_team_caller_through_the_real_router() {
    let (app, services) = build_app().await;
    insert_caller(&services, "conv-team-caller", r#"{"teamId":"team-1"}"#).await;
    let token = services
        .runtime_token_service
        .issue(
            USER,
            "conv-team-caller",
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;

    let response = app
        .oneshot(create_request("conv-team-caller", &token, r#"{"name":"x"}"#))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["error"]["code"],
        serde_json::json!("caller_is_team")
    );
    services.database.close().await;
}

#[tokio::test]
async fn runtime_create_without_a_token_is_401_not_a_redirect_into_user_auth() {
    let (app, services) = build_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime/conversations/create")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_json(response).await["error"]["code"],
        serde_json::json!("runtime_auth_failed")
    );
    services.database.close().await;
}
