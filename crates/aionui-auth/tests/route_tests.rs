//! Black-box integration tests for auth REST API routes.
//!
//! Covers test-plan items T4 (login), T5 (logout), T6 (auth status),
//! T7 (current user), T8 (change password), T9 (refresh token),
//! T10 (ws token), T11 (QR login).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_auth::{
    AuthIdentityMode, AuthRouterState, CookieConfig, JwtService, QrTokenStore, SessionRevokedHook, auth_routes,
    hash_password,
};
use aionui_db::{IUserRepository, SqliteUserRepository, UserStatus, init_database_memory};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Create a test app with an in-memory database.
async fn test_app() -> (Router, TestContext) {
    test_app_with_local(false).await
}

async fn test_app_with_local(local: bool) -> (Router, TestContext) {
    test_app_with_options(local, None).await
}

async fn test_app_with_options(local: bool, bootstrap_secret: Option<&str>) -> (Router, TestContext) {
    test_app_with_options_and_hook(local, bootstrap_secret, false, None).await
}

async fn test_app_with_options_and_hook(
    local: bool,
    bootstrap_secret: Option<&str>,
    aionpro_mode: bool,
    session_revoked_hook: Option<Arc<SessionRevokedHook>>,
) -> (Router, TestContext) {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let jwt_service = Arc::new(JwtService::new("test_secret_for_routes".into()));
    let cookie_config = Arc::new(CookieConfig {
        secure: false,
        same_site: "Lax",
    });
    let qr_token_store = Arc::new(QrTokenStore::new());

    let state = AuthRouterState {
        jwt_service: jwt_service.clone(),
        user_repo: user_repo.clone(),
        fs_adopter: None,
        cookie_config,
        qr_token_store: qr_token_store.clone(),
        identity_mode: if local {
            AuthIdentityMode::Local
        } else if aionpro_mode {
            AuthIdentityMode::AionPro
        } else {
            AuthIdentityMode::UserSession
        },
        bootstrap_secret: bootstrap_secret.map(Arc::<str>::from),
        session_revoked_hook,
        local,
        aionpro_mode,
    };

    let app = auth_routes(state);
    let ctx = TestContext {
        jwt_service,
        user_repo,
        qr_token_store,
        _db: db,
    };
    (app, ctx)
}

/// Holds references needed by test assertions.
struct TestContext {
    jwt_service: Arc<JwtService>,
    user_repo: Arc<dyn IUserRepository>,
    qr_token_store: Arc<QrTokenStore>,
    _db: aionui_db::Database,
}

/// Helper: create a test user with known credentials.
///
/// The seeded `system_default_user` row already uses `username = "admin"` with
/// an empty password hash. If the test asks for that username, update the seed
/// row in place instead of trying to INSERT a duplicate. Any other username
/// takes the normal create_user path.
async fn create_test_user(ctx: &TestContext, username: &str, password: &str) {
    let hash = hash_password(password).unwrap();
    if username == "admin" {
        ctx.user_repo
            .set_system_user_credentials(username, &hash)
            .await
            .unwrap();
    } else {
        ctx.user_repo.create_user(username, &hash).await.unwrap();
    }
}

