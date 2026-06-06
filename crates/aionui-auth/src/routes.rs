#![allow(clippy::disallowed_types)]

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, header};
use axum::middleware::from_fn_with_state;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;

use aionui_api_types::{
    ApiResponse, AuthStatusResponse, ChangePasswordRequest, LoginRequest, LoginResponse, PublicUser, QrLoginRequest,
    RefreshResponse, RefreshTokenRequest, UserInfoResponse, WebuiChangePasswordRequest, WebuiChangeUsernameRequest,
    WebuiChangeUsernameResponse, WebuiGenerateQrTokenResponse, WebuiResetPasswordResponse, WsTokenResponse,
};
use aionui_common::ApiError;
use aionui_common::constants::COOKIE_MAX_AGE_DAYS;
use aionui_db::{DbError, IUserRepository, models::User};

use crate::error::AuthError;
use crate::extract::extract_token_from_headers;
use crate::middleware::{AuthState, CurrentUser, auth_middleware};
use crate::password::{dummy_password_hash, generate_password, hash_password, verify_password_timed};
use crate::qr_token::QrTokenStore;
use crate::rate_limit::{
    RateLimiter, api_rate_limit_middleware, auth_rate_limit_middleware, authenticated_action_rate_limit_middleware,
};
use crate::validation::{validate_password, validate_username};
use crate::{CookieConfig, JwtService};

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials => ApiError::Unauthorized("Invalid username or password".into()),
            AuthError::WeakPassword(msg) => ApiError::BadRequest(msg),
            AuthError::InvalidUsername(msg) => ApiError::BadRequest(msg),
            AuthError::TokenExpired => ApiError::Unauthorized("Token expired".into()),
            AuthError::TokenInvalid(msg) => ApiError::Unauthorized(msg),
            AuthError::TokenBlacklisted => ApiError::Unauthorized("Token has been revoked".into()),
            AuthError::RateLimited => ApiError::RateLimited,
            AuthError::HashError(msg) => ApiError::Internal(format!("Password hash error: {msg}")),
        }
    }
}

fn db_error_to_api_error(err: DbError) -> ApiError {
    match err {
        DbError::NotFound(msg) => ApiError::NotFound(msg),
        DbError::Conflict(msg) => ApiError::Conflict(msg),
        DbError::Query(e) => ApiError::Internal(format!("Database error: {e}")),
        DbError::Migration(e) => ApiError::Internal(format!("Migration error: {e}")),
        DbError::Init(msg) => ApiError::Internal(format!("Database init error: {msg}")),
    }
}

/// Shared state for all auth route handlers.
#[derive(Clone)]
pub struct AuthRouterState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub local: bool,
}

