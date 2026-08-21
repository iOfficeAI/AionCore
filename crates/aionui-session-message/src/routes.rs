// `ApiError` is the HTTP-boundary error type, and this IS the boundary — the
// same allow `aionui-conversation/src/routes.rs:1` and
// `aionui-team/src/routes.rs:1` carry.
#![allow(clippy::disallowed_types)]

//! Routes. Request/response transformation only — no business logic.
//!
//! Two mentionable outlets over ONE service function: `mentionable` (ordinary
//! user auth, for the `@@` picker) and `targets` (runtime token, for the
//! agent's `session list`). Same filtering and ranking, different auth channel.
//!
//! The path is `/api/session-messages/mentionable`, not
//! `/api/conversations/mentionable`: this router lives in this crate, and
//! hanging a `/api/conversations/*` path here would make
//! `aionui-conversation/src/routes.rs`'s route list misleading.

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    ApiResponse, SessionCliEnvelope, SessionMentionableQuery, SessionMentionableResponse, SessionSendMessageRequest,
    SessionSendMessageResponse, SessionToolErrorCode, SessionToolErrorPayload,
};
use aionui_auth::CurrentUser;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use crate::error::SessionMessageError;
use crate::state::SessionMessageRouterState;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

pub fn session_message_routes(state: SessionMessageRouterState) -> Router {
    Router::new()
        .route("/api/runtime/session-messages/send", post(send))
        .route("/api/runtime/session-messages/targets", get(targets))
        .with_state(state)
}

/// Registered separately because it goes through the ordinary user auth
/// middleware in `aionui-app`, while the runtime routes above authenticate on
/// their own token header.
pub fn session_message_user_routes(state: SessionMessageRouterState) -> Router {
    Router::new()
        .route("/api/session-messages/mentionable", get(mentionable))
        .with_state(state)
}

struct RuntimeCaller {
    user_id: String,
    conversation_id: String,
}

/// Shared preamble for every runtime `session` route: validate the token. The
/// team-caller rejection lives one layer in (`send` / `guard_list_access`) so it
/// cannot be forgotten by a new subcommand that skips this helper.
fn runtime_caller(
    state: &SessionMessageRouterState,
    headers: &HeaderMap,
) -> Result<RuntimeCaller, SessionMessageError> {
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
            // Verified: `validate`'s 5th parameter is `session_generation: &str`
            // (`runtime_token.rs:139-146`), and every conversation's helper
            // token — team or not — is issued with this same constant (value
            // `"default"`, `runtime_token.rs:8`). Despite the `TEAM_` prefix it
            // is not team-specific; passing anything else fails validation.
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .map_err(|_| SessionMessageError::SchemaValidation {
            reason: "runtime auth failed".to_owned(),
        })?;
    Ok(RuntimeCaller {
        user_id,
        conversation_id,
    })
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Result<String, SessionMessageError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| SessionMessageError::SchemaValidation {
            reason: format!("missing header: {name}"),
        })
}

async fn send(
    State(state): State<SessionMessageRouterState>,
    headers: HeaderMap,
    Json(request): Json<SessionSendMessageRequest>,
) -> (StatusCode, Json<SessionCliEnvelope<SessionSendMessageResponse>>) {
    let command = Some("session send-message".to_owned());
    let caller = match runtime_caller(&state, &headers) {
        Ok(caller) => caller,
        Err(_) => return unauthorized(command),
    };
    match state
        .service
        .send(&caller.user_id, &caller.conversation_id, &request)
        .await
    {
        Ok(data) => (StatusCode::OK, Json(SessionCliEnvelope::success(data, command))),
        Err(error) => envelope_failure(error, command),
    }
}

async fn targets(
    State(state): State<SessionMessageRouterState>,
    headers: HeaderMap,
    Query(query): Query<SessionMentionableQuery>,
) -> (StatusCode, Json<SessionCliEnvelope<SessionMentionableResponse>>) {
    let command = Some("session list".to_owned());
    let caller = match runtime_caller(&state, &headers) {
        Ok(caller) => caller,
        Err(_) => return unauthorized(command),
    };
    if let Err(error) = state
        .service
        .guard_list_access(&caller.user_id, &caller.conversation_id)
        .await
    {
        return envelope_failure(error, command);
    }
    match state
        .targets
        .list(&caller.user_id, &caller.conversation_id, &query)
        .await
    {
        Ok(data) => (StatusCode::OK, Json(SessionCliEnvelope::success(data, command))),
        Err(error) => envelope_failure(error, command),
    }
}

/// The `@@` picker's outlet. Ordinary `ApiResponse`, not the CLI envelope —
/// this one is consumed by the front-end, not by an agent.
async fn mentionable(
    State(state): State<SessionMessageRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<SessionMentionableQuery>,
) -> Result<Json<ApiResponse<SessionMentionableResponse>>, aionui_common::ApiError> {
    // The picker is per-conversation; the current conversation arrives as a
    // query param rather than a path segment so the route stays flat.
    let current = query.current_conversation_id.clone().unwrap_or_default();
    // Same gate as `targets`. The front-end already hides `@@` when the feature
    // is off, but these two routes are one service function behind two auth
    // channels, and letting their POLICY diverge means a direct call answers a
    // question the master switch was supposed to have closed.
    state
        .service
        .guard_list_access(&user.id, &current)
        .await
        .map_err(to_api_error)?;
    let data = state
        .targets
        .list(&user.id, &current, &query)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(data)))
}

/// `ApiError` is a plain enum with tuple variants — there are no
/// `bad_request()`-style constructors. Mapped by code rather than collapsed into
/// one status because this route is user-facing: the picker must be able to tell
/// "you turned the feature off" from "bad input".
fn to_api_error(error: SessionMessageError) -> aionui_common::ApiError {
    use aionui_common::ApiError;
    let message = error.to_string();
    match error.http_status() {
        403 => ApiError::Forbidden(message),
        404 => ApiError::NotFound(message),
        409 => ApiError::Conflict(message),
        429 => ApiError::RateLimited,
        _ => ApiError::BadRequest(message),
    }
}

fn unauthorized<T>(command: Option<String>) -> (StatusCode, Json<SessionCliEnvelope<T>>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(SessionCliEnvelope::failure(
            SessionToolErrorPayload::new(SessionToolErrorCode::RuntimeAuthFailed, "runtime auth failed"),
            command,
        )),
    )
}

fn envelope_failure<T>(
    error: SessionMessageError,
    command: Option<String>,
) -> (StatusCode, Json<SessionCliEnvelope<T>>) {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(SessionCliEnvelope::failure(
            SessionToolErrorPayload::new(error.code(), error.to_string()),
            command,
        )),
    )
}
