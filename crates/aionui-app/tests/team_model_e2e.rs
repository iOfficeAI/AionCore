mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

const TEAM_ASSISTANT_ID: &str = "team-model-e2e-assistant";
const TEAM_AGENT_ID: &str = "2d23ff1c";

async fn create_team(app: &mut axum::Router, services: &aionui_app::AppServices, token: &str, csrf: &str) -> Value {
    let command = std::env::current_exe()
        .expect("test executable path")
        .to_string_lossy()
        .to_string();
    let source_info = json!({ "binary_name": command }).to_string();
    sqlx::query(
        "UPDATE agent_metadata \
         SET agent_source = 'custom', agent_source_info = ?, command = ?, args = '[]', env = '[]', \
             updated_at = unixepoch('now','subsec') * 1000 \
         WHERE id = ?",
    )
    .bind(&source_info)
    .bind(&command)
    .bind(TEAM_AGENT_ID)
    .execute(services.database.pool())
    .await
    .expect("seed deterministic team agent");
    services
        .agent_registry
        .reload_one(TEAM_AGENT_ID)
        .await
        .expect("reload deterministic team agent");

    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": TEAM_ASSISTANT_ID,
            "name": "Team Model E2E Assistant",
            "agent_id": TEAM_AGENT_ID
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Model Team",
            "agents": [
                {
                    "name": "Lead",
                    "role": "lead",
                    "model": "gpt-5.5",
                    "assistant_id": TEAM_ASSISTANT_ID
                },
                {
                    "name": "Worker",
                    "role": "teammate",
                    "model": "gpt-5.5",
                    "assistant_id": TEAM_ASSISTANT_ID
                }
            ]
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["data"].clone()
}

#[tokio::test]
async fn update_agent_model_persists_the_team_roster_value() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let slot_id = team["assistants"][1]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/model"),
        json!({ "model_id": "gpt-5.6-sol" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = common::get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let body = body_json(resp).await;
    let worker = body["data"]["assistants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["slot_id"] == slot_id)
        .unwrap();
    assert_eq!(worker["model"], "gpt-5.6-sol");
}

#[tokio::test]
async fn opening_a_legacy_team_repairs_roster_and_seed_from_the_confirmed_snapshot() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let conversation_id = team["assistants"][1]["conversation_id"].as_str().unwrap();

    sqlx::query(
        "UPDATE conversation_assistant_snapshots SET resolved_model_id = 'gpt-5.7-confirmed' \
         WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE acp_session SET session_config = json_set(session_config, \
         '$.runtime.current_model_id', 'gpt-5.4-stale') WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();

    let req = common::get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let worker = &body["data"]["assistants"][1];
    assert_eq!(worker["model"], "gpt-5.7-confirmed");

    let extra: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = ?")
        .bind(conversation_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&extra).unwrap()["current_model_id"],
        "gpt-5.7-confirmed"
    );
}

#[tokio::test]
async fn update_agent_model_requires_authentication() {
    let (app, _services) = build_app().await;
    let csrf = "csrf-test";
    let req = axum::http::Request::builder()
        .method("PATCH")
        .uri("/api/teams/team-1/agents/worker-1/model")
        .header("content-type", "application/json")
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(axum::body::Body::from(r#"{"model_id":"gpt-5.6-sol"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn update_agent_model_requires_csrf() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let slot_id = team["assistants"][1]["slot_id"].as_str().unwrap();
    let req = axum::http::Request::builder()
        .method("PATCH")
        .uri(format!("/api/teams/{team_id}/agents/{slot_id}/model"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"model_id":"gpt-5.6-sol"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn update_agent_model_rejects_cross_user_access() {
    let (mut app, services) = build_app().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &owner_token, &owner_csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let slot_id = team["assistants"][1]["slot_id"].as_str().unwrap();
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/model"),
        json!({ "model_id": "gpt-5.6-sol" }),
        &other_token,
        &other_csrf,
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
}

#[tokio::test]
async fn update_agent_model_rejects_empty_models() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let slot_id = team["assistants"][1]["slot_id"].as_str().unwrap();
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/model"),
        json!({ "model_id": "  " }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn update_agent_model_rejects_missing_agents() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/missing/model"),
        json!({ "model_id": "gpt-5.6-sol" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
}

#[tokio::test]
async fn set_team_config_option_requires_authentication() {
    let (app, _services) = build_app().await;
    let csrf = "csrf-test";
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/teams/team-1/conversations/conversation-1/config-options/model")
        .header("content-type", "application/json")
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(axum::body::Body::from(r#"{"value":"gpt-5.6-sol"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn set_team_config_option_requires_csrf() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let conversation_id = team["assistants"][1]["conversation_id"].as_str().unwrap();
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/teams/{team_id}/conversations/{conversation_id}/config-options/model"
        ))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"value":"gpt-5.6-sol"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn set_team_config_option_rejects_cross_user_access() {
    let (mut app, services) = build_app().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let team = create_team(&mut app, &services, &owner_token, &owner_csrf).await;
    let team_id = team["id"].as_str().unwrap();
    let conversation_id = team["assistants"][1]["conversation_id"].as_str().unwrap();
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let req = json_with_token(
        "PUT",
        &format!("/api/teams/{team_id}/conversations/{conversation_id}/config-options/model"),
        json!({ "value": "gpt-5.6-sol" }),
        &other_token,
        &other_csrf,
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
}
