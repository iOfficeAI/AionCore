//! Callback endpoint for the Antigravity permission hook.
//!
//! agy cannot prompt for tool permission in headless mode, so AionUi registers
//! its own binary as agy's `PreToolUse` hook. That hook process posts here and
//! blocks; we raise the user's permission card and answer once they choose.
//!
//! This route is deliberately NOT behind `auth_middleware`: the hook is a local
//! process with no user session. It authenticates with a per-conversation token
//! instead, checked here. Every failure answers `deny` — an unapproved tool must
//! never run because the bridge was unhealthy.

use std::sync::Arc;

use aionui_ai_agent::agent_task::AgentInstance;
use aionui_ai_agent::antigravity_hook::HookTokenRegistry;
use aionui_ai_agent::task_manager::IWorkerTaskManager;
use aionui_api_types::{AntigravityHookConfig, AntigravityHookInput, AntigravityHookOutput};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::post;

#[derive(Clone)]
pub struct AntigravityHookRouterState {
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub tokens: Arc<HookTokenRegistry>,
}

pub fn antigravity_hook_routes(state: AntigravityHookRouterState) -> Router {
    Router::new()
        .route("/internal/antigravity-hook/{conversation_id}", post(handle_hook))
        .with_state(state)
}

async fn handle_hook(
    State(state): State<AntigravityHookRouterState>,
    Path(conversation_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<AntigravityHookInput>,
) -> Json<AntigravityHookOutput> {
    let presented = headers
        .get(AntigravityHookConfig::TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !state.tokens.verify(&conversation_id, presented) {
        // Do not echo the tool or conversation: an unauthenticated caller
        // should learn nothing about the session.
        tracing::warn!("antigravity hook: rejected a callback with an invalid token");
        return Json(AntigravityHookOutput::deny(
            "aionui rejected an unauthenticated hook callback",
        ));
    }

    // Expanded so an MCP call names its real target instead of the generic
    // `call_mcp_tool` wrapper agy routes them all through.
    let tool_name = match input.tool_call.display_name() {
        n if n.is_empty() => "unknown tool".to_owned(),
        n => n,
    };

    let Some(AgentInstance::Session(task)) = state.task_manager.get_task(&conversation_id) else {
        // The conversation was closed or rebuilt while agy was mid-tool.
        tracing::warn!(
            conversation_id = %conversation_id,
            tool = %tool_name,
            "antigravity hook: no live session to ask; denying"
        );
        return Json(AntigravityHookOutput::deny(
            "aionui has no live session for this conversation",
        ));
    };

    // Blocks until the user answers, the turn ends, or the session is torn
    // down; every one of those paths resolves the request rather than leaving
    // the hook (and agy's tool call) hanging.
    let decision = task
        .request_external_permission(tool_name.clone(), input.tool_call.args.clone())
        .await;

    tracing::info!(
        conversation_id = %conversation_id,
        tool = %tool_name,
        approved = matches!(
            decision,
            aionui_session::PermissionDecision::Approved | aionui_session::PermissionDecision::AllowAlways
        ),
        "antigravity hook: permission answered"
    );

    Json(match decision {
        aionui_session::PermissionDecision::Approved | aionui_session::PermissionDecision::AllowAlways => {
            AntigravityHookOutput::allow("approved in AionUi")
        }
        aionui_session::PermissionDecision::Denied => AntigravityHookOutput::deny("rejected in AionUi"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::WorkerTaskManagerImpl;
    use aionui_ai_agent::task_manager::AgentFactory;
    use aionui_api_types::{AntigravityHookDecision, AntigravityHookToolCall};
    use futures_util::FutureExt;

    fn state() -> (AntigravityHookRouterState, Arc<HookTokenRegistry>) {
        // A factory that is never invoked: these tests never build a task, they
        // exercise the paths where no live session exists.
        let factory: AgentFactory = Arc::new(|_| async { unreachable!("no task is built in these tests") }.boxed());
        let tokens = Arc::new(HookTokenRegistry::new());
        (
            AntigravityHookRouterState {
                task_manager: Arc::new(WorkerTaskManagerImpl::new(factory)),
                tokens: Arc::clone(&tokens),
            },
            tokens,
        )
    }

    fn request(tool: &str) -> AntigravityHookInput {
        AntigravityHookInput {
            conversation_id: "conv-1".into(),
            tool_call: AntigravityHookToolCall {
                name: tool.into(),
                args: serde_json::json!({"CommandLine": "rm -rf /"}),
            },
            ..Default::default()
        }
    }

    fn headers_with(token: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = token {
            h.insert(AntigravityHookConfig::TOKEN_HEADER, t.parse().unwrap());
        }
        h
    }

    #[tokio::test]
    async fn a_missing_token_is_denied() {
        // Without this check any local process could approve tool execution on
        // the user's behalf — the endpoint has no user session to fall back on.
        let (st, _) = state();
        let Json(out) = handle_hook(
            State(st),
            Path("conv-1".to_owned()),
            headers_with(None),
            Json(request("run_command")),
        )
        .await;
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
    }

    #[tokio::test]
    async fn a_wrong_token_is_denied() {
        let (st, tokens) = state();
        tokens.issue("conv-1");
        let Json(out) = handle_hook(
            State(st),
            Path("conv-1".to_owned()),
            headers_with(Some("not-the-token")),
            Json(request("run_command")),
        )
        .await;
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
    }

    #[tokio::test]
    async fn a_valid_token_without_a_live_session_is_denied() {
        // The conversation was closed or rebuilt while agy was mid-tool: there
        // is nobody to ask, so the tool must not run.
        let (st, tokens) = state();
        let token = tokens.issue("conv-1");
        let Json(out) = handle_hook(
            State(st),
            Path("conv-1".to_owned()),
            headers_with(Some(&token)),
            Json(request("run_command")),
        )
        .await;
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
    }

    #[tokio::test]
    async fn a_token_for_another_conversation_is_denied() {
        let (st, tokens) = state();
        let other = tokens.issue("conv-2");
        let Json(out) = handle_hook(
            State(st),
            Path("conv-1".to_owned()),
            headers_with(Some(&other)),
            Json(request("run_command")),
        )
        .await;
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
    }
}
