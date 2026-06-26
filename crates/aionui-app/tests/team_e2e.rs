mod common;

use aionui_db::{IConversationRepository, MessagePageDirection, MessagePageParams};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, build_app, build_app_with_mock_agents, delete_with_token, get_with_token, json_with_token,
    setup_and_login,
};

const DEFAULT_TEAM_ASSISTANT_ID: &str = "team-e2e-assistant";

fn team_agent(name: &str, role: &str) -> serde_json::Value {
    json!({
        "name": name,
        "role": role,
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    })
}

fn two_agent_body() -> serde_json::Value {
    json!({
        "name": "Alpha",
        "agents": [
            team_agent("Lead", "lead"),
            team_agent("Worker", "teammate")
        ]
    })
}

async fn ensure_default_team_assistant(app: &mut axum::Router, token: &str, csrf: &str) {
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": DEFAULT_TEAM_ASSISTANT_ID,
            "name": "Team E2E Assistant",
            "agent_id": "2d23ff1c"
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::CONFLICT,
        "expected team assistant seed to be created or already exist, got {}",
        resp.status()
    );
}

async fn create_team(app: &mut axum::Router, token: &str, csrf: &str) -> serde_json::Value {
    ensure_default_team_assistant(app, token, csrf).await;
    let req = json_with_token("POST", "/api/teams", two_agent_body(), token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    json["data"].clone()
}

// ===========================================================================
// §1 Team CRUD (TC-*, TL-*, TG-*, TD-*, TR-*)
// ===========================================================================

// TC-1: Create team with multiple assistants
#[tokio::test]
async fn tc1_create_team_with_multiple_agents() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    assert_eq!(data["name"], "Alpha");
    assert_eq!(data["assistants"].as_array().unwrap().len(), 2);
    assert_eq!(data["assistants"][0]["role"], "lead");
    assert_eq!(data["assistants"][1]["role"], "teammate");
    assert!(data["leader_assistant_id"].is_string());
    assert_eq!(data["leader_assistant_id"], data["assistants"][0]["slot_id"]);
}

// TC-2: Create single assistant team
#[tokio::test]
async fn tc2_create_single_agent_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "Solo",
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn tc_create_team_rejects_existing_agent_conversation_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "No Adoption",
        "agents": [
            {
                "name": "Lead",
                "role": "lead",
                "model": "claude",
                "assistant_id": DEFAULT_TEAM_ASSISTANT_ID,
                "conversation_id": "solo-conv-1"
            }
        ]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
    assert_eq!(json["code"], "BAD_REQUEST");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("existing conversations are no longer supported")
    );
}

// TC-3: Each assistant has a conversation
#[tokio::test]
async fn tc3_each_agent_has_conversation_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    for agent in data["assistants"].as_array().unwrap() {
        assert!(agent["conversation_id"].is_string());
        assert!(!agent["conversation_id"].as_str().unwrap().is_empty());
    }
    assert_ne!(
        data["assistants"][0]["conversation_id"],
        data["assistants"][1]["conversation_id"]
    );
}

