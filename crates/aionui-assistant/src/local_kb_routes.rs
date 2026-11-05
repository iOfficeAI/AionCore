#![allow(clippy::disallowed_types)]

//! HTTP routes for `/api/local-kb/*` (M9 E15). Brand: CSBU WorkMate.
//! Default OFF. Not KnowHub. Not skill experience library. Not auto-injected into daily chat.

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};

use aionui_api_types::{
    ApiResponse, CreateLocalKbSourceRequest, LocalKbSearchHit, LocalKbSearchRequest, LocalKbSettingsResponse,
    LocalKbSourceResponse, UpdateLocalKbSettingsRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::local_kb_service::LocalKbService;

#[derive(Clone)]
pub struct LocalKbRouterState {
    pub service: Arc<LocalKbService>,
}

pub fn local_kb_routes(state: LocalKbRouterState) -> Router {
    Router::new()
        .route("/api/local-kb/settings", get(get_settings).put(update_settings))
        .route("/api/local-kb/sources", get(list_sources).post(create_source))
        .route("/api/local-kb/sources/{id}", delete(delete_source))
        .route("/api/local-kb/search", post(search))
        .with_state(state)
}

async fn get_settings(
    State(state): State<LocalKbRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<LocalKbSettingsResponse>>, ApiError> {
    let settings = state.service.get_settings(&current_user.id).await?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn update_settings(
    State(state): State<LocalKbRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<UpdateLocalKbSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<LocalKbSettingsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let settings = state.service.update_settings(&current_user.id, req).await?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn list_sources(
    State(state): State<LocalKbRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<LocalKbSourceResponse>>>, ApiError> {
    let items = state.service.list_sources(&current_user.id).await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create_source(
    State(state): State<LocalKbRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateLocalKbSourceRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<LocalKbSourceResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let created = state.service.create_source(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn delete_source(
    State(state): State<LocalKbRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete_source(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn search(
    State(state): State<LocalKbRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<LocalKbSearchRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<LocalKbSearchHit>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let hits = state.service.search(&current_user.id, req).await?;
    Ok(Json(ApiResponse::ok(hits)))
}
