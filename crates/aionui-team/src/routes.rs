#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    AddAgentRequest, ApiResponse, CancelTeamChildTurnRequest, CancelTeamRunRequest, CreateTeamRequest,
    PauseTeamSlotRequest, RenameAgentRequest, RenameTeamRequest, SendAgentMessageRequest, SendTeamMessageRequest,
    SetModeRequest, TeamAgentResponse, TeamListResponse, TeamResponse, TeamRunAckResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::DbError;

use crate::error::TeamError;
use crate::service::TeamSessionService;

#[derive(Clone)]
pub struct TeamRouterState {
    pub service: Arc<TeamSessionService>,
}

fn db_error_to_api_error(err: DbError) -> ApiError {
    match err {
        DbError::NotFound(msg) => ApiError::NotFound(msg),
        DbError::Conflict(msg) => ApiError::Conflict(msg),
        DbError::Query(e) => ApiError::Internal(format!("Database error: {e}")),
        DbError::Migration(e) => ApiError::Internal(format!("Migration error: {e}")),
        DbError::Init(msg) => ApiError::Internal(format!("Database init error: {msg}")),
    }
}

impl From<TeamError> for ApiError {
    fn from(err: TeamError) -> Self {
        match err {
            TeamError::TeamNotFound(msg) => ApiError::NotFound(msg),
            TeamError::AgentNotFound(msg) => ApiError::NotFound(msg),
            TeamError::TaskNotFound(msg) => ApiError::NotFound(msg),
            TeamError::InvalidRequest(msg) => ApiError::BadRequest(msg),
            TeamError::SlotBusy(msg) => ApiError::BadRequest(format!("Team slot is busy: {msg}")),
            TeamError::LeaderOnly(msg) => ApiError::Forbidden(msg),
            TeamError::Forbidden(msg) => ApiError::Forbidden(msg),
            TeamError::SessionNotFound(msg) => ApiError::NotFound(msg),
            TeamError::BlockedTaskNotFound(msg) => ApiError::BadRequest(msg),
            TeamError::BackendNotAllowed(msg) => ApiError::BadRequest(msg),
            TeamError::DuplicateAgentName(msg) => ApiError::BadRequest(format!("Agent name already taken: {msg}")),
            TeamError::WorkspacePathUnavailable(path) => ApiError::WorkspacePathUnavailable(path),
            TeamError::WorkspacePathRuntimeUnavailable(path) => ApiError::WorkspacePathRuntimeUnavailable(path),
            TeamError::Database(db_err) => db_error_to_api_error(db_err),
            TeamError::Json(e) => ApiError::Internal(format!("JSON error: {e}")),
        }
    }
}

pub fn team_routes(state: TeamRouterState) -> Router {
    Router::new()
        .route("/api/teams", post(create_team).get(list_teams))
        .route("/api/teams/{id}", get(get_team).delete(remove_team))
        .route("/api/teams/{id}/name", axum::routing::patch(rename_team))
        .route("/api/teams/{id}/agents", post(add_agent))
        .route("/api/teams/{id}/agents/{slot_id}", axum::routing::delete(remove_agent))
        .route(
            "/api/teams/{id}/agents/{slot_id}/name",
            axum::routing::patch(rename_agent),
        )
        .route("/api/teams/{id}/messages", post(send_message))
        .route("/api/teams/{id}/agents/{slot_id}/messages", post(send_message_to_agent))
        .route("/api/teams/{id}/runs/{team_run_id}/cancel", post(cancel_run))
        .route(
            "/api/teams/{id}/runs/{team_run_id}/agents/{slot_id}/cancel",
            post(cancel_child_turn),
        )
        .route(
            "/api/teams/{id}/runs/{team_run_id}/agents/{slot_id}/pause",
            post(pause_slot_work),
        )
        .route("/api/teams/{id}/session", post(ensure_session).delete(stop_session))
        .route("/api/teams/{id}/session-mode", post(set_session_mode))
        .with_state(state)
}

async fn create_team(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateTeamRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<TeamResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let team = state.service.create_team(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(team))))
}

