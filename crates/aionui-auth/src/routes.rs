#![allow(clippy::disallowed_types)]

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::{
    AccountStatus, AdminAuditListResponse, AdminUser, AdminUserListResponse, ApiResponse, AuthStatusResponse,
    ChangePasswordRequest, CreateAdminUserRequest, CreateShareRequest, EnsureExternalSessionRequest,
    EnsureExternalUserRequest, EnsureExternalUserResponse, ListAdminAuditQuery, ListAdminUsersQuery, ListSharesQuery,
    LoginRequest, LoginResponse, PublicUser, QrLoginRequest, RefreshResponse, RefreshTokenRequest, ResourceShare,
    RevokeExternalSessionRequest, RevokeExternalSessionResponse, ShareListResponse, UpdateAdminRoleRequest,
    UpdateAdminStatusRequest, UpdateAdminUsernameRequest, UserDirectoryResponse, UserInfoResponse, UserRole,
    WebuiChangePasswordRequest, WebuiChangeUsernameRequest, WebuiChangeUsernameResponse, WebuiGenerateQrTokenResponse,
    WebuiResetPasswordResponse, WsTokenResponse,
};
use aionui_common::ApiError;
use aionui_common::constants::COOKIE_MAX_AGE_DAYS;
use aionui_db::{
    AdminUserRepositoryError, DbError, IAdminUserRepository, IResourceShareRepository, IUserRepository, UserStatus,
    UserType, models::User,
};

use crate::error::AuthError;
use crate::extract::extract_token_from_headers;
use crate::middleware::{AuthIdentityMode, AuthState, CurrentUser, admin_required_middleware, auth_middleware};
use crate::password::{dummy_password_hash, generate_password, hash_password, verify_password_timed};
use crate::qr_token::QrTokenStore;
use crate::rate_limit::{
    RateLimiter, api_rate_limit_middleware, auth_rate_limit_middleware, authenticated_action_rate_limit_middleware,
};
use crate::service::{AuthProvisionService, ProvisionError};
use crate::share_service::{ShareService, ShareServiceError};
use crate::validation::{validate_password, validate_username};
use crate::{AdminUserService, AdminUserServiceError, audit_actor};
use crate::{CookieConfig, JwtService};

const BOOTSTRAP_SECRET_HEADER: &str = "x-aioncore-bootstrap-secret";

pub type SessionRevokedHook = dyn Fn(&str) + Send + Sync;

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
    pub admin_user_repo: Arc<dyn IAdminUserRepository>,
    pub share_repo: Arc<dyn IResourceShareRepository>,
    /// Optional on-disk adoption side-effect (AionUi → AionPro upgrade).
    pub fs_adopter: Option<Arc<dyn crate::service::SystemDefaultFilesystemAdopter>>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub identity_mode: AuthIdentityMode,
    pub bootstrap_secret: Option<Arc<str>>,
    pub session_revoked_hook: Option<Arc<SessionRevokedHook>>,
    /// One-time bootstrap credential file removed after the initial admin
    /// successfully changes their temporary password.
    pub initial_admin_credentials_file: Option<Arc<PathBuf>>,
    pub local: bool,
    pub aionpro_mode: bool,
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

#[derive(Debug, Serialize)]
struct InternalUserResponse {
    id: String,
    user_type: UserType,
    external_user_id: Option<String>,
    username: Option<String>,
    email: Option<String>,
    avatar_path: Option<String>,
    status: UserStatus,
    session_generation: i64,
    created_at: i64,
    updated_at: i64,
    last_login: Option<i64>,
}

impl From<User> for InternalUserResponse {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            user_type: user.user_type,
            external_user_id: user.external_user_id,
            username: user.username,
            email: user.email,
            avatar_path: user.avatar_path,
            status: user.status,
            session_generation: user.session_generation,
            created_at: user.created_at,
            updated_at: user.updated_at,
            last_login: user.last_login,
        }
    }
}

