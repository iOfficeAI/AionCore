//! Route-level contract: runtime-token auth, the team-caller guard on EVERY
//! subcommand, and the CLI envelope's error codes.

mod common;

use aionui_ai_agent::RuntimeTokenScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{Ctx, USER, setup};
use tower::ServiceExt;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

fn router(ctx: &Ctx) -> Router {
    aionui_session_message::session_message_routes(ctx.router_state())
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A helper token for (user, conversation), exactly as `ConversationService`
/// issues one for every conversation.
fn mint_token(ctx: &Ctx, user_id: &str, conversation_id: &str) -> String {
    ctx.runtime_token_service
        .issue(
            user_id,
            conversation_id,
            aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token
}

fn send_request(user_id: &str, conversation_id: &str, token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/runtime/session-messages/send")
        .header("content-type", "application/json")
        .header(HEADER_USER_ID, user_id)
        .header(HEADER_CONVERSATION_ID, conversation_id)
        .header(HEADER_RUNTIME_TOKEN, token)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn targets_request(user_id: &str, conversation_id: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/api/runtime/session-messages/targets")
        .header(HEADER_USER_ID, user_id)
        .header(HEADER_CONVERSATION_ID, conversation_id)
        .header(HEADER_RUNTIME_TOKEN, token)
        .body(Body::empty())
        .unwrap()
}

// ── Auth ────────────────────────────────────────────────────────────

#[tokio::test]
async fn send_without_a_runtime_token_is_401() {
    let ctx = setup().await;
    let response = router(&ctx)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime/session-messages/send")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"to":"conv_1","message":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let envelope = json_body(response).await;
    assert_eq!(envelope["success"], serde_json::json!(false));
    assert_eq!(envelope["error"]["code"], serde_json::json!("runtime_auth_failed"));
}

#[tokio::test]
async fn targets_without_a_runtime_token_is_401() {
    let ctx = setup().await;
    let response = router(&ctx)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/runtime/session-messages/targets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(response).await["error"]["code"],
        serde_json::json!("runtime_auth_failed")
    );
}

#[tokio::test]
async fn a_forged_user_header_cannot_deliver_for_another_user() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    // Token is minted for (user_1, conv_a); claiming to be user_2 must fail
    // token validation, which binds the (token, user, conversation) triple.
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(send_request(
            "user_2",
            "conv_a",
            &token,
            r#"{"to":"conv_b","message":"hi"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_bound_to_another_conversation_is_rejected() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(send_request(
            USER,
            "conv_b",
            &token,
            r#"{"to":"conv_a","message":"hi"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── Team caller ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_team_sender_gets_403_on_every_session_subcommand_not_just_send() {
    // spec §6.2: the team check must cover the whole `session` surface.
    let ctx = setup().await;
    ctx.create_team_conversation("conv_team", "team_1").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    let token = mint_token(&ctx, USER, "conv_team");

    let send = router(&ctx)
        .oneshot(send_request(
            USER,
            "conv_team",
            &token,
            r#"{"to":"conv_b","message":"hi"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(send.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(send).await["error"]["code"],
        serde_json::json!("sender_is_team")
    );

    let targets = router(&ctx)
        .oneshot(targets_request(USER, "conv_team", &token))
        .await
        .unwrap();
    assert_eq!(targets.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(targets).await["error"]["code"],
        serde_json::json!("sender_is_team")
    );
}

// ── Feature toggle ──────────────────────────────────────────────────

#[tokio::test]
async fn targets_returns_403_when_the_feature_is_disabled() {
    // Handing the agent a list it cannot act on only wastes a round trip.
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.disable_feature(USER).await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(targets_request(USER, "conv_a", &token))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(response).await["error"]["code"],
        serde_json::json!("feature_disabled")
    );
}

#[tokio::test]
async fn send_returns_403_when_the_feature_is_disabled() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    ctx.disable_feature(USER).await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(send_request(
            USER,
            "conv_a",
            &token,
            r#"{"to":"conv_b","message":"hi"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(response).await["error"]["code"],
        serde_json::json!("feature_disabled")
    );
}

// ── The `@@` picker's outlet shares the gate ────────────────────────
//
// `mentionable` and `targets` are one service function behind two auth
// channels. The front-end hides `@@` while the switch is off, but if the two
// routes' POLICY diverges then a direct call still answers a question the
// master switch was supposed to have closed.

fn mentionable_request(current: Option<&str>) -> Request<Body> {
    let uri = match current {
        Some(id) => format!("/api/session-messages/mentionable?current_conversation_id={id}"),
        None => "/api/session-messages/mentionable".to_owned(),
    };
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

/// The user-auth router expects `CurrentUser` from the auth middleware, which
/// is not in front of the router under test — inject it directly.
fn user_router(ctx: &Ctx) -> Router {
    aionui_session_message::session_message_user_routes(ctx.router_state()).layer(axum::Extension(
        aionui_auth::CurrentUser {
            id: USER.to_owned(),
            username: USER.to_owned(),
            user_type: aionui_db::UserType::Local,
            status: aionui_db::UserStatus::Active,
        },
    ))
}

#[tokio::test]
async fn mentionable_lists_targets_while_the_feature_is_on() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_plain", "plain", "/w/a").await;

    let response = user_router(&ctx)
        .oneshot(mentionable_request(Some("conv_a")))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let ids: Vec<&str> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["conv_plain"]);
}

#[tokio::test]
async fn mentionable_is_refused_when_the_feature_is_disabled() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_plain", "plain", "/w/a").await;
    ctx.disable_feature(USER).await;

    let response = user_router(&ctx)
        .oneshot(mentionable_request(Some("conv_a")))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mentionable_is_refused_for_a_team_caller() {
    let ctx = setup().await;
    ctx.create_team_conversation("conv_team", "team_1").await;
    ctx.create_conversation("conv_plain", "plain", "/w/a").await;

    let response = user_router(&ctx)
        .oneshot(mentionable_request(Some("conv_team")))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// The picker's `current_conversation_id` is optional, so an absent one must
/// still reach the list rather than being read as a missing sender row (which
/// would surface as a 409 and make the picker look broken).
#[tokio::test]
async fn mentionable_without_a_current_conversation_still_lists() {
    let ctx = setup().await;
    ctx.create_conversation("conv_plain", "plain", "/w/a").await;

    let response = user_router(&ctx).oneshot(mentionable_request(None)).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn mentionable_without_a_current_conversation_is_still_gated_by_the_switch() {
    let ctx = setup().await;
    ctx.create_conversation("conv_plain", "plain", "/w/a").await;
    ctx.disable_feature(USER).await;

    let response = user_router(&ctx).oneshot(mentionable_request(None)).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ── targets filtering ───────────────────────────────────────────────

#[tokio::test]
async fn targets_never_includes_a_team_conversation() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_plain", "plain", "/w/a").await;
    ctx.create_team_conversation("conv_team", "team_1").await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(targets_request(USER, "conv_a", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    let ids: Vec<&str> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"conv_plain"), "{ids:?}");
    assert!(!ids.contains(&"conv_team"), "{ids:?}");
    assert!(!ids.contains(&"conv_a"), "the caller itself must be excluded: {ids:?}");
}

#[tokio::test]
async fn targets_never_includes_another_users_conversation() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation_for(common::OTHER_USER, "conv_theirs", "theirs", "/w/b")
        .await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(targets_request(USER, "conv_a", &token))
        .await
        .unwrap();

    let body = json_body(response).await;
    let ids: Vec<&str> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"conv_theirs"), "{ids:?}");
}

// ── Envelope shape ──────────────────────────────────────────────────

#[tokio::test]
async fn a_successful_send_carries_the_envelope_and_the_command_name() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    ctx.create_conversation("conv_b", "B", "/w/a").await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(send_request(
            USER,
            "conv_a",
            &token,
            r#"{"to":"conv_b","message":"hi"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["success"], serde_json::json!(true));
    assert_eq!(body["data"]["status"], serde_json::json!("delivered"));
    assert_eq!(body["data"]["to"], serde_json::json!("conv_b"));
    assert_eq!(body["meta"]["schema_version"], serde_json::json!(1));
    assert_eq!(body["meta"]["command"], serde_json::json!("session send-message"));
    assert!(body.get("error").is_none(), "{body}");
}

#[tokio::test]
async fn a_missing_target_maps_to_404_with_the_target_not_found_code() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(send_request(
            USER,
            "conv_a",
            &token,
            r#"{"to":"conv_missing","message":"hi"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], serde_json::json!("target_not_found"));
    assert!(body.get("data").is_none(), "{body}");
}

#[tokio::test]
async fn a_broadcast_attempt_maps_to_400_schema_validation_failed() {
    let ctx = setup().await;
    ctx.create_conversation("conv_a", "A", "/w/a").await;
    let token = mint_token(&ctx, USER, "conv_a");

    let response = router(&ctx)
        .oneshot(send_request(USER, "conv_a", &token, r#"{"to":"*","message":"hi"}"#))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await["error"]["code"],
        serde_json::json!("schema_validation_failed")
    );
}
