#![allow(clippy::disallowed_types)]

//! Remote agent management API routes.
//!
//! Endpoints:
//!
//! - `GET  /api/remote-agents`                    — list remote agents
//! - `POST /api/remote-agents`                    — create new remote agent
//! - `GET  /api/remote-agents/{id}`                 — get remote agent details
//! - `PUT  /api/remote-agents/{id}`                 — update remote agent
//! - `DELETE /api/remote-agents/{id}`                 — delete remote agent
//! - `POST /api/remote-agents/test-connection`          — test connection to remote agent (without saving it)
//! - `POST /api/remote-agents/{id}/handshake`          — perform handshake with the remote agent to verify connectivity and retrieve agent info

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, CreateRemoteAgentRequest, HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse,
    TestRemoteAgentConnectionRequest, UpdateRemoteAgentRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use super::error_mapping::agent_error_to_api_error;
use super::state::RemoteAgentRouterState;

/// Build the remote agent router.
///
/// All routes require authentication (applied by the caller).
pub fn remote_agent_routes(state: RemoteAgentRouterState) -> Router {
    Router::new()
        .route("/api/remote-agents", get(list).post(create))
        .route("/api/remote-agents/test-connection", post(test_connection))
        .route("/api/remote-agents/{id}", get(get_one).put(update).delete(delete_one))
        .route("/api/remote-agents/{id}/handshake", post(handshake))
        .with_state(state)
}

async fn list(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<RemoteAgentListItem>>>, ApiError> {
    let items = state.service.list().await.map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_one(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RemoteAgentResponse>>, ApiError> {
    let agent = state.service.get(&id).await.map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(agent)))
}

async fn create(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateRemoteAgentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<RemoteAgentResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let agent = state.service.create(req).await.map_err(agent_error_to_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(agent))))
}

async fn update(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateRemoteAgentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RemoteAgentResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let agent = state.service.update(&id, req).await.map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(agent)))
}

async fn delete_one(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete(&id).await.map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::success()))
}

async fn test_connection(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<TestRemoteAgentConnectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .test_connection(req)
        .await
        .map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::success()))
}

async fn handshake(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<HandshakeResponse>>, ApiError> {
    let resp = state.service.handshake(&id).await.map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(resp)))
}