fn ensure_local_mode(local: bool) -> Result<(), ApiError> {
    if local {
        return Ok(());
    }
    Err(ApiError::Forbidden(
        "This endpoint is only available in local mode".into(),
    ))
}

fn require_bootstrap_secret(headers: &HeaderMap, expected: Option<&str>) -> Result<(), ApiError> {
    let Some(expected) = expected else {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "BOOTSTRAP_SECRET_REQUIRED",
            "Bootstrap secret required.",
            None,
        ));
    };
    let Some(actual) = headers.get(BOOTSTRAP_SECRET_HEADER).and_then(|v| v.to_str().ok()) else {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "BOOTSTRAP_SECRET_REQUIRED",
            "Bootstrap secret required.",
            None,
        ));
    };
    if constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "INVALID_BOOTSTRAP_SECRET",
            "Invalid bootstrap secret.",
            None,
        ))
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for idx in 0..max_len {
        let l = left.get(idx).copied().unwrap_or(0);
        let r = right.get(idx).copied().unwrap_or(0);
        diff |= usize::from(l ^ r);
    }
    diff == 0
}

fn provision_error_to_api_error(err: ProvisionError) -> ApiError {
    match err {
        ProvisionError::UnsupportedUserType => ApiError::BadRequest("Unsupported external user type".into()),
        ProvisionError::UserDisabled => ApiError::coded(StatusCode::FORBIDDEN, "USER_DISABLED", "User disabled.", None),
        ProvisionError::UserNotProvisioned => ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ),
        ProvisionError::Db(DbError::Conflict(_)) => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_USER_CONFLICT",
            "External user conflict.",
            None,
        ),
        ProvisionError::Db(e) => db_error_to_api_error(e),
        ProvisionError::Token(e) => ApiError::Internal(format!("Token signing error: {e}")),
    }
}

fn user_context_required() -> ApiError {
    ApiError::coded(
        StatusCode::UNAUTHORIZED,
        "USER_CONTEXT_REQUIRED",
        "User context required.",
        None,
    )
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
        identity_mode: state.identity_mode,
        // Auth endpoints manage sessions themselves; the helper CLI never
        // calls them, so the runtime-token channel stays disabled here.
        runtime_token_verifier: None,
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
            "/api/auth/internal/external-users/{external_user_id}",
            put(ensure_external_user_handler),
        )
        .route(
            "/api/auth/internal/external-sessions",
            post(create_external_session_handler),
        )
        .route(
            "/api/auth/internal/external-sessions/revoke",
            post(revoke_external_session_handler),
        )
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
        .route(
            "/api/shares",
            get(list_resource_shares_handler).post(create_share_handler),
        )
        .route("/api/shares/received", get(list_received_shares_handler))
        .route("/api/shares/granted", get(list_granted_shares_handler))
        .route("/api/shares/{id}", delete(revoke_share_handler))
        .route("/api/users/directory", get(list_user_directory_handler))
        .route_layer(from_fn_with_state(
            action_limiter.clone(),
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(auth_state.clone(), auth_middleware))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    let admin = if state.identity_mode == AuthIdentityMode::UserSession && !state.aionpro_mode {
        Router::new()
            .route(
                "/api/admin/users",
                get(list_admin_users_handler).post(create_admin_user_handler),
            )
            .route("/api/admin/users/{id}/username", patch(update_admin_username_handler))
            .route("/api/admin/users/{id}/role", patch(update_admin_role_handler))
            .route("/api/admin/users/{id}/status", patch(update_admin_status_handler))
            .route(
                "/api/admin/users/{id}/reset-password",
                post(reset_admin_password_handler),
            )
            .route(
                "/api/admin/users/{id}/sessions/revoke",
                post(revoke_admin_sessions_handler),
            )
            .route("/api/admin/audit", get(list_admin_audit_handler))
            .route_layer(from_fn_with_state(
                action_limiter.clone(),
                authenticated_action_rate_limit_middleware,
            ))
            .route_layer(axum::middleware::from_fn(admin_required_middleware))
            .route_layer(from_fn_with_state(auth_state, auth_middleware))
            .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
            .with_state(state.clone())
    } else {
        Router::new()
    };

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
        .merge(admin)
        .merge(api_action_limited)
        .merge(static_routes)
}

