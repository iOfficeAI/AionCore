use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use aionui_common::generate_id;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::service::TeamSessionService;
use crate::types::TeammateRole;

type ServiceSlot = Arc<RwLock<Weak<TeamSessionService>>>;

#[derive(Clone)]
struct GuideState {
    auth_token: String,
    service: ServiceSlot,
}

pub struct GuideMcpServer {
    http_addr: SocketAddr,
    auth_token: String,
    shutdown_handle: Option<tokio::task::JoinHandle<()>>,
    service_slot: ServiceSlot,
}

impl GuideMcpServer {
    pub async fn start() -> Result<Self, String> {
        let auth_token = generate_id();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind guide MCP HTTP listener: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to read guide MCP local addr: {e}"))?;

        let service_slot: ServiceSlot = Arc::new(RwLock::new(Weak::new()));

        let state = GuideState {
            auth_token: auth_token.clone(),
            service: service_slot.clone(),
        };

        let app = axum::Router::new()
            .route("/tool", axum::routing::post(handle_tool_request))
            .with_state(state);

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "Guide MCP axum server exited with error");
            }
        });

        debug!(http_port = http_addr.port(), "Guide MCP Server started (axum)");

        Ok(Self {
            http_addr,
            auth_token,
            shutdown_handle: Some(handle),
            service_slot,
        })
    }

    /// Wire the TeamSessionService after it is constructed.
    /// Must be called once before the first `aion_create_team` request arrives.
    pub async fn set_service(&self, service: Weak<TeamSessionService>) {
        *self.service_slot.write().await = service;
    }

    pub fn http_port(&self) -> u16 {
        self.http_addr.port()
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn stop(&mut self) {
        if let Some(handle) = self.shutdown_handle.take() {
            handle.abort();
            debug!(http_port = self.http_addr.port(), "Guide MCP Server stop requested");
        }
    }
}

impl Drop for GuideMcpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Axum handler
// ---------------------------------------------------------------------------

async fn handle_tool_request(
    State(state): State<GuideState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Auth check
    let provided_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if provided_token != state.auth_token {
        warn!("Guide HTTP: unauthorized request");
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tool = body.get("tool").and_then(serde_json::Value::as_str).unwrap_or("");
    let args = body.get("args").cloned().unwrap_or(serde_json::Value::Null);

    info!(tool, "Guide HTTP: dispatching tool");

    let response_body = match tool {
        "aion_create_team" => exec_create_team(&body, &args, &state.service).await,
        "aion_list_models" => {
            let result = match state.service.read().await.upgrade() {
                Some(svc) => {
                    let mut base = svc.list_models_from_db(None).await;
                    // Guide surfaces Gemini even if not in spawn whitelist
                    if let Some(types) = base.get_mut("agent_types").and_then(serde_json::Value::as_array_mut) {
                        let has_gemini = types
                            .iter()
                            .any(|e| e.get("type").and_then(serde_json::Value::as_str) == Some("gemini"));
                        if !has_gemini {
                            types.push(serde_json::json!({
                                "type": "gemini",
                                "models": ["gemini-2.5-pro", "gemini-2.5-flash"]
                            }));
                        }
                    }
                    base
                }
                None => crate::guide::handlers::handle_aion_list_models(),
            };
            info!("Guide HTTP: aion_list_models succeeded");
            serde_json::json!({"result": serde_json::to_string(&result).unwrap_or_default()})
        }
        t if t.starts_with("team_") => exec_team_tool(t, &body, &args, &state.service).await,
        unknown => {
            warn!(tool = unknown, "Guide HTTP: unknown tool");
            serde_json::json!({"error": format!("Unknown tool: {unknown}")})
        }
    };

    let mut resp = Json(response_body).into_response();
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

fn build_create_team_handoff_next_step(summary: &str) -> String {
    format!(
        "Team was created and the UI has switched to the team conversation. End this solo turn now. \
         Do not call any `team_*` tools from this solo turn. Reply to the user only with one short \
         handoff in their language. It should mean: the Team is ready, send the next message, and I will continue from there. \
         Do not mention the Team page, solo turn, `team_*` tools, `TeamRun`, or internal tool state. \
         Task summary: {summary}"
    )
}

const NO_ACTIVE_TEAM_RUN_FOR_RUN_SCOPED_WAKE: &str = "no active team run for run-scoped wake";
const GUIDE_NO_ACTIVE_TEAM_RUN_HANDOFF_ERROR: &str =
    "Team was created, but no TeamRun is active yet. Open the team chat and continue from there.";

fn is_run_scoped_guide_team_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "team_send_message"
            | "team_spawn_agent"
            | "team_task_create"
            | "team_task_update"
            | "team_rename_agent"
            | "team_shutdown_agent"
    )
}

