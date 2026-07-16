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

use crate::error::DevelopmentError;
use crate::service::{CompletionEvaluation, DevelopmentService};
use crate::types::{
    AssignDevelopmentRoleInput, CreateArtifactInput, CreateDevelopmentRunInput, CreateDevelopmentTaskInput,
    ExecuteQualityGateInput, ResolveFindingInput, SubmitReviewInput, TransitionDevelopmentTaskInput,
};

impl From<DevelopmentError> for ApiError {
    fn from(value: DevelopmentError) -> Self {
        match value {
            DevelopmentError::BadRequest(message) => Self::BadRequest(message),
            DevelopmentError::NotFound(message) => Self::NotFound(message),
            DevelopmentError::Conflict(message) => Self::Conflict(message),
            DevelopmentError::Internal(_) => Self::Internal("Development operation failed".into()),
        }
    }
}

#[derive(Clone)]
pub struct DevelopmentRouterState {
    pub service: Arc<DevelopmentService>,
}

#[derive(Debug, Default, Deserialize)]
struct RunListQuery {
    project_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EvidenceQuery {
    task_id: Option<String>,
}

pub fn development_routes(state: DevelopmentRouterState) -> Router {
    Router::new()
        .route("/api/development-runs", post(create_run).get(list_runs))
        .route("/api/development-runs/{run_id}", get(get_run))
        .route(
            "/api/development-runs/{run_id}/roles",
            post(assign_role).get(list_roles),
        )
        .route(
            "/api/development-runs/{run_id}/tasks",
            post(create_task).get(list_tasks),
        )
        .route(
            "/api/development-runs/{run_id}/tasks/{task_id}/completion",
            get(evaluate_completion).post(complete_task),
        )
        .route(
            "/api/development-runs/{run_id}/tasks/{task_id}/transition",
            post(transition_task),
        )
        .route(
            "/api/development-runs/{run_id}/artifacts",
            post(create_artifact).get(list_artifacts),
        )
        .route(
            "/api/development-runs/{run_id}/quality-gates",
            post(execute_gate).get(list_gates),
        )
        .route(
            "/api/development-runs/{run_id}/reviews",
            post(submit_review).get(list_findings),
        )
        .route(
            "/api/development-runs/{run_id}/findings/{finding_id}",
            post(resolve_finding),
        )
        .with_state(state)
}

async fn assign_role(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<AssignDevelopmentRoleInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::DevelopmentRunRoleRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .assign_role(&user.id, &run_id, &input.slot_id, &input.role)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_roles(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentRunRoleRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_roles(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_run(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateDevelopmentRunInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::DevelopmentRunRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .create_run(&user.id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_runs(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<RunListQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentRunRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_runs(&user.id, query.project_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_run(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentRunRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_run(&user.id, &run_id).await.map_err(ApiError::from)?,
    )))
}

async fn create_task(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<CreateDevelopmentTaskInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .create_task(&user.id, &run_id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_tasks(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentTaskRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_tasks(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn evaluate_completion(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<CompletionEvaluation>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .evaluate_completion(&user.id, &run_id, &task_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn complete_task(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .complete_task(&user.id, &run_id, &task_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn transition_task(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<TransitionDevelopmentTaskInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .transition_task(&user.id, &run_id, &task_id, &input.status)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_artifact(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<CreateArtifactInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::TaskArtifactRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .create_artifact(&user.id, &run_id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_artifacts(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Query(query): Query<EvidenceQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::TaskArtifactRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_artifacts(&user.id, &run_id, query.task_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn execute_gate(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<ExecuteQualityGateInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::QualityGateRunRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .execute_gate(
            &user.id,
            &run_id,
            input.task_id.as_deref(),
            &input.gate_type,
            input.workspace_lease_id.as_deref(),
            input.required,
        )
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_gates(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Query(query): Query<EvidenceQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::QualityGateRunRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_gates(&user.id, &run_id, query.task_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn submit_review(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<SubmitReviewInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .submit_review(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn list_findings(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Query(query): Query<EvidenceQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::ReviewFindingRow>>>, ApiError> {
    let task_id = query
        .task_id
        .ok_or_else(|| ApiError::BadRequest("task_id is required".into()))?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_findings(&user.id, &run_id, &task_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn resolve_finding(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, finding_id)): Path<(String, String)>,
    body: Result<Json<ResolveFindingInput>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    state
        .service
        .resolve_finding(&user.id, &run_id, &finding_id, &input.status)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}