async fn list_admin_users_handler(
    State(state): State<AuthRouterState>,
    Query(query): Query<ListAdminUsersQuery>,
) -> Result<Json<ApiResponse<AdminUserListResponse>>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 100);
    let offset = query.offset.unwrap_or(0);
    let response = AdminUserService::new(state.admin_user_repo)
        .list_users(limit, offset)
        .await
        .map_err(admin_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

fn share_service(state: &AuthRouterState) -> ShareService {
    ShareService::new(state.share_repo.clone(), state.user_repo.clone())
}

fn share_service_error_to_api_error(err: ShareServiceError) -> ApiError {
    match err {
        ShareServiceError::Database(db) => db_error_to_api_error(db),
        ShareServiceError::NotFound(msg) => ApiError::NotFound(msg),
        ShareServiceError::Forbidden(msg) => ApiError::Forbidden(msg),
        ShareServiceError::Conflict(msg) => ApiError::Conflict(msg),
        ShareServiceError::BadRequest(msg) => ApiError::BadRequest(msg),
    }
}

async fn create_share_handler(
    State(state): State<AuthRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateShareRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ResourceShare>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let share = share_service(&state)
        .grant(&user.id, req)
        .await
        .map_err(share_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(share)))
}

async fn revoke_share_handler(
    State(state): State<AuthRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(share_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    share_service(&state)
        .revoke(&user.id, &share_id)
        .await
        .map_err(share_service_error_to_api_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_resource_shares_handler(
    State(state): State<AuthRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListSharesQuery>,
) -> Result<Json<ApiResponse<ShareListResponse>>, ApiError> {
    let response = share_service(&state)
        .list_for_resource(&user.id, query.resource_type, &query.resource_id)
        .await
        .map_err(share_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn list_received_shares_handler(
    State(state): State<AuthRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<ShareListResponse>>, ApiError> {
    let response = share_service(&state)
        .list_received_by(&user.id)
        .await
        .map_err(share_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn list_granted_shares_handler(
    State(state): State<AuthRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<ShareListResponse>>, ApiError> {
    let response = share_service(&state)
        .list_granted_by(&user.id)
        .await
        .map_err(share_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn list_user_directory_handler(
    State(state): State<AuthRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<UserDirectoryResponse>>, ApiError> {
    let response = share_service(&state)
        .list_directory(&user.id)
        .await
        .map_err(share_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn create_admin_user_handler(
    State(state): State<AuthRouterState>,
    Extension(actor): Extension<CurrentUser>,
    body: Result<Json<CreateAdminUserRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = AdminUserService::new(state.admin_user_repo)
        .create_user(
            &request.username,
            request.role,
            &audit_actor(&actor.id, &actor.username),
        )
        .await
        .map_err(admin_service_error_to_api_error)?;
    Ok(no_store_json(StatusCode::CREATED, ApiResponse::ok(response)))
}

async fn update_admin_username_handler(
    State(state): State<AuthRouterState>,
    Extension(actor): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAdminUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AdminUser>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let user = AdminUserService::new(state.admin_user_repo.clone())
        .update_username(&id, &request.username, &audit_actor(&actor.id, &actor.username))
        .await
        .map_err(admin_service_error_to_api_error)?;
    notify_session_revoked(&state, &id);
    Ok(Json(ApiResponse::ok(user)))
}

async fn update_admin_role_handler(
    State(state): State<AuthRouterState>,
    Extension(actor): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAdminRoleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AdminUser>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let user = AdminUserService::new(state.admin_user_repo.clone())
        .update_role(&id, request.role, &audit_actor(&actor.id, &actor.username))
        .await
        .map_err(admin_service_error_to_api_error)?;
    notify_session_revoked(&state, &id);
    Ok(Json(ApiResponse::ok(user)))
}

async fn update_admin_status_handler(
    State(state): State<AuthRouterState>,
    Extension(actor): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAdminStatusRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AdminUser>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let user = AdminUserService::new(state.admin_user_repo.clone())
        .update_status(&id, request.status, &audit_actor(&actor.id, &actor.username))
        .await
        .map_err(admin_service_error_to_api_error)?;
    notify_session_revoked(&state, &id);
    Ok(Json(ApiResponse::ok(user)))
}

async fn reset_admin_password_handler(
    State(state): State<AuthRouterState>,
    Extension(actor): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let response = AdminUserService::new(state.admin_user_repo.clone())
        .reset_password(&id, &audit_actor(&actor.id, &actor.username))
        .await
        .map_err(admin_service_error_to_api_error)?;
    notify_session_revoked(&state, &id);
    Ok(no_store_json(StatusCode::OK, ApiResponse::ok(response)))
}

async fn revoke_admin_sessions_handler(
    State(state): State<AuthRouterState>,
    Extension(actor): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AdminUser>>, ApiError> {
    let user = AdminUserService::new(state.admin_user_repo.clone())
        .revoke_sessions(&id, &audit_actor(&actor.id, &actor.username))
        .await
        .map_err(admin_service_error_to_api_error)?;
    notify_session_revoked(&state, &id);
    Ok(Json(ApiResponse::ok(user)))
}

async fn list_admin_audit_handler(
    State(state): State<AuthRouterState>,
    Query(query): Query<ListAdminAuditQuery>,
) -> Result<Json<ApiResponse<AdminAuditListResponse>>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let response = AdminUserService::new(state.admin_user_repo)
        .list_audit(query.cursor.as_deref(), limit)
        .await
        .map_err(admin_service_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

fn notify_session_revoked(state: &AuthRouterState, user_id: &str) {
    if let Some(hook) = &state.session_revoked_hook {
        hook(user_id);
    }
}

fn no_store_json<T: Serialize>(status: StatusCode, body: ApiResponse<T>) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn admin_service_error_to_api_error(error: AdminUserServiceError) -> ApiError {
    match error {
        AdminUserServiceError::Repository(AdminUserRepositoryError::LastActiveAdmin) => ApiError::coded(
            StatusCode::CONFLICT,
            "LAST_ACTIVE_ADMIN",
            "The last active administrator cannot be changed.",
            None,
        ),
        AdminUserServiceError::Repository(AdminUserRepositoryError::UnsupportedIdentity) => {
            ApiError::coded(StatusCode::NOT_FOUND, "USER_NOT_FOUND", "User not found.", None)
        }
        AdminUserServiceError::Repository(AdminUserRepositoryError::Database(error))
        | AdminUserServiceError::Database(error) => match error {
            DbError::NotFound(_) => ApiError::coded(StatusCode::NOT_FOUND, "USER_NOT_FOUND", "User not found.", None),
            DbError::Conflict(_) => {
                ApiError::coded(StatusCode::CONFLICT, "USERNAME_TAKEN", "Username already exists.", None)
            }
            other => db_error_to_api_error(other),
        },
        AdminUserServiceError::Validation(error) => ApiError::from(error),
        AdminUserServiceError::HashTask(error) => ApiError::Internal(format!("Password hashing failed: {error}")),
    }
}

// ---------------------------------------------------------------------------
// PUT /api/auth/internal/external-users/{external_user_id}
// ---------------------------------------------------------------------------

async fn ensure_external_user_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    Path(external_user_id): Path<String>,
    body: Result<Json<EnsureExternalUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<EnsureExternalUserResponse>>, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(req) = body.map_err(ApiError::from)?;
    let mut service = AuthProvisionService::new(state.user_repo, state.jwt_service);
    if let Some(fs_adopter) = state.fs_adopter {
        service = service.with_filesystem_adopter(fs_adopter);
    }
    let response = service
        .ensure_external_user(&external_user_id, req)
        .await
        .map_err(provision_error_to_api_error)?;
    tracing::info!(
        user_id = %response.user_id,
        user_type = ?response.user_type,
        "external user provision succeeded"
    );
    Ok(Json(ApiResponse::ok(response)))
}

// ---------------------------------------------------------------------------
// POST /api/auth/internal/external-sessions
// ---------------------------------------------------------------------------

async fn create_external_session_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Result<Json<EnsureExternalSessionRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(req) = body.map_err(ApiError::from)?;
    let service = AuthProvisionService::new(state.user_repo, state.jwt_service);
    let exchange = service
        .create_external_session(req)
        .await
        .map_err(provision_error_to_api_error)?;
    tracing::info!(
        user_id = %exchange.response.user.id,
        "external core session exchange succeeded"
    );
    let cookie = state.cookie_config.build_session_cookie(&exchange.token);
    Ok(([(header::SET_COOKIE, cookie)], Json(ApiResponse::ok(exchange.response))).into_response())
}

// ---------------------------------------------------------------------------
// POST /api/auth/internal/external-sessions/revoke
// ---------------------------------------------------------------------------

async fn revoke_external_session_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Result<Json<RevokeExternalSessionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RevokeExternalSessionResponse>>, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(req) = body.map_err(ApiError::from)?;
    let service = AuthProvisionService::new(state.user_repo, state.jwt_service);
    let response = service
        .revoke_external_session(req)
        .await
        .map_err(provision_error_to_api_error)?;
    tracing::info!(
        user_id = %response.user_id,
        session_generation = response.session_generation,
        "external core session revoked"
    );
    if let Some(hook) = &state.session_revoked_hook {
        hook(&response.user_id);
    }
    Ok(Json(ApiResponse::ok(response)))
}

// ---------------------------------------------------------------------------
// POST /login
// ---------------------------------------------------------------------------

async fn login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    if state.aionpro_mode {
        return Err(user_context_required());
    }

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
        Some(u) if u.user_type != UserType::Local || u.status != UserStatus::Active => {
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
        Some(u) if u.password_hash.as_deref().unwrap_or_default().trim().is_empty() => {
            // Seeded user with no password yet (first-run local mode).
            // Treat as invalid credentials; run dummy verify for timing symmetry
            // and to avoid bcrypt error on empty hash leaking as a 500.
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
        Some(u) => {
            let Some(password_hash) = u.password_hash.as_deref() else {
                let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
                return Err(ApiError::Unauthorized("Invalid username or password".into()));
            };
            let valid = verify_password_timed(&req.password, password_hash).await?;
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

    let (token, cookie) = issue_persistent_session(&state, &user).await?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.id);
    }

    let resp = LoginResponse::new(public_user(user), token);

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// POST /logout
// ---------------------------------------------------------------------------

async fn logout_handler(State(state): State<AuthRouterState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = extract_token_from_headers(&headers) {
        if let Ok(payload) = state.jwt_service.verify(&token)
            && let Some(session_id) = payload.session_id.as_deref()
        {
            state
                .user_repo
                .revoke_auth_session(session_id, &payload.user_id, "logout")
                .await
                .map_err(db_error_to_api_error)?;
        }
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
        .has_usable_admin()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    let user_count = state
        .admin_user_repo
        .count_managed_users()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    // Check authentication without requiring it
    let is_authenticated = if let Some(payload) =
        extract_token_from_headers(&headers).and_then(|token| state.jwt_service.verify(&token).ok())
    {
        match state.user_repo.find_active_by_id(&payload.user_id).await {
            Ok(Some(user)) if user.session_generation == payload.session_generation => {
                if let Some(session_id) = payload.session_id.as_deref() {
                    state
                        .user_repo
                        .is_auth_session_active(session_id, &user.id)
                        .await
                        .unwrap_or(false)
                } else {
                    true
                }
            }
            _ => false,
        }
    } else {
        false
    };

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
) -> Result<Json<ApiResponse<Vec<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let users = state.user_repo.list_users().await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(
        users.into_iter().map(InternalUserResponse::from).collect(),
    )))
}

async fn get_system_user_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Option<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state.user_repo.get_system_user().await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user.map(InternalUserResponse::from))))
}

async fn find_user_by_username_handler(
    State(state): State<AuthRouterState>,
    Path(username): Path<String>,
) -> Result<Json<ApiResponse<Option<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state
        .user_repo
        .find_by_username(&username)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user.map(InternalUserResponse::from))))
}

async fn find_user_by_id_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Option<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state.user_repo.find_by_id(&id).await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user.map(InternalUserResponse::from))))
}

