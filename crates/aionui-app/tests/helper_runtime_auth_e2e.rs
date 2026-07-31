//! AionPro-mode auth for the in-conversation helper CLI (`aioncore config`).
//!
//! Regression for the 401 that broke `/cron` inside agent conversations: the
//! helper cannot carry a JWT, so it authenticates through the runtime-token
//! channel with a conversation-helper-scoped token minted by the backend.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};

const CONVERSATION_ID: &str = "conv-helper-auth";

async fn build_aionpro_app() -> (axum::Router, aionui_app::AppServices, String) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");

    let provision_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/helper-pro-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Helper Pro User"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(provision_response.status(), StatusCode::OK);
    let body = to_bytes(provision_response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let user_id = json["data"]["user_id"].as_str().unwrap().to_owned();

    (router, services, user_id)
}

fn helper_get(path: &str, user_id: &str, conversation_id: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-aionui-user-id", user_id)
        .header("x-aionui-conversation-id", conversation_id);
    if let Some(token) = token {
        builder = builder.header("x-aionui-runtime-token", token);
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn helper_token_channel_passes_auth_and_reaches_cron_domain() {
    let (app, services, user_id) = build_aionpro_app().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );

    let resp = app
        .oneshot(helper_get(
            "/api/internal/conversation-cron/list",
            &user_id,
            CONVERSATION_ID,
            Some(&issue.token),
        ))
        .await
        .unwrap();

    // Auth + CSRF accepted the token channel; the request reached the cron
    // domain, whose active-turn precondition rejects it (this e2e runs no
    // agent turn). The 0724-1 regression failed earlier, with a 401.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert!(
        json["error"].as_str().unwrap().contains("active conversation turn"),
        "expected cron-domain active-turn rejection, got: {json}"
    );

    services.database.close().await;
}

#[tokio::test]
async fn helper_token_channel_reads_owned_conversation() {
    let (app, services, user_id) = build_aionpro_app().await;
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES (?, ?, 'Helper Conversation', 'acp', '{}', 'pending', 1, 1)",
    )
    .bind(CONVERSATION_ID)
    .bind(&user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );

    let resp = app
        .oneshot(helper_get(
            &format!("/api/conversations/{CONVERSATION_ID}"),
            &user_id,
            CONVERSATION_ID,
            Some(&issue.token),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], CONVERSATION_ID);

    services.database.close().await;
}

#[tokio::test]
async fn helper_without_token_is_rejected_in_aionpro_mode() {
    let (app, services, user_id) = build_aionpro_app().await;

    let resp = app
        .oneshot(helper_get(
            "/api/internal/conversation-cron/list",
            &user_id,
            CONVERSATION_ID,
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");

    services.database.close().await;
}

#[tokio::test]
async fn helper_token_bound_to_other_user_is_rejected() {
    let (app, services, user_id) = build_aionpro_app().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );

    let resp = app
        .oneshot(helper_get(
            "/api/internal/conversation-cron/list",
            "forged-user",
            CONVERSATION_ID,
            Some(&issue.token),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "Invalid runtime token");

    services.database.close().await;
}

#[tokio::test]
async fn helper_token_bound_to_other_conversation_is_rejected() {
    let (app, services, user_id) = build_aionpro_app().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        "another-conversation",
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );

    let resp = app
        .oneshot(helper_get(
            "/api/internal/conversation-cron/list",
            &user_id,
            CONVERSATION_ID,
            Some(&issue.token),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "Invalid runtime token");

    services.database.close().await;
}

#[tokio::test]
async fn team_scoped_token_cannot_use_helper_channel() {
    let (app, services, user_id) = build_aionpro_app().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::TeamContext, RuntimeTokenScope::TeamCall],
    );

    let resp = app
        .oneshot(helper_get(
            "/api/internal/conversation-cron/list",
            &user_id,
            CONVERSATION_ID,
            Some(&issue.token),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "Invalid runtime token");

    services.database.close().await;
}

#[tokio::test]
async fn helper_write_request_is_exempt_from_csrf() {
    let (app, services, user_id) = build_aionpro_app().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );

    // Invalid payload on purpose: the request must get PAST auth + CSRF and be
    // judged by the handler itself (a 4xx that is NOT CSRF/401 proves the
    // channel works for state-changing calls without a CSRF cookie).
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/internal/conversation-cron/create")
                .header("content-type", "application/json")
                .header("x-aionui-user-id", &user_id)
                .header("x-aionui-conversation-id", CONVERSATION_ID)
                .header("x-aionui-runtime-token", &issue.token)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);

    services.database.close().await;
}