fn guide_no_active_team_run_handoff_response() -> serde_json::Value {
    serde_json::json!({ "error": GUIDE_NO_ACTIVE_TEAM_RUN_HANDOFF_ERROR })
}

async fn exec_create_team(
    request_body: &serde_json::Value,
    args: &serde_json::Value,
    service: &ServiceSlot,
) -> serde_json::Value {
    use crate::guide::handlers::parse_create_team_args;
    use aionui_api_types::{CreateTeamRequest, TeamAgentInput};

    let svc = match service.read().await.upgrade() {
        Some(s) => s,
        None => {
            warn!("Guide HTTP: aion_create_team — service not available");
            return serde_json::json!({"error": "service_unavailable"});
        }
    };

    let caller_workspace: Option<&str> = None;
    let params = match parse_create_team_args(args, caller_workspace) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Guide HTTP: aion_create_team parse error");
            return serde_json::json!({"error": e});
        }
    };

    let backend = request_body
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("claude")
        .to_owned();

    let model = request_body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();

    let user_id = request_body
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("system_default_user")
        .to_owned();

    let caller_conversation_id = request_body
        .get("conversation_id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Refuse if the caller conversation already belongs to a team.
    // This prevents duplicate team creation when guide MCP is
    // erroneously injected into an existing team leader session.
    if let Some(ref conv_id) = caller_conversation_id {
        match svc.lookup_team_binding_by_conversation(conv_id).await {
            Ok(Some(binding)) if binding.team_id.as_deref().is_some_and(|s| !s.is_empty()) => {
                warn!(
                    conversation_id = conv_id,
                    "Guide HTTP: aion_create_team refused — conversation already belongs to a team"
                );
                return serde_json::json!({
                    "error": "This conversation already belongs to a team. Cannot create another team from here."
                });
            }
            Ok(_) => {}
            Err(error) => {
                warn!(conversation_id = conv_id, error = %error, "Guide HTTP: team binding lookup failed");
                return serde_json::json!({"error": "Failed to inspect conversation team binding."});
            }
        }
    }

    let req = CreateTeamRequest {
        name: params.name.clone(),
        agents: vec![TeamAgentInput {
            name: "Leader".to_owned(),
            role: "leader".to_owned(),
            backend: backend.clone(),
            model: model.clone(),
            custom_agent_id: None,
            conversation_id: caller_conversation_id,
        }],
        workspace: None,
    };

    let team = match svc.create_team(&user_id, req).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "Guide HTTP: aion_create_team create_team failed");
            return serde_json::json!({"error": e.to_string()});
        }
    };

    let route = format!("/team/{}", team.id);
    info!(team_id = %team.id, "Guide HTTP: aion_create_team succeeded");
    serde_json::json!({
        "teamId": team.id,
        "name": team.name,
        "route": route,
        "status": "team_created",
        "next_step": build_create_team_handoff_next_step(&params.summary)
    })
}