async fn create_internal_user_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<CreateInternalUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<InternalUserResponse>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let user = state
        .user_repo
        .create_user(&req.username, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(InternalUserResponse::from(user))))
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
            role: map_site_role(user.site_role),
            status: map_account_status(user.status),
            must_change_password: user.must_change_password,
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
) -> Result<Response, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    // Validate new password strength
    validate_password_for_api(&req.new_password)?;

    // Fetch user record
    let user = state
        .user_repo
        .find_by_id(&current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::NotFound("User not found".into()))?;

    // Verify current password
    let Some(password_hash) = user.password_hash.as_deref() else {
        return Err(ApiError::Unauthorized("Current password is incorrect".into()));
    };
    let valid = verify_password_timed(&req.current_password, password_hash).await?;
    if !valid {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "INVALID_CURRENT_PASSWORD",
            "Current password is incorrect.",
            None,
        ));
    }
    if verify_password_timed(&req.new_password, password_hash).await? {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "PASSWORD_REUSED",
            "New password must differ from the current password.",
            None,
        ));
    }

    // Hash new password on blocking thread
    let password = req.new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    // Persist new password hash
    let updated = state
        .admin_user_repo
        .change_own_password(
            &current_user.id,
            &new_hash,
            &audit_actor(&current_user.id, &current_user.username),
        )
        .await
        .map_err(|error| admin_service_error_to_api_error(AdminUserServiceError::Repository(error)))?;
    notify_session_revoked(&state, &current_user.id);
    let (_token, cookie) = issue_persistent_session(&state, &updated).await?;
    if let Some(path) = &state.initial_admin_credentials_file {
        match initial_admin_credentials_belong_to(path, &current_user.username) {
            Ok(true) => {
                if let Err(error) = std::fs::remove_file(path.as_ref())
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %path.display(), error = %error, "failed to remove consumed initial admin credentials");
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "failed to validate consumed initial admin credentials");
            }
        }
    }
    Ok((
        [
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(ApiResponse::ok(public_user(updated))),
    )
        .into_response())
}

