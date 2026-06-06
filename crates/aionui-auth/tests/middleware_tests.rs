use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware;
use axum::routing::{get, post};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use tower::ServiceExt;

use aionui_auth::{
    AuthState, CookieConfig, CurrentUser, JwtService, RateLimiter, TokenPayload, api_rate_limit_middleware,
    auth_middleware, auth_rate_limit_middleware, authenticated_action_rate_limit_middleware, csrf_middleware,
    security_headers_middleware,
};
use aionui_db::{IUserRepository, SqliteUserRepository, init_database_memory};

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

// ============================================================
// T12.1 — Security response headers
// ============================================================

#[tokio::test]
async fn t12_1_security_headers_on_get() {
    let app = Router::new()
        .route("/test", get(|| async { "ok" }))
        .layer(middleware::from_fn(security_headers_middleware));

    let resp = app
        .oneshot(Request::get("/test").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
    assert_eq!(resp.headers().get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(resp.headers().get("x-xss-protection").unwrap(), "1; mode=block");
    assert_eq!(
        resp.headers().get("referrer-policy").unwrap(),
        "strict-origin-when-cross-origin"
    );
}

// ============================================================
// T12.2 — CSRF protection
// ============================================================

fn csrf_app() -> Router {
    let config = Arc::new(CookieConfig {
        secure: false,
        same_site: "Lax",
    });
    Router::new()
        .route("/api/test", post(|| async { "ok" }))
        .route("/login", post(|| async { "logged in" }))
        .route("/api/auth/qr-login", post(|| async { "qr ok" }))
        .route("/get-test", get(|| async { "get ok" }))
        .layer(middleware::from_fn_with_state(config, csrf_middleware))
}

#[tokio::test]
async fn t12_2_get_requests_bypass_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::get("/get-test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_post_without_csrf_token_rejected() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::post("/api/test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn t12_2_post_with_matching_csrf_tokens_accepted() {
    let app = csrf_app();
    let token = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
    let resp = app
        .oneshot(
            Request::post("/api/test")
                .header("cookie", format!("aionui-csrf-token={token}"))
                .header("x-csrf-token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_post_with_mismatched_csrf_tokens_rejected() {
    let app = csrf_app();
    let resp = app
        .oneshot(
            Request::post("/api/test")
                .header("cookie", "aionui-csrf-token=token_a")
                .header("x-csrf-token", "token_b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

// ============================================================
// Auth middleware
// ============================================================

async fn auth_app(jwt_service: Arc<JwtService>) -> Router {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    protected_auth_app(jwt_service, user_repo)
}

fn protected_auth_app(jwt_service: Arc<JwtService>, user_repo: Arc<dyn IUserRepository>) -> Router {
    let state = AuthState {
        jwt_service,
        user_repo,
        local: false,
    };

    Router::new()
        .route("/protected", get(|| async { "ok" }))
        .route_layer(middleware::from_fn_with_state(state, auth_middleware))
}

fn expired_token(jwt_service: &JwtService, secret: &str, user_id: &str, username: &str) -> String {
    let token = jwt_service.sign(user_id, username).unwrap();
    let mut validation = Validation::default();
    validation.validate_exp = false;
    validation.validate_aud = false;

    let mut claims = decode::<TokenPayload>(&token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
        .unwrap()
        .claims;

    claims.iat = 1000;
    claims.exp = 1001;

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn auth_middleware_missing_token_returns_unauthorized_code() {
    let app = auth_app(Arc::new(JwtService::new("middleware_test_secret".into()))).await;

    let resp = app
        .oneshot(Request::get("/protected").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_invalid_token_returns_unauthorized_code() {
    let app = auth_app(Arc::new(JwtService::new("middleware_test_secret".into()))).await;

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, "Bearer not-a-valid-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_expired_token_returns_unauthorized_code() {
    let secret = "middleware_test_secret";
    let jwt_service = Arc::new(JwtService::new(secret.into()));
    let token = expired_token(&jwt_service, secret, "system_default_user", "system_default_user");
    let app = auth_app(jwt_service).await;

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_missing_user_returns_unauthorized_code() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let token = jwt_service.sign("missing_user", "ghost").unwrap();
    let app = auth_app(jwt_service).await;

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_database_error_returns_internal_error_code() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let token = jwt_service.sign("system_default_user", "system_default_user").unwrap();
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    db.close().await;
    let app = protected_auth_app(jwt_service, user_repo);

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "INTERNAL_ERROR");
    assert_eq!(json["error"], "Internal server error.");
    let error = json["error"].as_str().unwrap();
    assert!(!error.contains("Database error"));
    assert!(!error.contains("Authentication service unavailable"));
    assert!(!error.to_ascii_lowercase().contains("closed"));
    assert!(!error.to_ascii_lowercase().contains("sqlx"));
}

#[tokio::test]
async fn t12_2_login_exempt_from_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::post("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_qr_login_exempt_from_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::post("/api/auth/qr-login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_csrf_cookie_set_on_first_request() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::get("/get-test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set_cookie.contains("aionui-csrf-token="));
    // NOT HttpOnly (JS must read it)
    assert!(!set_cookie.contains("HttpOnly"));
}

// ============================================================
// Rate limiter middleware
// ============================================================

fn rate_limit_app(limiter: Arc<RateLimiter>) -> Router {
    Router::new()
        .route("/test", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(limiter, api_rate_limit_middleware))
}

#[tokio::test]
async fn api_rate_limit_allows_within_quota() {
    let limiter = Arc::new(RateLimiter::new(3, Duration::from_secs(60)));
    let app = rate_limit_app(limiter);

    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(Request::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn api_rate_limit_rejects_over_quota() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = rate_limit_app(limiter);

    // First two pass
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(Request::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Third rejected
    let resp = app
        .oneshot(Request::get("/test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn auth_rate_limit_skips_successful_responses() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = Router::new()
        .route("/login", post(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(limiter, auth_rate_limit_middleware));

    // Successful responses (200) don't count toward the limit
    for _ in 0..5 {
        let resp = app
            .clone()
            .oneshot(Request::post("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn auth_rate_limit_counts_failed_responses() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = Router::new()
        .route("/login", post(|| async { StatusCode::UNAUTHORIZED }))
        .layer(middleware::from_fn_with_state(limiter, auth_rate_limit_middleware));

    // First two failures pass through
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(Request::post("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // Third request blocked by rate limiter
    let resp = app
        .oneshot(Request::post("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn authenticated_action_limit_uses_user_id_key() {
    let limiter = Arc::new(RateLimiter::new(1, Duration::from_secs(60)));

    // Handler that injects a CurrentUser extension before the limiter
    let app = Router::new()
        .route("/action", post(|| async { "done" }))
        .layer(middleware::from_fn_with_state(
            limiter.clone(),
            authenticated_action_rate_limit_middleware,
        ))
        .layer(middleware::from_fn(
            |mut request: axum::extract::Request, next: axum::middleware::Next| async {
                request.extensions_mut().insert(CurrentUser {
                    id: "user_42".into(),
                    username: "admin".into(),
                });
                Ok::<_, std::convert::Infallible>(next.run(request).await)
            },
        ));

    // First request passes
    let resp = app
        .clone()
        .oneshot(Request::post("/action").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second request for same user is rate limited
    let resp = app
        .oneshot(Request::post("/action").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ============================================================
// T12.3 — Cookie security attributes (via CookieConfig)
// ============================================================

#[test]
fn t12_3_session_cookie_is_httponly() {
    let config = CookieConfig {
        secure: false,
        same_site: "Lax",
    };
    let cookie = config.build_session_cookie("token123");
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("Max-Age="));
}

#[test]
fn t12_3_session_cookie_secure_when_https() {
    let config = CookieConfig {
        secure: true,
        same_site: "Strict",
    };
    let cookie = config.build_session_cookie("token123");
    assert!(cookie.contains("; Secure"));
    assert!(cookie.contains("SameSite=Strict"));
}

// ============================================================
// T13 — Token extraction strategy
// ============================================================

#[test]
fn t13_1_authorization_header_takes_priority() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, "Bearer header_tok".parse().unwrap());
    headers.insert(header::COOKIE, "aionui-session=cookie_tok".parse().unwrap());
    assert_eq!(
        aionui_auth::extract_token_from_headers(&headers),
        Some("header_tok".into())
    );
}

#[test]
fn t13_2_cookie_fallback() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::COOKIE, "aionui-session=fallback_tok".parse().unwrap());
    assert_eq!(
        aionui_auth::extract_token_from_headers(&headers),
        Some("fallback_tok".into())
    );
}

#[test]
fn t13_3_no_token_returns_none() {
    let headers = axum::http::HeaderMap::new();
    assert_eq!(aionui_auth::extract_token_from_headers(&headers), None);
}
