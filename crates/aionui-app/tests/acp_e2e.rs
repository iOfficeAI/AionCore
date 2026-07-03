//! E2E integration tests for ACP management routes.
//!
//! Tests cover: agents list, legacy agents/refresh removal, agents/test,
//! and session-bound routes (mode/model).

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use aionui_db::{
    IAgentMetadataRepository, SqliteAgentMetadataRepository, UpdateAgentAvailabilitySnapshotParams,
    UpsertAgentMetadataParams,
};

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

// ── Global ACP routes ────────────────────────────────────────────

#[tokio::test]
async fn management_list_returns_array() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert!(body["data"].is_array());
    let agents = body["data"].as_array().unwrap();
    assert!(agents.iter().any(|a| a["agent_type"] == "aionrs"));
}

#[tokio::test]
async fn legacy_refresh_agents_endpoint_is_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = json_with_token("POST", "/api/agents/refresh", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_custom_agent_nonexistent_command() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    // Endpoint was renamed from /api/agents/test to /api/agents/custom/try-connect
    // when the custom-agent CRUD routes were introduced.  The new endpoint always
    // returns HTTP 200 and encodes failure in the JSON body (step = "fail_cli" or
    // "fail_acp"), so we assert on the body rather than the HTTP status.
    let req = json_with_token(
        "POST",
        "/api/agents/custom/try-connect",
        json!({ "command": "/nonexistent/path/to/agent" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = common::body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["step"], "fail_cli");
}

#[tokio::test]
async fn management_list_includes_missing_custom_agents() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let repo: std::sync::Arc<dyn IAgentMetadataRepository> =
        std::sync::Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone()));
    repo.upsert(&UpsertAgentMetadataParams {
        id: "custom-missing-agent",
        icon: None,
        name: "Missing Custom Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude"),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some(r#"{"binary_name":"aionui-missing-agent-binary"}"#),
        enabled: true,
        command: Some("aionui-missing-agent-binary"),
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 1500,
    })
    .await
    .unwrap();
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let rows = body["data"].as_array().expect("data should be an array");
    let row = rows
        .iter()
        .find(|item| item["id"].as_str() == Some("custom-missing-agent"))
        .expect("management list should include missing custom agent");
    assert_eq!(row["status"], "missing");
}

#[tokio::test]
async fn management_list_marks_rows_with_unavailable_snapshot() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let repo: std::sync::Arc<dyn IAgentMetadataRepository> =
        std::sync::Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone()));
    repo.upsert(&UpsertAgentMetadataParams {
        id: "custom-unavailable-agent",
        icon: None,
        name: "Unavailable Custom Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude"),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some(r#"{"binary_name":"cargo"}"#),
        enabled: true,
        command: Some("cargo"),
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 1500,
    })
    .await
    .unwrap();
    repo.update_availability_snapshot(
        "custom-unavailable-agent",
        &UpdateAgentAvailabilitySnapshotParams {
            last_check_status: Some("offline"),
            last_check_kind: Some("scheduled"),
            last_check_error_code: Some("acp_init_failed"),
            last_check_error_message: Some("Synthetic unavailable snapshot"),
            last_check_guidance: None,
            last_check_latency_ms: Some(42),
            last_check_at: Some(1_750_000_000_000),
            last_success_at: None,
            last_failure_at: Some(1_750_000_000_000),
        },
    )
    .await
    .unwrap();
    services.agent_registry.hydrate().await.unwrap();

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let rows = body["data"].as_array().expect("data should be an array");
    let row = rows
        .iter()
        .find(|item| item["id"].as_str() == Some("custom-unavailable-agent"))
        .expect("management list should include unavailable rows");
    assert_eq!(row["status"], "offline");
}

#[tokio::test]
async fn legacy_agents_endpoint_is_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/agents", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn health_check_by_id_returns_missing_status_for_uninstalled_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let repo: std::sync::Arc<dyn IAgentMetadataRepository> =
        std::sync::Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone()));
    repo.upsert(&UpsertAgentMetadataParams {
        id: "custom-missing-agent",
        icon: None,
        name: "Missing Custom Agent",
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude"),
        agent_type: "acp",
        agent_source: "custom",
        agent_source_info: Some(r#"{"binary_name":"aionui-missing-agent-binary"}"#),
        enabled: true,
        command: Some("aionui-missing-agent-binary"),
        args: Some("[]"),
        env: Some("[]"),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 1500,
    })
    .await
    .unwrap();
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let req = json_with_token(
        "POST",
        "/api/agents/custom-missing-agent/health-check",
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["data"]["id"], "custom-missing-agent");
    assert_eq!(body["data"]["status"], "missing");
}

// ── Session-bound ACP routes (no active task → 404) ──────────────

#[tokio::test]
async fn get_mode_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/conversations/nonexistent/mode", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_mode_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = json_with_token(
        "PUT",
        "/api/conversations/nonexistent/mode",
        json!({ "mode": "code" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_model_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/conversations/nonexistent/model", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_model_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = json_with_token(
        "PUT",
        "/api/conversations/nonexistent/model",
        json!({ "model_id": "claude-sonnet-4" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
