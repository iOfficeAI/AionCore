//! Channel integration E2E tests.
//!
//! Covers test-plan §1-5: plugin CRUD, pairing flow, user management,
//! session management, settings sync.

mod common;

use aionui_common::now_ms;
use aionui_db::models::{ChannelConnectionRow, ChannelConversationBindingRow, ChannelUserRow};
use aionui_db::{IChannelRepository, SqliteChannelRepository};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

const OWNER_ID: &str = "system_default_user";

// ===========================================================================
// §1 Plugin management
// ===========================================================================

// PS-1: Get plugins when none exist
#[tokio::test]
async fn get_plugins_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/channel/plugins", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    let data = json["data"].as_array().unwrap();
    assert_eq!(data.len(), 7);
    let types: std::collections::HashSet<_> = data.iter().filter_map(|item| item["type"].as_str()).collect();
    assert_eq!(
        types,
        std::collections::HashSet::from(["telegram", "lark", "dingtalk", "slack", "discord", "weixin", "wecom",])
    );
    assert!(data.iter().all(|item| item["enabled"] == false));
}

// PS-3: Unauthenticated request returns 401
#[tokio::test]
async fn get_plugins_unauthenticated() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/channel/plugins")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

// EP-3: Enable missing pluginId fails
#[tokio::test]
async fn enable_plugin_missing_plugin_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/plugins/enable",
        json!({ "config": {} }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// EP-4: Enable missing config fails
#[tokio::test]
async fn enable_plugin_missing_config() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/plugins/enable",
        json!({ "plugin_id": "telegram" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// EP-5: Enable invalid plugin type returns error in response body
#[tokio::test]
async fn enable_plugin_invalid_type() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/plugins/enable",
        json!({
            "plugin_id": "nonexistent",
            "config": { "credentials": { "token": "x" } }
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = &json["data"];
    assert!(!data["success"].as_bool().unwrap());
    assert!(data["error"].as_str().unwrap().contains("Invalid plugin type"));
}

// DP-3: Disable missing pluginId fails
#[tokio::test]
async fn disable_plugin_missing_plugin_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/channel/plugins/disable", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// DP-2: Disable non-existent plugin returns success=false (not registered)
#[tokio::test]
async fn disable_plugin_not_registered() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/plugins/disable",
        json!({ "plugin_id": "telegram" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    // Plugin was never enabled, so disable returns success=false with error
    assert!(!json["data"]["success"].as_bool().unwrap());
    assert!(json["data"]["error"].as_str().is_some());
}

// TP-4: Test plugin missing pluginId fails
#[tokio::test]
async fn test_plugin_missing_plugin_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/plugins/test",
        json!({ "token": "xxx" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// TP-5: Test plugin missing token fails
#[tokio::test]
async fn test_plugin_missing_token() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/plugins/test",
        json!({ "plugin_id": "telegram" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Seeds the connection a channel user or pairing request attaches to,
/// returning its connection id.
async fn seed_connection(repo: &std::sync::Arc<dyn IChannelRepository>, plugin_key: &str) -> String {
    let now = now_ms();
    let id = format!("conn-{plugin_key}");
    repo.upsert_connection(
        OWNER_ID,
        &ChannelConnectionRow {
            id: id.clone(),
            owner_user_id: OWNER_ID.to_owned(),
            plugin_key: plugin_key.to_owned(),
            name: format!("{plugin_key} bot"),
            enabled: true,
            config: "{}".to_owned(),
            status: None,
            last_connected: None,
            created_at: now,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    id
}

// ===========================================================================
// §2 Pairing management
// ===========================================================================

// PP-1: No pending pairings
#[tokio::test]
async fn get_pairings_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/channel/pairings", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"].as_array().unwrap().is_empty());
}

// AP-6: Approve missing code fails
#[tokio::test]
async fn approve_pairing_missing_code() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/channel/pairings/approve", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// AP-3: Approve non-existent code returns 404
#[tokio::test]
async fn approve_pairing_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/pairings/approve",
        json!({ "code": "000000" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// RP-3: Reject non-existent code returns 404
#[tokio::test]
async fn reject_pairing_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/pairings/reject",
        json!({ "code": "000000" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// §3 User management
// ===========================================================================

// GU-1: No authorized users
#[tokio::test]
async fn get_users_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/channel/users", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"].as_array().unwrap().is_empty());
}

// RU-5: Revoke missing userId fails
#[tokio::test]
async fn revoke_user_missing_user_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/channel/users/revoke", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// RU-4: Revoke non-existent user returns 404
#[tokio::test]
async fn revoke_user_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/users/revoke",
        json!({ "user_id": "nonexistent" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// §4 Session management
// ===========================================================================

// GS-1: No active sessions
#[tokio::test]
async fn get_sessions_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/channel/sessions", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"].as_array().unwrap().is_empty());
}

// GS-2: A populated session response carries the binding fields, and the
// deprecated agent_type/workspace fields are omitted rather than serialized.
#[tokio::test]
async fn get_sessions_returns_binding_without_deprecated_agent_fields() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let repo: std::sync::Arc<dyn IChannelRepository> =
        std::sync::Arc::new(SqliteChannelRepository::new(services.database.pool().clone()));

    let now = now_ms();
    let connection_id = seed_connection(&repo, "telegram").await;
    repo.create_user(
        OWNER_ID,
        &ChannelUserRow {
            id: "cu-sessions".to_owned(),
            owner_user_id: OWNER_ID.to_owned(),
            connection_id,
            platform_user_id: "tg-sessions".to_owned(),
            platform_type: "telegram".to_owned(),
            display_name: Some("Sessions User".to_owned()),
            status: "active".to_owned(),
            revoked_at: None,
            authorized_at: now,
            last_active: None,
        },
    )
    .await
    .unwrap();
    repo.get_or_create_session(
        OWNER_ID,
        "cu-sessions",
        "chat-sessions",
        &ChannelConversationBindingRow {
            id: "cs-sessions".to_owned(),
            owner_user_id: String::new(),
            connection_id: String::new(),
            user_id: "cu-sessions".to_owned(),
            conversation_id: None,
            chat_id: Some("chat-sessions".to_owned()),
            created_at: now,
            last_activity: now,
        },
    )
    .await
    .unwrap();

    let req = get_with_token("/api/channel/sessions", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let sessions = json["data"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session["id"], "cs-sessions");
    assert_eq!(session["user_id"], "cu-sessions");
    assert_eq!(session["chat_id"], "chat-sessions");
    assert!(
        session.get("agent_type").is_none(),
        "deprecated agent_type must be omitted, got: {session}"
    );
    assert!(
        session.get("workspace").is_none(),
        "deprecated workspace must be omitted, got: {session}"
    );
}

// ===========================================================================
// §5 Settings sync
// ===========================================================================

#[tokio::test]
async fn get_channel_settings_defaults_to_generated_aionrs_assistant() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/channel/settings/telegram", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert_eq!(json["data"]["platform"], "telegram");
    // With no explicit binding the platform now falls back to the generated
    // aionrs bare assistant (see channel "default to bare assistant bindings");
    // only the assistant_id is canonical, legacy fields are omitted.
    let assistant_id = json["data"]["assistant"]["assistant_id"]
        .as_str()
        .expect("default channel assistant should be the generated aionrs bare assistant");
    assert!(
        assistant_id.starts_with("bare:"),
        "expected bare assistant id, got {assistant_id}"
    );
    assert!(json["data"]["assistant"]["backend"].is_null());
    assert!(json["data"]["assistant"]["agent_type"].is_null());
    assert!(json["data"]["default_model"].is_null());
}

#[tokio::test]
async fn put_channel_assistant_setting_persists_binding() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PUT",
        "/api/channel/settings/telegram/assistant",
        json!({
            "assistant_id": "bare-claude",
            "name": "Claude",
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = get_with_token("/api/channel/settings/telegram", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(
        json["data"]["assistant"],
        json!({
            "assistant_id": "bare-claude",
            "name": "Claude",
        })
    );
}

#[tokio::test]
async fn put_channel_default_model_setting_persists_model_ref() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PUT",
        "/api/channel/settings/lark/default-model",
        json!({
            "id": "provider-1",
            "use_model": "gemini-2.5-pro",
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = get_with_token("/api/channel/settings/lark", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(
        json["data"]["default_model"],
        json!({
            "id": "provider-1",
            "use_model": "gemini-2.5-pro",
        })
    );
}

#[tokio::test]
async fn put_channel_assistant_setting_clears_active_sessions() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let repo: std::sync::Arc<dyn IChannelRepository> =
        std::sync::Arc::new(SqliteChannelRepository::new(services.database.pool().clone()));

    let now = now_ms();
    let connection_id = seed_connection(&repo, "lark").await;
    repo.create_user(
        OWNER_ID,
        &ChannelUserRow {
            id: "user-channel-assistant".to_owned(),
            owner_user_id: OWNER_ID.to_owned(),
            connection_id,
            platform_user_id: "user-channel-assistant".to_owned(),
            platform_type: "lark".to_owned(),
            display_name: Some("Channel Assistant User".to_owned()),
            status: "active".to_owned(),
            revoked_at: None,
            authorized_at: now,
            last_active: Some(now),
        },
    )
    .await
    .unwrap();
    let new_session = ChannelConversationBindingRow {
        id: "sess-channel-assistant".to_owned(),
        // Owner and connection are derived by the repository from the
        // active channel user; the caller leaves them empty.
        owner_user_id: String::new(),
        connection_id: String::new(),
        user_id: "user-channel-assistant".to_owned(),
        conversation_id: None,
        chat_id: Some("chat-channel-assistant".to_owned()),
        created_at: now,
        last_activity: now,
    };
    repo.get_or_create_session(
        OWNER_ID,
        "user-channel-assistant",
        "chat-channel-assistant",
        &new_session,
    )
    .await
    .unwrap();
    assert_eq!(repo.get_all_sessions(OWNER_ID).await.unwrap().len(), 1);

    let req = json_with_token(
        "PUT",
        "/api/channel/settings/lark/assistant",
        json!({
            "assistant_id": "assistant-1",
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(repo.get_all_sessions(OWNER_ID).await.unwrap().is_empty());
}

#[tokio::test]
async fn put_channel_default_model_setting_clears_active_sessions() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let repo: std::sync::Arc<dyn IChannelRepository> =
        std::sync::Arc::new(SqliteChannelRepository::new(services.database.pool().clone()));

    let now = now_ms();
    let connection_id = seed_connection(&repo, "lark").await;
    repo.create_user(
        OWNER_ID,
        &ChannelUserRow {
            id: "user-channel-model".to_owned(),
            owner_user_id: OWNER_ID.to_owned(),
            connection_id,
            platform_user_id: "user-channel-model".to_owned(),
            platform_type: "lark".to_owned(),
            display_name: Some("Channel Model User".to_owned()),
            status: "active".to_owned(),
            revoked_at: None,
            authorized_at: now,
            last_active: Some(now),
        },
    )
    .await
    .unwrap();
    let new_session = ChannelConversationBindingRow {
        id: "sess-channel-model".to_owned(),
        // Owner and connection are derived by the repository from the
        // active channel user; the caller leaves them empty.
        owner_user_id: String::new(),
        connection_id: String::new(),
        user_id: "user-channel-model".to_owned(),
        conversation_id: None,
        chat_id: Some("chat-channel-model".to_owned()),
        created_at: now,
        last_activity: now,
    };
    repo.get_or_create_session(OWNER_ID, "user-channel-model", "chat-channel-model", &new_session)
        .await
        .unwrap();
    assert_eq!(repo.get_all_sessions(OWNER_ID).await.unwrap().len(), 1);

    let req = json_with_token(
        "PUT",
        "/api/channel/settings/lark/default-model",
        json!({
            "id": "provider-1",
            "use_model": "gemini-2.5-pro",
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(repo.get_all_sessions(OWNER_ID).await.unwrap().is_empty());
}

// SS-1: Sync valid platform clears sessions
#[tokio::test]
async fn sync_settings_valid() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/settings/sync",
        json!({ "platform": "telegram" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"]["success"].as_bool().unwrap());
}

// SS-2: Sync missing platform fails deserialization
#[tokio::test]
async fn sync_settings_missing_platform() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/channel/settings/sync", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SS-3: Sync invalid platform fails validation
#[tokio::test]
async fn sync_settings_invalid_platform() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/channel/settings/sync",
        json!({ "platform": "invalid" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Full pairing → user → session lifecycle
// ===========================================================================

/// Test the complete pairing flow using direct DB access for the parts
/// that normally come from IM platform (pairing request).
#[tokio::test]
async fn pairing_approve_creates_user() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create a pairing request directly via the pairing service
    let pool = services.database.pool().clone();
    let repo: std::sync::Arc<dyn aionui_db::IChannelRepository> =
        std::sync::Arc::new(aionui_db::SqliteChannelRepository::new(pool));
    // The route's pairing service hashes codes with the app's channel key;
    // this one must share it so a code minted here approves over HTTP.
    let pairing_svc = aionui_channel::pairing::PairingService::new(
        repo.clone(),
        services.event_bus.clone(),
        aionui_app::derive_encryption_key(&services.jwt_secret_raw),
    );
    seed_connection(&repo, "telegram").await;

    let code = pairing_svc
        .request_pairing(OWNER_ID, "tg_user_42", "telegram", Some("Alice"))
        .await
        .unwrap();

    // Verify pairing appears in pending list
    let req = get_with_token("/api/channel/pairings", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let pairings = json["data"].as_array().unwrap();
    assert_eq!(pairings.len(), 1);
    // The cold-loaded list exposes the addressable id, never the code.
    let pairing_id = pairings[0]["id"].as_str().unwrap().to_owned();
    assert!(!pairing_id.is_empty());
    assert!(pairings[0].get("code").is_none());
    assert_eq!(pairings[0]["platform_user_id"], "tg_user_42");
    assert_eq!(pairings[0]["platform_type"], "telegram");
    assert_eq!(pairings[0]["display_name"], "Alice");

    // Approve the pairing
    let req = json_with_token(
        "POST",
        "/api/channel/pairings/approve",
        json!({ "code": code }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"]["success"].as_bool().unwrap());

    // Verify user appears in authorized users
    let req = get_with_token("/api/channel/users", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let users = json["data"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["platform_user_id"], "tg_user_42");
    assert_eq!(users[0]["platform_type"], "telegram");
    assert_eq!(users[0]["display_name"], "Alice");
    let user_id = users[0]["id"].as_str().unwrap().to_owned();

    // Verify double-approve fails. A used code is no longer resolvable —
    // the hash lookup only matches pending requests — so replaying it is a
    // 404, while addressing the same request by id reports it as already
    // processed.
    let req = json_with_token(
        "POST",
        "/api/channel/pairings/approve",
        json!({ "code": code }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let req = json_with_token(
        "POST",
        "/api/channel/pairings/approve",
        json!({ "id": pairing_id }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Pairing should no longer appear in pending list
    let req = get_with_token("/api/channel/pairings", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());

    // Revoke the user
    let req = json_with_token(
        "POST",
        "/api/channel/users/revoke",
        json!({ "user_id": user_id }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"]["success"].as_bool().unwrap());

    // Verify user no longer in list
    let req = get_with_token("/api/channel/users", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());

    // Revocation is a soft delete: the authorization history survives.
    let (status, revoked_at): (String, Option<i64>) =
        sqlx::query_as("SELECT status, revoked_at FROM channel_users WHERE id = ?")
            .bind(&user_id)
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    assert_eq!(status, "revoked");
    assert!(revoked_at.is_some());
}

/// Test pairing rejection flow.
#[tokio::test]
async fn pairing_reject_removes_from_pending() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create a pairing request
    let pool = services.database.pool().clone();
    let repo: std::sync::Arc<dyn aionui_db::IChannelRepository> =
        std::sync::Arc::new(aionui_db::SqliteChannelRepository::new(pool));
    let pairing_svc = aionui_channel::pairing::PairingService::new(
        repo.clone(),
        services.event_bus.clone(),
        aionui_app::derive_encryption_key(&services.jwt_secret_raw),
    );
    seed_connection(&repo, "telegram").await;

    let code = pairing_svc
        .request_pairing(OWNER_ID, "tg_user_99", "telegram", None)
        .await
        .unwrap();

    // Reject the pairing
    let req = json_with_token(
        "POST",
        "/api/channel/pairings/reject",
        json!({ "code": code }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"]["success"].as_bool().unwrap());

    // Verify pairing no longer in pending list
    let req = get_with_token("/api/channel/pairings", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());

    // Verify no user was created
    let req = get_with_token("/api/channel/users", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());

    // Verify rejecting the same code again fails: the rejected request is
    // no longer pending, so its code hash resolves to nothing (404).
    let req = json_with_token(
        "POST",
        "/api/channel/pairings/reject",
        json!({ "code": code }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Plugin enable/disable with real telegram factory
// ===========================================================================

/// Enable a Telegram plugin with mock-friendly config, verify status
/// appears in the plugin list, then disable it.
#[tokio::test]
async fn enable_disable_plugin_lifecycle() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Enable Telegram plugin (will fail connecting to real API, but
    // the error is captured in response, not an HTTP error)
    let req = json_with_token(
        "POST",
        "/api/channel/plugins/enable",
        json!({
            "plugin_id": "telegram",
            "config": {
                "credentials": { "token": "000000000:FAKE_TOKEN" },
                "config": { "mode": "polling" }
            }
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The result may be success or failure depending on network —
    // either way, the plugin should appear in the list
    let req = get_with_token("/api/channel/plugins", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let plugins = json["data"].as_array().unwrap();
    assert_eq!(plugins.len(), 7);
    let telegram = plugins
        .iter()
        .find(|plugin| plugin["plugin_id"] == "telegram")
        .expect("telegram plugin should be present");
    assert_eq!(telegram["type"], "telegram");
    assert_eq!(telegram["name"], "Telegram Bot");
    assert_eq!(telegram["enabled"], true);

    // Disable the plugin
    let req = json_with_token(
        "POST",
        "/api/channel/plugins/disable",
        json!({ "plugin_id": "telegram" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"]["success"].as_bool().unwrap());

    // Verify plugin is now disabled
    let req = get_with_token("/api/channel/plugins", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let plugins = json["data"].as_array().unwrap();
    assert_eq!(plugins.len(), 7);
    let telegram = plugins
        .iter()
        .find(|plugin| plugin["plugin_id"] == "telegram")
        .expect("telegram plugin should remain listed after disable");
    assert!(!telegram["enabled"].as_bool().unwrap());
}
