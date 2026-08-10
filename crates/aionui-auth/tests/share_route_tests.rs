//! Route-level tests for resource sharing endpoints.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_auth::{
    AuthIdentityMode, AuthRouterState, CookieConfig, JwtService, QrTokenStore, auth_routes, hash_password,
};
use aionui_db::{
    IConversationRepository, IUserRepository, SqliteConversationRepository, SqliteResourceShareRepository,
    SqliteUserRepository, init_database_memory, models::ConversationRow,
};

struct TestCtx {
    app: Router,
    conv_id: String,
    _db: aionui_db::Database,
}

async fn setup() -> TestCtx {
    let db = init_database_memory().await.unwrap();
    let sqlite_user = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let users: Arc<dyn IUserRepository> = sqlite_user.clone();
    let share_repo = Arc::new(SqliteResourceShareRepository::new(db.pool().clone()));
    let jwt = Arc::new(JwtService::new("share-route-test-secret".into()));

    let owner_hash = hash_password("OwnerPass1!").unwrap();
    let grantee_hash = hash_password("GranteePass1!").unwrap();
    let owner = users.create_user("owner_share", &owner_hash).await.unwrap();
    let _grantee = users.create_user("grantee_share", &grantee_hash).await.unwrap();

    let conv_repo = SqliteConversationRepository::new(db.pool().clone());
    let now = aionui_common::now_ms();
    let conv = ConversationRow {
        id: aionui_common::generate_prefixed_id("conv"),
        user_id: owner.id.clone(),
        name: "owner chat".into(),
        r#type: "gemini".into(),
        extra: "{}".into(),
        model: None,
        status: Some("pending".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now,
        updated_at: now,
        project_id: None,
        folder_id: None,
        name_source: None,
    };
    conv_repo.create(&conv).await.unwrap();

    let state = AuthRouterState {
        jwt_service: jwt,
        user_repo: users,
        admin_user_repo: sqlite_user,
        share_repo,
        initial_admin_credentials_file: None,
        fs_adopter: None,
        cookie_config: Arc::new(CookieConfig {
            secure: false,
            same_site: "Lax",
        }),
        qr_token_store: Arc::new(QrTokenStore::new()),
        identity_mode: AuthIdentityMode::UserSession,
        bootstrap_secret: None,
        session_revoked_hook: None,
        local: false,
        aionpro_mode: false,
    };

    TestCtx {
        app: auth_routes(state),
        conv_id: conv.id,
        _db: db,
    }
}

async fn login(app: &Router, username: &str, password: &str) -> String {
    let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login failed for {username}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["token"].as_str().unwrap().to_owned()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn authed(method: &str, uri: &str, token: &str, body: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    builder.body(Body::from(body.unwrap_or("").to_owned())).unwrap()
}

#[tokio::test]
async fn owner_can_grant_list_and_revoke_share() {
    let ctx = setup().await;
    let token = login(&ctx.app, "owner_share", "OwnerPass1!").await;

    let create_body = format!(
        r#"{{"resource_type":"conversation","resource_id":"{}","grantee_username":"grantee_share","permission":"view"}}"#,
        ctx.conv_id
    );
    let create = ctx
        .app
        .clone()
        .oneshot(authed("POST", "/api/shares", &token, Some(&create_body)))
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let created = body_json(create).await;
    let share_id = created["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["data"]["permission"], "view");

    let list = ctx
        .app
        .clone()
        .oneshot(authed(
            "GET",
            &format!("/api/shares?resource_type=conversation&resource_id={}", ctx.conv_id),
            &token,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let listed = body_json(list).await;
    assert_eq!(listed["data"]["items"].as_array().unwrap().len(), 1);

    let grantee_token = login(&ctx.app, "grantee_share", "GranteePass1!").await;
    let received = ctx
        .app
        .clone()
        .oneshot(authed("GET", "/api/shares/received", &grantee_token, None))
        .await
        .unwrap();
    assert_eq!(received.status(), StatusCode::OK);
    let received_json = body_json(received).await;
    assert_eq!(received_json["data"]["items"].as_array().unwrap().len(), 1);

    let directory = ctx
        .app
        .clone()
        .oneshot(authed("GET", "/api/users/directory", &token, None))
        .await
        .unwrap();
    assert_eq!(directory.status(), StatusCode::OK);
    let dir = body_json(directory).await;
    let names: Vec<&str> = dir["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["username"].as_str())
        .collect();
    assert!(names.contains(&"grantee_share"));
    assert!(!names.contains(&"owner_share"));

    let revoke = ctx
        .app
        .clone()
        .oneshot(authed("DELETE", &format!("/api/shares/{share_id}"), &token, None))
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn grantee_cannot_revoke_owner_share() {
    let ctx = setup().await;
    let owner = login(&ctx.app, "owner_share", "OwnerPass1!").await;
    let create_body = format!(
        r#"{{"resource_type":"conversation","resource_id":"{}","grantee_username":"grantee_share","permission":"view"}}"#,
        ctx.conv_id
    );
    let create = ctx
        .app
        .clone()
        .oneshot(authed("POST", "/api/shares", &owner, Some(&create_body)))
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let share_id = body_json(create).await["data"]["id"].as_str().unwrap().to_owned();

    let grantee = login(&ctx.app, "grantee_share", "GranteePass1!").await;
    let revoke = ctx
        .app
        .clone()
        .oneshot(authed("DELETE", &format!("/api/shares/{share_id}"), &grantee, None))
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn non_owner_cannot_grant_share() {
    let ctx = setup().await;
    let token = login(&ctx.app, "grantee_share", "GranteePass1!").await;
    let body = format!(
        r#"{{"resource_type":"conversation","resource_id":"{}","grantee_username":"owner_share","permission":"view"}}"#,
        ctx.conv_id
    );
    let resp = ctx
        .app
        .clone()
        .oneshot(authed("POST", "/api/shares", &token, Some(&body)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