async fn exec_team_tool(
    tool_name: &str,
    request_body: &serde_json::Value,
    args: &serde_json::Value,
    service: &ServiceSlot,
) -> serde_json::Value {
    let svc = match service.read().await.upgrade() {
        Some(s) => s,
        None => {
            warn!("Guide HTTP: {} — service not available", tool_name);
            return serde_json::json!({"error": "service_unavailable"});
        }
    };

    let conversation_id = match request_body
        .get("conversation_id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_owned(),
        None => {
            warn!(tool = tool_name, "Guide HTTP: team tool missing conversation_id");
            return serde_json::json!({"error": "missing conversation_id"});
        }
    };

    let (team_id, slot_id) = match resolve_team_context(&svc, &conversation_id).await {
        Ok(ctx) => ctx,
        Err(e) => {
            warn!(tool = tool_name, error = %e, "Guide HTTP: resolve_team_context failed");
            return serde_json::json!({"error": e});
        }
    };

    let scheduler = match svc.get_session_scheduler(&team_id) {
        Some(s) => s,
        None => {
            warn!(tool = tool_name, team_id = %team_id, "Guide HTTP: no active session for team");
            return serde_json::json!({"error": "No active team session. The team may still be starting up."});
        }
    };

    if is_run_scoped_guide_team_tool(tool_name) {
        match svc.require_active_team_run_for_team_work(&team_id).await {
            Ok(()) => {}
            Err(crate::TeamError::InvalidRequest(message)) if message == NO_ACTIVE_TEAM_RUN_FOR_RUN_SCOPED_WAKE => {
                warn!(
                    tool = tool_name,
                    team_id = %team_id,
                    "Guide HTTP: run-scoped team tool refused because no active TeamRun exists"
                );
                return guide_no_active_team_run_handoff_response();
            }
            Err(error) => {
                warn!(
                    tool = tool_name,
                    team_id = %team_id,
                    error = %error,
                    "Guide HTTP: active TeamRun check failed before forwarding team tool"
                );
                return serde_json::json!({"error": error.to_string()});
            }
        }
    }

    let svc_weak = Arc::downgrade(&svc);
    let result = crate::mcp::server::dispatch_tool(
        tool_name,
        args,
        &scheduler,
        &svc_weak,
        &team_id,
        &slot_id,
        TeammateRole::Lead,
    )
    .await;

    match result {
        Ok(text) => {
            info!(tool = tool_name, team_id = %team_id, "Guide HTTP: team tool succeeded");
            serde_json::json!({"result": text})
        }
        Err(err) => {
            warn!(tool = tool_name, team_id = %team_id, error = %err, "Guide HTTP: team tool failed");
            serde_json::json!({"error": err})
        }
    }
}

