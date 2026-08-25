// `ApiError` is not used here at all -- this router speaks the agent-facing CLI
// envelope, not the front-end's `ApiResponse`. Kept explicit so a future handler
// does not silently mix the two.
#![allow(clippy::disallowed_types)]

//! Routes. Request/response transformation only.
//!
//! Every route authenticates on its own runtime-token header rather than going
//! through the user auth middleware, exactly like the `session` runtime routes:
//! the caller is an agent process holding a conversation-scoped token, not a
//! browser session.
//!
//! READ-ONLY BY CONSTRUCTION: only `get` is registered. `config skills *` remains
//! the read-write management surface; letting a runtime token reach it would hand
//! an agent authority over the whole installation's skills.

use aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION;
use aionui_ai_agent::runtime_token::RuntimeTokenScope;
use aionui_api_types::{
    RuntimeSkillFileQuery, RuntimeSkillFileResponse, RuntimeSkillListResponse, RuntimeSkillShowResponse,
    SkillRuntimeEnvelope, SkillRuntimeErrorCode, SkillRuntimeErrorPayload,
};
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;

use crate::error::SkillRuntimeError;
use crate::state::SkillRuntimeRouterState;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

pub fn skill_runtime_routes(state: SkillRuntimeRouterState) -> Router {
    Router::new()
        .route("/api/runtime/skills", get(list))
        .route("/api/runtime/skills/{name}", get(show))
        .route("/api/runtime/skills/{name}/file", get(read_file))
        .with_state(state)
}

struct RuntimeCaller {
    user_id: String,
    conversation_id: String,
}

/// Shared preamble for every route. The token is validated against BOTH the user
/// and the conversation, so a token minted for one conversation cannot read
/// another's skills even within the same user.
fn runtime_caller(state: &SkillRuntimeRouterState, headers: &HeaderMap) -> Result<RuntimeCaller, SkillRuntimeError> {
    let user_id = required_header(headers, HEADER_USER_ID)?;
    let conversation_id = required_header(headers, HEADER_CONVERSATION_ID)?;
    let token = required_header(headers, HEADER_RUNTIME_TOKEN)?;
    state
        .runtime_token_service
        .validate(
            Some(&token),
            &user_id,
            &conversation_id,
            RuntimeTokenScope::ConversationHelper,
            // Despite the `TEAM_` prefix this is not team-specific: every
            // conversation's helper token is issued with this same constant
            // (value "default"), and passing anything else fails validation.
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .map_err(|_| SkillRuntimeError::RuntimeAuthFailed)?;
    Ok(RuntimeCaller {
        user_id,
        conversation_id,
    })
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Result<String, SkillRuntimeError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(SkillRuntimeError::RuntimeAuthFailed)
}

async fn list(
    State(state): State<SkillRuntimeRouterState>,
    headers: HeaderMap,
) -> (StatusCode, Json<SkillRuntimeEnvelope<RuntimeSkillListResponse>>) {
    let command = Some("skills list".to_owned());
    let caller = match runtime_caller(&state, &headers) {
        Ok(caller) => caller,
        Err(error) => return envelope_failure(error, command),
    };
    match state.service.list(&caller.user_id, &caller.conversation_id).await {
        Ok(data) => (StatusCode::OK, Json(SkillRuntimeEnvelope::success(data, command))),
        Err(error) => envelope_failure(error, command),
    }
}

async fn show(
    State(state): State<SkillRuntimeRouterState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> (StatusCode, Json<SkillRuntimeEnvelope<RuntimeSkillShowResponse>>) {
    let command = Some("skills show".to_owned());
    let caller = match runtime_caller(&state, &headers) {
        Ok(caller) => caller,
        Err(error) => return envelope_failure(error, command),
    };
    match state
        .service
        .show(&caller.user_id, &caller.conversation_id, &name)
        .await
    {
        Ok(data) => (StatusCode::OK, Json(SkillRuntimeEnvelope::success(data, command))),
        Err(error) => envelope_failure(error, command),
    }
}

async fn read_file(
    State(state): State<SkillRuntimeRouterState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<RuntimeSkillFileQuery>,
) -> (StatusCode, Json<SkillRuntimeEnvelope<RuntimeSkillFileResponse>>) {
    let command = Some("skills cat".to_owned());
    let caller = match runtime_caller(&state, &headers) {
        Ok(caller) => caller,
        Err(error) => return envelope_failure(error, command),
    };
    match state
        .service
        .read_file(&caller.user_id, &caller.conversation_id, &name, &query.path)
        .await
    {
        Ok(data) => (StatusCode::OK, Json(SkillRuntimeEnvelope::success(data, command))),
        Err(error) => envelope_failure(error, command),
    }
}

fn envelope_failure<T>(
    error: SkillRuntimeError,
    command: Option<String>,
) -> (StatusCode, Json<SkillRuntimeEnvelope<T>>) {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    // The message is the crate error's Display, which never carries a resolved
    // filesystem path -- only the requested name / relative shape.
    let payload = SkillRuntimeErrorPayload::new(error.code(), error.to_string());
    debug_assert!(
        payload.code != SkillRuntimeErrorCode::TransportUnavailable || status == StatusCode::INTERNAL_SERVER_ERROR
    );
    (status, Json(SkillRuntimeEnvelope::failure(payload, command)))
}
