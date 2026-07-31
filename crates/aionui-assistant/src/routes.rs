#![allow(clippy::disallowed_types)]

//! HTTP route handlers for `/api/assistants/*`.

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, patch, post};

use aionui_api_types::{
    ApiResponse, AssistantDetailResponse, AssistantResponse, CreateAssistantRequest, ImportAssistantsRequest,
    ImportAssistantsResult, SetAssistantStateRequest, UpdateAssistantRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::error::AssistantError;
pub use crate::state::AssistantRouterState;

/// Build the router for `/api/assistants/*`.
pub fn assistant_routes(state: AssistantRouterState) -> Router {
    Router::new()
        .route("/api/assistants", get(list).post(create))
        .route("/api/assistants/{id}", get(get_one).put(update).delete(delete_one))
        .route("/api/assistants/{id}/state", patch(set_state))
        .route("/api/assistants/{id}/avatar", get(get_avatar))
        .route("/api/assistants/import", post(import))
        .with_state(state)
}

#[derive(Debug, serde::Deserialize, Default)]
struct GetAssistantDetailQuery {
    locale: Option<String>,
}

impl From<AssistantError> for ApiError {
    fn from(error: AssistantError) -> Self {
        match error {
            AssistantError::NotFound(message) => Self::NotFound(message),
            AssistantError::BadRequest(message) => Self::BadRequest(message),
            AssistantError::Forbidden(message) => Self::Forbidden(message),
            AssistantError::Conflict(message) => Self::Conflict(message),
            AssistantError::Internal(message) => Self::Internal(message),
            // Only produced by startup assistant-storage bootstrap (never on an
            // HTTP path); treated as a transient internal condition if it ever
            // surfaces through the API boundary.
            AssistantError::ConcurrentBootstrapContention(message) => Self::Internal(message),
        }
    }
}

async fn list(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<AssistantResponse>>>, ApiError> {
    let items = state.service.list_for_user(&current_user.id).await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateAssistantRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AssistantResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let created = state.service.create_for_user(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn get_one(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<GetAssistantDetailQuery>,
) -> Result<Json<ApiResponse<AssistantDetailResponse>>, ApiError> {
    let detail = state
        .service
        .get_detail_for_user(&current_user.id, &id, query.locale.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(detail)))
}

async fn update(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAssistantRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let updated = state.service.update_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn delete_one(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn set_state(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SetAssistantStateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let resp = state.service.set_state_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn import(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ImportAssistantsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportAssistantsResult>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.service.import_for_user(&current_user.id, req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Serve the raw avatar bytes for an assistant. Content-Type inferred from the
/// file extension (png/jpg/svg default). Extensions return 404 — the frontend
/// serves those via `aion-asset://`.
async fn get_avatar(
    State(state): State<AssistantRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let asset = state
        .service
        .avatar_asset_for_user(&current_user.id, &id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("avatar '{id}' not found")))?;

    let content_type = content_type_for_extension(asset.extension.as_deref());

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
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
