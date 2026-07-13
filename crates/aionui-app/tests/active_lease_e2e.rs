mod common;

use aionui_common::now_ms;
use aionui_db::models::TeamRow;
use aionui_db::{ITeamRepository, SqliteTeamRepository};
use aionui_team::{TeamAgent, TeammateRole};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

fn empty_post_with_token(uri: &str, token: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(Body::empty())
        .unwrap()
}

fn empty_post_with_auth_without_csrf(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn empty_post_without_auth(uri: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(Body::empty())
        .unwrap()
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str) -> String {
    let req = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Lease Conversation",
            "extra": {}
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    body["data"]["id"].as_str().unwrap().to_owned()
}

fn team_agent(slot_id: &str, name: &str, role: TeammateRole, conversation_id: &str) -> TeamAgent {
    TeamAgent {
        slot_id: slot_id.to_owned(),
        name: name.to_owned(),
        role,
        conversation_id: conversation_id.to_owned(),
        backend: "acp".to_owned(),
        model: "claude".to_owned(),
        assistant_id: None,
        status: None,
        conversation_type: None,
        cli_path: None,
    }
}

async fn insert_team(services: &aionui_app::AppServices, user_id: &str, team_id: &str, agents: Vec<TeamAgent>) {
    let repo = SqliteTeamRepository::new(services.database.pool().clone());
    repo.create_team(&TeamRow {
        id: team_id.to_owned(),
        user_id: user_id.to_owned(),
        name: "Lease Team".to_owned(),
        workspace: String::new(),
        workspace_mode: "shared".to_owned(),
        agents: serde_json::to_string(&agents).unwrap(),
        lead_agent_id: agents.first().map(|agent| agent.slot_id.clone()),
        session_mode: None,
        agents_version: "1.0.1".to_owned(),
        created_at: now_ms(),
        updated_at: now_ms(),
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn conversation_active_lease_renews_owned_conversation() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &token, &csrf).await;

    let req = empty_post_with_token(
        &format!("/api/conversations/{conversation_id}/active-lease"),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(services.active_lease_registry.active_until(&conversation_id).is_some());
}

#[tokio::test]
async fn conversation_active_lease_rejects_missing_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &token, &csrf).await;

    let resp = app
        .oneshot(empty_post_without_auth(
            &format!("/api/conversations/{conversation_id}/active-lease"),
            &csrf,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn conversation_active_lease_rejects_missing_csrf() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &token, &csrf).await;

    let resp = app
        .oneshot(empty_post_with_auth_without_csrf(
            &format!("/api/conversations/{conversation_id}/active-lease"),
            &token,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn conversation_active_lease_rejects_cross_user_without_renewing() {
    let (mut app, services) = build_app().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &owner_token, &owner_csrf).await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;

    let req = empty_post_with_token(
        &format!("/api/conversations/{conversation_id}/active-lease"),
        &other_token,
        &other_csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "NOT_FOUND");
    assert!(services.active_lease_registry.active_until(&conversation_id).is_none());
}

#[tokio::test]
async fn team_active_lease_renews_member_conversations() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let owner = services.user_repo.find_by_username("admin").await.unwrap().unwrap();
    insert_team(
        &services,
        &owner.id,
        "team-lease",
        vec![
            team_agent("lead", "Lead", TeammateRole::Lead, "team-conv-lead"),
            team_agent("worker", "Worker", TeammateRole::Teammate, "team-conv-worker"),
        ],
    )
    .await;

    let req = empty_post_with_token("/api/teams/team-lease/active-lease", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(services.active_lease_registry.active_until("team-conv-lead").is_some());
    assert!(
        services
            .active_lease_registry
            .active_until("team-conv-worker")
            .is_some()
    );
}

#[tokio::test]
async fn team_active_lease_allows_empty_agents() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let owner = services.user_repo.find_by_username("admin").await.unwrap().unwrap();
    insert_team(&services, &owner.id, "team-empty", vec![]).await;

    let req = empty_post_with_token("/api/teams/team-empty/active-lease", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(services.active_lease_registry.active_until("team-empty").is_none());
    assert!(services.active_lease_registry.active_until("unrelated").is_none());
}

#[tokio::test]
async fn team_active_lease_rejects_missing_auth() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let owner = services.user_repo.find_by_username("admin").await.unwrap().unwrap();
    insert_team(&services, &owner.id, "team-auth", vec![]).await;

    let resp = app
        .oneshot(empty_post_without_auth("/api/teams/team-auth/active-lease", &_csrf))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "UNAUTHORIZED");
    drop(token);
}

#[tokio::test]
async fn team_active_lease_rejects_missing_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let owner = services.user_repo.find_by_username("admin").await.unwrap().unwrap();
    insert_team(&services, &owner.id, "team-csrf", vec![]).await;

    let resp = app
        .oneshot(empty_post_with_auth_without_csrf(
            "/api/teams/team-csrf/active-lease",
            &token,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn team_active_lease_rejects_cross_user_without_renewing() {
    let (mut app, services) = build_app().await;
    let (_owner_token, _owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let owner = services.user_repo.find_by_username("admin").await.unwrap().unwrap();
    insert_team(
        &services,
        &owner.id,
        "team-cross-user",
        vec![team_agent("lead", "Lead", TeammateRole::Lead, "team-cross-conv")],
    )
    .await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;

    let req = empty_post_with_token("/api/teams/team-cross-user/active-lease", &other_token, &other_csrf);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "FORBIDDEN");
    assert!(services.active_lease_registry.active_until("team-cross-conv").is_none());
}
