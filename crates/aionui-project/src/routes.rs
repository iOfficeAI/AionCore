// This module is the HTTP boundary where crate-owned `ProjectError` values
// are intentionally mapped to the shared API error envelope.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::error::ProjectError;
use crate::service::ProjectService;
use crate::types::{CreateProjectInput, ProjectCommandProfileInput, ProjectRuntimeProfileInput, UpdateProjectInput};

impl From<ProjectError> for ApiError {
    fn from(error: ProjectError) -> Self {
        match error {
            ProjectError::BadRequest(message) => Self::BadRequest(message),
            ProjectError::NotFound(message) => Self::NotFound(message),
            ProjectError::Conflict(message) => Self::Conflict(message),
            ProjectError::Internal(_) => Self::Internal("Project operation failed".into()),
        }
    }
}

#[derive(Clone)]
pub struct ProjectRouterState {
    pub service: Arc<ProjectService>,
}

#[derive(Debug, Deserialize)]
struct BindResourceRequest {
    resource_type: String,
    resource_id: String,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectPreflightRequest {
    #[serde(default)]
    agent_ids: Vec<String>,
    #[serde(default)]
    refresh_agents: bool,
}

#[derive(Debug, Deserialize)]
struct ResourceQuery {
    resource_type: String,
    resource_id: String,
}

pub fn project_routes(state: ProjectRouterState) -> Router {
    Router::new()
        .route("/api/projects", post(create).get(list))
        .route("/api/projects/by-resource", get(get_for_resource))
        .route("/api/projects/{id}", get(get_one).patch(update).delete(delete_one))
        .route(
            "/api/projects/{id}/command-profile",
            get(get_command_profile).put(upsert_command_profile),
        )
        .route(
            "/api/projects/{id}/runtime-profile",
            get(get_runtime_profile).put(upsert_runtime_profile),
        )
        .route("/api/projects/{id}/links", get(list_links).post(bind_resource))
        .route("/api/projects/{id}/preflight", post(preflight))
        .with_state(state)
}

async fn create(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateProjectInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::ProjectRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let project = state.service.create(&user.id, input).await.map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(project))))
}

async fn list(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::ProjectRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list(&user.id).await.map_err(ApiError::from)?,
    )))
}

async fn get_one(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get(&user.id, &id).await.map_err(ApiError::from)?,
    )))
}

async fn update(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateProjectInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .update(&user.id, &id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn delete_one(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.service.delete(&user.id, &id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_command_profile(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectCommandProfileRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_command_profile(&user.id, &id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn upsert_command_profile(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ProjectCommandProfileInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectCommandProfileRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .upsert_command_profile(&user.id, &id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_runtime_profile(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectRuntimeProfileRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_runtime_profile(&user.id, &id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn upsert_runtime_profile(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ProjectRuntimeProfileInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectRuntimeProfileRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .upsert_runtime_profile(&user.id, &id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn bind_resource(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<BindResourceRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    state
        .service
        .bind_resource(&user.id, &id, &input.resource_type, &input.resource_id)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_links(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::ProjectResourceLinkRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_resource_links(&user.id, &id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_for_resource(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<ApiResponse<aionui_db::models::ProjectRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_for_resource(&user.id, &query.resource_type, &query.resource_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn preflight(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ProjectPreflightRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::types::ProjectPreflightResult>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .preflight(&user.id, &id, &input.agent_ids, input.refresh_agents)
            .await
            .map_err(ApiError::from)?,
    )))
}
