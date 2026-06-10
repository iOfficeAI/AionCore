#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};

use aionui_api_types::{
    ApiResponse, ClientPreferencesResponse, CreateProviderRequest, DetectProtocolRequest, EnsureManagedAcpToolRequest,
    EnsureManagedAcpToolResponse, EnsureNodeRuntimeRequest, EnsureNodeRuntimeResponse, FetchModelsAnonymousRequest,
    FetchModelsRequest, FetchModelsResponse, ProtocolDetectionResponse, ProviderResponse, SystemInfoResponse,
    SystemSettingsResponse, UpdateCheckRequest, UpdateCheckResult, UpdateClientPreferencesRequest,
    UpdateProviderRequest, UpdateSettingsRequest,
};
use aionui_common::ApiError;

use crate::client_pref::ClientPrefService;
use crate::error::SystemError;
use crate::model_fetcher::ModelFetchService;
use crate::protocol::ProtocolDetectionService;
use crate::provider::ProviderService;
use crate::runtime_prepare::RuntimePrepareService;
use crate::settings::SettingsService;
use crate::version::VersionCheckService;

/// Shared state for system route handlers.
#[derive(Clone)]
pub struct SystemRouterState {
    pub settings_service: SettingsService,
    pub client_pref_service: ClientPrefService,
    pub provider_service: ProviderService,
    pub model_fetch_service: ModelFetchService,
    pub protocol_detection_service: ProtocolDetectionService,
    pub version_check_service: VersionCheckService,
    pub runtime_prepare_service: RuntimePrepareService,
}

impl From<SystemError> for ApiError {
    fn from(error: SystemError) -> Self {
        match error {
            SystemError::NotFound(reason) => ApiError::NotFound(reason),
            SystemError::BadRequest(reason) => ApiError::BadRequest(reason),
            SystemError::Conflict(reason) => ApiError::Conflict(reason),
            SystemError::Internal(reason) => ApiError::Internal(reason),
            SystemError::BadGateway(reason) => ApiError::BadGateway(reason),
            SystemError::Timeout(reason) => ApiError::Timeout(reason),
            SystemError::UnprocessableEntity(reason) => ApiError::UnprocessableEntity(reason),
        }
    }
}

/// Build the system router (settings + client prefs + providers + system).
///
/// All routes require authentication (applied by the caller).
///
/// Endpoints:
/// - `GET  /api/settings`                    — get all backend settings
/// - `PATCH /api/settings`                   — partial update backend settings
/// - `GET  /api/settings/client`             — get client preferences
/// - `PUT  /api/settings/client`             — batch update client preferences
/// - `GET  /api/providers`                   — list all providers
/// - `POST /api/providers`                   — create a provider
/// - `PUT  /api/providers/:id`               — update a provider
/// - `DELETE /api/providers/:id`             — delete a provider
/// - `POST /api/providers/:id/models`        — fetch models from remote API
/// - `POST /api/providers/fetch-models`      — fetch models anonymously (pre-create preview)
/// - `POST /api/providers/detect-protocol`   — detect API protocol
/// - `GET  /api/system/info`                 — system directory & platform info
/// - `POST /api/system/check-update`         — check GitHub for new versions
/// - `POST /api/system/ensure-node-runtime`  — prepare managed Node runtime
/// - `POST /api/system/ensure-managed-acp-tool` — prepare managed ACP tool artifact
pub fn system_routes(state: SystemRouterState) -> Router {
    Router::new()
        .route("/api/settings", get(get_settings).patch(update_settings))
        .route(
            "/api/settings/client",
            get(get_client_preferences).put(update_client_preferences),
        )
        .route("/api/providers", get(list_providers).post(create_provider))
        // Literal-segment routes must register BEFORE the `/{id}` routes so
        // axum matches the literals instead of treating "detect-protocol" /
        // "fetch-models" as a provider id.
        .route("/api/providers/detect-protocol", post(detect_protocol))
        .route("/api/providers/fetch-models", post(fetch_models_anonymous))
        .route("/api/providers/{id}", delete(delete_provider).put(update_provider))
        .route("/api/providers/{id}/models", post(fetch_models))
        .route("/api/system/info", get(get_system_info))
        .route("/api/system/check-update", post(check_update))
        .route("/api/system/ensure-node-runtime", post(ensure_node_runtime))
        .route("/api/system/ensure-managed-acp-tool", post(ensure_managed_acp_tool))
        .with_state(state)
}

