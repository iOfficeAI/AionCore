use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};

use aionui_api_types::{
    ActiveCountResponse, ApiResponse, ApprovalCheckQuery, ApprovalCheckResponse, CloneConversationRequest,
    ConfirmRequest, ConfirmationListResponse, ConversationArtifactListResponse, ConversationArtifactResponse,
    ConversationListResponse, ConversationResponse, CreateConversationRequest, ListConversationsQuery,
    ListMessagesQuery, MessageListResponse, MessageResponse, MessageSearchResponse, SearchMessagesQuery,
    SendMessageRequest, SendMessageResponse, UpdateConversationArtifactRequest, UpdateConversationRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::AppError;

use crate::ConversationError;
use crate::state::ConversationRouterState;

impl From<ConversationError> for AppError {
    fn from(error: ConversationError) -> Self {
        match error {
            ConversationError::NotFound { id } => AppError::NotFound(format!("Conversation {id} not found")),
            ConversationError::MessageNotFound { id } => AppError::NotFound(format!("Message {id} not found")),
            ConversationError::ArtifactNotFound { id } => AppError::NotFound(format!("Artifact {id} not found")),
            ConversationError::ActiveAgentNotFound { .. } => {
                AppError::NotFound("No active agent for this conversation".into())
            }
            ConversationError::Archived { reason, .. } => AppError::ConversationArchived(reason),
            ConversationError::BadRequest { reason } => AppError::BadRequest(reason),
            ConversationError::Busy { reason } => AppError::Conflict(reason),
            ConversationError::Forbidden { reason } => AppError::Forbidden(reason),
            ConversationError::Acp(_) => AppError::BadGateway("Agent protocol error".into()),
            ConversationError::App(error) => error,
        }
    }
}

/// Build the conversation router (CRUD + message flow + confirmation + extended operations).
///
/// All routes require authentication (applied by the caller).
pub fn conversation_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations", post(create).get(list))
        .route("/api/conversations/{id}", get(get_one).patch(update).delete(delete_one))
        .route("/api/conversations/{id}/reset", post(reset))
        .route("/api/conversations/{id}/associated", get(associated))
        .route("/api/conversations/{id}/messages", get(list_msg).post(send_msg))
        .route("/api/conversations/{id}/messages/{messageId}", get(get_msg))
        .route("/api/conversations/{id}/artifacts", get(list_artifacts))
        .route("/api/conversations/{id}/artifacts/{artifactId}", patch(update_artifact))
        .route("/api/conversations/{id}/cancel", post(cancel))
        .route("/api/conversations/{id}/warmup", post(warmup))
        // Confirmation system
        .route("/api/conversations/{id}/confirmations", get(list_confirmations))
        .route("/api/conversations/{id}/confirmations/{callId}/confirm", post(confirm))
        .route("/api/conversations/{id}/approvals/check", get(check_approval))
        .route("/api/conversations/active-count", get(active_count))
        .route("/api/conversations/clone", post(clone))
        .route("/api/messages/search", get(search_messages))
        .with_state(state)
}

// ── Handlers ───────────────────────────────────────────────────────

