#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, BatchImportMcpServersRequest, CreateMcpServerRequest, DetectedMcpServerResponse, ErrorResponse,
    McpConnectionTestErrorCode, McpServerResponse, OAuthCheckStatusRequest, OAuthLoginRequest, OAuthLoginResponse,
    OAuthLogoutRequest, OAuthStatusResponse, TestMcpConnectionRequest, UpdateMcpServerRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::SiteRole;

use crate::connection_test::McpConnectionTestService;
use crate::error::McpError;
use crate::oauth_service::McpOAuthService;
use crate::service::McpConfigService;
use crate::sync_service::McpSyncService;
use crate::types::McpServerTransport;

impl From<McpError> for ApiError {
    fn from(err: McpError) -> Self {
        match err {
            McpError::NotFound(msg) => ApiError::NotFound(msg),
            McpError::Conflict(msg) => ApiError::Conflict(msg),
            McpError::InvalidEdit(msg) => ApiError::BadRequest(msg),
            McpError::InvalidTransport(msg) => ApiError::BadRequest(msg),
            McpError::AgentNotInstalled(msg) => ApiError::BadRequest(msg),
            McpError::AgentOperationFailed(msg) => ApiError::Internal(msg),
            McpError::ConnectionFailed(msg) => ApiError::BadGateway(msg),
            McpError::OAuth(msg) => ApiError::Internal(format!("OAuth error: {msg}")),
            McpError::Database(db_err) => ApiError::Internal(db_err.to_string()),
            McpError::Json(e) => ApiError::Internal(format!("JSON error: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for MCP route handlers.
#[derive(Clone)]
pub struct McpRouterState {
    pub config_service: McpConfigService,
    pub sync_service: McpSyncService,
    pub connection_test_service: McpConnectionTestService,
    pub oauth_service: McpOAuthService,
}

fn require_infrastructure_admin(user: &CurrentUser) -> Result<(), ApiError> {
    if user.site_role != SiteRole::Admin {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "ADMIN_REQUIRED",
            "Administrator access required.",
            None,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the MCP router with all `/api/mcp/*` routes.
///
/// Includes CRUD routes, agent config detection, connection tests, and OAuth.
/// All routes require authentication (applied by the caller).
pub fn mcp_routes(state: McpRouterState) -> Router {
    Router::new()
        .route("/api/mcp/servers", get(list_servers).post(add_server))
        .route("/api/mcp/servers/import", post(batch_import))
        .route(
            "/api/mcp/servers/{id}",
            get(get_server).put(edit_server).delete(delete_server),
        )
        .route("/api/mcp/servers/{id}/toggle", post(toggle_server))
        // Connection test route
        .route("/api/mcp/test-connection", post(test_connection))
        // Agent config discovery route
        .route("/api/mcp/agent-configs", get(get_agent_configs))
        // OAuth routes
        .route("/api/mcp/oauth/check-status", post(oauth_check_status))
        .route("/api/mcp/oauth/login", post(oauth_login))
        .route("/api/mcp/oauth/logout", post(oauth_logout))
        .route("/api/mcp/oauth/authenticated", get(oauth_authenticated))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// CRUD Handlers
// ---------------------------------------------------------------------------

/// `GET /api/mcp/servers` — list all MCP servers.
async fn list_servers(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<McpServerResponse>>>, ApiError> {
    let servers = state
        .config_service
        .list_servers(&user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(servers)))
}

/// `GET /api/mcp/servers/:id` — get a single MCP server.
async fn get_server(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpServerResponse>>, ApiError> {
    let server = state
        .config_service
        .get_server(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(server)))
}

/// `POST /api/mcp/servers` — create (or upsert by name) an MCP server.
async fn add_server(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateMcpServerRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<McpServerResponse>>), ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let server = state
        .config_service
        .add_server(&user.id, req)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(server))))
}

/// `PUT /api/mcp/servers/:id` — partial update an MCP server.
async fn edit_server(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateMcpServerRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<McpServerResponse>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let server = state
        .config_service
        .edit_server(&user.id, &id, req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(server)))
}

/// `DELETE /api/mcp/servers/:id` — delete an MCP server.
async fn delete_server(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .config_service
        .delete_server(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/mcp/servers/:id/toggle` — toggle enabled state.
async fn toggle_server(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpServerResponse>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let server = state
        .config_service
        .toggle_server(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(server)))
}

/// `POST /api/mcp/servers/import` — batch import MCP servers.
async fn batch_import(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<BatchImportMcpServersRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<McpServerResponse>>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let servers = state
        .config_service
        .batch_import(&user.id, req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(servers)))
}

// ---------------------------------------------------------------------------
// Connection Test Handler
// ---------------------------------------------------------------------------

/// `POST /api/mcp/test-connection` — test MCP server connectivity.
///
/// Creates a temporary MCP client, connects, lists tools, and closes.
async fn test_connection(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<TestMcpConnectionRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let transport = McpServerTransport::from(req.transport);
    let result = state
        .connection_test_service
        .test_connection_with_runtime_scope(
            &req.name,
            &transport,
            Some(&user.id),
            req.runtime_scope_id.as_deref().or(req.id.as_deref()),
        )
        .await;
    if let Some(server_id) = req.id.as_deref() {
        state
            .config_service
            .persist_test_result(&user.id, server_id, &result)
            .await
            .map_err(ApiError::from)?;
    }
    if result.success || result.needs_auth == Some(true) {
        return Ok(Json(ApiResponse::ok(result)).into_response());
    }

    let status = result
        .code
        .map(connection_test_failure_status)
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let error = result
        .error
        .clone()
        .unwrap_or_else(|| "MCP connection test failed".to_string());
    let code = result
        .code
        .map(McpConnectionTestErrorCode::as_str)
        .unwrap_or("MCP_CONNECTION_FAILED");

    Ok((
        status,
        Json(ErrorResponse::new_with_details(error, code, result.details.clone())),
    )
        .into_response())
}

fn connection_test_failure_status(code: McpConnectionTestErrorCode) -> StatusCode {
    match code {
        McpConnectionTestErrorCode::CommandNotFound
        | McpConnectionTestErrorCode::CommandPermissionDenied
        | McpConnectionTestErrorCode::CommandStartFailed => StatusCode::UNPROCESSABLE_ENTITY,
        McpConnectionTestErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        McpConnectionTestErrorCode::ConnectionFailed
        | McpConnectionTestErrorCode::HttpError
        | McpConnectionTestErrorCode::RpcError
        | McpConnectionTestErrorCode::ProtocolError => StatusCode::BAD_GATEWAY,
    }
}

// ---------------------------------------------------------------------------
// Agent Sync Handlers
// ---------------------------------------------------------------------------

/// `GET /api/mcp/agent-configs` — scan all installed Agent CLIs
/// and return their current MCP server configurations.
async fn get_agent_configs(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<DetectedMcpServerResponse>>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let configs = state
        .sync_service
        .get_agent_configs(&user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(configs)))
}

// ---------------------------------------------------------------------------
// OAuth Handlers
// ---------------------------------------------------------------------------

/// `POST /api/mcp/oauth/check-status` — check OAuth authentication status.
async fn oauth_check_status(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<OAuthCheckStatusRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OAuthStatusResponse>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let status = state
        .oauth_service
        .check_oauth_status(&user.id, &req.server_url)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(status)))
}

/// `POST /api/mcp/oauth/login` — start OAuth PKCE login flow.
///
/// Discovers endpoints, opens the browser for authorization, waits for
/// the callback, and exchanges the code for tokens.
async fn oauth_login(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<OAuthLoginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OAuthLoginResponse>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .oauth_service
        .login(&user.id, &req.server_url)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

/// `POST /api/mcp/oauth/logout` — delete stored OAuth token.
async fn oauth_logout(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<OAuthLogoutRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .oauth_service
        .logout(&user.id, &req.server_url)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

/// `GET /api/mcp/oauth/authenticated` — list server URLs with stored tokens.
async fn oauth_authenticated(
    State(state): State<McpRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<String>>>, ApiError> {
    require_infrastructure_admin(&user)?;
    let urls = state
        .oauth_service
        .get_authenticated_servers(&user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(urls)))
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;
    use std::sync::Arc;

    use aionui_db::{SiteRole, SqliteMcpServerRepository, SqliteOAuthTokenRepository, UserStatus, UserType};
    use aionui_realtime::BroadcastEventBus;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use tower::ServiceExt;

    async fn make_state() -> McpRouterState {
        let db = aionui_db::init_database_memory().await.unwrap();
        let mcp_repo = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
        let oauth_repo = Arc::new(SqliteOAuthTokenRepository::new(db.pool().clone()));
        let http_client = reqwest::Client::new();
        McpRouterState {
            config_service: McpConfigService::new(mcp_repo.clone()),
            sync_service: McpSyncService::new(mcp_repo, Vec::new()),
            connection_test_service: McpConnectionTestService::new(
                http_client.clone(),
                Arc::new(BroadcastEventBus::new(16)),
            ),
            oauth_service: McpOAuthService::new(oauth_repo, http_client),
        }
    }

    fn current_user(site_role: SiteRole) -> CurrentUser {
        CurrentUser {
            id: "user_route-test".into(),
            username: "route-test".into(),
            user_type: UserType::Local,
            status: UserStatus::Active,
            site_role,
            must_change_password: false,
        }
    }

    async fn response_code(response: Response) -> String {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn json_request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap()
    }

    #[tokio::test]
    async fn member_cannot_run_connection_tests_or_scan_host_agent_configs() {
        let app = mcp_routes(make_state().await).layer(Extension(current_user(SiteRole::Member)));
        let connection_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp/test-connection")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(connection_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_code(connection_response).await, "ADMIN_REQUIRED");

        let configs_response = app
            .oneshot(
                Request::builder()
                    .uri("/api/mcp/agent-configs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(configs_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_code(configs_response).await, "ADMIN_REQUIRED");
    }

    #[tokio::test]
    async fn member_cannot_persist_or_authorize_executable_mcp_configuration() {
        let app = mcp_routes(make_state().await).layer(Extension(current_user(SiteRole::Member)));
        let requests = [
            json_request("POST", "/api/mcp/servers"),
            json_request("PUT", "/api/mcp/servers/server-1"),
            json_request("POST", "/api/mcp/servers/server-1/toggle"),
            json_request("POST", "/api/mcp/servers/import"),
            json_request("POST", "/api/mcp/oauth/check-status"),
            json_request("POST", "/api/mcp/oauth/login"),
            json_request("POST", "/api/mcp/oauth/logout"),
            Request::builder()
                .uri("/api/mcp/oauth/authenticated")
                .body(Body::empty())
                .unwrap(),
        ];

        for request in requests {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response_code(response).await, "ADMIN_REQUIRED");
        }
    }

    #[tokio::test]
    async fn admin_can_scan_agent_configs_and_reaches_connection_validation() {
        let app = mcp_routes(make_state().await).layer(Extension(current_user(SiteRole::Admin)));
        let configs_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/mcp/agent-configs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(configs_response.status(), StatusCode::OK);

        let connection_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp/test-connection")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(connection_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response_code(connection_response).await, "BAD_REQUEST");
    }

    #[test]
    fn not_found_maps_to_app_not_found() {
        let err = ApiError::from(McpError::NotFound("mcp_123".into()));
        assert!(matches!(err, ApiError::NotFound(msg) if msg == "mcp_123"));
    }

    #[test]
    fn conflict_maps_to_app_conflict() {
        let err = ApiError::from(McpError::Conflict("test-server".into()));
        assert!(matches!(err, ApiError::Conflict(_)));
    }

    #[test]
    fn invalid_transport_maps_to_bad_request() {
        let err = ApiError::from(McpError::InvalidTransport("missing command".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_edit_maps_to_bad_request() {
        let err = ApiError::from(McpError::InvalidEdit("rename forbidden".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn agent_not_installed_maps_to_bad_request() {
        let err = ApiError::from(McpError::AgentNotInstalled("claude".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn agent_operation_failed_maps_to_internal() {
        let err = ApiError::from(McpError::AgentOperationFailed("exit code 1".into()));
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn connection_failed_maps_to_bad_gateway() {
        let err = ApiError::from(McpError::ConnectionFailed("timeout".into()));
        assert!(matches!(err, ApiError::BadGateway(_)));
    }

    #[test]
    fn oauth_maps_to_internal() {
        let err = ApiError::from(McpError::OAuth("discovery failed".into()));
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err = ApiError::from(McpError::Json(json_err));
        assert!(matches!(err, ApiError::Internal(_)));
    }
}