async fn list_teams(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<TeamListResponse>>, ApiError> {
    let teams = state.service.list_teams(&user.id).await?;
    Ok(Json(ApiResponse::ok(teams)))
}

async fn get_team(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<TeamResponse>>, ApiError> {
    let team = state.service.get_team(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(team)))
}

async fn remove_team(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.remove_team(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn rename_team(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<RenameTeamRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.service.rename_team(&user.id, &id, &req.name).await?;
    Ok(Json(ApiResponse::success()))
}

#[derive(serde::Deserialize)]
struct AgentPathParams {
    id: String,
    slot_id: String,
}

#[derive(serde::Deserialize)]
struct RunPathParams {
    id: String,
    team_run_id: String,
}

#[derive(serde::Deserialize)]
struct RunAgentPathParams {
    id: String,
    team_run_id: String,
    slot_id: String,
}

async fn add_agent(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AddAgentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<TeamAgentResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let agent = state.service.add_agent(&user.id, &id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(agent))))
}

async fn remove_agent(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<AgentPathParams>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .remove_agent(&user.id, &params.id, &params.slot_id)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn rename_agent(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<AgentPathParams>,
    body: Result<Json<RenameAgentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .rename_agent(&user.id, &params.id, &params.slot_id, &req.name)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn send_message(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SendTeamMessageRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TeamRunAckResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let ack = state
        .service
        .send_message(&user.id, &id, &req.content, req.files)
        .await?;
    Ok(Json(ApiResponse::ok(ack)))
}

async fn send_message_to_agent(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<AgentPathParams>,
    body: Result<Json<SendAgentMessageRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TeamRunAckResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let ack = state
        .service
        .send_message_to_agent(&user.id, &params.id, &params.slot_id, &req.content, req.files)
        .await?;
    Ok(Json(ApiResponse::ok(ack)))
}

async fn cancel_run(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<RunPathParams>,
    body: Result<Json<CancelTeamRunRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .cancel_run(
            &user.id,
            &params.id,
            &params.team_run_id,
            req.target_slot_id,
            req.reason,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn cancel_child_turn(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<RunAgentPathParams>,
    body: Result<Json<CancelTeamChildTurnRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .cancel_child_turn(&user.id, &params.id, &params.team_run_id, &params.slot_id, req.reason)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn pause_slot_work(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<RunAgentPathParams>,
    body: Result<Json<PauseTeamSlotRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .pause_slot_work(&user.id, &params.id, &params.team_run_id, &params.slot_id, req.reason)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn set_session_mode(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SetModeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.service.set_session_mode(&user.id, &id, &req.mode).await?;
    Ok(Json(ApiResponse::success()))
}

async fn ensure_session(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.ensure_session(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn stop_session(
    State(state): State<TeamRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.stop_session(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_router_state_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<TeamRouterState>();
    }

    #[test]
    fn team_not_found_maps_to_app_not_found() {
        let err: ApiError = TeamError::TeamNotFound("t1".into()).into();
        assert!(matches!(err, ApiError::NotFound(msg) if msg == "t1"));
    }

    #[test]
    fn agent_not_found_maps_to_app_not_found() {
        let err: ApiError = TeamError::AgentNotFound("slot-1".into()).into();
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[test]
    fn task_not_found_maps_to_app_not_found() {
        let err: ApiError = TeamError::TaskNotFound("tk-1".into()).into();
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[test]
    fn invalid_request_maps_to_bad_request() {
        let err: ApiError = TeamError::InvalidRequest("empty agents".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn leader_only_maps_to_forbidden() {
        let err: ApiError = TeamError::LeaderOnly("spawn_agent".into()).into();
        assert!(matches!(err, ApiError::Forbidden(msg) if msg == "spawn_agent"));
    }

    #[test]
    fn session_not_found_maps_to_not_found() {
        let err: ApiError = TeamError::SessionNotFound("t1".into()).into();
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[test]
    fn blocked_task_not_found_maps_to_bad_request() {
        let err: ApiError = TeamError::BlockedTaskNotFound("tk-x".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn backend_not_allowed_maps_to_bad_request() {
        let err: ApiError = TeamError::BackendNotAllowed("gemini".into()).into();
        assert!(matches!(err, ApiError::BadRequest(msg) if msg == "gemini"));
    }

    #[test]
    fn duplicate_agent_name_maps_to_bad_request() {
        let err: ApiError = TeamError::DuplicateAgentName("alice".into()).into();
        assert!(matches!(err, ApiError::BadRequest(msg) if msg.contains("alice")));
    }

    #[test]
    fn workspace_error_preserves_code() {
        let err: ApiError = TeamError::WorkspacePathUnavailable("/tmp/a b".into()).into();
        assert!(matches!(err, ApiError::WorkspacePathUnavailable(msg) if msg == "/tmp/a b"));
    }

    #[test]
    fn invalid_request_maps_to_bad_request_without_internal_details() {
        let err: ApiError = TeamError::InvalidRequest("failed to adopt conversation".into()).into();
        assert!(matches!(err, ApiError::BadRequest(msg) if msg == "failed to adopt conversation"));
    }

    #[test]
    fn runtime_workspace_error_preserves_code() {
        let err: ApiError = TeamError::WorkspacePathRuntimeUnavailable("/tmp/a b".into()).into();
        assert!(matches!(
            err,
            ApiError::WorkspacePathRuntimeUnavailable(msg) if msg == "/tmp/a b"
        ));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("bad").unwrap_err();
        let err: ApiError = TeamError::Json(json_err).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }
}
