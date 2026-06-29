#![allow(clippy::disallowed_types)]

use crate::state::ConversationRouterState;
use aionui_api_types::{
    ApiResponse, GetConfigOptionsResponse, SetConfigOptionRequest, SetConfigOptionResponse, SideQuestionRequest,
    SideQuestionResponse, SlashCommandItem, WorkspaceBrowseQuery, WorkspaceEntry,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::routing::{get, post, put};

/// Build the conversation-ops router (no auth layer applied — the caller is
/// responsible for wrapping this with the auth middleware).
pub fn conversation_ops_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations/{id}/side-question", post(side_question))
        .route("/api/conversations/{id}/slash-commands", get(get_slash_commands))
        .route("/api/conversations/{id}/usage", get(get_usage))
        .route("/api/conversations/{id}/config-options", get(get_config_options))
        .route(
            "/api/conversations/{id}/config-options/{option_id}",
            put(set_config_option),
        )
        .route("/api/conversations/{id}/workspace", get(browse_workspace))
        .with_state(state)
}

// ── Route handlers ─────────────────────────────────────────────────

async fn get_config_options(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<GetConfigOptionsResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_config_options(&id).await.map_err(ApiError::from)?,
    )))
}

async fn set_config_option(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((id, option_id)): Path<(String, String)>,
    body: Result<Json<SetConfigOptionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SetConfigOptionResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_config_option(&id, &option_id, req)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_usage(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Option<serde_json::Value>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_usage(&id).await.map_err(ApiError::from)?,
    )))
}

async fn side_question(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(req): Json<SideQuestionRequest>,
) -> Result<Json<ApiResponse<SideQuestionResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .handle_side_question(&id, req)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_slash_commands(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<SlashCommandItem>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_slash_commands(&id).await.map_err(ApiError::from)?,
    )))
}

async fn browse_workspace(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<WorkspaceBrowseQuery>,
) -> Result<Json<ApiResponse<Vec<WorkspaceEntry>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .browse_workspace(&id, query)
            .await
            .map_err(ApiError::from)?,
    )))
}