#[derive(Debug, Deserialize)]
struct CreateInternalUserRequest {
    username: String,
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct SetSystemUserCredentialsRequest {
    username: String,
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct UpdatePasswordHashRequest {
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct UpdateUsernameRequest {
    username: String,
}

#[derive(Debug, Deserialize)]
struct UpdateJwtSecretRequest {
    jwt_secret: String,
}

fn ensure_local_mode(local: bool) -> Result<(), ApiError> {
    if local {
        return Ok(());
    }
    Err(ApiError::Forbidden(
        "This endpoint is only available in local mode".into(),
    ))
}

/// Build the auth router with all endpoints and middleware layers.
///
/// Returns a `Router` with these endpoints:
/// - `POST /login`
/// - `POST /logout`
/// - `GET /api/auth/status`
/// - `GET /api/auth/user`
/// - `POST /api/auth/change-password`
/// - `POST /api/auth/refresh`
/// - `GET /api/ws-token`
/// - `POST /api/auth/qr-login`
/// - `GET /qr-login`
/// - `POST /api/webui/change-password` (local-only)
/// - `POST /api/webui/change-username` (local-only)
/// - `POST /api/webui/reset-password` (local-only)
/// - `POST /api/webui/generate-qr-token` (local-only)
pub fn auth_routes(state: AuthRouterState) -> Router {
    let auth_limiter = Arc::new(RateLimiter::auth());
    let api_limiter = Arc::new(RateLimiter::api());
    let action_limiter = Arc::new(RateLimiter::authenticated_action());

    // Start periodic cleanup for rate limiters
    let cleanup_interval = Duration::from_secs(60);
    auth_limiter.start_cleanup_task(cleanup_interval);
    api_limiter.start_cleanup_task(cleanup_interval);
    action_limiter.start_cleanup_task(cleanup_interval);

    let auth_state = AuthState {
        jwt_service: state.jwt_service.clone(),
        user_repo: state.user_repo.clone(),
        local: false,
    };

    // Auth rate limited routes (login, qr-login)
    let auth_rate_limited = Router::new()
        .route("/login", post(login_handler))
        .route("/api/auth/qr-login", post(qr_login_handler))
        .route_layer(from_fn_with_state(auth_limiter, auth_rate_limit_middleware))
        .with_state(state.clone());

    // API rate limited public routes (no auth required)
    let api_public = Router::new()
        .route("/api/auth/status", get(status_handler))
        .route(
            "/api/auth/internal/users",
            get(list_internal_users_handler).post(create_internal_user_handler),
        )
        .route("/api/auth/internal/users/system", get(get_system_user_handler))
        .route(
            "/api/auth/internal/users/system/credentials",
            post(set_system_user_credentials_handler),
        )
        .route(
            "/api/auth/internal/users/by-username/{username}",
            get(find_user_by_username_handler),
        )
        .route("/api/auth/internal/users/{id}", get(find_user_by_id_handler))
        .route(
            "/api/auth/internal/users/{id}/password",
            post(update_user_password_hash_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/username",
            post(update_user_username_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/jwt-secret",
            post(update_user_jwt_secret_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/last-login",
            post(update_user_last_login_handler),
        )
        // WebUI admin credential endpoints — local-only, enforced inside each handler.
        .route("/api/webui/change-password", post(webui_change_password_handler))
        .route("/api/webui/change-username", post(webui_change_username_handler))
        .route("/api/webui/reset-password", post(webui_reset_password_handler))
        .route("/api/webui/generate-qr-token", post(webui_generate_qr_token_handler))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // Authenticated routes: api limiter -> auth -> action limiter
    // route_layer order: last added = outermost (first to process)
    let authenticated = Router::new()
        .route("/logout", post(logout_handler))
        .route("/api/auth/user", get(user_handler))
        .route("/api/auth/change-password", post(change_password_handler))
        .route("/api/ws-token", get(ws_token_handler))
        .route_layer(from_fn_with_state(
            action_limiter.clone(),
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(auth_state, auth_middleware))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // API + action limited routes (token in body, no auth middleware)
    let api_action_limited = Router::new()
        .route("/api/auth/refresh", post(refresh_handler))
        .route_layer(from_fn_with_state(
            action_limiter,
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(api_limiter, api_rate_limit_middleware))
        .with_state(state);

    // Static page (no middleware)
    let static_routes = Router::new().route("/qr-login", get(qr_login_page));

    Router::new()
        .merge(auth_rate_limited)
        .merge(api_public)
        .merge(authenticated)
        .merge(api_action_limited)
        .merge(static_routes)
}

// ---------------------------------------------------------------------------
// POST /login
// ---------------------------------------------------------------------------

async fn login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    // Input length validation (per API spec)
    if req.username.len() > 32 {
        return Err(ApiError::BadRequest("Username must not exceed 32 characters".into()));
    }
    if req.password.len() > 128 {
        return Err(ApiError::BadRequest("Password must not exceed 128 characters".into()));
    }

    // Look up user; run dummy verify on miss to prevent timing attacks
    let user = state
        .user_repo
        .find_by_username(&req.username)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    let (found_user, password_valid) = match user {
        Some(u) if u.password_hash.trim().is_empty() => {
            // Seeded user with no password yet (first-run local mode).
            // Treat as invalid credentials; run dummy verify for timing symmetry
            // and to avoid bcrypt error on empty hash leaking as a 500.
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
        Some(u) => {
            let valid = verify_password_timed(&req.password, &u.password_hash).await?;
            (Some(u), valid)
        }
        None => {
            // Prevent user enumeration via timing
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
    };

    if !password_valid {
        return Err(ApiError::Unauthorized("Invalid username or password".into()));
    }

    let user = found_user.ok_or_else(|| ApiError::Unauthorized("Invalid username or password".into()))?;

    let token = state
        .jwt_service
        .sign(&user.id, &user.username)
        .map_err(|e| ApiError::Internal(format!("Token signing error: {e}")))?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(
        PublicUser {
            id: user.id,
            username: user.username,
        },
        token,
    );

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// POST /logout
// ---------------------------------------------------------------------------

async fn logout_handler(State(state): State<AuthRouterState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = extract_token_from_headers(&headers) {
        state.jwt_service.blacklist_token(&token);
    }

    let cookie = state.cookie_config.clear_session_cookie();
    let resp = ApiResponse::message("Logged out successfully");

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// GET /api/auth/status
// ---------------------------------------------------------------------------

async fn status_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
) -> Result<Json<AuthStatusResponse>, ApiError> {
    let has_users = state
        .user_repo
        .has_users()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    let user_count = state
        .user_repo
        .count_users()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    // Check authentication without requiring it
    let is_authenticated = extract_token_from_headers(&headers)
        .and_then(|token| state.jwt_service.verify(&token).ok())
        .is_some();

    Ok(Json(AuthStatusResponse {
        success: true,
        needs_setup: !has_users,
        user_count: user_count as u64,
        is_authenticated,
    }))
}

// ---------------------------------------------------------------------------
// Local-only internal user routes
// ---------------------------------------------------------------------------

async fn list_internal_users_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Vec<User>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let users = state.user_repo.list_users().await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(users)))
}

async fn get_system_user_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Option<User>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state.user_repo.get_system_user().await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn find_user_by_username_handler(
    State(state): State<AuthRouterState>,
    Path(username): Path<String>,
) -> Result<Json<ApiResponse<Option<User>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state
        .user_repo
        .find_by_username(&username)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn find_user_by_id_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Option<User>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state.user_repo.find_by_id(&id).await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn create_internal_user_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<CreateInternalUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<User>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let user = state
        .user_repo
        .create_user(&req.username, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn set_system_user_credentials_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<SetSystemUserCredentialsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .set_system_user_credentials(&req.username, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_password_hash_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePasswordHashRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .update_password(&id, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_username_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .update_username(&id, &req.username)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_jwt_secret_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateJwtSecretRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .update_jwt_secret(&id, &req.jwt_secret)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_last_login_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    state
        .user_repo
        .update_last_login(&id)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

// ---------------------------------------------------------------------------
// GET /api/auth/user
// ---------------------------------------------------------------------------

async fn user_handler(Extension(user): Extension<CurrentUser>) -> Json<UserInfoResponse> {
    Json(UserInfoResponse {
        success: true,
        user: PublicUser {
            id: user.id,
            username: user.username,
        },
    })
}

// ---------------------------------------------------------------------------
// POST /api/auth/change-password
// ---------------------------------------------------------------------------

async fn change_password_handler(
    State(state): State<AuthRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ChangePasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    // Validate new password strength
    validate_password(&req.new_password)?;

    // Fetch user record
    let user = state
        .user_repo
        .find_by_id(&current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::NotFound("User not found".into()))?;

    // Verify current password
    let valid = verify_password_timed(&req.current_password, &user.password_hash).await?;
    if !valid {
        return Err(ApiError::Unauthorized("Current password is incorrect".into()));
    }

    // Hash new password on blocking thread
    let password = req.new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    // Persist new password hash
    state
        .user_repo
        .update_password(&current_user.id, &new_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    // Rotate JWT secret to invalidate all sessions
    let new_secret = state
        .jwt_service
        .rotate_secret()
        .map_err(|e| ApiError::Internal(format!("Secret rotation error: {e}")))?;

    // Persist new secret to database
    state
        .user_repo
        .update_jwt_secret(&current_user.id, &new_secret)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::message("Password changed successfully")))
}

// ---------------------------------------------------------------------------
// POST /api/auth/refresh
// ---------------------------------------------------------------------------

async fn refresh_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<RefreshTokenRequest>, JsonRejection>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let payload = state
        .jwt_service
        .verify(&req.token)
        .map_err(|_| ApiError::Unauthorized("Invalid or expired token".into()))?;

    let new_token = state
        .jwt_service
        .sign(&payload.user_id, &payload.username)
        .map_err(|e| ApiError::Internal(format!("Token signing error: {e}")))?;

    Ok(Json(RefreshResponse {
        success: true,
        token: new_token,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/ws-token
// ---------------------------------------------------------------------------

async fn ws_token_handler(
    State(state): State<AuthRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<WsTokenResponse>, ApiError> {
    // Reuse the existing session token for WebSocket connections
    let token = extract_token_from_headers(&headers).ok_or_else(|| ApiError::Unauthorized("No token found".into()))?;

    // Ensure user still exists
    state
        .user_repo
        .find_by_id(&current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::Unauthorized("User not found".into()))?;

    // Cookie max age in milliseconds
    let expires_in = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60 * 1000;

    Ok(Json(WsTokenResponse {
        success: true,
        ws_token: token,
        expires_in,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/auth/qr-login
// ---------------------------------------------------------------------------

async fn qr_login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<QrLoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    // Validate and consume QR token (one-time use)
    state.qr_token_store.validate_and_consume(&req.qr_token)?;

    // Get primary WebUI user for QR login
    let user = state
        .user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::Internal("No primary user configured".into()))?;

    let token = state
        .jwt_service
        .sign(&user.id, &user.username)
        .map_err(|e| ApiError::Internal(format!("Token signing error: {e}")))?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(
        PublicUser {
            id: user.id,
            username: user.username,
        },
        token,
    );

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// GET /qr-login (static HTML page)
// ---------------------------------------------------------------------------

async fn qr_login_page() -> Html<&'static str> {
    Html(QR_LOGIN_HTML)
}

const QR_LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>QR Login - AionUI</title>
<style>
  body { font-family: system-ui, sans-serif; display: flex; justify-content: center;
         align-items: center; min-height: 100vh; margin: 0; background: #f5f5f5; }
  .card { background: white; padding: 2rem; border-radius: 8px;
          box-shadow: 0 2px 8px rgba(0,0,0,0.1); text-align: center; max-width: 400px; }
  .status { margin-top: 1rem; color: #666; }
  .error { color: #d32f2f; }
  .success { color: #388e3c; }
</style>
</head>
<body>
<div class="card">
  <h1>AionUI</h1>
  <p id="status" class="status">Processing login...</p>
</div>
<script>
(function() {
  var el = document.getElementById('status');
  var params = new URLSearchParams(window.location.search);
  var token = params.get('token');
  if (!token) {
    el.textContent = 'Error: No token provided';
    el.className = 'status error';
    return;
  }
  fetch('/api/auth/qr-login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ qrToken: token })
  })
  .then(function(r) { return r.json(); })
  .then(function(data) {
    if (data.success) {
      el.textContent = 'Login successful! Redirecting...';
      el.className = 'status success';
      setTimeout(function() { window.location.href = '/'; }, 1000);
    } else {
      el.textContent = 'Login failed: ' + (data.error || 'Unknown error');
      el.className = 'status error';
    }
  })
  .catch(function(err) {
    el.textContent = 'Error: ' + err.message;
    el.className = 'status error';
  });
})();
</script>
</body>
</html>"#;

// ---------------------------------------------------------------------------
// WebUI admin credential endpoints (local-only)
// ---------------------------------------------------------------------------

/// Random password length for `/api/webui/reset-password`.
const RESET_PASSWORD_LEN: usize = 16;

/// Resolve the WebUI admin user, falling back to NotFound when absent.
async fn resolve_webui_admin(user_repo: &dyn IUserRepository) -> Result<User, ApiError> {
    user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::NotFound("No WebUI admin user configured".into()))
}

// ---------------------------------------------------------------------------
// POST /api/webui/change-password
// ---------------------------------------------------------------------------

async fn webui_change_password_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<WebuiChangePasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;

    validate_password(&req.new_password)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    let password = req.new_password;
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    state
        .user_repo
        .update_password(&user.id, &new_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::message("Password changed successfully")))
}

// ---------------------------------------------------------------------------
// POST /api/webui/change-username
// ---------------------------------------------------------------------------

async fn webui_change_username_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<WebuiChangeUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<WebuiChangeUsernameResponse>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;

    let trimmed = req.new_username.trim().to_owned();
    validate_username(&trimmed)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    if user.username != trimmed {
        state
            .user_repo
            .update_username(&user.id, &trimmed)
            .await
            .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;
    }

    Ok(Json(ApiResponse::ok(WebuiChangeUsernameResponse { username: trimmed })))
}

// ---------------------------------------------------------------------------
// POST /api/webui/reset-password
// ---------------------------------------------------------------------------

async fn webui_reset_password_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<WebuiResetPasswordResponse>>, ApiError> {
    ensure_local_mode(state.local)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    let new_password = generate_password(RESET_PASSWORD_LEN);
    let password_for_hash = new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password_for_hash))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    state
        .user_repo
        .update_password(&user.id, &new_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::ok(WebuiResetPasswordResponse { new_password })))
}

// ---------------------------------------------------------------------------
// POST /api/webui/generate-qr-token
// ---------------------------------------------------------------------------

async fn webui_generate_qr_token_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<WebuiGenerateQrTokenResponse>>, ApiError> {
    ensure_local_mode(state.local)?;

    let (token, expires_at_ms) = state.qr_token_store.generate_with_expiry();

    Ok(Json(ApiResponse::ok(WebuiGenerateQrTokenResponse {
        token,
        expires_at_ms,
    })))
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn invalid_credentials_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::InvalidCredentials);
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn weak_password_maps_to_bad_request() {
        let api_err = ApiError::from(AuthError::WeakPassword("too short".into()));
        assert_eq!(api_err.status_code(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn invalid_username_maps_to_bad_request() {
        let api_err = ApiError::from(AuthError::InvalidUsername("bad chars".into()));
        assert_eq!(api_err.status_code(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn token_expired_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::TokenExpired);
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn token_invalid_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::TokenInvalid("bad".into()));
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn token_blacklisted_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::TokenBlacklisted);
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rate_limited_maps_to_rate_limited() {
        let api_err = ApiError::from(AuthError::RateLimited);
        assert_eq!(api_err.status_code(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn hash_error_maps_to_internal() {
        let api_err = ApiError::from(AuthError::HashError("failed".into()));
        assert_eq!(api_err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
