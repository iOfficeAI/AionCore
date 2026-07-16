use std::sync::Arc;

use aionui_auth::CurrentUser;
use aionui_db::{
    IConversationRepository, IProjectRepository, ITeamRepository, SqliteConversationRepository,
    SqliteProjectRepository, SqliteTeamRepository, init_database_memory,
};
use aionui_project::{
    AgentCapabilitySnapshot, ProjectAgentCapabilityPort, ProjectError, ProjectRouterState, ProjectService,
    project_routes,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use tower::ServiceExt;

struct NoAgents;

#[async_trait::async_trait]
impl ProjectAgentCapabilityPort for NoAgents {
    async fn snapshot(&self, _id: &str, _refresh: bool) -> Result<Option<AgentCapabilitySnapshot>, ProjectError> {
        Ok(None)
    }
}

async fn app() -> (Router, tempfile::TempDir, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let project_repo: Arc<dyn IProjectRepository> = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    let conversation_repo: Arc<dyn IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let team_repo: Arc<dyn ITeamRepository> = Arc::new(SqliteTeamRepository::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(
        project_repo,
        conversation_repo,
        team_repo,
        Arc::new(NoAgents),
    ));
    let temp = tempfile::tempdir().unwrap();
    let router = project_routes(ProjectRouterState { service }).layer(Extension(CurrentUser {
        id: "system_default_user".into(),
        username: "system_default_user".into(),
    }));
    (router, temp, db)
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn project_routes_support_crud_profiles_and_preflight() {
    let (app, temp, _db) = app().await;
    let create = Request::builder()
        .method("POST")
        .uri("/api/projects")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": "Example",
                "local_path": temp.path(),
                "project_type": "unknown"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(response).await;
    let project_id = created["data"]["id"].as_str().unwrap();

    let profile = Request::builder()
        .method("PUT")
        .uri(format!("/api/projects/{project_id}/command-profile"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"unit_test_command":"cargo test","command_timeout_seconds":300}"#,
        ))
        .unwrap();
    assert_eq!(app.clone().oneshot(profile).await.unwrap().status(), StatusCode::OK);

    let runtime = Request::builder()
        .method("PUT")
        .uri(format!("/api/projects/{project_id}/runtime-profile"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"environment_kind":"local","language":"rust","env_keys":["RUST_LOG"]}"#,
        ))
        .unwrap();
    assert_eq!(app.clone().oneshot(runtime).await.unwrap().status(), StatusCode::OK);

    let preflight = Request::builder()
        .method("POST")
        .uri(format!("/api/projects/{project_id}/preflight"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"agent_ids":[],"refresh_agents":false}"#))
        .unwrap();
    let response = app.clone().oneshot(preflight).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = json(response).await;
    assert_eq!(result["data"]["project_id"], project_id);

    let list = Request::builder().uri("/api/projects").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(list).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["data"].as_array().unwrap().len(), 1);

    let delete = Request::builder()
        .method("DELETE")
        .uri(format!("/api/projects/{project_id}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(delete).await.unwrap().status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn project_routes_reject_invalid_json_and_missing_project() {
    let (app, _temp, _db) = app().await;
    let invalid = Request::builder()
        .method("POST")
        .uri("/api/projects")
        .header("content-type", "application/json")
        .body(Body::from("not-json"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(invalid).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    let missing = Request::builder()
        .uri("/api/projects/missing")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(missing).await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn project_routes_bind_and_resolve_an_owned_conversation() {
    let (app, temp, db) = app().await;
    let create = Request::builder()
        .method("POST")
        .uri("/api/projects")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": "Linked",
                "local_path": temp.path(),
                "project_type": "unknown"
            })
            .to_string(),
        ))
        .unwrap();
    let project_id = json(app.clone().oneshot(create).await.unwrap()).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES ('c1', 'system_default_user', 'Chat', 'chat', '{}', 'pending', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let bind = Request::builder()
        .method("POST")
        .uri(format!("/api/projects/{project_id}/links"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"resource_type":"conversation","resource_id":"c1"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(bind).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let links = Request::builder()
        .uri(format!("/api/projects/{project_id}/links"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        json(app.clone().oneshot(links).await.unwrap()).await["data"][0]["resource_id"],
        "c1"
    );

    let resolve = Request::builder()
        .uri("/api/projects/by-resource?resource_type=conversation&resource_id=c1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        json(app.oneshot(resolve).await.unwrap()).await["data"]["id"],
        project_id
    );
}
