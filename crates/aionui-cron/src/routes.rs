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
use aionui_common::AppError;

use crate::error::CronError;
use crate::service::CronService;
use crate::state::CronRouterState;

impl From<CronError> for AppError {
    fn from(err: CronError) -> Self {
        match err {
            CronError::JobNotFound(msg) => AppError::NotFound(msg),
            CronError::InvalidSchedule(msg) => AppError::BadRequest(msg),
            CronError::InvalidCronExpression(msg) => AppError::BadRequest(msg),
            CronError::InvalidExecutionMode(msg) => AppError::BadRequest(msg),
            CronError::InvalidCreatedBy(msg) => AppError::BadRequest(msg),
            CronError::InvalidJobStatus(msg) => AppError::BadRequest(msg),
            CronError::InvalidTimezone(msg) => AppError::BadRequest(msg),
            CronError::InvalidSkillContent(msg) => AppError::BadRequest(msg),
            CronError::InvalidAgentConfig(msg) => AppError::BadRequest(msg),
            CronError::Scheduler(msg) => AppError::Internal(msg),
            CronError::App(app_err) => app_err,
            CronError::Conversation(conversation_err) => AppError::from(conversation_err),
            CronError::Database(db_err) => AppError::from(db_err),
            CronError::Json(e) => AppError::Internal(format!("JSON error: {e}")),
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
) -> Result<(StatusCode, Json<ApiResponse<CronJobResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let job = state.cron_service.add_job(req).await?;
    let resp = CronService::to_response(&job);
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(resp))))
}

async fn list_jobs(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListCronJobsQuery>,
) -> Result<Json<ApiResponse<Vec<CronJobResponse>>>, AppError> {
    let jobs = state.cron_service.list_jobs(&query).await?;
    let items: Vec<CronJobResponse> = jobs.iter().map(CronService::to_response).collect();
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<CronJobResponse>>, AppError> {
    let job = state.cron_service.get_job(&id).await?;
    Ok(Json(ApiResponse::ok(CronService::to_response(&job))))
}

async fn update_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateCronJobRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CronJobResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let job = state.cron_service.update_job(&id, req).await?;
    Ok(Json(ApiResponse::ok(CronService::to_response(&job))))
}

async fn delete_job(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.cron_service.remove_job(&id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn run_now(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RunNowResponse>>, AppError> {
    let resp = state.cron_service.run_now(&id).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn system_resume(
    State(state): State<CronRouterState>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let is_internal = headers.get("x-aionui-internal").and_then(|value| value.to_str().ok()) == Some("1");
    if !is_internal {
        return Err(AppError::Forbidden("internal route".into()));
    }

    state.cron_service.handle_system_resume().await;
    Ok(Json(ApiResponse::success()))
}

async fn save_skill(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SaveCronSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.cron_service.save_skill(&id, req).await?;
    Ok(Json(ApiResponse::success()))
}

async fn list_conversations_by_cron_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, AppError> {
    let items = state.conversation_service.list_by_cron_job(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn has_skill(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<HasSkillResponse>>, AppError> {
    let resp = state.cron_service.has_skill(&id).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn delete_skill(
    State(state): State<CronRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.cron_service.delete_skill(&id).await?;
    Ok(Json(ApiResponse::success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_not_found_maps_to_not_found() {
        let err: AppError = CronError::JobNotFound("cron_abc".into()).into();
        assert!(matches!(err, AppError::NotFound(msg) if msg == "cron_abc"));
    }

    #[test]
    fn invalid_schedule_maps_to_bad_request() {
        let err: AppError = CronError::InvalidSchedule("missing kind".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_cron_expression_maps_to_bad_request() {
        let err: AppError = CronError::InvalidCronExpression("bad expr".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_execution_mode_maps_to_bad_request() {
        let err: AppError = CronError::InvalidExecutionMode("unknown".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_created_by_maps_to_bad_request() {
        let err: AppError = CronError::InvalidCreatedBy("robot".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_job_status_maps_to_bad_request() {
        let err: AppError = CronError::InvalidJobStatus("unknown".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_timezone_maps_to_bad_request() {
        let err: AppError = CronError::InvalidTimezone("Mars/Olympus".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_skill_content_maps_to_bad_request() {
        let err: AppError = CronError::InvalidSkillContent("empty".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_agent_config_maps_to_bad_request() {
        let err: AppError = CronError::InvalidAgentConfig("missing backend".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn scheduler_error_maps_to_internal() {
        let err: AppError = CronError::Scheduler("timer failed".into()).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn app_error_passthrough_preserves_code() {
        let err: AppError = CronError::App(AppError::WorkspacePathContainsWhitespace("/tmp/a b".into())).into();
        assert!(matches!(err, AppError::WorkspacePathContainsWhitespace(msg) if msg == "/tmp/a b"));
    }

    #[test]
    fn conversation_error_maps_through_boundary_mapper() {
        let err: AppError =
            CronError::Conversation(aionui_conversation::ConversationError::NotFound { id: "conv-1".into() }).into();
        assert!(matches!(err, AppError::NotFound(msg) if msg == "Conversation conv-1 not found"));
    }

    #[test]
    fn runtime_workspace_app_error_passthrough_preserves_code() {
        let err: AppError = CronError::App(AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported(
            "/tmp/a b".into(),
        ))
        .into();
        assert!(matches!(
            err,
            AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported(msg) if msg == "/tmp/a b"
        ));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: AppError = CronError::Json(json_err).into();
        assert!(matches!(err, AppError::Internal(_)));
    }
}
