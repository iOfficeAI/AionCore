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

use crate::delivery::{CreatePullRequestInput, DeliveryService, PrepareDeliveryInput};
use crate::error::DevelopmentError;
use crate::operations::{
    DevelopmentOperationsService, DevelopmentOperationsSnapshot, DevelopmentPolicyInput, RecoveryDecisionInput,
};
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
    pub delivery_service: Arc<DeliveryService>,
    pub operations_service: Arc<DevelopmentOperationsService>,
}

#[derive(Debug, Default, Deserialize)]
struct RunListQuery {
    project_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EvidenceQuery {
    task_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OperationsQuery {
    run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReconcileInput {
    #[serde(default = "default_stale_after_ms")]
    stale_after_ms: i64,
}

fn default_stale_after_ms() -> i64 {
    30 * 60 * 1000
}

#[derive(Debug, Deserialize)]
struct ConfirmedInput {
    #[serde(default)]
    confirmed: bool,
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
        .route("/api/development-runs/{run_id}/delivery", get(get_delivery))
        .route(
            "/api/development-runs/{run_id}/delivery/prepare",
            post(prepare_delivery),
        )
        .route("/api/development-runs/{run_id}/delivery/push", post(push_delivery))
        .route(
            "/api/development-runs/{run_id}/delivery/pull-request",
            post(create_pull_request),
        )
        .route("/api/development-runs/{run_id}/delivery/sync", post(sync_delivery))
        .route("/api/development-runs/{run_id}/delivery/merge", post(merge_delivery))
        .route("/api/development-runs/{run_id}/delivery/report", get(delivery_report))
        .route(
            "/api/development-projects/{project_id}/operations/policy",
            get(get_operations_policy).put(update_operations_policy),
        )
        .route(
            "/api/development-projects/{project_id}/operations",
            get(get_operations_snapshot),
        )
        .route(
            "/api/development-projects/{project_id}/operations/alerts/{alert_id}/ack",
            post(acknowledge_operations_alert),
        )
        .route("/api/development-operations/reconcile", post(reconcile_operations))
        .route("/api/development-runs/{run_id}/recovery", post(decide_recovery))
        .with_state(state)
}

async fn get_operations_policy(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentPolicyRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .get_policy(&user.id, &project_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn update_operations_policy(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    body: Result<Json<DevelopmentPolicyInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentPolicyRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .upsert_policy(&user.id, &project_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_operations_snapshot(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    Query(query): Query<OperationsQuery>,
) -> Result<Json<ApiResponse<DevelopmentOperationsSnapshot>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .snapshot(&user.id, &project_id, query.run_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn acknowledge_operations_alert(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, alert_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    state
        .operations_service
        .get_policy(&user.id, &project_id)
        .await
        .map_err(ApiError::from)?;
    state
        .operations_service
        .acknowledge_alert(&user.id, &project_id, &alert_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(serde_json::json!({"acknowledged": true}))))
}

async fn reconcile_operations(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ReconcileInput>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentRecoveryRecordRow>>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .reconcile_stale_runs_for_user(&user.id, input.stale_after_ms)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn decide_recovery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<RecoveryDecisionInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentRecoveryRecordRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .decide_recovery(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .get(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn prepare_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<PrepareDeliveryInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .prepare(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn push_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<ConfirmedInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .push(&user.id, &run_id, input.confirmed)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_pull_request(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<CreatePullRequestInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .create_pull_request(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn sync_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .sync(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn merge_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<ConfirmedInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .merge(&user.id, &run_id, input.confirmed)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn delivery_report(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .report(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
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
