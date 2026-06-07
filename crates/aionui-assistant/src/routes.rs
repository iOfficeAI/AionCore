#![allow(clippy::disallowed_types)]

//! HTTP route handlers for `/api/assistants/*`.

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, patch, post};

use aionui_api_types::{
    ApiResponse, AssistantResponse, CreateAssistantRequest, ImportAssistantsRequest, ImportAssistantsResult,
    SetAssistantStateRequest, UpdateAssistantRequest,
};
use aionui_common::ApiError;

use crate::error::AssistantError;
pub use crate::state::AssistantRouterState;

/// Build the router for `/api/assistants/*`.
pub fn assistant_routes(state: AssistantRouterState) -> Router {
    Router::new()
        .route("/api/assistants", get(list).post(create))
        .route("/api/assistants/{id}", axum::routing::put(update).delete(delete_one))
        .route("/api/assistants/{id}/state", patch(set_state))
        .route("/api/assistants/{id}/avatar", get(get_avatar))
        .route("/api/assistants/import", post(import))
        .with_state(state)
}

impl From<AssistantError> for ApiError {
    fn from(error: AssistantError) -> Self {
        match error {
            AssistantError::NotFound(message) => Self::NotFound(message),
            AssistantError::BadRequest(message) => Self::BadRequest(message),
            AssistantError::Forbidden(message) => Self::Forbidden(message),
            AssistantError::Conflict(message) => Self::Conflict(message),
            AssistantError::Internal(message) => Self::Internal(message),
        }
    }
}

async fn list(
    State(state): State<AssistantRouterState>,
) -> Result<Json<ApiResponse<Vec<AssistantResponse>>>, ApiError> {
    let items = state.service.list().await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create(
    State(state): State<AssistantRouterState>,
    body: Result<Json<CreateAssistantRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AssistantResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let created = state.service.create(req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn update(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAssistantRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let updated = state.service.update(&id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn delete_one(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete(&id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn set_state(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
    body: Result<Json<SetAssistantStateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let resp = state.service.set_state(&id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn import(
    State(state): State<AssistantRouterState>,
    body: Result<Json<ImportAssistantsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportAssistantsResult>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.service.import(req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Serve the raw avatar bytes for an assistant. Content-Type inferred from the
/// file extension (png/jpg/svg default). Extensions return 404 — the frontend
/// serves those via `aion-asset://`.
async fn get_avatar(State(state): State<AssistantRouterState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let asset = state
        .service
        .avatar_asset(&id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("avatar '{id}' not found")))?;

    let content_type = content_type_for_extension(asset.extension.as_deref());

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(asset.bytes))
        .map_err(|e| ApiError::Internal(e.to_string()))
}

fn content_type_for_extension(ext: Option<&str>) -> HeaderValue {
    let mime = match ext {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(mime)
}
