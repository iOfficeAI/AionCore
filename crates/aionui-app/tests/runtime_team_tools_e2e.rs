mod common;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{body_json, build_app};

const USER_ID: &str = "system_default_user";
const CONVERSATION_ID: &str = "conv-runtime-team-tools";

fn context_request(user_id: &str, conversation_id: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/runtime/team-tools/context")
        .header("x-aionui-user-id", user_id)
        .header("x-aionui-conversation-id", conversation_id);
    if let Some(token) = token {
        builder = builder.header("x-aionui-runtime-token", token);
    }
    builder.body(Body::empty()).unwrap()
}

fn call_request(user_id: &str, conversation_id: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/runtime/team-tools/call")
        .header("content-type", "application/json")
        .header("x-aionui-user-id", user_id)
        .header("x-aionui-conversation-id", conversation_id);
    if let Some(token) = token {
        builder = builder.header("x-aionui-runtime-token", token);
    }
    builder
        .body(Body::from(
            serde_json::to_vec(&json!({
                "tool": "team_members",
                "arguments": {}
            }))
            .unwrap(),
        ))
        .unwrap()
}

fn assert_runtime_auth_failed(json: &Value) {
    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["code"], "runtime_auth_failed");
}

#[tokio::test]
async fn runtime_team_tools_reject_missing_runtime_token() {
    let (app, _services) = build_app().await;

    let resp = app
        .oneshot(context_request(USER_ID, CONVERSATION_ID, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_runtime_auth_failed(&json);
}

#[tokio::test]
async fn runtime_team_tools_reject_forged_user_header() {
    let (app, services) = build_app().await;
    let issue = services.runtime_token_service.issue(
        USER_ID,
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::TeamContext],
    );

    let resp = app
        .oneshot(context_request("other-user", CONVERSATION_ID, Some(&issue.token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_runtime_auth_failed(&json);
}

#[tokio::test]
async fn runtime_team_tools_reject_forged_conversation_header() {
    let (app, services) = build_app().await;
    let issue = services.runtime_token_service.issue(
        USER_ID,
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::TeamContext],
    );

    let resp = app
        .oneshot(context_request(USER_ID, "other-conversation", Some(&issue.token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_runtime_auth_failed(&json);
}

#[tokio::test]
async fn runtime_team_tools_reject_wrong_scope_for_call() {
    let (app, services) = build_app().await;
    let issue = services.runtime_token_service.issue(
        USER_ID,
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::TeamContext],
    );

    let resp = app
        .oneshot(call_request(USER_ID, CONVERSATION_ID, Some(&issue.token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_runtime_auth_failed(&json);
}

#[tokio::test]
async fn runtime_team_tools_accept_matching_runtime_token_before_service_lookup() {
    let (app, services) = build_app().await;
    let issue = services.runtime_token_service.issue(
        USER_ID,
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::TeamContext],
    );

    let resp = app
        .oneshot(context_request(USER_ID, CONVERSATION_ID, Some(&issue.token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["code"], "conversation_not_found");
}