#[tokio::test]
async fn tc3b_create_team_writes_legacy_extra_shape() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let repo = aionui_db::SqliteConversationRepository::new(services.database.pool().clone());
    let row = repo.get(conversation_id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();

    assert_eq!(extra["teamId"], data["id"]);
    assert!(extra["slot_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(extra["role"], "lead");
    assert_eq!(extra["backend"], "claude");
    assert_eq!(extra["session_mode"], "bypassPermissions");
    assert_eq!(extra["current_model_id"], "claude");
}

// TC-4: First assistant defaults to lead
#[tokio::test]
async fn tc4_first_agent_is_lead() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "T",
        "agents": [
            team_agent("A", "teammate"),
            team_agent("B", "teammate")
        ]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"][0]["role"], "lead");
    assert_eq!(
        json["data"]["leader_assistant_id"],
        json["data"]["assistants"][0]["slot_id"]
    );
}

// TC-5: Empty agents returns 400
#[tokio::test]
async fn tc5_empty_agents_returns_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({ "name": "Empty", "agents": [] });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// TC-6: Missing name returns 400
#[tokio::test]
async fn tc6_missing_name_returns_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &token, &csrf).await;

    let body = json!({ "agents": [json!({
        "name": "L",
        "role": "lead",
        "model": "c",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    })] });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tc6b_workspace_with_whitespace_segment_is_accepted() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &token, &csrf).await;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("Archive ");
    std::fs::create_dir_all(&workspace).unwrap();

    let body = json!({
        "name": "Alpha",
        "workspace": workspace.to_string_lossy(),
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn tc6c_create_team_rejects_missing_workspace_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &token, &csrf).await;
    let missing_workspace =
        std::env::temp_dir().join(format!("aionui-team-missing-{}", aionui_common::generate_short_id()));

    let body = json!({
        "name": "Alpha",
        "workspace": missing_workspace.to_string_lossy(),
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_UNAVAILABLE");
    assert_eq!(json["details"]["operation"], "create");
    assert_eq!(
        json["details"]["workspace_path"],
        missing_workspace.to_string_lossy().to_string()
    );

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(
        json["data"].as_array().unwrap().is_empty(),
        "invalid team should not be persisted"
    );
}

// TC-7: Unauthenticated returns 401
#[tokio::test]
async fn tc7_unauthenticated_returns_401() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/teams")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

// TL-1: Empty team list
#[tokio::test]
async fn tl1_empty_team_list() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}

// TL-2: List multiple teams
#[tokio::test]
async fn tl2_list_multiple_teams() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    create_team(&mut app, &token, &csrf).await;
    ensure_default_team_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "Beta",
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn team_api_rejects_cross_user_access() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "alice", "StrongP@ss2").await;

    let data = create_team(&mut app, &owner_token, &owner_csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = get_with_token("/api/teams", &other_token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());

    let req = get_with_token(&format!("/api/teams/{team_id}"), &other_token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let forbidden_requests = [
        json_with_token(
            "PATCH",
            &format!("/api/teams/{team_id}/name"),
            json!({ "name": "Nope" }),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/messages"),
            json!({ "content": "Nope" }),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/agents/{slot_id}/messages"),
            json!({ "content": "Nope" }),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/session"),
            json!({}),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "DELETE",
            &format!("/api/teams/{team_id}/session"),
            json!({}),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/session-mode"),
            json!({ "mode": "auto" }),
            &other_token,
            &other_csrf,
        ),
    ];

    for req in forbidden_requests {
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn pause_team_slot_endpoint_requires_owned_team_and_active_run() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let lead_slot_id = data["assistants"][0]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/runs/not-a-run/agents/{lead_slot_id}/pause"),
        json!({"reason": "user stopped"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(body["success"].as_bool().is_some_and(|success| !success));
}

// TL-3: Each team contains full assistants info
#[tokio::test]
async fn tl3_teams_contain_full_agent_info() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    create_team(&mut app, &token, &csrf).await;

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let teams = json["data"].as_array().unwrap();
    let agent = &teams[0]["assistants"][0];
    assert!(agent["slot_id"].is_string());
    assert!(agent["name"].is_string());
    assert!(agent["role"].is_string());
    assert!(agent["conversation_id"].is_string());
    assert!(agent["backend"].is_string());
    assert!(agent["model"].is_string());
}

// TG-1: Get existing team
#[tokio::test]
async fn tg1_get_existing_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], team_id);
    assert_eq!(json["data"]["name"], "Alpha");
}

// TG-2: Get nonexistent team returns 404
#[tokio::test]
async fn tg2_get_nonexistent_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/teams/nonexistent", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// TD-1: Delete existing team
#[tokio::test]
async fn td1_delete_existing_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// TD-2: Delete then list confirms removal
#[tokio::test]
async fn td2_delete_then_list_empty() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}

