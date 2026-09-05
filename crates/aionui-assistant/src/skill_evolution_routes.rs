//! HTTP routes for `/api/skill-evolution/*` (CSBU WorkMate 技能进化).

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, ApproveSkillEvolutionResponse, CreateExperienceArticleRequest,
    CreateSkillEvolutionProposalRequest, ExperienceArticleResponse, ExperienceListQuery,
    ReviewSkillEvolutionRequest, SkillEvolutionListQuery, SkillEvolutionProposalResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::skill_evolution_service::SkillEvolutionService;

#[derive(Clone)]
pub struct SkillEvolutionRouterState {
    pub service: Arc<SkillEvolutionService>,
}

pub fn skill_evolution_routes(state: SkillEvolutionRouterState) -> Router {
    Router::new()
        .route(
            "/api/skill-evolution/proposals",
            get(list_proposals).post(create_proposal),
        )
        .route("/api/skill-evolution/proposals/{id}", get(get_proposal))
        .route("/api/skill-evolution/proposals/{id}/submit", post(submit_proposal))
        .route("/api/skill-evolution/proposals/{id}/approve", post(approve_proposal))
        .route("/api/skill-evolution/proposals/{id}/reject", post(reject_proposal))
        .route("/api/skill-evolution/proposals/{id}/apply", post(apply_proposal))
        .route("/api/skill-evolution/proposals/{id}/rollback", post(rollback_proposal))
        .route(
            "/api/skill-evolution/experience",
            get(list_experience).post(create_experience),
        )
        .with_state(state)
}

async fn create_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateSkillEvolutionProposalRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SkillEvolutionProposalResponse>>), ApiError> {
    let Json(req) = body?;
    let created = state.service.create_proposal(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn list_proposals(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Query(query): Query<SkillEvolutionListQuery>,
) -> Result<Json<ApiResponse<Vec<SkillEvolutionProposalResponse>>>, ApiError> {
    let items = state
        .service
        .list_proposals(
            &current_user.id,
            query.status.as_deref(),
            query.assistant_id.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<SkillEvolutionProposalResponse>>, ApiError> {
    let item = state.service.get_proposal(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn submit_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<SkillEvolutionProposalResponse>>, ApiError> {
    let item = state.service.submit(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn approve_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ReviewSkillEvolutionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ApproveSkillEvolutionResponse>>, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => ReviewSkillEvolutionRequest::default(),
    };
    let item = state.service.approve(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn reject_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ReviewSkillEvolutionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SkillEvolutionProposalResponse>>, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => ReviewSkillEvolutionRequest::default(),
    };
    let item = state.service.reject(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn apply_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ApproveSkillEvolutionResponse>>, ApiError> {
    let item = state.service.apply(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn rollback_proposal(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ReviewSkillEvolutionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SkillEvolutionProposalResponse>>, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => ReviewSkillEvolutionRequest::default(),
    };
    let item = state.service.rollback(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn list_experience(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Query(query): Query<ExperienceListQuery>,
) -> Result<Json<ApiResponse<Vec<ExperienceArticleResponse>>>, ApiError> {
    let items = state
        .service
        .list_experience(
            &current_user.id,
            query.assistant_id.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create_experience(
    State(state): State<SkillEvolutionRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateExperienceArticleRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ExperienceArticleResponse>>), ApiError> {
    let Json(req) = body?;
    let created = state.service.create_experience(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}
