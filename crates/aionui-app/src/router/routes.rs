//! Top-level router assembly: middleware stack + module route merges.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use tower_http::cors::{AllowCredentials, AllowOrigin, Any, CorsLayer};

use aionui_ai_agent::{
    RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION, agent_routes, remote_agent_routes,
};
use aionui_api_types::ErrorResponse;
use aionui_assets::{AssetRouterState, asset_routes};
use aionui_assistant::assistant_routes;
use aionui_auth::{
    AuthIdentityMode, AuthRouterState, AuthState, IRuntimeTokenVerifier, SystemDefaultFilesystemAdopter,
    auth_middleware, auth_routes, csrf_middleware, security_headers_middleware,
};
use aionui_channel::channel_routes;
#[cfg(feature = "weixin")]
use aionui_channel::weixin_login_route;
use aionui_common::ApiErrorLogContext;
use aionui_conversation::{conversation_ops_routes, conversation_routes};
use aionui_cron::cron_routes;
use aionui_extension::{extension_routes, hub_routes, skill_routes};
use aionui_file::file_routes;
use aionui_mcp::mcp_routes;
use aionui_office::{office_proxy_routes, office_routes};
use aionui_project::project_routes;
use aionui_realtime::{NoopMessageRouter, WsHandlerState, ws_upgrade_handler};
use aionui_shell::shell_routes;
use aionui_system::{ClientPrefService, connection_test_routes, system_routes};
use aionui_team::{TeamSessionService, team_routes};

use crate::services::AppServices;

use super::fs_monitor::spawn_fs_monitor;
use super::health::health_check;
use super::runtime_team_tools::{RuntimeTeamToolsState, runtime_team_tools_routes};
use super::scm_monitor::{CompositeMessageRouter, spawn_scm_monitor};
use super::state::{ModuleStates, RouterBuildError, build_module_states, build_ws_state};
use super::trace::with_access_log;

pub struct RouterRuntime {
    pub client_pref_service: ClientPrefService,
    pub team_service: Arc<TeamSessionService>,
}

/// Create the application router with all routes and global middleware.
///
/// Middleware stack (outermost → innermost):
/// 1. Security response headers (X-Frame-Options, etc.)
/// 2. CSRF protection (Double Submit Cookie)
/// 3. Route handlers (auth routes + system routes + conversation routes + file routes + health check)
pub async fn create_router(services: &AppServices) -> Result<Router, RouterBuildError> {
    let (router, _runtime) = create_router_with_runtime(services).await?;
    Ok(router)
}

/// Create the application router and return runtime handles needed by
/// background services started outside the router tree.
pub async fn create_router_with_runtime(services: &AppServices) -> Result<(Router, RouterRuntime), RouterBuildError> {
    let boot = Instant::now();
    tracing::info!("startup: router assembly started");

    // Bridge event bus → WebSocket manager: forward all broadcast events
    // to connected WebSocket clients.
    let mut event_rx = services.event_bus.subscribe();
    let ws_manager = services.ws_manager.clone();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            if let Some(user_id) = event
                .data
                .get("user_id")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
            {
                ws_manager.broadcast_to_user(&user_id, event);
            } else if is_global_websocket_event(&event.name) {
                ws_manager.broadcast_all(event);
            } else {
                tracing::warn!(
                    event_name = %event.name,
                    "dropping websocket event without user_id; add user_id to payload or whitelist explicit global event"
                );
            }
        }
    });

    let (states, channel_components) = build_module_states(services).await?;
    let client_pref_service = states.system.client_pref_service.clone();
    let team_service = states.team.service.clone();
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: module states built");

    // Start channel orchestrator (message loop)
    tokio::spawn(
        channel_components
            .orchestrator
            .run(channel_components.message_rx, channel_components.confirm_rx),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel orchestrator spawned"
    );

    // Restore enabled channel plugins (starts receiving IM messages)
    let chan_mgr = channel_components.manager;
    let chan_factory = channel_components.plugin_factory;
    let restore_owner_user_ids = channel_components.restore_owner_user_ids;
    tokio::spawn(async move {
        if restore_owner_user_ids.is_empty() {
            tracing::info!(
                stage = "channel.restore",
                "skipping channel plugin restore until an owner user is available"
            );
            return;
        }

        for owner_user_id in restore_owner_user_ids {
            if let Err(e) = chan_mgr.restore_plugins(&owner_user_id, &chan_factory).await {
                tracing::warn!(
                    code = "BOOTSTRAP_DEGRADED_CHANNEL_RESTORE",
                    stage = "channel.restore",
                    owner_user_id = %owner_user_id,
                    error = %e,
                    "failed to restore channel plugins"
                );
            }
        }
    });
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel plugin restore scheduled"
    );

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route tree build started"
    );
    // Spawn the Project Explorer filesystem monitor and install its inbound
    // router (fs/* frames). Built here — inside the runtime — because the actor
    // runs as a background task. The sync test-only assembly path keeps a no-op.
    let fs_router = spawn_fs_monitor(Arc::new(services.project_service.clone()), services.ws_manager.clone());
    // Source control shares the connection but owns its own envelope name, so the
    // two inbound routers are composed behind the realtime layer's single slot.
    let scm_router = spawn_scm_monitor(Arc::new(services.project_service.clone()), services.ws_manager.clone());
    let inbound_router: Arc<dyn aionui_realtime::MessageRouter> = match scm_router {
        Some(scm) => Arc::new(CompositeMessageRouter::new(vec![fs_router, scm])),
        None => fs_router,
    };
    let ws_state = build_ws_state(services, inbound_router);
    let router = create_router_with_all_state(services, states, ws_state);
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: router assembly completed"
    );
    Ok((
        router,
        RouterRuntime {
            client_pref_service,
            team_service,
        },
    ))
}