async fn create(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state.service.create(&user.id, req).await.map_err(AppError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn list(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListConversationsQuery>,
) -> Result<Json<ApiResponse<ConversationListResponse>>, AppError> {
    let result = state.service.list(&user.id, query).await.map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn clone(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CloneConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state
        .service
        .clone_create(&user.id, req)
        .await
        .map_err(AppError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn get_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let conversation = state.service.get(&user.id, &id).await.map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn update(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateConversationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state
        .service
        .update(&user.id, &id, req, &state.task_manager)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn delete_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete(&user.id, &id).await.map_err(AppError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn reset(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.reset(&user.id, &id).await.map_err(AppError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn associated(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, AppError> {
    let items = state
        .service
        .list_associated(&user.id, &id)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn list_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<ListMessagesQuery>,
) -> Result<Json<ApiResponse<MessageListResponse>>, AppError> {
    let result = state
        .service
        .list_messages(&user.id, &id, query)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(serde::Deserialize)]
struct MessagePathParams {
    id: String,
    #[serde(rename = "messageId")]
    message_id: String,
}

async fn get_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<MessagePathParams>,
) -> Result<Json<ApiResponse<MessageResponse>>, AppError> {
    let result = state
        .service
        .get_message(&user.id, &params.id, &params.message_id)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn send_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let msg_id = state
        .service
        .send_message(&user.id, &id, req, &state.task_manager)
        .await
        .map_err(AppError::from)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(SendMessageResponse { msg_id })),
    ))
}

async fn list_artifacts(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationArtifactListResponse>>, AppError> {
    let result = state
        .service
        .list_artifacts(&user.id, &id)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(serde::Deserialize)]
struct ArtifactPathParams {
    id: String,
    #[serde(rename = "artifactId")]
    artifact_id: String,
}

async fn update_artifact(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<ArtifactPathParams>,
    body: Result<Json<UpdateConversationArtifactRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationArtifactResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let artifact = state
        .service
        .update_artifact(&user.id, &params.id, &params.artifact_id, req)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(artifact)))
}

async fn cancel(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .service
        .cancel(&user.id, &id, &state.task_manager)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn warmup(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .service
        .warmup(&user.id, &id, &state.task_manager)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn search_messages(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<SearchMessagesQuery>,
) -> Result<Json<ApiResponse<MessageSearchResponse>>, AppError> {
    let result = state
        .service
        .search_messages(&user.id, query)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

// ── Confirmation handlers ─────────────────────────────────────────

async fn list_confirmations(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConfirmationListResponse>>, AppError> {
    let items = state
        .service
        .list_confirmations(&user.id, &id, &state.task_manager)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(items)))
}

#[derive(serde::Deserialize)]
struct ConfirmPathParams {
    id: String,
    #[serde(rename = "callId")]
    call_id: String,
}

async fn confirm(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<ConfirmPathParams>,
    body: Result<Json<ConfirmRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .service
        .confirm(&user.id, &params.id, &params.call_id, req, &state.task_manager)
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn check_approval(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<ApprovalCheckQuery>,
) -> Result<Json<ApiResponse<ApprovalCheckResponse>>, AppError> {
    if query.action.trim().is_empty() {
        return Err(AppError::BadRequest("action must not be empty".into()));
    }

    let result = state
        .service
        .check_approval(
            &user.id,
            &id,
            &query.action,
            query.command_type.as_deref(),
            &state.task_manager,
        )
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn active_count(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<ActiveCountResponse>>, AppError> {
    let count = state.task_manager.active_count();
    Ok(Json(ApiResponse::ok(ActiveCountResponse { count })))
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;

    #[test]
    fn conversation_not_found_maps_to_app_not_found() {
        let app = AppError::from(ConversationError::NotFound { id: "conv_1".into() });
        assert!(matches!(app, AppError::NotFound(message) if message == "Conversation conv_1 not found"));
    }

    #[test]
    fn conversation_archived_maps_to_app_conversation_archived() {
        let app = AppError::from(ConversationError::Archived {
            id: "conv_1".into(),
            reason: "legacy runtime".into(),
        });
        assert!(matches!(app, AppError::ConversationArchived(message) if message == "legacy runtime"));
    }

    #[test]
    fn message_not_found_maps_to_app_not_found() {
        let app = AppError::from(ConversationError::MessageNotFound { id: "msg_1".into() });
        assert!(matches!(app, AppError::NotFound(message) if message == "Message msg_1 not found"));
    }

    #[test]
    fn artifact_not_found_maps_to_app_not_found() {
        let app = AppError::from(ConversationError::ArtifactNotFound {
            id: "artifact_1".into(),
        });
        assert!(matches!(app, AppError::NotFound(message) if message == "Artifact artifact_1 not found"));
    }

    #[test]
    fn active_agent_not_found_maps_to_app_not_found() {
        let app = AppError::from(ConversationError::ActiveAgentNotFound {
            conversation_id: "conv_1".into(),
        });
        assert!(matches!(app, AppError::NotFound(message) if message == "No active agent for this conversation"));
    }

    #[test]
    fn conversation_app_error_passthrough_preserves_special_codes() {
        let app = AppError::from(ConversationError::from(
            AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported("/tmp/my project".into()),
        ));
        assert!(matches!(
            app,
            AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported(message) if message == "/tmp/my project"
        ));
    }
}