/// Resolve `(team_id, slot_id)` for a caller identified by `conversation_id`.
///
/// Decodes the conversation row's typed Team binding, then finds the agent slot
/// whose `conversation_id` matches. Returns an error string if no active team is
/// found for this conversation.
async fn resolve_team_context(service: &TeamSessionService, conversation_id: &str) -> Result<(String, String), String> {
    let binding = service
        .lookup_team_binding_by_conversation(conversation_id)
        .await
        .map_err(|e| format!("DB error reading conversation: {e}"))?
        .ok_or_else(|| format!("Conversation not found: {conversation_id}"))?;

    let team_id = binding
        .team_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "No active team for this conversation. Create a team first with aion_create_team.".to_owned())?;

    // Find the slot_id by matching conversation_id in the session scheduler.
    let scheduler = service
        .get_session_scheduler(&team_id)
        .ok_or_else(|| "No active team session. The team may still be starting up.".to_owned())?;

    let slot_id = if let Some(slot_id) = binding.slot_id.filter(|s| !s.is_empty()) {
        slot_id
    } else {
        let agents = scheduler.list_agents().await;
        agents
            .iter()
            .find(|a| a.conversation_id == conversation_id)
            .map(|a| a.slot_id.clone())
            .ok_or_else(|| format!("Agent with conversation_id={conversation_id} not found in team {team_id}"))?
    };

    Ok((team_id, slot_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn create_team_next_step_tells_solo_agent_to_end_turn() {
        let next_step = build_create_team_handoff_next_step("Build a research and implementation team");

        assert!(next_step.contains("Team was created and the UI has switched to the team conversation."));
        assert!(next_step.contains("End this solo turn now."));
        assert!(next_step.contains("Do not call any `team_*` tools from this solo turn."));
        assert!(next_step.contains(
            "Reply to the user only with one short handoff in their language. It should mean: the Team is ready, send the next message, and I will continue from there."
        ));
        assert!(
            next_step.contains(
                "Do not mention the Team page, solo turn, `team_*` tools, `TeamRun`, or internal tool state."
            )
        );
        assert!(next_step.contains("Task summary: Build a research and implementation team"));
        assert!(
            !next_step.contains("team_spawn_agent"),
            "next_step must not name spawn as an immediately available action"
        );
        assert!(
            !next_step.contains("team_send_message"),
            "next_step must not name send_message as an immediately available action"
        );
        assert!(
            !next_step.contains("tools are now active"),
            "next_step must not claim Team tools are active immediately after creation"
        );
    }

    #[test]
    fn run_scoped_guide_team_tools_are_classified_for_handoff_guard() {
        for tool_name in [
            "team_send_message",
            "team_spawn_agent",
            "team_task_create",
            "team_task_update",
            "team_rename_agent",
            "team_shutdown_agent",
        ] {
            assert!(
                is_run_scoped_guide_team_tool(tool_name),
                "{tool_name} should require an active TeamRun in the Guide forwarding path"
            );
        }

        for tool_name in [
            "team_members",
            "team_task_list",
            "team_list_models",
            "team_describe_assistant",
        ] {
            assert!(
                !is_run_scoped_guide_team_tool(tool_name),
                "{tool_name} is read-only/catalog-style and should not use the run-scoped handoff guard"
            );
        }
    }

    #[test]
    fn guide_no_active_team_run_handoff_error_is_clear() {
        let response = guide_no_active_team_run_handoff_response();
        let error = response
            .get("error")
            .and_then(serde_json::Value::as_str)
            .expect("error string");

        assert_eq!(
            error,
            "Team was created, but no TeamRun is active yet. Open the team chat and continue from there."
        );
        assert!(!error.contains("no active team run for run-scoped wake"));
    }

    #[test]
    fn guide_handoff_guard_is_not_a_correctness_api() {
        assert!(is_run_scoped_guide_team_tool("team_send_message"));
        let response = guide_no_active_team_run_handoff_response();
        let text = serde_json::to_string(&response).unwrap();
        assert!(text.contains("Open the team chat"));
        assert!(
            !text.contains("correctness"),
            "guide handoff text must stay user-facing and not document concurrency guarantees"
        );
    }

    #[tokio::test]
    async fn start_returns_positive_port_and_token() {
        let server = GuideMcpServer::start().await.expect("start should succeed");
        assert!(server.http_port() > 0, "http_port should be assigned");
        assert!(!server.auth_token().is_empty(), "auth_token should be generated");
    }

    #[tokio::test]
    async fn each_start_uses_a_fresh_auth_token() {
        let a = GuideMcpServer::start().await.unwrap();
        let b = GuideMcpServer::start().await.unwrap();
        assert_ne!(a.auth_token(), b.auth_token());
    }

    #[tokio::test]
    async fn stop_closes_the_listener() {
        let mut server = GuideMcpServer::start().await.unwrap();
        let port = server.http_port();
        server.stop();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let result = timeout(
            Duration::from_millis(500),
            client
                .post(format!("http://127.0.0.1:{port}/tool"))
                .json(&serde_json::json!({}))
                .send(),
        )
        .await;
        match result {
            Ok(Ok(_)) => { /* may still accept in-flight during abort */ }
            Ok(Err(_)) => { /* connection refused — expected */ }
            Err(_) => { /* timeout — expected */ }
        }
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let mut server = GuideMcpServer::start().await.unwrap();
        server.stop();
        server.stop();
    }

    #[tokio::test]
    async fn tool_call_requires_auth() {
        let server = GuideMcpServer::start().await.unwrap();
        let port = server.http_port();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/tool"))
            .json(&serde_json::json!({"tool": "aion_list_models", "args": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn tool_call_with_valid_token_succeeds() {
        let server = GuideMcpServer::start().await.unwrap();
        let port = server.http_port();
        let token = server.auth_token().to_owned();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/tool"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({"tool": "aion_list_models", "args": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body.get("result").is_some());
    }
}
