//! `POST /api/runtime/conversations/create` — the agent-facing create route.
//!
//! Authenticates on the three `x-aionui-*` headers itself, exactly like
//! `aionui-session-message/src/routes.rs`, and is therefore mounted OUTSIDE the
//! ordinary auth and CSRF middleware in `aionui-app`. The caller conversation is
//! the token-bound header — the body cannot name a different caller.

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    ConversationCliEnvelope, ConversationCreateRequest, ConversationCreateResponse, ConversationToolErrorCode,
    ConversationToolErrorPayload,
};
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use tracing::warn;

use crate::runtime_create::ConversationCreateError;
use crate::service::ConversationService;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";
const COMMAND: &str = "conversation create";

/// Separate from `ConversationRouterState` so the ordinary conversation routes
/// keep their shape and this one carries only what it needs.
#[derive(Clone)]
pub struct ConversationRuntimeRouterState {
    pub service: ConversationService,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}

pub fn conversation_runtime_routes(state: ConversationRuntimeRouterState) -> Router {
    Router::new()
        .route("/api/runtime/conversations/create", post(create))
        .with_state(state)
}

struct RuntimeCaller {
    user_id: String,
    conversation_id: String,
}

fn runtime_caller(state: &ConversationRuntimeRouterState, headers: &HeaderMap) -> Option<RuntimeCaller> {
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
            // Every conversation's helper token is issued with this constant;
            // despite the prefix it is not team-specific.
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .ok()?;
    Some(RuntimeCaller {
        user_id,
        conversation_id,
    })
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

type Reply = (StatusCode, Json<ConversationCliEnvelope<ConversationCreateResponse>>);

async fn create(
    State(state): State<ConversationRuntimeRouterState>,
    headers: HeaderMap,
    body: Result<Json<ConversationCreateRequest>, JsonRejection>,
) -> Reply {
    let Some(caller) = runtime_caller(&state, &headers) else {
        // No header values are logged — the token must never reach the logs.
        warn!(
            outcome = "rejected",
            error_code = "runtime_auth_failed",
            "agent conversation create refused"
        );
        return failure(
            StatusCode::UNAUTHORIZED,
            ConversationToolErrorPayload::new(ConversationToolErrorCode::RuntimeAuthFailed, "runtime auth failed"),
        );
    };
    // Body parsing is deliberately AFTER auth so an unauthenticated caller
    // learns nothing about the schema; `deny_unknown_fields` on the request
    // type turns stray fields into this same 400.
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => {
            return failure(
                StatusCode::BAD_REQUEST,
                ConversationToolErrorPayload::new(
                    ConversationToolErrorCode::SchemaValidationFailed,
                    format!("request body does not match the schema: {}", rejection.body_text()),
                ),
            );
        }
    };
    match state
        .service
        .create_for_conversation_helper(&caller.user_id, &caller.conversation_id, &request)
        .await
    {
        Ok(data) => (
            StatusCode::OK,
            Json(ConversationCliEnvelope::success(data, Some(COMMAND.to_owned()))),
        ),
        Err(error) => envelope_failure(error),
    }
}

fn envelope_failure(error: ConversationCreateError) -> Reply {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    failure(
        status,
        ConversationToolErrorPayload::new(error.code(), error.to_string()),
    )
}

fn failure(status: StatusCode, error: ConversationToolErrorPayload) -> Reply {
    (
        status,
        Json(ConversationCliEnvelope::failure(error, Some(COMMAND.to_owned()))),
    )
}
