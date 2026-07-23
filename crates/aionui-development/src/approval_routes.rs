#![allow(clippy::disallowed_types)] // HTTP boundary maps crate-owned errors to the shared API response.

use std::sync::Arc;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::models::ApprovalRequestRow;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::approval::{ApprovalError, ApprovalService, ResolveApprovalContext};

impl From<ApprovalError> for ApiError {
    fn from(value: ApprovalError) -> Self {
        match value {
            ApprovalError::BadRequest(message) => Self::BadRequest(message),
            ApprovalError::NotFound => Self::NotFound("Approval request".into()),
            ApprovalError::Forbidden(message) => Self::Forbidden(message),
            ApprovalError::Conflict(message) => Self::Conflict(message),
            ApprovalError::Resolver(_) => Self::BadGateway("Agent could not consume the approval".into()),
            ApprovalError::Internal(_) => Self::Internal("Approval operation failed".into()),
        }
    }
}

#[derive(Clone)]
pub struct ApprovalRouterState {
    pub service: Arc<ApprovalService>,
}

#[derive(Debug, Default, Deserialize)]
struct ApprovalListQuery {
    run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResolveApprovalBody {
    option_index: usize,
}

pub fn approval_routes(state: ApprovalRouterState) -> Router {
    Router::new()
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{approval_id}", get(get_approval))
        .route("/api/approvals/{approval_id}/resolve", post(resolve_approval))
        .with_state(state)
}

async fn list_approvals(
    State(state): State<ApprovalRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ApprovalListQuery>,
) -> Result<Json<ApiResponse<Vec<ApprovalRequestRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list(&user.id, query.run_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_approval(
    State(state): State<ApprovalRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(approval_id): Path<String>,
) -> Result<Json<ApiResponse<ApprovalRequestRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get(&user.id, &approval_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn resolve_approval(
    State(state): State<ApprovalRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(approval_id): Path<String>,
    body: Result<Json<ResolveApprovalBody>, JsonRejection>,
) -> Result<Json<ApiResponse<ApprovalRequestRow>>, ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .resolve(
                &approval_id,
                body.option_index,
                ResolveApprovalContext::Web { user_id: user.id },
            )
            .await
            .map_err(ApiError::from)?,
    )))
}