/// Helper: perform a JSON POST request.
fn json_post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn json_post_with_bootstrap_secret(uri: &str, body: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-aioncore-bootstrap-secret", secret)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn json_put_with_bootstrap_secret(uri: &str, body: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-aioncore-bootstrap-secret", secret)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn json_put_anonymous(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// Helper: perform a JSON POST request with auth token.
fn json_post_with_token(uri: &str, body: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// Helper: perform a GET request with auth token.
fn get_with_token(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// Helper: perform a GET request without auth.
fn get_anonymous(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

/// Helper: extract response body as JSON.
async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn extract_session_token(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            cookie
                .split(',')
                .find(|part| part.trim_start().starts_with("aionui-session="))
                .and_then(|part| part.trim_start().strip_prefix("aionui-session="))
                .and_then(|value| value.split(';').next())
                .map(str::to_owned)
        })
}

/// Helper: login and return (token, user_id).
async fn login(app: &mut Router, username: &str, password: &str) -> (String, String) {
    let req = json_post(
        "/login",
        &format!(r#"{{"username":"{username}","password":"{password}"}}"#),
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let token = json["token"].as_str().unwrap().to_owned();
    let user_id = json["user"]["id"].as_str().unwrap().to_owned();
    (token, user_id)
}

fn json_post_anonymous(uri: &str, body: &str) -> Request<Body> {
    json_post(uri, body)
}

// ===========================================================================
// T4. Login (POST /login)
// ===========================================================================

#[tokio::test]
async fn t4_1_login_success() {
    let (app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;

    let req = json_post("/login", r#"{"username":"admin","password":"StrongP@ss1"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Check Set-Cookie header
    let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set_cookie.contains("aionui-session="));
    assert!(set_cookie.contains("HttpOnly"));

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["message"], "Login successful");
    assert!(json["token"].is_string());
    assert_eq!(json["user"]["username"], "admin");
    assert!(json["user"]["id"].is_string());

    // Verify the returned token is valid
    let token = json["token"].as_str().unwrap();
    assert!(ctx.jwt_service.verify(token).is_ok());
}

#[tokio::test]
async fn t4_2_login_nonexistent_user() {
    let (app, _ctx) = test_app().await;

    let req = json_post("/login", r#"{"username":"ghost","password":"whatever"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t4_3_login_wrong_password() {
    let (app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "CorrectP@ss1").await;

    let req = json_post("/login", r#"{"username":"admin","password":"WrongPass1"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t4_4_login_missing_fields() {
    let (app, _ctx) = test_app().await;

    // Missing password
    let req = json_post("/login", r#"{"username":"admin"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Missing username
    let req = json_post("/login", r#"{"password":"test"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Empty body
    let req = json_post("/login", r#"{}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_rejects_aionpro_mode() {
    let (app, ctx) = test_app_with_options_and_hook(false, Some("bootstrap-secret"), true, None).await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(json_post("/login", r#"{"username":"admin","password":"StrongP@ss1"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "USER_CONTEXT_REQUIRED");
}

#[tokio::test]
async fn t4_5_login_empty_password_hash_returns_401() {
    // Regression: when the seeded system user has an empty password_hash
    // (first-run local mode), POST /login must return 401, not 500.
    let (app, _ctx) = test_app_with_local(true).await;

    let req = json_post("/login", r#"{"username":"admin","password":"anything"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t4_6_login_username_too_long() {
    let (app, _ctx) = test_app().await;

    let long_name = "a".repeat(33);
    let body = format!(r#"{{"username":"{long_name}","password":"test1234"}}"#);
    let req = json_post("/login", &body);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t4_6_login_password_too_long() {
    let (app, _ctx) = test_app().await;

    let long_pass = "a".repeat(129);
    let body = format!(r#"{{"username":"admin","password":"{long_pass}"}}"#);
    let req = json_post("/login", &body);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// T5. Logout (POST /logout)
// ===========================================================================

#[tokio::test]
async fn t5_1_logout_success() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;
    let (token, _) = login(&mut app, "admin", "StrongP@ss1").await;

    let req = json_post_with_token("/logout", "", &token);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Cookie should be cleared
    let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set_cookie.contains("Max-Age=0"));

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["message"], "Logged out successfully");
}

#[tokio::test]
async fn t5_2_logout_token_becomes_invalid() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;
    let (token, _) = login(&mut app, "admin", "StrongP@ss1").await;

    // Logout
    let req = json_post_with_token("/logout", "", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Try to use the token
    let req = get_with_token("/api/auth/user", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t5_3_logout_unauthenticated() {
    let (app, _ctx) = test_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/logout")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

// ===========================================================================
// T6. Auth Status (GET /api/auth/status)
// ===========================================================================

#[tokio::test]
async fn t6_1_status_needs_setup() {
    let (app, _ctx) = test_app().await;

    let req = get_anonymous("/api/auth/status");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["needs_setup"], true);
    assert_eq!(json["is_authenticated"], false);
}

#[tokio::test]
async fn t6_2_status_has_users() {
    let (app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;

    let req = get_anonymous("/api/auth/status");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["needs_setup"], false);
}

#[tokio::test]
async fn t6_3_status_authenticated() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;
    let (token, _) = login(&mut app, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/auth/status", &token);
    let resp = app.oneshot(req).await.unwrap();

    let json = body_json(resp).await;
    assert_eq!(json["is_authenticated"], true);
}

#[tokio::test]
async fn t6_4_status_unauthenticated() {
    let (app, _ctx) = test_app().await;

    let req = get_anonymous("/api/auth/status");
    let resp = app.oneshot(req).await.unwrap();

    let json = body_json(resp).await;
    assert_eq!(json["is_authenticated"], false);
}

// ===========================================================================
// T7. Current User (GET /api/auth/user)
// ===========================================================================

#[tokio::test]
async fn t7_1_get_user_success() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;
    let (token, _) = login(&mut app, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/auth/user", &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["user"]["username"], "admin");
    assert!(json["user"]["id"].is_string());
}

#[tokio::test]
async fn t7_2_get_user_invalid_token() {
    let (app, _ctx) = test_app().await;

    let req = get_with_token("/api/auth/user", "invalid.jwt.token");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t7_3_get_user_no_token() {
    let (app, _ctx) = test_app().await;

    let req = get_anonymous("/api/auth/user");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

// ===========================================================================
// T8. Change Password (POST /api/auth/change-password)
// ===========================================================================

#[tokio::test]
async fn t8_1_change_password_success() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "OldP@ssword1").await;
    let (token, _) = login(&mut app, "admin", "OldP@ssword1").await;

    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"current_password":"OldP@ssword1","new_password":"NewP@ssword2"}"#,
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["message"], "Password changed successfully");
}

#[tokio::test]
async fn t8_2_change_password_old_token_invalidated() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "OldP@ssword1").await;
    let (token, _) = login(&mut app, "admin", "OldP@ssword1").await;

    // Change password
    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"current_password":"OldP@ssword1","new_password":"NewP@ssword2"}"#,
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Old token should be invalid (JWT secret rotated)
    let req = get_with_token("/api/auth/user", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t8_3_change_password_wrong_current() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "CorrectP@ss1").await;
    let (token, _) = login(&mut app, "admin", "CorrectP@ss1").await;

    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"current_password":"WrongP@ss1","new_password":"NewP@ssword2"}"#,
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t8_4_change_password_new_too_short() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "OldP@ssword1").await;
    let (token, _) = login(&mut app, "admin", "OldP@ssword1").await;

    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"current_password":"OldP@ssword1","new_password":"short"}"#,
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t8_6_change_password_weak() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "OldP@ssword1").await;
    let (token, _) = login(&mut app, "admin", "OldP@ssword1").await;

    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"current_password":"OldP@ssword1","new_password":"password"}"#,
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t8_7_change_password_missing_fields() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "OldP@ssword1").await;
    let (token, _) = login(&mut app, "admin", "OldP@ssword1").await;

    // Missing newPassword
    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"current_password":"OldP@ssword1"}"#,
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Missing currentPassword
    let req = json_post_with_token(
        "/api/auth/change-password",
        r#"{"new_password":"NewP@ssword2"}"#,
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// T9. Refresh Token (POST /api/auth/refresh)
// ===========================================================================

#[tokio::test]
async fn t9_1_refresh_token_success() {
    let (mut app, ctx) = test_app().await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;
    let (token, _) = login(&mut app, "admin", "StrongP@ss1").await;

    let body = format!(r#"{{"token":"{token}"}}"#);
    let req = json_post("/api/auth/refresh", &body);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["token"].is_string());

    // New token should be valid
    let new_token = json["token"].as_str().unwrap();
    assert!(ctx.jwt_service.verify(new_token).is_ok());
}

#[tokio::test]
async fn t9_2_refresh_invalid_token() {
    let (app, _ctx) = test_app().await;

    let req = json_post("/api/auth/refresh", r#"{"token":"fake.jwt.token"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t9_3_refresh_missing_token() {
    let (app, _ctx) = test_app().await;

    let req = json_post("/api/auth/refresh", r#"{}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn refresh_rejects_local_user_token_in_aionpro_mode() {
    let (app, ctx) = test_app_with_options_and_hook(false, Some("bootstrap-secret"), true, None).await;
    let token = ctx
        .jwt_service
        .sign_with_session_generation("system_default_user", "system_default_user", 0)
        .unwrap();

    let body = format!(r#"{{"token":"{token}"}}"#);
    let resp = app.oneshot(json_post("/api/auth/refresh", &body)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "USER_CONTEXT_REQUIRED");
}

// ===========================================================================
// T10. WebSocket Token (GET /api/ws-token)
// ===========================================================================

#[tokio::test]
async fn t10_1_ws_token_success() {
    let (mut app, _ctx) = test_app().await;
    create_test_user(&_ctx, "admin", "StrongP@ss1").await;
    let (token, _) = login(&mut app, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/ws-token", &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["ws_token"].is_string());
    assert!(json["expires_in"].is_number());

    // expires_in should be 30 days in milliseconds
    let expires_in = json["expires_in"].as_u64().unwrap();
    assert_eq!(expires_in, 30 * 24 * 60 * 60 * 1000);
}

#[tokio::test]
async fn t10_2_ws_token_unauthenticated() {
    let (app, _ctx) = test_app().await;

    let req = get_anonymous("/api/ws-token");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

// ===========================================================================
// T11. QR Login (POST /api/auth/qr-login)
// ===========================================================================

#[tokio::test]
async fn t11_1_qr_login_success() {
    let (app, ctx) = test_app().await;

    // Set up system user with credentials so login works
    let hash = hash_password("syspass123").unwrap();
    ctx.user_repo
        .set_system_user_credentials("sysadmin", &hash)
        .await
        .unwrap();

    // Generate QR token
    let qr_token = ctx.qr_token_store.generate();

    let body = format!(r#"{{"qr_token":"{qr_token}"}}"#);
    let req = json_post("/api/auth/qr-login", &body);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Check Set-Cookie
    let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set_cookie.contains("aionui-session="));

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["token"].is_string());
    assert_eq!(json["user"]["username"], "sysadmin");
}

#[tokio::test]
async fn t11_2_qr_login_invalid_token() {
    let (app, _ctx) = test_app().await;

    let req = json_post("/api/auth/qr-login", r#"{"qr_token":"nonexistent"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t11_4_qr_login_already_used() {
    let (app, ctx) = test_app().await;

    let hash = hash_password("syspass123").unwrap();
    ctx.user_repo
        .set_system_user_credentials("sysadmin", &hash)
        .await
        .unwrap();

    let qr_token = ctx.qr_token_store.generate();

    // First use succeeds
    let body = format!(r#"{{"qr_token":"{qr_token}"}}"#);
    let req = json_post("/api/auth/qr-login", &body);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second use fails
    let req = json_post("/api/auth/qr-login", &body);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn t11_5_qr_login_missing_token() {
    let (app, _ctx) = test_app().await;

    let req = json_post("/api/auth/qr-login", r#"{}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn qr_login_rejects_aionpro_mode() {
    let (app, ctx) = test_app_with_options_and_hook(false, Some("bootstrap-secret"), true, None).await;
    let qr_token = ctx.qr_token_store.generate();

    let body = format!(r#"{{"qr_token":"{qr_token}"}}"#);
    let resp = app.oneshot(json_post("/api/auth/qr-login", &body)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "USER_CONTEXT_REQUIRED");
}

// ===========================================================================
// QR Login Page (GET /qr-login)
// ===========================================================================

#[tokio::test]
async fn qr_login_page_returns_html() {
    let (app, _ctx) = test_app().await;

    let req = get_anonymous("/qr-login");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(content_type.contains("text/html"));
}

// ===========================================================================
// Internal external user provision and session exchange
// ===========================================================================

#[tokio::test]
async fn external_user_provision_requires_bootstrap_secret() {
    let (app, _ctx) = test_app_with_options(false, Some("bootstrap-secret")).await;

    let resp = app
        .oneshot(json_put_anonymous(
            "/api/auth/internal/external-users/pro-1",
            r#"{"user_type":"aionpro","username":"Pro User"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "BOOTSTRAP_SECRET_REQUIRED");
}

#[tokio::test]
async fn external_user_provision_rejects_wrong_bootstrap_secret() {
    let (app, _ctx) = test_app_with_options(false, Some("bootstrap-secret")).await;

    let resp = app
        .oneshot(json_put_with_bootstrap_secret(
            "/api/auth/internal/external-users/pro-1",
            r#"{"user_type":"aionpro","username":"Pro User"}"#,
            "wrong-secret",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "INVALID_BOOTSTRAP_SECRET");
}

#[tokio::test]
async fn external_user_provision_is_idempotent() {
    let (app, _ctx) = test_app_with_options(false, Some("bootstrap-secret")).await;
    let body = r#"{"user_type":"aionpro","username":"Pro User","email":"pro@example.com"}"#;

    let first = app
        .clone()
        .oneshot(json_put_with_bootstrap_secret(
            "/api/auth/internal/external-users/pro-1",
            body,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_json = body_json(first).await;
    let user_id = first_json["data"]["user_id"].as_str().unwrap().to_owned();
    assert_eq!(first_json["data"]["user_type"], "aionpro");
    assert_eq!(first_json["data"]["external_user_id"], "pro-1");

    let second = app
        .oneshot(json_put_with_bootstrap_secret(
            "/api/auth/internal/external-users/pro-1",
            r#"{"user_type":"aionpro","username":"Ignored"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_json = body_json(second).await;
    assert_eq!(second_json["data"]["user_id"], user_id);
}

#[tokio::test]
async fn external_session_exchange_returns_cookie_without_json_token() {
    let (app, _ctx) = test_app_with_options(false, Some("bootstrap-secret")).await;

    let provision = app
        .clone()
        .oneshot(json_put_with_bootstrap_secret(
            "/api/auth/internal/external-users/pro-session",
            r#"{"user_type":"aionpro","username":"Pro User"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(provision.status(), StatusCode::OK);

    let session = app
        .clone()
        .oneshot(json_post_with_bootstrap_secret(
            "/api/auth/internal/external-sessions",
            r#"{"user_type":"aionpro","external_user_id":"pro-session"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    assert!(session.headers().get(header::SET_COOKIE).is_some());
    let token = extract_session_token(&session).unwrap();
    let json = body_json(session).await;
    assert!(json["data"].get("token").is_none());
    assert_eq!(json["data"]["user"]["username"], "Pro User");

    let user_resp = app.oneshot(get_with_token("/api/auth/user", &token)).await.unwrap();
    assert_eq!(user_resp.status(), StatusCode::OK);
    let user_json = body_json(user_resp).await;
    assert_eq!(user_json["user"]["username"], "Pro User");
}

#[tokio::test]
async fn external_session_exchange_rejects_unprovisioned_user() {
    let (app, _ctx) = test_app_with_options(false, Some("bootstrap-secret")).await;

    let resp = app
        .oneshot(json_post_with_bootstrap_secret(
            "/api/auth/internal/external-sessions",
            r#"{"user_type":"aionpro","external_user_id":"missing"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "USER_CONTEXT_REQUIRED");
}

#[tokio::test]
async fn external_session_revoke_invalidates_existing_token() {
    let revoked_users = Arc::new(Mutex::new(Vec::new()));
    let hook_users = revoked_users.clone();
    let (app, _ctx) = test_app_with_options_and_hook(
        false,
        Some("bootstrap-secret"),
        false,
        Some(Arc::new(move |user_id: &str| {
            hook_users.lock().unwrap().push(user_id.to_owned());
        })),
    )
    .await;

    let provision = app
        .clone()
        .oneshot(json_put_with_bootstrap_secret(
            "/api/auth/internal/external-users/pro-revoke",
            r#"{"user_type":"aionpro","username":"Pro User"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(provision.status(), StatusCode::OK);

    let session = app
        .clone()
        .oneshot(json_post_with_bootstrap_secret(
            "/api/auth/internal/external-sessions",
            r#"{"user_type":"aionpro","external_user_id":"pro-revoke"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let token = extract_session_token(&session).unwrap();
    let session_json = body_json(session).await;
    assert_eq!(session_json["data"]["session_generation"], 0);
    assert!(session_json["data"].get("token").is_none());

    let revoke = app
        .clone()
        .oneshot(json_post_with_bootstrap_secret(
            "/api/auth/internal/external-sessions/revoke",
            r#"{"user_type":"aionpro","external_user_id":"pro-revoke"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::OK);
    let revoke_json = body_json(revoke).await;
    assert_eq!(revoke_json["data"]["session_generation"], 1);
    assert_eq!(
        revoked_users.lock().unwrap().as_slice(),
        &[revoke_json["data"]["user_id"].as_str().unwrap().to_owned()]
    );

    let user_resp = app.oneshot(get_with_token("/api/auth/user", &token)).await.unwrap();
    assert_eq!(user_resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn external_session_exchange_rejects_disabled_user() {
    let (app, ctx) = test_app_with_options(false, Some("bootstrap-secret")).await;
    let provision = app
        .clone()
        .oneshot(json_put_with_bootstrap_secret(
            "/api/auth/internal/external-users/pro-disabled",
            r#"{"user_type":"aionpro","username":"Disabled"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();
    let provision_json = body_json(provision).await;
    let user_id = provision_json["data"]["user_id"].as_str().unwrap();
    ctx.user_repo.set_status(user_id, UserStatus::Disabled).await.unwrap();

    let resp = app
        .oneshot(json_post_with_bootstrap_secret(
            "/api/auth/internal/external-sessions",
            r#"{"user_type":"aionpro","external_user_id":"pro-disabled"}"#,
            "bootstrap-secret",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "USER_DISABLED");
}

// ===========================================================================
// T12. Local-only internal user routes
// ===========================================================================

#[tokio::test]
async fn t12_1_internal_user_routes_forbidden_outside_local_mode() {
    let (app, _ctx) = test_app().await;

    let resp = app
        .oneshot(get_anonymous("/api/auth/internal/users/system"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn t12_2_internal_user_routes_work_in_local_mode() {
    let (app, ctx) = test_app_with_local(true).await;
    create_test_user(&ctx, "admin", "StrongP@ss1").await;

    let system_resp = app
        .clone()
        .oneshot(get_anonymous("/api/auth/internal/users/system"))
        .await
        .unwrap();
    assert_eq!(system_resp.status(), StatusCode::OK);
    let system_json = body_json(system_resp).await;
    assert_eq!(system_json["data"]["id"], "system_default_user");
    assert!(system_json["data"].get("password_hash").is_none());
    assert!(system_json["data"].get("jwt_secret").is_none());

    let user_resp = app
        .clone()
        .oneshot(get_anonymous("/api/auth/internal/users/by-username/admin"))
        .await
        .unwrap();
    assert_eq!(user_resp.status(), StatusCode::OK);
    let user_json = body_json(user_resp).await;
    assert!(user_json["data"].get("password_hash").is_none());
    assert!(user_json["data"].get("jwt_secret").is_none());
    let user_id = user_json["data"]["id"].as_str().unwrap().to_owned();

    let update_resp = app
        .clone()
        .oneshot(json_post_anonymous(
            &format!("/api/auth/internal/users/{user_id}/username"),
            r#"{"username":"renamed-admin"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(update_resp.status(), StatusCode::OK);

    let renamed_resp = app
        .oneshot(get_anonymous("/api/auth/internal/users/by-username/renamed-admin"))
        .await
        .unwrap();
    assert_eq!(renamed_resp.status(), StatusCode::OK);
    let renamed_json = body_json(renamed_resp).await;
    assert_eq!(renamed_json["data"]["id"], user_id);
    assert_eq!(renamed_json["data"]["username"], "renamed-admin");
}