// TD-6: Delete nonexistent team returns 404
#[tokio::test]
async fn td6_delete_nonexistent_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token("/api/teams/nonexistent", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// TR-1: Rename existing team
#[tokio::test]
async fn tr1_rename_existing_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/name"),
        json!({ "name": "New Name" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// TR-2: Rename then get confirms new name
#[tokio::test]
async fn tr2_rename_then_get_confirms_new_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/name"),
        json!({ "name": "New Name" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "New Name");
}

// TR-4: Rename nonexistent team returns 404
#[tokio::test]
async fn tr4_rename_nonexistent_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PATCH",
        "/api/teams/nonexistent/name",
        json!({ "name": "X" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// §2 Agent Management (AA-*, AR-*, AN-*)
// ===========================================================================

// AA-1: Add agent to team
#[tokio::test]
async fn aa1_add_agent_to_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let body = json!({
        "name": "New Agent",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "New Agent");
    assert!(json["data"]["conversation_id"].is_string());
}

// AA-2: After adding, agent count increases
#[tokio::test]
async fn aa2_add_agent_increases_count() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let body = json!({
        "name": "X",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 3);
}

// AA-4: Add agent to nonexistent team returns 404
#[tokio::test]
async fn aa4_add_agent_nonexistent_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "X",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", "/api/teams/nonexistent/agents", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// AA-5: Missing required fields returns 400
#[tokio::test]
async fn aa5_add_agent_missing_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let body = json!({ "role": "teammate" });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// AR-1: Remove agent from team
#[tokio::test]
async fn ar1_remove_agent_from_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/{slot_id}"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// AR-2: After removal, agent not in team
#[tokio::test]
async fn ar2_after_removal_agent_gone() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/{slot_id}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let assistants = json["data"]["assistants"].as_array().unwrap();
    assert_eq!(assistants.len(), 1);
    assert!(assistants.iter().all(|a| a["slot_id"] != slot_id));
}

// AR-4: Remove nonexistent agent returns 404
#[tokio::test]
async fn ar4_remove_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/nonexistent"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// AN-1: Rename agent
#[tokio::test]
async fn an1_rename_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/name"),
        json!({ "name": "Senior Worker" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// AN-2: Rename then get confirms new name
#[tokio::test]
async fn an2_rename_then_get_confirms_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/name"),
        json!({ "name": "Senior Worker" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let assistants = json["data"]["assistants"].as_array().unwrap();
    let agent = assistants.iter().find(|a| a["slot_id"] == slot_id).unwrap();
    assert_eq!(agent["name"], "Senior Worker");
}

// AN-3: Rename nonexistent agent returns 404
#[tokio::test]
async fn an3_rename_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/nonexistent/name"),
        json!({ "name": "X" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// §3 Session Management (ES-*, SS-*)
// ===========================================================================

// ES-1: Ensure session
#[tokio::test]
async fn es1_ensure_session() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ES-2: Ensure session is idempotent
#[tokio::test]
async fn es2_ensure_session_idempotent() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ES-3: Ensure session for nonexistent team returns 404
#[tokio::test]
async fn es3_ensure_session_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/teams/nonexistent/session", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// SS-1: Stop session
#[tokio::test]
async fn ss1_stop_session() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// SS-3: Stop session without active is noop
#[tokio::test]
async fn ss3_stop_session_noop() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// §4 Message sending (SM-*, SA-*)
// ===========================================================================

// SM-1: Send message with active session
#[tokio::test]
async fn sm1_send_message_with_session() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    // Start session first
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "Hello team" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn sm1b_team_send_persists_user_bubble_through_projection_adapter() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let lead_conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "Hello through adapter" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let repo = aionui_db::SqliteConversationRepository::new(services.database.pool().clone());
    let messages = repo
        .list_messages_page(
            lead_conversation_id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert!(messages.items.iter().any(|row| {
        row.position.as_deref() == Some("right")
            && row.status.as_deref() == Some("finish")
            && row.content.contains("Hello through adapter")
    }));
}

#[tokio::test]
async fn sm1c_team_owned_conversation_regular_send_is_forbidden() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/messages"),
        json!({ "content": "must go through team api" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "FORBIDDEN");
    assert_eq!(body["error"], "Forbidden.");
}

#[tokio::test]
async fn sm1d_team_send_rejects_missing_csrf() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/teams/{team_id}/messages"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"content":"x"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// SM-4: Send message without session returns 404
#[tokio::test]
async fn sm4_send_message_no_session() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/teams/nonexistent/messages",
        json!({ "content": "Hello" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// SM-5: Missing content returns 400
#[tokio::test]
async fn sm5_send_message_missing_content() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SA-1: Send message to specific agent
#[tokio::test]
async fn sa1_send_message_to_agent() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    // Start session first
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/agents/{slot_id}/messages"),
        json!({ "content": "Do this" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// §5 Full lifecycle
// ===========================================================================

// Full CRUD lifecycle
#[tokio::test]
async fn full_team_lifecycle() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create
    let data = create_team(&mut app, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    assert_eq!(data["assistants"].as_array().unwrap().len(), 2);

    // Add agent
    let body = json!({
        "name": "Helper",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let added = body_json(resp).await;
    let new_slot = added["data"]["slot_id"].as_str().unwrap().to_owned();

    // Verify 3 assistants
    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 3);

    // Rename team
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/name"),
        json!({ "name": "Renamed" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Rename agent
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{new_slot}/name"),
        json!({ "name": "Senior Helper" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Ensure session
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Send message
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "Hello" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Stop session
    let req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    // Remove added agent
    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/{new_slot}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    // Verify 2 assistants remain
    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 2);
    assert_eq!(json["data"]["name"], "Renamed");

    // Delete team
    let req = delete_with_token(&format!("/api/teams/{team_id}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    // Verify empty
    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}
