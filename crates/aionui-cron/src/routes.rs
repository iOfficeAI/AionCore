#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, ConversationResponse, CreateCronJobRequest, CronJobResponse, HasSkillResponse, ListCronJobsQuery,
    RunNowResponse, SaveCronSkillRequest, UpdateCronJobRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::DbError;

use crate::error::CronError;
use crate::service::CronService;
use crate::state::CronRouterState;

fn db_error_to_api_error(err: DbError) -> ApiError {
    match err {
        DbError::NotFound(msg) => ApiError::NotFound(msg),
        DbError::Conflict(msg) => ApiError::Conflict(msg),
        DbError::Query(e) => ApiError::Internal(format!("Database error: {e}")),
        DbError::Migration(e) => ApiError::Internal(format!("Migration error: {e}")),
        DbError::Init(msg) => ApiError::Internal(format!("Database init error: {msg}")),
    }
}

impl From<CronError> for ApiError {
    fn from(err: CronError) -> Self {
        match err {
            CronError::JobNotFound(msg) => ApiError::NotFound(msg),
            CronError::InvalidSchedule(msg) => ApiError::BadRequest(msg),
            CronError::InvalidCronExpression(msg) => ApiError::BadRequest(msg),
            CronError::InvalidExecutionMode(msg) => ApiError::BadRequest(msg),
            CronError::InvalidCreatedBy(msg) => ApiError::BadRequest(msg),
            CronError::InvalidJobStatus(msg) => ApiError::BadRequest(msg),
            CronError::InvalidTimezone(msg) => ApiError::BadRequest(msg),
            CronError::InvalidSkillContent(msg) => ApiError::BadRequest(msg),
            CronError::InvalidAgentConfig(msg) => ApiError::BadRequest(msg),
            CronError::Scheduler(msg) => ApiError::Internal(msg),
            CronError::WorkspacePathUnavailable(path) => ApiError::WorkspacePathUnavailable(path),
            CronError::WorkspacePathRuntimeUnavailable(path) => ApiError::WorkspacePathRuntimeUnavailable(path),
            CronError::Conversation(conversation_err) => ApiError::from(conversation_err),
            CronError::Database(db_err) => db_error_to_api_error(db_err),
            CronError::Json(e) => ApiError::Internal(format!("JSON error: {e}")),
        }
    }
}

pub fn cron_routes(state: CronRouterState) -> Router {
    Router::new()
        .route("/api/cron/jobs", get(list_jobs).post(create_job))
        .route("/api/cron/jobs/{id}", get(get_job).put(update_job).delete(delete_job))
        .route("/api/cron/jobs/{id}/run", post(run_now))
        .route("/api/cron/internal/system-resume", post(system_resume))
        .route("/api/cron/jobs/{id}/conversations", get(list_conversations_by_cron_job))
        .route(
            "/api/cron/jobs/{id}/skill",
            get(has_skill).post(save_skill).delete(delete_skill),
        )
        .with_state(state)
}

async fn create_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateCronJobRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<CronJobResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let job = state.cron_service.add_job(req).await?;
    let resp = CronService::to_response(&job);
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(resp))))
}

async fn list_jobs(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListCronJobsQuery>,
) -> Result<Json<ApiResponse<Vec<CronJobResponse>>>, ApiError> {
    let jobs = state.cron_service.list_jobs(&query).await?;
    let items: Vec<CronJobResponse> = jobs.iter().map(CronService::to_response).collect();
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<CronJobResponse>>, ApiError> {
    let job = state.cron_service.get_job(&id).await?;
    Ok(Json(ApiResponse::ok(CronService::to_response(&job))))
}

async fn update_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateCronJobRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CronJobResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let job = state.cron_service.update_job(&id, req).await?;
    Ok(Json(ApiResponse::ok(CronService::to_response(&job))))
}

async fn delete_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.cron_service.remove_job(&id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn run_now(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RunNowResponse>>, ApiError> {
    let resp = state.cron_service.run_now(&id).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn system_resume(
    State(state): State<CronRouterState>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let is_internal = headers.get("x-aionui-internal").and_then(|value| value.to_str().ok()) == Some("1");
    if !is_internal {
        return Err(ApiError::Forbidden("internal route".into()));
    }

    state.cron_service.handle_system_resume().await;
    Ok(Json(ApiResponse::success()))
}

async fn save_skill(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SaveCronSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.cron_service.save_skill(&id, req).await?;
    Ok(Json(ApiResponse::success()))
}

async fn list_conversations_by_cron_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, ApiError> {
    let items = state.conversation_service.list_by_cron_job(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn has_skill(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<HasSkillResponse>>, ApiError> {
    let resp = state.cron_service.has_skill(&id).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn delete_skill(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.cron_service.delete_skill(&id).await?;
    Ok(Json(ApiResponse::success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_not_found_maps_to_not_found() {
        let err: ApiError = CronError::JobNotFound("cron_abc".into()).into();
        assert!(matches!(err, ApiError::NotFound(msg) if msg == "cron_abc"));
    }

    #[test]
    fn invalid_schedule_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidSchedule("missing kind".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_cron_expression_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidCronExpression("bad expr".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_execution_mode_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidExecutionMode("unknown".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_created_by_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidCreatedBy("robot".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_job_status_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidJobStatus("unknown".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_timezone_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidTimezone("Mars/Olympus".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_skill_content_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidSkillContent("empty".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn invalid_agent_config_maps_to_bad_request() {
        let err: ApiError = CronError::InvalidAgentConfig("missing backend".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn scheduler_error_maps_to_internal() {
        let err: ApiError = CronError::Scheduler("timer failed".into()).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn workspace_error_preserves_code() {
        let err: ApiError = CronError::WorkspacePathUnavailable("/tmp/a b".into()).into();
        assert!(matches!(err, ApiError::WorkspacePathUnavailable(msg) if msg == "/tmp/a b"));
    }

    #[test]
    fn conversation_error_maps_through_boundary_mapper() {
        let err: ApiError =
            CronError::Conversation(aionui_conversation::ConversationError::NotFound { id: "conv-1".into() }).into();
        assert!(matches!(err, ApiError::NotFound(msg) if msg == "Conversation conv-1 not found"));
    }

    #[test]
    fn runtime_workspace_error_preserves_code() {
        let err: ApiError = CronError::WorkspacePathRuntimeUnavailable("/tmp/a b".into()).into();
        assert!(matches!(
            err,
            ApiError::WorkspacePathRuntimeUnavailable(msg) if msg == "/tmp/a b"
        ));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: ApiError = CronError::Json(json_err).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }
}
