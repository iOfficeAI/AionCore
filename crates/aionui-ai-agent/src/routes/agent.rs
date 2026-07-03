#![allow(clippy::disallowed_types)]

//! Agent-related API routes.
//!
//! Endpoints:
//!
//! - `GET  /api/agents/management` — list diagnostics-first agent rows
//! - `POST /api/agents/custom/try-connect` — test custom agent configuration (e.g. ACP connection)

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::routing::{get, patch, post, put};

use aionui_api_types::{
    AgentLogoEntry, AgentManagementRow, AgentMetadata, AgentOverridesResponse, ApiResponse, CustomAgentUpsertRequest,
    DeleteCustomAgentResponse, ProviderHealthCheckRequest, ProviderHealthCheckResponse, SetAgentOverridesRequest,
    SetEnabledRequest, TryConnectCustomAgentRequest, TryConnectCustomAgentResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::routes::error_mapping::agent_error_to_api_error;
use crate::routes::state::AgentRouterState;

pub fn agent_routes(state: AgentRouterState) -> Router {
    Router::new()
        .route("/api/agents/logos", get(list_agent_logos))
        .route("/api/agents/management", get(list_management_agents))
        .route("/api/agents/{id}/health-check", post(health_check_by_id))
        .route("/api/agents/provider-health-check", post(provider_health_check))
        .route("/api/agents/{id}/enabled", patch(set_agent_enabled))
        .route(
            "/api/agents/{id}/overrides",
            get(get_agent_overrides).put(set_agent_overrides),
        )
        .route("/api/agents/custom", post(create_custom))
        .route("/api/agents/custom/{id}", put(update_custom).delete(delete_custom))
        .route("/api/agents/custom/try-connect", post(try_connect_custom))
        .with_state(state)
}

async fn list_agent_logos(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<AgentLogoEntry>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_agent_logos()
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn list_management_agents(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<AgentManagementRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_management_agents()
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn health_check_by_id(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentManagementRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .health_check_agent_by_id(&id)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn provider_health_check(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ProviderHealthCheckRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProviderHealthCheckResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .provider_health_check(req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn try_connect_custom(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<TryConnectCustomAgentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TryConnectCustomAgentResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .try_connect_custom_agent(req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn create_custom(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CustomAgentUpsertRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentMetadata>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .create_custom_agent(req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn update_custom(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<CustomAgentUpsertRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentMetadata>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .update_custom_agent(&id, req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn delete_custom(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteCustomAgentResponse>>, ApiError> {
    state
        .service
        .delete_custom_agent(&id)
        .await
        .map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(DeleteCustomAgentResponse { deleted: true })))
}

async fn set_agent_enabled(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SetEnabledRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentMetadata>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_agent_enabled(&id, req.enabled)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn get_agent_overrides(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentOverridesResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_agent_overrides(&id)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn set_agent_overrides(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SetAgentOverridesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentManagementRow>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_agent_overrides(&id, req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}
