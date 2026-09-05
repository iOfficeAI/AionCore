#![allow(clippy::disallowed_types)]

//! HTTP routes for `/api/agent-center/*` (CSBU WorkMate 智能体中心).

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    AgentCenterDetailResponse, AgentCenterListItem, AgentCenterListQuery, AgentCenterRevisionResponse,
    AgentCenterRunPlanResponse, ApiResponse, CreateAgentCenterRequest, PublishAgentCenterRequest,
    UpdateAgentCenterRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::agent_center_service::AgentCenterService;

#[derive(Clone)]
pub struct AgentCenterRouterState {
    pub service: Arc<AgentCenterService>,
}

pub fn agent_center_routes(state: AgentCenterRouterState) -> Router {
    Router::new()
        .route("/api/agent-center/agents", get(list_agents).post(create_agent))
        .route("/api/agent-center/agents/{id}", get(get_agent).put(update_agent))
        .route("/api/agent-center/agents/{id}/publish", post(publish_agent))
        .route("/api/agent-center/agents/{id}/versions", get(list_versions))
        .route("/api/agent-center/agents/{id}/run", post(run_agent))
        .with_state(state)
}

async fn list_agents(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Query(query): Query<AgentCenterListQuery>,
) -> Result<Json<ApiResponse<Vec<AgentCenterListItem>>>, ApiError> {
    let items = state
        .service
        .list_for_user(&current_user.id, &query.scope, query.team_id.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateAgentCenterRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentCenterDetailResponse>>), ApiError> {
    let Json(req) = body?;
    let created = state.service.create_for_user(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn get_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let detail = state.service.get_detail_for_user(&current_user.id, &id, None).await?;
    Ok(Json(ApiResponse::ok(detail)))
}

async fn update_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAgentCenterRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let Json(req) = body?;
    let updated = state.service.update_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn publish_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<PublishAgentCenterRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => PublishAgentCenterRequest::default(),
    };
    let published = state.service.publish_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(published)))
}

async fn list_versions(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<AgentCenterRevisionResponse>>>, ApiError> {
    let versions = state.service.list_versions_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(versions)))
}

async fn run_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterRunPlanResponse>>, ApiError> {
    let plan = state.service.run_plan_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(plan)))
}