/// Create the application router with custom module states.
///
/// Used for testing when specific service overrides are needed
/// (e.g. injecting a mock HTTP server URL for version check).
pub fn create_router_with_states(services: &AppServices, states: ModuleStates) -> Router {
    // No-op inbound router: this sync assembly path is for HTTP-focused tests and
    // does not spawn the fs monitor (which requires a runtime task).
    let ws_state = build_ws_state(services, Arc::new(NoopMessageRouter));
    create_router_with_all_state(services, states, ws_state)
}

/// Create the application router with custom module states and WebSocket state.
///
/// Full-control variant used by tests that need to override
/// module services and WebSocket behaviour.
pub fn create_router_with_all_state(services: &AppServices, states: ModuleStates, ws_state: WsHandlerState) -> Router {
    let boot = Instant::now();
    tracing::info!("startup: route tree build with states started");

    let auth_state = AuthRouterState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        admin_user_repo: services.admin_user_repo.clone(),
        share_repo: services.share_repo.clone(),
        initial_admin_credentials_file: services.initial_admin_credentials_file.clone(),
        fs_adopter: Some(Arc::new(SkillFilesystemAdopter {
            skill_paths: services.skill_paths.clone(),
            skill_repo: services.skill_repo.clone(),
        })),
        cookie_config: services.cookie_config.clone(),
        qr_token_store: services.qr_token_store.clone(),
        identity_mode: auth_identity_mode(services.identity_mode),
        bootstrap_secret: services.bootstrap_secret.clone(),
        session_revoked_hook: {
            let ws_manager = services.ws_manager.clone();
            let conversation_service = states.conversation.service.clone();
            let team_service = states.team.service.clone();
            let channel_manager = states.channel.manager.clone();
            let channel_session_manager = states.channel.session_manager.clone();
            let office_watch_manager = states.office.watch_manager.clone();
            Some(Arc::new(move |user_id: &str| {
                ws_manager.disconnect_user(user_id, "session revoked");
                let stopped_team_sessions = team_service.stop_sessions_for_user(user_id);
                if stopped_team_sessions > 0 {
                    tracing::info!(
                        user_id = %user_id,
                        stopped_team_sessions,
                        "stopped team sessions after session revocation"
                    );
                }
                let user_id = user_id.to_owned();
                let conversation_service = conversation_service.clone();
                let channel_manager = channel_manager.clone();
                let channel_session_manager = channel_session_manager.clone();
                let office_watch_manager = office_watch_manager.clone();
                tokio::spawn(async move {
                    channel_manager.shutdown_for_user(&user_id).await;
                    office_watch_manager.stop_all_for_user(&user_id);
                    if let Err(err) = channel_session_manager.clear_all_sessions(&user_id).await {
                        tracing::warn!(
                            user_id = %user_id,
                            error = %err,
                            "failed to clear channel sessions after session revocation"
                        );
                    }
                    if let Err(err) = conversation_service.terminate_runtime_for_user(&user_id).await {
                        tracing::warn!(
                            user_id = %user_id,
                            error = %err,
                            "failed to terminate runtimes after session revocation"
                        );
                    }
                });
            }))
        },
        local: services.local,
        aionpro_mode: services.identity_mode == crate::config::IdentityMode::AionPro,
    };

    let auth_mw_state = AuthState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        identity_mode: auth_identity_mode(services.identity_mode),
        runtime_token_verifier: Some(Arc::new(ConversationHelperTokenVerifier {
            runtime_token_service: services.runtime_token_service.clone(),
        })),
    };

    // System routes protected by auth middleware
    let system_authenticated =
        system_routes(states.system).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Conversation routes protected by auth middleware
    let conversation_authenticated = conversation_routes(states.conversation.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    let conversation_ops_authenticated = conversation_ops_routes(states.conversation)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Remote agent routes protected by auth middleware
    let remote_agent_authenticated = remote_agent_routes(states.remote_agent)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Unified agent listing/refresh/test routes protected by auth middleware
    let agent_authenticated =
        agent_routes(states.agent).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Connection test routes (Bedrock, Gemini) protected by auth middleware
    let connection_test_authenticated = connection_test_routes(states.connection_test)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // File routes protected by auth middleware
    let file_authenticated =
        file_routes(states.file).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Project control-plane routes protected by auth middleware
    let project_authenticated =
        project_routes(states.project).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // MCP routes protected by auth middleware
    let mcp_authenticated =
        mcp_routes(states.mcp).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Extension routes protected by auth middleware
    let extension_authenticated =
        extension_routes(states.extension).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Hub routes protected by auth middleware
    let hub_authenticated =
        hub_routes(states.hub).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Skill routes protected by auth middleware
    let skill_authenticated =
        skill_routes(states.skill).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Channel routes protected by auth middleware
    #[cfg(feature = "weixin")]
    let weixin_login_authenticated = weixin_login_route(states.channel.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
    let channel_authenticated =
        channel_routes(states.channel).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Team routes protected by auth middleware
    let team_authenticated =
        team_routes(states.team.clone()).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Cron routes protected by auth middleware
    let cron_authenticated =
        cron_routes(states.cron).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Office routes protected by auth middleware
    let office_authenticated =
        office_routes(states.office.clone()).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Shell + STT routes protected by auth middleware
    let shell_authenticated =
        shell_routes(states.shell).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Assistant routes protected by auth middleware (T1a skeleton: all
    // handlers return 500 "not implemented"; T1b wires real service)
    let assistant_authenticated =
        assistant_routes(states.assistant).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Office proxy routes serve iframe content but still require auth so
    // preview ports remain scoped to the active Core user.
    let office_proxy =
        office_proxy_routes(states.office).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
    let public_assets = asset_routes(AssetRouterState::default());

    // WebSocket upgrade route — exempt from CSRF (no cookie-based
    // double-submit) but still gets security response headers.
    let ws_routes = Router::new().route("/ws", get(ws_upgrade_handler)).with_state(ws_state);
    let runtime_team_tools = runtime_team_tools_routes(RuntimeTeamToolsState {
        team_service: states.team.service.clone(),
        runtime_token_service: services.runtime_token_service.clone(),
    });
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: route groups built");

    // Antigravity permission hook callback. Deliberately NOT behind
    // auth_middleware: the hook is a local process with no user session and
    // presents a per-conversation token instead (checked in the handler).
    let antigravity_hook = crate::router::antigravity_hook::antigravity_hook_routes(
        crate::router::antigravity_hook::AntigravityHookRouterState {
            task_manager: services.worker_task_manager.clone(),
            tokens: services.antigravity_hook_tokens.clone(),
        },
    );

    let router = Router::new()
        .merge(antigravity_hook)
        .route("/health", get(health_check))
        .merge(auth_routes(auth_state))
        .merge(system_authenticated)
        .merge(conversation_authenticated)
        .merge(conversation_ops_authenticated)
        .merge(remote_agent_authenticated)
        .merge(agent_authenticated)
        .merge(connection_test_authenticated)
        .merge(file_authenticated)
        .merge(project_authenticated)
        .merge(mcp_authenticated)
        .merge(extension_authenticated)
        .merge(hub_authenticated)
        .merge(skill_authenticated)
        .merge(channel_authenticated)
        .merge(team_authenticated)
        .merge(cron_authenticated)
        .merge(office_authenticated)
        .merge(shell_authenticated)
        .merge(assistant_authenticated);

    // Conditionally merge WeChat login SSE route (feature-gated)
    #[cfg(feature = "weixin")]
    let router = router.merge(weixin_login_authenticated);

    let router = if services.identity_mode.is_local() {
        router
    } else {
        router.layer(middleware::from_fn_with_state(
            services.cookie_config.clone(),
            csrf_middleware,
        ))
    }
    .merge(ws_routes)
    .merge(runtime_team_tools)
    .merge(office_proxy)
    .merge(public_assets)
    .layer(middleware::from_fn(security_headers_middleware));

    // Raise the default request body limit from axum's 2MB default to
    // `BODY_LIMIT` (10MB). Routes that need a larger cap (e.g. `/api/fs/upload`)
    // disable this default and install their own `RequestBodyLimitLayer`.
    let router = router.layer(DefaultBodyLimit::max(aionui_common::constants::BODY_LIMIT));
    let router = router.layer(middleware::from_fn(normalize_boundary_error_response));

    let router = with_access_log(router);
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route tree build with states completed"
    );

    let allowed_origins = effective_allowed_origins(services);
    let router = match services.identity_mode {
        crate::config::IdentityMode::Local => router
            .layer(middleware::from_fn_with_state(
                LocalClientSecretPolicy::new(
                    services
                        .local_client_secret
                        .clone()
                        .expect("validated Local services must carry a client secret"),
                ),
                local_client_secret_guard,
            ))
            .layer(middleware::from_fn_with_state(
                NativeOriginPolicy {
                    allowed: allowed_origins.clone(),
                },
                native_origin_guard,
            )),
        crate::config::IdentityMode::AionPro => router.layer(middleware::from_fn_with_state(
            NativeOriginPolicy {
                allowed: allowed_origins.clone(),
            },
            native_origin_guard,
        )),
        crate::config::IdentityMode::WebUi => router,
    };

    match services.identity_mode {
        crate::config::IdentityMode::Local => {
            let cors = CorsLayer::new()
                .allow_origin(AllowOrigin::list(allowed_origins.iter().cloned()))
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers(Any);
            router.layer(cors)
        }
        crate::config::IdentityMode::WebUi => {
            // The WebUI is served by the AionUI web host on the same origin.
            // Do not opt credentialed browser requests into cross-origin access.
            router
        }
        crate::config::IdentityMode::AionPro => {
            if allowed_origins.is_empty() {
                return router;
            }
            // AionPro uses an external renderer that needs cookies and CSRF
            // headers. Only operator-configured exact origins are trusted.
            let credential_origins = allowed_origins.clone();
            let cors = CorsLayer::new()
                .allow_origin(AllowOrigin::list(allowed_origins.iter().cloned()))
                .allow_credentials(AllowCredentials::predicate(move |origin, _| {
                    credential_origins.iter().any(|allowed| allowed == origin)
                }))
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::AUTHORIZATION,
                    HeaderName::from_static("x-csrf-token"),
                ]);
            router.layer(cors)
        }
    }
}