/// Backwards-compatible alias — delegates to `system_routes`.
pub fn settings_routes(state: SystemRouterState) -> Router {
    system_routes(state)
}

// ===========================================================================
// Settings handlers
// ===========================================================================

async fn get_settings(
    State(state): State<SystemRouterState>,
) -> Result<Json<ApiResponse<SystemSettingsResponse>>, ApiError> {
    let settings = state.settings_service.get_settings().await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn update_settings(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SystemSettingsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let settings = state
        .settings_service
        .update_settings(req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(settings)))
}

// ===========================================================================
// Client preferences handlers
// ===========================================================================

#[derive(Debug, serde::Deserialize, Default)]
struct ClientPrefQuery {
    keys: Option<String>,
}

async fn get_client_preferences(
    State(state): State<SystemRouterState>,
    Query(query): Query<ClientPrefQuery>,
) -> Result<Json<ApiResponse<ClientPreferencesResponse>>, ApiError> {
    let keys_filter: Option<Vec<String>> = query.keys.map(|k| {
        k.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let key_refs: Option<Vec<&str>> = keys_filter.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect());

    let prefs = state
        .client_pref_service
        .get_preferences(key_refs.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(prefs)))
}

async fn update_client_preferences(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateClientPreferencesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .client_pref_service
        .update_preferences(req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

// ===========================================================================
// Provider handlers
// ===========================================================================

async fn list_providers(
    State(state): State<SystemRouterState>,
) -> Result<Json<ApiResponse<Vec<ProviderResponse>>>, ApiError> {
    let providers = state.provider_service.list().await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(providers)))
}

async fn create_provider(
    State(state): State<SystemRouterState>,
    body: Result<Json<CreateProviderRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ProviderResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let provider = state.provider_service.create(req).await.map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(provider))))
}

async fn update_provider(
    State(state): State<SystemRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateProviderRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProviderResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let provider = state.provider_service.update(&id, req).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(provider)))
}

async fn delete_provider(
    State(state): State<SystemRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.provider_service.delete(&id).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn fetch_models(
    State(state): State<SystemRouterState>,
    Path(id): Path<String>,
    body: Result<Json<FetchModelsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FetchModelsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .model_fetch_service
        .fetch_models(&id, &req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn fetch_models_anonymous(
    State(state): State<SystemRouterState>,
    body: Result<Json<FetchModelsAnonymousRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FetchModelsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .model_fetch_service
        .fetch_models_anonymous(&req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn detect_protocol(
    State(state): State<SystemRouterState>,
    body: Result<Json<DetectProtocolRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProtocolDetectionResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .protocol_detection_service
        .detect_protocol(&req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

// ===========================================================================
// System info & version check handlers
// ===========================================================================

async fn get_system_info() -> Json<ApiResponse<SystemInfoResponse>> {
    let info = crate::sysinfo::get_system_info();
    Json(ApiResponse::ok(info))
}

async fn check_update(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateCheckRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<UpdateCheckResult>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .version_check_service
        .check_update(&req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn ensure_node_runtime(
    State(state): State<SystemRouterState>,
    body: Result<Json<EnsureNodeRuntimeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<EnsureNodeRuntimeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.runtime_prepare_service.ensure_node_runtime(req.scope).await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn ensure_managed_acp_tool(
    State(state): State<SystemRouterState>,
    body: Result<Json<EnsureManagedAcpToolRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<EnsureManagedAcpToolResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .runtime_prepare_service
        .ensure_managed_acp_tool(req.scope, &req.tool_id)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}
