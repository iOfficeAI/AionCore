use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    TeamToolCliEnvelope, TeamToolContextResponse, TeamToolErrorCode, TeamToolErrorPayload, TeamToolRuntimeCallRequest,
};
use aionui_team::TeamSessionService;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

#[derive(Clone)]
pub struct RuntimeTeamToolsState {
    pub team_service: Arc<TeamSessionService>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}

pub fn runtime_team_tools_routes(state: RuntimeTeamToolsState) -> Router {
    Router::new()
        .route("/api/runtime/team-tools/context", get(context))
        .route("/api/runtime/team-tools/call", post(call))
        .with_state(state)
}

async fn context(
    State(state): State<RuntimeTeamToolsState>,
    headers: HeaderMap,
) -> (StatusCode, Json<TeamToolCliEnvelope<TeamToolContextResponse>>) {
    let Ok(runtime) = runtime_headers(&headers, RuntimeTokenScope::TeamContext, &state.runtime_token_service) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(TeamToolCliEnvelope::failure(
                TeamToolErrorPayload::new(TeamToolErrorCode::RuntimeAuthFailed, "runtime auth failed"),
                Some("team context".to_owned()),
            )),
        );
    };
    match state
        .team_service
        .resolve_team_tool_context(&runtime.user_id, &runtime.conversation_id)
        .await
    {
        Ok(resolved) => (
            StatusCode::OK,
            Json(TeamToolCliEnvelope::success(
                resolved.response,
                Some("team context".to_owned()),
            )),
        ),
        Err(error) => (
            status_for_error(error.code),
            Json(TeamToolCliEnvelope::failure(error, Some("team context".to_owned()))),
        ),
    }
}

async fn call(
    State(state): State<RuntimeTeamToolsState>,
    headers: HeaderMap,
    Json(request): Json<TeamToolRuntimeCallRequest>,
) -> (StatusCode, Json<TeamToolCliEnvelope<Value>>) {
    let Ok(runtime) = runtime_headers(&headers, RuntimeTokenScope::TeamCall, &state.runtime_token_service) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(TeamToolCliEnvelope::failure(
                TeamToolErrorPayload::new(TeamToolErrorCode::RuntimeAuthFailed, "runtime auth failed"),
                Some("team call".to_owned()),
            )),
        );
    };
    let resolved = match state
        .team_service
        .resolve_team_tool_context(&runtime.user_id, &runtime.conversation_id)
        .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            return (
                status_for_error(error.code),
                Json(TeamToolCliEnvelope::failure(error, Some("team call".to_owned()))),
            );
        }
    };
    let Some(context) = resolved.context else {
        return (
            StatusCode::FORBIDDEN,
            Json(TeamToolCliEnvelope::failure(
                TeamToolErrorPayload::new(TeamToolErrorCode::NotInTeam, "conversation is not in a team"),
                Some("team call".to_owned()),
            )),
        );
    };
    match state
        .team_service
        .execute_team_tool(
            &context,
            aionui_api_types::TeamToolCall {
                tool: request.tool,
                arguments: request.arguments,
            },
        )
        .await
    {
        Ok(data) => (
            StatusCode::OK,
            Json(TeamToolCliEnvelope::success(data, Some("team call".to_owned()))),
        ),
        Err(error) => (
            status_for_error(error.code),
            Json(TeamToolCliEnvelope::failure(error, Some("team call".to_owned()))),
        ),
    }
}

struct RuntimeHeaders {
    user_id: String,
    conversation_id: String,
}

fn runtime_headers(
    headers: &HeaderMap,
    scope: RuntimeTokenScope,
    token_service: &RuntimeTokenService,
) -> Result<RuntimeHeaders, ()> {
    let user_id = required_header(headers, HEADER_USER_ID)?;
    let conversation_id = required_header(headers, HEADER_CONVERSATION_ID)?;
    let token = required_header(headers, HEADER_RUNTIME_TOKEN)?;
    token_service
        .validate(
            Some(&token),
            &user_id,
            &conversation_id,
            scope,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .map_err(|_| ())?;
    Ok(RuntimeHeaders {
        user_id,
        conversation_id,
    })
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Result<String, ()> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(())
}

fn status_for_error(code: TeamToolErrorCode) -> StatusCode {
    match code {
        TeamToolErrorCode::RuntimeAuthFailed => StatusCode::UNAUTHORIZED,
        TeamToolErrorCode::PermissionDenied | TeamToolErrorCode::NotInTeam => StatusCode::FORBIDDEN,
        TeamToolErrorCode::ConversationNotFound
        | TeamToolErrorCode::TeamNotFound
        | TeamToolErrorCode::AgentNotFound => StatusCode::NOT_FOUND,
        TeamToolErrorCode::UnknownTool | TeamToolErrorCode::SchemaValidationFailed => StatusCode::BAD_REQUEST,
        TeamToolErrorCode::TransportUnavailable | TeamToolErrorCode::RuntimeContextMissing => StatusCode::CONFLICT,
    }
}
