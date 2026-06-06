#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use axum::routing::{get, post};
use tracing::warn;

use aionui_api_types::{
    ApiResponse, ApprovePairingRequest, BridgeResponse, ChannelSessionResponse, ChannelUserResponse,
    DisablePluginRequest, EnablePluginRequest, PairingRequestResponse, PluginStatusResponse, RejectPairingRequest,
    RevokeUserRequest, SyncChannelSettingsRequest, TestPluginRequest, TestPluginResponse,
};
use aionui_common::ApiError;
use aionui_db::{DbError, IChannelRepository};
use aionui_extension::{ExtensionRegistry, ResolvedChannelPlugin};
use serde::Serialize;

use crate::channel_settings::ChannelSettingsService;
use crate::error::ChannelError;
use crate::manager::{ChannelManager, PluginFactory};
use crate::pairing::PairingService;
use crate::session::SessionManager;
use crate::types::{PluginConfig, PluginConfigOptions, PluginCredentials, PluginType};

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for channel route handlers.
#[derive(Clone)]
pub struct ChannelRouterState {
    pub manager: Arc<ChannelManager>,
    pub pairing_service: Arc<PairingService>,
    pub session_manager: Arc<SessionManager>,
    pub repo: Arc<dyn IChannelRepository>,
    pub plugin_factory: Arc<PluginFactory>,
    pub settings_service: Arc<ChannelSettingsService>,
    pub extension_registry: ExtensionRegistry,
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

impl From<ChannelError> for ApiError {
    fn from(err: ChannelError) -> Self {
        match err {
            ChannelError::PluginNotFound(msg) => ApiError::NotFound(msg),
            ChannelError::InvalidPluginType(msg) => ApiError::BadRequest(msg),
            ChannelError::PluginAlreadyRunning(msg) => ApiError::Conflict(msg),
            ChannelError::InvalidConfig(msg) => ApiError::BadRequest(msg),
            ChannelError::ConnectionFailed(msg) => ApiError::BadGateway(msg),
            ChannelError::PairingNotFound(msg) => ApiError::NotFound(msg),
            ChannelError::PairingExpired(msg) => ApiError::BadRequest(msg),
            ChannelError::PairingAlreadyProcessed(msg) => ApiError::BadRequest(msg),
            ChannelError::UserNotFound(msg) => ApiError::NotFound(msg),
            ChannelError::UserNotAuthorized(msg) => ApiError::Forbidden(msg),
            ChannelError::SessionNotFound(msg) => ApiError::NotFound(msg),
            ChannelError::EncryptionFailed(msg) => ApiError::Internal(msg),
            ChannelError::DecryptionFailed(msg) => ApiError::Internal(msg),
            ChannelError::PlatformApi(msg) => ApiError::BadGateway(msg),
            ChannelError::MessageSendFailed(msg) => ApiError::Internal(msg),
            ChannelError::Database(db_err) => db_error_to_api_error(db_err),
            ChannelError::Json(e) => ApiError::Internal(format!("JSON error: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the channel router with all `/api/channel/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn channel_routes(state: ChannelRouterState) -> Router {
    Router::new()
        // Plugin management
        .route("/api/channel/plugins", get(get_plugin_status))
        .route("/api/channel/plugins/enable", post(enable_plugin))
        .route("/api/channel/plugins/disable", post(disable_plugin))
        .route("/api/channel/plugins/test", post(test_plugin))
        // Pairing management
        .route("/api/channel/pairings", get(get_pending_pairings))
        .route("/api/channel/pairings/approve", post(approve_pairing))
        .route("/api/channel/pairings/reject", post(reject_pairing))
        // User management
        .route("/api/channel/users", get(get_authorized_users))
        .route("/api/channel/users/revoke", post(revoke_user))
        // Session management
        .route("/api/channel/sessions", get(get_active_sessions))
        // Settings sync
        .route("/api/channel/settings/sync", post(sync_channel_settings))
        .with_state(state)
}

/// Build the WeChat login SSE route (feature-gated).
///
/// Separated from `channel_routes` because it's behind the `weixin` feature.
#[cfg(feature = "weixin")]
pub fn weixin_login_route(state: ChannelRouterState) -> Router {
    Router::new()
        .route("/api/channel/weixin/login", get(weixin_login_sse))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Plugin management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/plugins` — get status of all registered plugins.
async fn get_plugin_status(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<ChannelPluginStatusView>>>, ApiError> {
    let statuses = state.manager.get_plugin_status().await?;
    let extension_plugins = state.extension_registry.get_channel_plugins().await;

    let extension_map: HashMap<String, ResolvedChannelPlugin> = extension_plugins
        .into_iter()
        .map(|plugin| (plugin.id.clone(), plugin))
        .collect();

    let builtin_names: [(&str, &str); 7] = [
        ("telegram", "Telegram"),
        ("lark", "Lark"),
        ("dingtalk", "DingTalk"),
        ("slack", "Slack"),
        ("discord", "Discord"),
        ("weixin", "WeChat"),
        ("wecom", "WeCom"),
    ];
    let builtin_types: std::collections::HashSet<&str> = builtin_names.iter().map(|(id, _)| *id).collect();

    let mut status_map: HashMap<String, ChannelPluginStatusView> = HashMap::new();

    for status in statuses {
        let plugin_type = status.plugin_type.clone();
        let is_extension = !builtin_types.contains(plugin_type.as_str());

        if is_extension && !extension_map.contains_key(&plugin_type) {
            continue;
        }

        status_map.insert(
            plugin_type.clone(),
            ChannelPluginStatusView::from_manager_status(
                status,
                is_extension
                    .then(|| extension_map.get(&plugin_type).map(ChannelExtensionMetaView::from))
                    .flatten(),
            ),
        );
    }

    for plugin in extension_map.values() {
        status_map
            .entry(plugin.id.clone())
            .or_insert_with(|| ChannelPluginStatusView::extension_placeholder(plugin));
    }

    for (plugin_type, display_name) in builtin_names {
        status_map
            .entry(plugin_type.to_string())
            .or_insert_with(|| ChannelPluginStatusView::builtin_placeholder(plugin_type, display_name));
    }

    let mut merged: Vec<ChannelPluginStatusView> = status_map.into_values().collect();
    merged.sort_by(|left, right| left.plugin_type.cmp(&right.plugin_type));

    Ok(Json(ApiResponse::ok(merged)))
}

#[derive(Debug, Clone, Serialize)]
struct ChannelPluginStatusView {
    plugin_id: String,
    #[serde(rename = "type")]
    plugin_type: String,
    name: String,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_connected: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
    connected: bool,
    has_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_username: Option<String>,
    active_users: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_extension: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extension_meta: Option<ChannelExtensionMetaView>,
}

#[derive(Debug, Clone, Serialize)]
struct ChannelExtensionMetaView {
    #[serde(rename = "credentialFields")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    credential_fields: Vec<serde_json::Value>,
    #[serde(rename = "configFields")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_fields: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(rename = "extensionName")]
    extension_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

impl ChannelPluginStatusView {
    fn from_manager_status(status: PluginStatusResponse, extension_meta: Option<ChannelExtensionMetaView>) -> Self {
        Self {
            plugin_id: status.plugin_id,
            plugin_type: status.plugin_type,
            name: status.name,
            enabled: status.enabled,
            status: status.status,
            last_connected: status.last_connected,
            created_at: Some(status.created_at),
            updated_at: Some(status.updated_at),
            connected: status.connected,
            has_token: status.has_token,
            bot_username: status.bot_username,
            active_users: status.active_users,
            is_extension: extension_meta.as_ref().map(|_| true),
            extension_meta,
        }
    }

    fn extension_placeholder(plugin: &ResolvedChannelPlugin) -> Self {
        Self {
            plugin_id: plugin.id.clone(),
            plugin_type: plugin.id.clone(),
            name: plugin.name.clone(),
            enabled: false,
            status: Some("stopped".to_string()),
            last_connected: None,
            created_at: None,
            updated_at: None,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
            is_extension: Some(true),
            extension_meta: Some(ChannelExtensionMetaView::from(plugin)),
        }
    }

    fn builtin_placeholder(plugin_type: &str, display_name: &str) -> Self {
        Self {
            plugin_id: plugin_type.to_string(),
            plugin_type: plugin_type.to_string(),
            name: display_name.to_string(),
            enabled: false,
            status: Some("stopped".to_string()),
            last_connected: None,
            created_at: None,
            updated_at: None,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
            is_extension: Some(false),
            extension_meta: None,
        }
    }
}

impl From<&ResolvedChannelPlugin> for ChannelExtensionMetaView {
    fn from(plugin: &ResolvedChannelPlugin) -> Self {
        Self {
            credential_fields: plugin.credential_fields.clone(),
            config_fields: plugin.config_fields.clone(),
            description: plugin.description.clone(),
            extension_name: plugin.extension_name.clone(),
            icon: plugin.icon.clone(),
        }
    }
}

/// `POST /api/channel/plugins/enable` — enable a plugin with config.
async fn enable_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<EnablePluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    if let Some(extension_plugin) = resolve_extension_channel_plugin(&state, &req.plugin_id).await {
        let config = build_extension_config(&extension_plugin, &req.config)?;
        match state
            .manager
            .enable_extension_plugin(&req.plugin_id, &extension_plugin.name, &config)
            .await
        {
            Ok(()) => {
                return Ok(Json(ApiResponse::ok(BridgeResponse {
                    success: true,
                    message: Some("Plugin enabled".into()),
                    error: None,
                })));
            }
            Err(e) => {
                warn!(plugin_id = %req.plugin_id, error = %e, "enable extension plugin failed");
                return Ok(Json(ApiResponse::ok(BridgeResponse {
                    success: false,
                    message: None,
                    error: Some(e.to_string()),
                })));
            }
        }
    }

    match state
        .manager
        .enable_plugin(&req.plugin_id, &req.config, state.plugin_factory.as_ref())
        .await
    {
        Ok(()) => Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some("Plugin enabled".into()),
            error: None,
        }))),
        Err(e) => {
            warn!(plugin_id = %req.plugin_id, error = %e, "enable plugin failed");
            Ok(Json(ApiResponse::ok(BridgeResponse {
                success: false,
                message: None,
                error: Some(e.to_string()),
            })))
        }
    }
}

/// `POST /api/channel/plugins/disable` — disable a plugin.
async fn disable_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<DisablePluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    if resolve_extension_channel_plugin(&state, &req.plugin_id).await.is_some()
        && state
            .repo
            .get_plugin(&req.plugin_id)
            .await
            .map_err(db_error_to_api_error)?
            .is_none()
    {
        return Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some("Plugin disabled".into()),
            error: None,
        })));
    }

    match state.manager.disable_plugin(&req.plugin_id).await {
        Ok(()) => Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some("Plugin disabled".into()),
            error: None,
        }))),
        Err(e) => {
            warn!(plugin_id = %req.plugin_id, error = %e, "disable plugin failed");
            Ok(Json(ApiResponse::ok(BridgeResponse {
                success: false,
                message: None,
                error: Some(e.to_string()),
            })))
        }
    }
}

/// `POST /api/channel/plugins/test` — test plugin credentials.
async fn test_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<TestPluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TestPluginResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    if let Some(extension_plugin) = resolve_extension_channel_plugin(&state, &req.plugin_id).await {
        let _config = build_extension_test_config(&extension_plugin, &req)?;
        return Ok(Json(ApiResponse::ok(TestPluginResponse {
            success: true,
            bot_username: None,
            error: None,
        })));
    }

    let config = build_test_config(&req);

    match state
        .manager
        .test_plugin(&req.plugin_id, config, state.plugin_factory.as_ref())
        .await
    {
        Ok(bot_username) => Ok(Json(ApiResponse::ok(TestPluginResponse {
            success: true,
            bot_username,
            error: None,
        }))),
        Err(e) => Ok(Json(ApiResponse::ok(TestPluginResponse {
            success: false,
            bot_username: None,
            error: Some(e.to_string()),
        }))),
    }
}

// ---------------------------------------------------------------------------
// Pairing management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/pairings` — get all pending pairing requests.
async fn get_pending_pairings(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<PairingRequestResponse>>>, ApiError> {
    let rows = state.pairing_service.get_pending_pairings().await?;
    let responses: Vec<PairingRequestResponse> = rows
        .into_iter()
        .map(|r| PairingRequestResponse {
            code: r.code,
            platform_user_id: r.platform_user_id,
            platform_type: r.platform_type,
            display_name: r.display_name,
            requested_at: r.requested_at,
            expires_at: r.expires_at,
        })
        .collect();
    Ok(Json(ApiResponse::ok(responses)))
}

/// `POST /api/channel/pairings/approve` — approve a pairing request.
async fn approve_pairing(
    State(state): State<ChannelRouterState>,
    body: Result<Json<ApprovePairingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    state.pairing_service.approve_pairing(&req.code).await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some("Pairing approved".into()),
        error: None,
    })))
}

/// `POST /api/channel/pairings/reject` — reject a pairing request.
async fn reject_pairing(
    State(state): State<ChannelRouterState>,
    body: Result<Json<RejectPairingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    state.pairing_service.reject_pairing(&req.code).await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some("Pairing rejected".into()),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// User management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/users` — get all authorized users.
async fn get_authorized_users(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<ChannelUserResponse>>>, ApiError> {
    let rows = state.repo.get_all_users().await.map_err(db_error_to_api_error)?;
    let responses: Vec<ChannelUserResponse> = rows
        .into_iter()
        .map(|r| ChannelUserResponse {
            id: r.id,
            platform_user_id: r.platform_user_id,
            platform_type: r.platform_type,
            display_name: r.display_name,
            authorized_at: r.authorized_at,
            last_active: r.last_active,
        })
        .collect();
    Ok(Json(ApiResponse::ok(responses)))
}

/// `POST /api/channel/users/revoke` — revoke a user's authorization.
///
/// Also cleans up the user's sessions.
async fn revoke_user(
    State(state): State<ChannelRouterState>,
    body: Result<Json<RevokeUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    // Clean up sessions first
    state.session_manager.cleanup_user_sessions(&req.user_id).await?;

    // Delete user record
    state
        .repo
        .delete_user(&req.user_id)
        .await
        .map_err(db_error_to_api_error)?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some("User revoked".into()),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// Session management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/sessions` — get all active sessions.
async fn get_active_sessions(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<ChannelSessionResponse>>>, ApiError> {
    let rows = state.session_manager.get_active_sessions().await?;
    let responses: Vec<ChannelSessionResponse> = rows
        .into_iter()
        .map(|r| ChannelSessionResponse {
            id: r.id,
            user_id: r.user_id,
            agent_type: r.agent_type,
            conversation_id: r.conversation_id,
            workspace: r.workspace,
            chat_id: r.chat_id,
            created_at: r.created_at,
            last_activity: r.last_activity,
        })
        .collect();
    Ok(Json(ApiResponse::ok(responses)))
}

// ---------------------------------------------------------------------------
// Settings sync handler
// ---------------------------------------------------------------------------

/// `POST /api/channel/settings/sync` — invalidate channel sessions.
///
/// Clears all sessions so they are recreated with the latest
/// agent/model configuration on the next incoming message.
/// Agent/model config is persisted separately via `PUT /api/settings/client`.
async fn sync_channel_settings(
    State(state): State<ChannelRouterState>,
    body: Result<Json<SyncChannelSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let _platform = PluginType::from_str_opt(&req.platform)
        .ok_or_else(|| ApiError::BadRequest(format!("Invalid platform: {}", req.platform)))?;

    state.session_manager.clear_all_sessions().await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some(format!("Sessions cleared for {}", req.platform)),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// WeChat login SSE handler
// ---------------------------------------------------------------------------

/// `GET /api/channel/weixin/login` — start WeChat QR code login SSE stream.
#[cfg(feature = "weixin")]
async fn weixin_login_sse(State(_state): State<ChannelRouterState>) -> impl axum::response::IntoResponse {
    use std::convert::Infallible;

    use axum::response::sse::{Event, KeepAlive, Sse};

    use tokio::sync::mpsc;

    use crate::plugins::weixin::WeixinLoginEvent;
    use crate::plugins::weixin::weixin_login_stream;

    let rx = weixin_login_stream();

    let sse_stream = futures_util::stream::unfold(rx, |mut rx: mpsc::Receiver<WeixinLoginEvent>| async move {
        match rx.recv().await {
            Some(event) => {
                let sse_event = Event::default().event(event.event_name()).data(event.to_json_data());
                Some((Ok::<_, Infallible>(sse_event), rx))
            }
            None => None,
        }
    });

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Build a `PluginConfig` from a `TestPluginRequest`.
///
/// Maps the `token` and optional `extra_config` fields into the
/// correct credential fields based on the plugin type.
fn build_test_config(req: &TestPluginRequest) -> PluginConfig {
    let mut credentials = PluginCredentials {
        token: None,
        app_id: None,
        app_secret: None,
        encrypt_key: None,
        verification_token: None,
        client_id: None,
        client_secret: None,
        account_id: None,
        bot_token: None,
        extra: HashMap::new(),
    };

    match req.plugin_id.as_str() {
        "lark" => {
            if let Some(ref extra) = req.extra_config {
                credentials.app_id = extra.app_id.clone();
                credentials.app_secret = extra.app_secret.clone();
            }
            credentials.token = Some(req.token.clone());
        }
        "dingtalk" => {
            credentials.client_id = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.client_secret = extra.app_secret.clone();
            }
        }
        "weixin" => {
            credentials.bot_token = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.account_id = extra.app_id.clone();
            }
        }
        _ => {
            // Default: use token field (Telegram)
            credentials.token = Some(req.token.clone());
        }
    }

    PluginConfig {
        credentials,
        config: None,
    }
}

async fn resolve_extension_channel_plugin(
    state: &ChannelRouterState,
    plugin_id: &str,
) -> Option<ResolvedChannelPlugin> {
    state
        .extension_registry
        .get_channel_plugins()
        .await
        .into_iter()
        .find(|plugin| plugin.id == plugin_id)
}

fn build_extension_test_config(
    plugin: &ResolvedChannelPlugin,
    req: &TestPluginRequest,
) -> Result<PluginConfig, ChannelError> {
    let mut map = serde_json::Map::new();
    if !req.token.is_empty() {
        map.insert("token".to_string(), serde_json::Value::String(req.token.clone()));
    }
    if let Some(extra) = &req.extra_config {
        if let Some(app_id) = &extra.app_id {
            map.insert("appId".to_string(), serde_json::Value::String(app_id.clone()));
        }
        if let Some(app_secret) = &extra.app_secret {
            map.insert("appSecret".to_string(), serde_json::Value::String(app_secret.clone()));
        }
    }
    build_extension_config(plugin, &serde_json::Value::Object(map))
}

fn build_extension_config(
    plugin: &ResolvedChannelPlugin,
    raw: &serde_json::Value,
) -> Result<PluginConfig, ChannelError> {
    let object = raw
        .as_object()
        .ok_or_else(|| ChannelError::InvalidConfig("Extension plugin config must be an object".into()))?;

    let mut credentials = PluginCredentials {
        token: None,
        app_id: None,
        app_secret: None,
        encrypt_key: None,
        verification_token: None,
        client_id: None,
        client_secret: None,
        account_id: None,
        bot_token: None,
        extra: HashMap::new(),
    };
    let mut config_extra = HashMap::new();

    let credential_keys: std::collections::HashSet<String> = plugin
        .credential_fields
        .iter()
        .filter_map(field_key)
        .map(ToOwned::to_owned)
        .collect();
    for field in &plugin.config_fields {
        if let Some((key, value)) = field_default_entry(field) {
            config_extra.entry(key.to_string()).or_insert(value);
        }
    }

    for (key, value) in object {
        if credential_keys.contains(key) {
            credentials.extra.insert(key.clone(), value.clone());
        } else {
            config_extra.insert(key.clone(), value.clone());
        }
    }

    Ok(PluginConfig {
        credentials,
        config: if config_extra.is_empty() {
            None
        } else {
            Some(PluginConfigOptions {
                mode: None,
                webhook_url: None,
                rate_limit: None,
                require_mention: None,
                extra: config_extra,
            })
        },
    })
}

fn field_key(value: &serde_json::Value) -> Option<&str> {
    value.get("key").and_then(serde_json::Value::as_str)
}

fn field_default_entry(value: &serde_json::Value) -> Option<(&str, serde_json::Value)> {
    let key = field_key(value)?;
    let default = value.get("default")?;
    Some((key, default.clone()))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::TestPluginExtraConfig;

    #[test]
    fn plugin_not_found_maps_to_api_not_found() {
        let err = ApiError::from(ChannelError::PluginNotFound("telegram".into()));
        assert!(matches!(err, ApiError::NotFound(msg) if msg == "telegram"));
    }

    #[test]
    fn invalid_plugin_type_maps_to_bad_request() {
        let err = ApiError::from(ChannelError::InvalidPluginType("unknown".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn plugin_already_running_maps_to_conflict() {
        let err = ApiError::from(ChannelError::PluginAlreadyRunning("telegram".into()));
        assert!(matches!(err, ApiError::Conflict(_)));
    }

    #[test]
    fn invalid_config_maps_to_bad_request() {
        let err = ApiError::from(ChannelError::InvalidConfig("missing token".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn connection_failed_maps_to_bad_gateway() {
        let err = ApiError::from(ChannelError::ConnectionFailed("timeout".into()));
        assert!(matches!(err, ApiError::BadGateway(_)));
    }

    #[test]
    fn pairing_not_found_maps_to_not_found() {
        let err = ApiError::from(ChannelError::PairingNotFound("123456".into()));
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[test]
    fn pairing_expired_maps_to_bad_request() {
        let err = ApiError::from(ChannelError::PairingExpired("123456".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn pairing_already_processed_maps_to_bad_request() {
        let err = ApiError::from(ChannelError::PairingAlreadyProcessed("123456".into()));
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn user_not_found_maps_to_not_found() {
        let err = ApiError::from(ChannelError::UserNotFound("user-1".into()));
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[test]
    fn user_not_authorized_maps_to_forbidden() {
        let err = ApiError::from(ChannelError::UserNotAuthorized("tg_42".into()));
        assert!(matches!(err, ApiError::Forbidden(_)));
    }

    #[test]
    fn session_not_found_maps_to_not_found() {
        let err = ApiError::from(ChannelError::SessionNotFound("sess-1".into()));
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[test]
    fn encryption_failed_maps_to_internal() {
        let err = ApiError::from(ChannelError::EncryptionFailed("bad key".into()));
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn decryption_failed_maps_to_internal() {
        let err = ApiError::from(ChannelError::DecryptionFailed("corrupt".into()));
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn platform_api_maps_to_bad_gateway() {
        let err = ApiError::from(ChannelError::PlatformApi("429 rate limited".into()));
        assert!(matches!(err, ApiError::BadGateway(_)));
    }

    #[test]
    fn message_send_failed_maps_to_internal() {
        let err = ApiError::from(ChannelError::MessageSendFailed("chat not found".into()));
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err = ApiError::from(ChannelError::Json(json_err));
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn build_test_config_telegram() {
        let req = TestPluginRequest {
            plugin_id: "telegram".into(),
            token: "bot123:ABC".into(),
            extra_config: None,
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.token.as_deref(), Some("bot123:ABC"));
    }

    #[test]
    fn build_test_config_lark() {
        let req = TestPluginRequest {
            plugin_id: "lark".into(),
            token: "xxx".into(),
            extra_config: Some(TestPluginExtraConfig {
                app_id: Some("cli_abc".into()),
                app_secret: Some("secret".into()),
            }),
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.app_id.as_deref(), Some("cli_abc"));
        assert_eq!(config.credentials.app_secret.as_deref(), Some("secret"));
        assert_eq!(config.credentials.token.as_deref(), Some("xxx"));
    }

    #[test]
    fn build_test_config_dingtalk() {
        let req = TestPluginRequest {
            plugin_id: "dingtalk".into(),
            token: "client_id_123".into(),
            extra_config: Some(TestPluginExtraConfig {
                app_id: None,
                app_secret: Some("client_secret_456".into()),
            }),
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.client_id.as_deref(), Some("client_id_123"));
        assert_eq!(config.credentials.client_secret.as_deref(), Some("client_secret_456"));
    }

    #[test]
    fn build_test_config_weixin() {
        let req = TestPluginRequest {
            plugin_id: "weixin".into(),
            token: "bot_token_xyz".into(),
            extra_config: Some(TestPluginExtraConfig {
                app_id: Some("account_abc".into()),
                app_secret: None,
            }),
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.bot_token.as_deref(), Some("bot_token_xyz"));
        assert_eq!(config.credentials.account_id.as_deref(), Some("account_abc"));
    }
}
