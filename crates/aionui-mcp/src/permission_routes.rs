#![allow(clippy::disallowed_types)]
// HTTP router for agent-side permission-policy management.
//
// Routes (all behind the app's auth middleware, wired in `aionui-app`):
//   GET  /api/agents/permission-policy           -> list all supported agents' policy
//   GET  /api/agents/permission-policy/{agent}   -> one agent's policy
//   PUT  /api/agents/permission-policy/{agent}   -> write-through a level
//   POST /api/agents/permission-policy/{agent}/full-auto-free   <unused placeholder>
use std::sync::Arc;

use axum::Router;
use axum::extract::{Json, Path, State};
use axum::routing::{get, post, put};

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;

use crate::error::McpError;
use crate::permission::{PermissionLevel, PermissionPolicyAdapter, PermissionPolicyView, policy_view};

/// Shared state for permission-policy route handlers.
#[derive(Clone)]
pub struct PermissionRouterState {
    /// All registered permission-policy adapters (one per supported agent).
    pub adapters: Vec<Arc<dyn PermissionPolicyAdapter>>,
}

impl PermissionRouterState {
    fn find(&self, agent: &str) -> Option<Arc<dyn PermissionPolicyAdapter>> {
        self.adapters.iter().find(|a| a.agent() == agent).cloned()
    }
}

/// Build the `/api/agents/permission-policy/*` routes.
///
/// Returns a `Vec` view that includes every known agent (supported or not) so the
/// frontend can render the control only for supported ones.
pub fn permission_policy_routes(state: PermissionRouterState) -> Router {
    Router::new()
        .route("/api/agents/permission-policy", get(list_policies))
        .route("/api/agents/permission-policy/{agent}", get(get_policy))
        .route("/api/agents/permission-policy/{agent}", put(set_policy))
        .route("/api/agents/permission-policy/{agent}/clear", post(clear_policy))
        .with_state(state)
}

/// `GET /api/agents/permission-policy` — list all known agents' policy views.
async fn list_policies(
    State(state): State<PermissionRouterState>,
) -> Result<Json<ApiResponse<Vec<PermissionPolicyView>>>, ApiError> {
    let mut out = Vec::with_capacity(state.adapters.len());
    for adapter in &state.adapters {
        out.push(policy_view(adapter.as_ref()).await);
    }
    Ok(Json(ApiResponse::ok(out)))
}

/// `GET /api/agents/permission-policy/{agent}` — single agent policy view.
async fn get_policy(
    State(state): State<PermissionRouterState>,
    Path(agent): Path<String>,
) -> Result<Json<ApiResponse<PermissionPolicyView>>, ApiError> {
    let adapter = state
        .find(&agent)
        .ok_or_else(|| ApiError::NotFound(format!("no permission-policy adapter for agent '{agent}'")))?;
    Ok(Json(ApiResponse::ok(policy_view(adapter.as_ref()).await)))
}

/// Request body for applying a permission level.
#[derive(serde::Deserialize)]
struct ApplyPermissionRequest {
    level: String,
}

/// `PUT /api/agents/permission-policy/{agent}` — write-through a permission level.
async fn set_policy(
    State(state): State<PermissionRouterState>,
    Path(agent): Path<String>,
    Json(body): Json<ApplyPermissionRequest>,
) -> Result<Json<ApiResponse<PermissionPolicyView>>, ApiError> {
    let adapter = state
        .find(&agent)
        .ok_or_else(|| ApiError::NotFound(format!("no permission-policy adapter for agent '{agent}'")))?;
    let level = PermissionLevel::from_name(&body.level)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown permission level '{}'", body.level)))?;
    if !adapter.installed().await.map_err(ApiError::from)? {
        return Err(McpError::AgentNotInstalled(agent.to_string()).into());
    }
    adapter.apply(level).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(policy_view(adapter.as_ref()).await)))
}

/// `POST /api/agents/permission-policy/{agent}/clear` — remove the agent-side policy.
async fn clear_policy(
    State(state): State<PermissionRouterState>,
    Path(agent): Path<String>,
) -> Result<Json<ApiResponse<PermissionPolicyView>>, ApiError> {
    let adapter = state
        .find(&agent)
        .ok_or_else(|| ApiError::NotFound(format!("no permission-policy adapter for agent '{agent}'")))?;
    adapter.clear().await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(policy_view(adapter.as_ref()).await)))
}