#[derive(Clone)]
struct NativeOriginPolicy {
    allowed: Arc<[HeaderValue]>,
}

#[derive(Clone)]
struct LocalClientSecretPolicy {
    secret: Arc<str>,
    websocket_protocol: Arc<str>,
}

impl LocalClientSecretPolicy {
    fn new(secret: Arc<str>) -> Self {
        Self {
            websocket_protocol: Arc::from(format!("aionui-local-v1.{secret}")),
            secret,
        }
    }
}

fn effective_allowed_origins(services: &AppServices) -> Arc<[HeaderValue]> {
    let mut values = Vec::new();
    if services.identity_mode == crate::config::IdentityMode::Local {
        values.push(HeaderValue::from_static("null"));
    }
    for origin in services.allowed_origins.iter() {
        if let Ok(value) = HeaderValue::try_from(origin.as_str())
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    values.into()
}

async fn native_origin_guard(State(policy): State<NativeOriginPolicy>, request: Request, next: Next) -> Response {
    if request.uri().path() == "/api/ws-token" || is_websocket_upgrade(&request) {
        let mut origins = request.headers().get_all(header::ORIGIN).iter();
        if let Some(origin) = origins.next()
            && (origins.next().is_some() || !policy.allowed.iter().any(|allowed| allowed == origin))
        {
            return (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new(
                    "Request origin is not allowed.",
                    "ORIGIN_NOT_ALLOWED",
                )),
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn local_client_secret_guard(
    State(policy): State<LocalClientSecretPolicy>,
    request: Request,
    next: Next,
) -> Response {
    let health_exempt = request.method() == Method::GET && request.uri().path() == "/health";
    let authenticated = if is_websocket_upgrade(&request) {
        websocket_protocol_matches(request.headers(), &policy.websocket_protocol)
    } else {
        header_secret_matches(request.headers(), &policy.secret)
    };
    if !health_exempt && !authenticated {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "Local client capability required.",
                "LOCAL_CLIENT_SECRET_REQUIRED",
            )),
        )
            .into_response();
    }
    next.run(request).await
}

fn is_websocket_upgrade(request: &Request) -> bool {
    request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

fn header_secret_matches(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all("x-aionui-local-secret").iter();
    let Some(actual) = values.next() else {
        return false;
    };
    values.next().is_none() && constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn websocket_protocol_matches(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let mut matched: Option<&str> = None;
    for value in headers.get_all(header::SEC_WEBSOCKET_PROTOCOL) {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for protocol in value.split(',').map(str::trim) {
            if protocol.is_empty() || matched.replace(protocol).is_some() {
                return false;
            }
        }
    }
    matched.is_some_and(|actual| constant_time_eq(actual.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for idx in 0..max_len {
        let left = left.get(idx).copied().unwrap_or(0);
        let right = right.get(idx).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

/// Adapter running the on-disk side of AionUi → AionPro adoption over the
/// skill filesystem (auth crate cannot depend on the extension/filesystem).
struct SkillFilesystemAdopter {
    skill_paths: Arc<aionui_extension::SkillPaths>,
    skill_repo: Arc<dyn aionui_db::ISkillRepository>,
}

#[async_trait::async_trait]
impl SystemDefaultFilesystemAdopter for SkillFilesystemAdopter {
    async fn adopt_filesystem(&self, adopter_user_id: &str) {
        aionui_extension::fs_adopt::adopt_user_filesystem(
            self.skill_paths.as_ref(),
            self.skill_repo.as_ref(),
            adopter_user_id,
        )
        .await;
    }
}

/// Adapter exposing the agent runtime's token service to the auth middleware
/// as the conversation-helper credential channel (aionui-auth cannot depend on
/// aionui-ai-agent, so the binding happens here in the composition layer).
struct ConversationHelperTokenVerifier {
    runtime_token_service: Arc<RuntimeTokenService>,
}

impl IRuntimeTokenVerifier for ConversationHelperTokenVerifier {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool {
        self.runtime_token_service
            .validate(
                Some(token),
                user_id,
                conversation_id,
                RuntimeTokenScope::ConversationHelper,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .is_ok()
    }
}

fn auth_identity_mode(identity_mode: crate::config::IdentityMode) -> AuthIdentityMode {
    match identity_mode {
        crate::config::IdentityMode::Local => AuthIdentityMode::Local,
        crate::config::IdentityMode::WebUi => AuthIdentityMode::UserSession,
        crate::config::IdentityMode::AionPro => AuthIdentityMode::AionPro,
    }
}

fn is_global_websocket_event(event_name: &str) -> bool {
    matches!(
        event_name,
        "runtime.statusChanged" | "extensions.lifecycle" | "hub.state-changed"
    )
}

async fn normalize_boundary_error_response(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if response.status().is_success() || response_has_json_content_type(&response) {
        return response;
    }

    let status = response.status();
    let Some((error, code)) = boundary_error_for_status(status) else {
        return response;
    };

    let original_headers = response.headers().clone();
    let mut normalized = (status, Json(ErrorResponse::new(error, code))).into_response();
    normalized.extensions_mut().insert(ApiErrorLogContext {
        code,
        message: error.to_owned(),
    });
    for (name, value) in original_headers.iter() {
        if *name != header::CONTENT_TYPE && *name != header::CONTENT_LENGTH {
            normalized.headers_mut().insert(name, value.clone());
        }
    }
    normalized
}

fn response_has_json_content_type(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("application/json"))
}

fn boundary_error_for_status(status: StatusCode) -> Option<(&'static str, &'static str)> {
    match status {
        StatusCode::BAD_REQUEST => Some(("Bad request.", "BAD_REQUEST")),
        StatusCode::UNAUTHORIZED => Some(("Unauthorized.", "UNAUTHORIZED")),
        StatusCode::FORBIDDEN => Some(("Forbidden.", "FORBIDDEN")),
        StatusCode::NOT_FOUND => Some(("Route not found.", "NOT_FOUND")),
        StatusCode::METHOD_NOT_ALLOWED => Some(("Method not allowed.", "METHOD_NOT_ALLOWED")),
        StatusCode::CONFLICT => Some(("Conflict.", "CONFLICT")),
        StatusCode::GONE => Some(("Gone.", "GONE")),
        StatusCode::PAYLOAD_TOO_LARGE => Some(("Request body is too large.", "PAYLOAD_TOO_LARGE")),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => Some(("Unsupported media type.", "UNSUPPORTED_MEDIA_TYPE")),
        StatusCode::UNPROCESSABLE_ENTITY => Some(("Unprocessable entity.", "UNPROCESSABLE_ENTITY")),
        StatusCode::TOO_MANY_REQUESTS => Some(("Rate limited", "RATE_LIMITED")),
        StatusCode::INTERNAL_SERVER_ERROR => Some(("Internal server error.", "INTERNAL_ERROR")),
        StatusCode::BAD_GATEWAY => Some(("Upstream service unavailable.", "BAD_GATEWAY")),
        StatusCode::GATEWAY_TIMEOUT => Some(("Request timed out.", "GATEWAY_TIMEOUT")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::{boundary_error_for_status, create_router_with_runtime, is_global_websocket_event};
    use crate::config::{AppConfig, IdentityMode};
    use crate::services::AppServices;

    const TEST_LOCAL_CLIENT_SECRET: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGH012345678";
    const TEST_LOCAL_WS_PROTOCOL: &str = "aionui-local-v1.abcdefghijklmnopqrstuvwxyzABCDEFGH012345678";

    fn local_websocket_request(path: &str, origin: Option<&str>, protocol: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .uri(path)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
            .header(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn boundary_error_for_status_covers_common_fallback_statuses() {
        let cases = [
            (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            (StatusCode::FORBIDDEN, "FORBIDDEN"),
            (StatusCode::NOT_FOUND, "NOT_FOUND"),
            (StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED"),
            (StatusCode::CONFLICT, "CONFLICT"),
            (StatusCode::GONE, "GONE"),
            (StatusCode::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE"),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "UNSUPPORTED_MEDIA_TYPE"),
            (StatusCode::UNPROCESSABLE_ENTITY, "UNPROCESSABLE_ENTITY"),
            (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),
            (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
            (StatusCode::BAD_GATEWAY, "BAD_GATEWAY"),
            (StatusCode::GATEWAY_TIMEOUT, "GATEWAY_TIMEOUT"),
        ];

        for (status, code) in cases {
            let (_, actual_code) = boundary_error_for_status(status).expect("status should be normalized");
            assert_eq!(actual_code, code);
        }
    }

    #[test]
    fn extension_enablement_events_are_user_scoped() {
        assert!(!is_global_websocket_event("extensions.state-changed"));
        assert!(is_global_websocket_event("extensions.lifecycle"));
    }

    #[tokio::test]
    async fn create_router_with_runtime_exposes_team_service_for_background_coordinators() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();

        let (_router, _runtime) = create_router_with_runtime(&services)
            .await
            .expect("router runtime should build");
    }

    async fn router_for_identity_mode(identity_mode: IdentityMode) -> (axum::Router, AppServices, TempDir) {
        router_for_identity_mode_with_origins(identity_mode, &[]).await
    }

    async fn router_for_identity_mode_with_origins(
        identity_mode: IdentityMode,
        allowed_origins: &[&str],
    ) -> (axum::Router, AppServices, TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            data_dir: temp_dir.path().join("data"),
            work_dir: temp_dir.path().join("work"),
            local: identity_mode == IdentityMode::Local,
            identity_mode,
            bootstrap_secret: (identity_mode == IdentityMode::AionPro).then(|| "test-bootstrap-secret".to_string()),
            local_client_secret: (identity_mode == IdentityMode::Local).then(|| TEST_LOCAL_CLIENT_SECRET.to_string()),
            allowed_origins: allowed_origins.iter().map(|value| (*value).to_string()).collect(),
            ..AppConfig::default()
        };
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &config).await.unwrap();
        let router = super::create_router(&services).await.unwrap();
        (router, services, temp_dir)
    }

    fn assert_no_cors_opt_in(response: &axum::response::Response) {
        assert!(response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none()
        );
    }

    #[tokio::test]
    async fn webui_cross_origin_response_does_not_opt_in_to_cors() {
        let (router, services, _temp_dir) = router_for_identity_mode(IdentityMode::WebUi).await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_no_cors_opt_in(&response);
        services.database.close().await;
    }

    #[tokio::test]
    async fn webui_cross_origin_login_preflight_is_not_approved() {
        let (router, services, _temp_dir) = router_for_identity_mode(IdentityMode::WebUi).await;
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/login")
                    .header(header::ORIGIN, "https://attacker.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, Method::POST.as_str())
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_no_cors_opt_in(&response);
        services.database.close().await;
    }

    #[tokio::test]
    async fn webui_originless_direct_login_still_works() {
        let (router, services, _temp_dir) = router_for_identity_mode(IdentityMode::WebUi).await;
        let password_hash = aionui_auth::hash_password("test-password").unwrap();
        services
            .user_repo
            .set_system_user_credentials("admin", &password_hash)
            .await
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"test-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_no_cors_opt_in(&response);
        services.database.close().await;
    }

    #[tokio::test]
    async fn local_mode_only_allows_packaged_null_origin_by_default() {
        let (router, services, _temp_dir) = router_for_identity_mode(IdentityMode::Local).await;
        let attacker = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(attacker.status(), StatusCode::OK);
        assert_no_cors_opt_in(&attacker);

        let packaged = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "null")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(packaged.status(), StatusCode::OK);
        assert_eq!(
            packaged.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&header::HeaderValue::from_static("null"))
        );
        assert!(
            packaged
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none()
        );
        services.database.close().await;
    }

    #[tokio::test]
    async fn local_mode_allows_an_explicit_dev_origin() {
        let (router, services, _temp_dir) =
            router_for_identity_mode_with_origins(IdentityMode::Local, &["http://127.0.0.1:5173"]).await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "http://127.0.0.1:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&header::HeaderValue::from_static("http://127.0.0.1:5173"))
        );
        services.database.close().await;
    }

    #[tokio::test]
    async fn aionpro_only_allows_configured_credentialed_cors() {
        let (router, services, _temp_dir) =
            router_for_identity_mode_with_origins(IdentityMode::AionPro, &["https://desktop.example"]).await;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "https://desktop.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&header::HeaderValue::from_static("https://desktop.example"))
        );
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            Some(&header::HeaderValue::from_static("true"))
        );

        let attacker = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_no_cors_opt_in(&attacker);
        services.database.close().await;
    }

    #[tokio::test]
    async fn local_ws_token_and_websocket_reject_unlisted_origins_but_allow_native_origins() {
        let (router, services, _temp_dir) = router_for_identity_mode(IdentityMode::Local).await;
        let local_jwt = services.jwt_service.sign("system_default_user", "local_user").unwrap();
        let missing_secret = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/settings")
                    .header(header::ORIGIN, "null")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_secret.status(), StatusCode::UNAUTHORIZED);

        let missing_ws_token_secret = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ws-token")
                    .header(header::ORIGIN, "null")
                    .header(header::AUTHORIZATION, format!("Bearer {local_jwt}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_ws_token_secret.status(), StatusCode::UNAUTHORIZED);

        let allowed_http = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ws-token")
                    .header(header::ORIGIN, "null")
                    .header("x-aionui-local-secret", TEST_LOCAL_CLIENT_SECRET)
                    .header(header::AUTHORIZATION, format!("Bearer {local_jwt}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if allowed_http.status() != StatusCode::OK {
            let status = allowed_http.status();
            let body = to_bytes(allowed_http.into_body(), usize::MAX).await.unwrap();
            panic!(
                "allowed local ws-token failed with {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }

        let attacker_http = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ws-token")
                    .header(header::ORIGIN, "https://attacker.example")
                    .header("x-aionui-local-secret", TEST_LOCAL_CLIENT_SECRET)
                    .header(header::AUTHORIZATION, format!("Bearer {local_jwt}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(attacker_http.status(), StatusCode::FORBIDDEN);

        for path in ["/ws", "/api/stt/stream"] {
            let missing_protocol = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::ORIGIN, "null")
                        .header(header::CONNECTION, "upgrade")
                        .header(header::UPGRADE, "websocket")
                        .header(header::SEC_WEBSOCKET_VERSION, "13")
                        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(missing_protocol.status(), StatusCode::UNAUTHORIZED, "{path}");

            let attacker = router
                .clone()
                .oneshot(local_websocket_request(
                    path,
                    Some("https://attacker.example"),
                    TEST_LOCAL_WS_PROTOCOL,
                ))
                .await
                .unwrap();
            assert_eq!(attacker.status(), StatusCode::FORBIDDEN, "{path}");

            let wrong_secret = router
                .clone()
                .oneshot(local_websocket_request(
                    path,
                    Some("null"),
                    "aionui-local-v1.abcdefghijklmnopqrstuvwxyzABCDEFGH012345679",
                ))
                .await
                .unwrap();
            assert_eq!(wrong_secret.status(), StatusCode::UNAUTHORIZED, "{path}");

            let packaged = router
                .clone()
                .oneshot(local_websocket_request(path, Some("null"), TEST_LOCAL_WS_PROTOCOL))
                .await
                .unwrap();
            assert_ne!(packaged.status(), StatusCode::FORBIDDEN, "{path}");
            assert_ne!(packaged.status(), StatusCode::UNAUTHORIZED, "{path}");

            let originless = router
                .clone()
                .oneshot(local_websocket_request(path, None, TEST_LOCAL_WS_PROTOCOL))
                .await
                .unwrap();
            assert_ne!(originless.status(), StatusCode::FORBIDDEN, "{path}");
            assert_ne!(originless.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
        services.database.close().await;
    }

    #[tokio::test]
    async fn aionpro_ws_token_and_websocket_enforce_the_configured_origin() {
        let (router, services, _temp_dir) =
            router_for_identity_mode_with_origins(IdentityMode::AionPro, &["https://desktop.example"]).await;
        let attacker_token = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ws-token")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(attacker_token.status(), StatusCode::FORBIDDEN);
        let allowed_token = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ws-token")
                    .header(header::ORIGIN, "https://desktop.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(allowed_token.status(), StatusCode::FORBIDDEN);

        for path in ["/ws", "/api/stt/stream"] {
            let attacker = router
                .clone()
                .oneshot(local_websocket_request(
                    path,
                    Some("https://attacker.example"),
                    "invalid-session-token",
                ))
                .await
                .unwrap();
            assert_eq!(attacker.status(), StatusCode::FORBIDDEN, "{path}");

            let allowed = router
                .clone()
                .oneshot(local_websocket_request(
                    path,
                    Some("https://desktop.example"),
                    "invalid-session-token",
                ))
                .await
                .unwrap();
            assert_ne!(allowed.status(), StatusCode::FORBIDDEN, "{path}");
        }
        services.database.close().await;
    }
}
