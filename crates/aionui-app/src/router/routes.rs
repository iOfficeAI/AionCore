//! Top-level router assembly: middleware stack + module route merges.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::http::{HeaderName, Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use aionui_ai_agent::{
    RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION, agent_routes, remote_agent_routes,
};
use aionui_api_types::{ErrorResponse, WebSocketMessage};
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
use aionui_realtime::{NoopMessageRouter, WebSocketManager, WsHandlerState, ws_upgrade_handler};
use aionui_shell::shell_routes;
use aionui_sidebar::sidebar_routes;
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

async fn forward_event_bus_to_websocket(
    mut event_rx: tokio::sync::broadcast::Receiver<WebSocketMessage<serde_json::Value>>,
    ws_manager: Arc<WebSocketManager>,
) {
    loop {
        let event = match event_rx.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "websocket event bus bridge lagged; skipped stale events and will continue"
                );
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

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
    let event_rx = services.event_bus.subscribe();
    let ws_manager = services.ws_manager.clone();
    tokio::spawn(forward_event_bus_to_websocket(event_rx, ws_manager));

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
    let chan_owner_user_id = channel_components.owner_user_id;
    tokio::spawn(async move {
        if let Some(chan_owner_user_id) = chan_owner_user_id {
            if let Err(e) = chan_mgr.restore_plugins(&chan_owner_user_id, &chan_factory).await {
                tracing::warn!(
                    code = "BOOTSTRAP_DEGRADED_CHANNEL_RESTORE",
                    stage = "channel.restore",
                    owner_user_id = %chan_owner_user_id,
                    error = %e,
                    "failed to restore channel plugins"
                );
            }
        } else {
            tracing::info!(
                stage = "channel.restore",
                "skipping channel plugin restore until an owner user is available"
            );
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

    // Sidebar read + ordering routes protected by auth middleware
    let sidebar_authenticated =
        sidebar_routes(states.sidebar).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

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
        .merge(sidebar_authenticated)
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

    if services.identity_mode.is_local() {
        let cors = CorsLayer::new()
            .allow_origin(Any)
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
    } else {
        // Non-local (external identity) mode: the desktop renderer is a
        // cross-origin browser context (localhost:5173 in dev, file:// when
        // packaged) authenticating with the session cookie, so responses must
        // opt in to credentialed CORS. Credentialed mode forbids wildcards:
        // reflect the request origin and enumerate headers explicitly
        // (x-csrf-token is required by the CSRF double-submit middleware).
        let cors = CorsLayer::new()
            .allow_origin(AllowOrigin::mirror_request())
            .allow_credentials(true)
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
    use std::sync::Arc;

    use aionui_api_types::WebSocketMessage;
    use aionui_realtime::{BroadcastEventBus, EventBroadcaster, WebSocketManager, WsOutbound};
    use axum::http::StatusCode;
    use serde_json::json;

    use super::{
        boundary_error_for_status, create_router_with_runtime, forward_event_bus_to_websocket,
        is_global_websocket_event,
    };
    use crate::config::AppConfig;
    use crate::services::AppServices;

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

    #[tokio::test]
    async fn websocket_event_bridge_continues_after_receiver_lag() {
        let event_bus = BroadcastEventBus::new(2);
        let event_rx = event_bus.subscribe();
        let ws_manager = Arc::new(WebSocketManager::new());
        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel(8);
        ws_manager.add_client_for_user("user-a".into(), "token".into(), outbound_tx);

        for sequence in 1..=3 {
            event_bus.broadcast(WebSocketMessage::new(
                "test.beforeLag",
                json!({"user_id": "user-a", "sequence": sequence}),
            ));
        }

        let bridge = tokio::spawn(forward_event_bus_to_websocket(event_rx, ws_manager));
        event_bus.broadcast(WebSocketMessage::new(
            "test.afterLag",
            json!({"user_id": "user-a", "sequence": 4}),
        ));

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                match outbound_rx.recv().await {
                    Some(WsOutbound::Text(text)) => {
                        let event: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if event["name"] == "test.afterLag" {
                            break event;
                        }
                    }
                    other => panic!("expected a text websocket event, got {other:?}"),
                }
            }
        })
        .await
        .expect("the bridge should deliver events after recovering from lag");

        assert_eq!(delivered["data"]["sequence"], 4);
        bridge.abort();
    }
}