fn initial_admin_credentials_belong_to(path: &FsPath, username: &str) -> Result<bool, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let value: serde_json::Value = serde_json::from_reader(file).map_err(|error| error.to_string())?;
    Ok(value.get("username").and_then(serde_json::Value::as_str) == Some(username))
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

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "refresh token user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.aionpro_mode && user.user_type != aionui_db::UserType::Aionpro {
        return Err(user_context_required());
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    if let Some(session_id) = payload.session_id.as_deref()
        && !state
            .user_repo
            .is_auth_session_active(session_id, &user.id)
            .await
            .map_err(db_error_to_api_error)?
    {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    let session_id = if let Some(session_id) = payload.session_id {
        state
            .user_repo
            .touch_auth_session(&session_id, &user.id)
            .await
            .map_err(db_error_to_api_error)?;
        session_id
    } else {
        state
            .user_repo
            .create_auth_session(&user.id, aionui_common::now_ms() + crate::jwt::TOKEN_EXPIRY_MS)
            .await
            .map_err(db_error_to_api_error)?
    };
    let new_token = state
        .jwt_service
        .sign_with_session_id(
            &user.id,
            user.username.as_deref().unwrap_or("external_user"),
            user.session_generation,
            &session_id,
        )
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
    if state.aionpro_mode {
        return Err(user_context_required());
    }

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
    if user.user_type != UserType::Local
        || user.status != UserStatus::Active
        || user.password_hash.as_deref().unwrap_or_default().is_empty()
    {
        return Err(ApiError::Unauthorized("No active primary user configured".into()));
    }

    let (token, cookie) = issue_persistent_session(&state, &user).await?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.id);
    }

    let resp = LoginResponse::new(public_user(user), token);

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

