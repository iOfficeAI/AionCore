#![allow(clippy::disallowed_types)]

//! Agent-related API routes.
//!
//! Endpoints:
//!
//! - `GET  /api/agents`         — list available agents
//! - `POST /api/agents/refresh` — refresh agent list (e.g. after new agent is added to the system)
//! - `POST /api/agents/test`    — test custom agent configuration (e.g. LLM connection)

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::routing::{get, patch, post, put};

use aionui_api_types::{
    AcpHealthCheckRequest, AcpHealthCheckResponse, AgentMetadata, ApiResponse, CustomAgentUpsertRequest,
    DeleteCustomAgentResponse, ProviderHealthCheckRequest, ProviderHealthCheckResponse, SetEnabledRequest,
    TryConnectCustomAgentRequest, TryConnectCustomAgentResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::routes::error_mapping::agent_error_to_api_error;
use crate::routes::state::AgentRouterState;

pub fn agent_routes(state: AgentRouterState) -> Router {
    Router::new()
        .route("/api/agents", get(list_agents))
        .route("/api/agents/refresh", post(refresh_agents))
        .route("/api/agents/health-check", post(health_check))
        .route("/api/agents/provider-health-check", post(provider_health_check))
        .route("/api/agents/{id}/enabled", patch(set_agent_enabled))
        .route("/api/agents/custom", post(create_custom))
        .route("/api/agents/custom/{id}", put(update_custom).delete(delete_custom))
        .route("/api/agents/custom/try-connect", post(try_connect_custom))
        .with_state(state)
}

async fn list_agents(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<AgentMetadata>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_agents().await.map_err(agent_error_to_api_error)?,
    )))
}

async fn refresh_agents(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<AgentMetadata>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.refresh_agents().await.map_err(agent_error_to_api_error)?,
    )))
}

async fn health_check(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<AcpHealthCheckRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AcpHealthCheckResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .acp_health_check(req)
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