fn public_user(user: User) -> PublicUser {
    PublicUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        role: map_site_role(user.site_role),
        status: map_account_status(user.status),
        must_change_password: user.must_change_password,
    }
}

fn map_site_role(role: aionui_db::SiteRole) -> UserRole {
    match role {
        aionui_db::SiteRole::Admin => UserRole::Admin,
        aionui_db::SiteRole::Member => UserRole::Member,
    }
}

fn map_account_status(status: UserStatus) -> AccountStatus {
    match status {
        UserStatus::Active => AccountStatus::Active,
        UserStatus::Disabled => AccountStatus::Disabled,
    }
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
        .admin_user_repo
        .change_own_password(&user.id, &new_hash, &aionui_db::AuditActor::system())
        .await
        .map_err(|error| admin_service_error_to_api_error(AdminUserServiceError::Repository(error)))?;
    notify_session_revoked(&state, &user.id);

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

    if user.username.as_deref() != Some(trimmed.as_str()) {
        state
            .admin_user_repo
            .update_managed_username(&user.id, &trimmed, &aionui_db::AuditActor::system())
            .await
            .map_err(|error| admin_service_error_to_api_error(AdminUserServiceError::Repository(error)))?;
        notify_session_revoked(&state, &user.id);
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
        .admin_user_repo
        .reset_managed_password(&user.id, &new_hash, &aionui_db::AuditActor::system())
        .await
        .map_err(|error| admin_service_error_to_api_error(AdminUserServiceError::Repository(error)))?;
    notify_session_revoked(&state, &user.id);

    Ok(Json(ApiResponse::ok(WebuiResetPasswordResponse { new_password })))
}

async fn issue_persistent_session(state: &AuthRouterState, user: &User) -> Result<(String, String), ApiError> {
    let expires_at = aionui_common::now_ms() + crate::jwt::TOKEN_EXPIRY_MS;
    let session_id = state
        .user_repo
        .create_auth_session(&user.id, expires_at)
        .await
        .map_err(db_error_to_api_error)?;
    let token = state
        .jwt_service
        .sign_with_session_id(
            &user.id,
            user.username.as_deref().unwrap_or("external_user"),
            user.session_generation,
            &session_id,
        )
        .map_err(|error| ApiError::Internal(format!("Token signing error: {error}")))?;
    let cookie = state.cookie_config.build_session_cookie(&token);
    Ok((token, cookie))
}

fn validate_password_for_api(password: &str) -> Result<(), ApiError> {
    validate_password(password).map_err(|error| {
        let message = error.to_string();
        let code = if message.contains("at least") {
            "PASSWORD_TOO_SHORT"
        } else if message.contains("exceed") {
            "PASSWORD_TOO_LONG"
        } else {
            "PASSWORD_TOO_COMMON"
        };
        ApiError::coded(StatusCode::BAD_REQUEST, code, message, None)
    })
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

    #[test]
    fn bootstrap_credentials_are_only_consumed_by_the_matching_user() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        std::fs::write(&path, r#"{"username":"initial-admin","temporary_password":"secret"}"#).unwrap();
        assert!(initial_admin_credentials_belong_to(&path, "initial-admin").unwrap());
        assert!(!initial_admin_credentials_belong_to(&path, "other-admin").unwrap());
    }
}
