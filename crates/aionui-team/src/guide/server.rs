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

use crate::error::classify_public_error;
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
                    if args.get("backend").is_some() {
                        return Json(serde_json::json!({
                            "error": "backend is no longer accepted; use assistant_id"
                        }))
                        .into_response();
                    }
                    if args.get("agent_type").is_some() {
                        return Json(serde_json::json!({
                            "error": "agent_type is no longer accepted; use assistant_id"
                        }))
                        .into_response();
                    }
                    let mut base = match svc
                        .list_models_from_db(args.get("assistant_id").and_then(serde_json::Value::as_str))
                        .await
                    {
                        Ok(value) => value,
                        Err(error) => {
                            return Json(serde_json::json!({
                                "error": error.to_string()
                            }))
                            .into_response();
                        }
                    };
                    // Guide surfaces Gemini even if not in spawn whitelist
                    if let Some(backends) = base.get_mut("backends").and_then(serde_json::Value::as_array_mut) {
                        let has_gemini = backends
                            .iter()
                            .any(|entry| entry.get("backend").and_then(serde_json::Value::as_str) == Some("gemini"));
                        if !has_gemini {
                            backends.push(serde_json::json!({
                                "backend": "gemini",
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

fn error_response(message: impl Into<String>) -> serde_json::Value {
    let message = message.into();
    if let Some(public) = classify_public_error(&message) {
        let mut data = serde_json::json!({
            "domainCode": public.code,
        });
        if let Some(details) = public.details {
            data["details"] = details;
        }
        serde_json::json!({
            "error": {
                "message": message,
                "data": data,
            }
        })
    } else {
        serde_json::json!({ "error": message })
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

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

    let assistant_id =
        match resolve_requested_assistant_id(&svc, request_body, args, caller_conversation_id.as_deref()).await {
            Ok(assistant_id) => assistant_id,
            Err(error) => {
                warn!(error, "Guide HTTP: aion_create_team missing assistant identity");
                return error_response(error);
            }
        };

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
                return error_response("Failed to inspect conversation team binding.");
            }
        }
    }

    let req = CreateTeamRequest {
        name: params.name.clone(),
        agents: vec![TeamAgentInput {
            name: "Leader".to_owned(),
            role: "leader".to_owned(),
            backend: None,
            model: model.clone(),
            assistant_id: Some(assistant_id),
            conversation_id: caller_conversation_id,
        }],
        workspace: None,
    };

    let team = match svc.create_team(&user_id, req).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "Guide HTTP: aion_create_team create_team failed");
            return error_response(e.to_string());
        }
    };

    let lead_slot_id = match team.leader_assistant_id.as_deref().or_else(|| {
        team.assistants
            .iter()
            .find(|assistant| assistant.role == "leader" || assistant.role == "lead")
            .map(|assistant| assistant.slot_id.as_str())
    }) {
        Some(slot_id) if !slot_id.is_empty() => slot_id,
        _ => {
            warn!(
                team_id = %team.id,
                "Guide HTTP: aion_create_team created team but response did not include a leader slot"
            );
            return error_response("Created team is missing a leader slot.");
        }
    };

    let team_run = match svc.accept_assistant_first_team_run(&team.id, lead_slot_id).await {
        Ok(ack) => ack,
        Err(error) => {
            warn!(
                team_id = %team.id,
                lead_slot_id,
                error = %error,
                "Guide HTTP: aion_create_team created team but failed to open assistant-first TeamRun"
            );
            let route = format!("/team/{}", team.id);
            return serde_json::json!({
                "teamId": team.id,
                "name": team.name,
                "route": route,
                "status": "team_created",
                "error": GUIDE_NO_ACTIVE_TEAM_RUN_HANDOFF_ERROR,
                "next_step": GUIDE_NO_ACTIVE_TEAM_RUN_HANDOFF_ERROR
            });
        }
    };

    let route = format!("/team/{}", team.id);
    info!(
        team_id = %team.id,
        team_run_id = %team_run.team_run_id,
        "Guide HTTP: aion_create_team succeeded"
    );
    serde_json::json!({
        "teamId": team.id,
        "teamRunId": team_run.team_run_id,
        "name": team.name,
        "route": route,
        "status": "team_created",
        "next_step": "You are now the team Leader. Your team tools (team_spawn_agent, team_send_message, etc.) are now active. \
             First call `team_list_assistants` if you need the real catalog for the confirmed lineup. When calling \
             `team_spawn_agent`, use only `assistant_id` values returned by `team_list_assistants` / the `Available \
             Assistants for Spawning` catalog. Do not use backend names like `claude/codex` as `assistant_id`; for \
             generic vendor teammates, choose the matching catalog entry. Treat any backend/model labels from the earlier \
             planning summary as runtime hints only, and map each teammate to a real catalog `assistant_id` before spawning."
    })
}

fn extract_assistant_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("assistant_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("assistant")
                .and_then(|assistant| assistant.get("id"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn validate_requested_assistant_identity_payload(
    request_body: &serde_json::Value,
    args: &serde_json::Value,
) -> Result<(), String> {
    if request_body.get("custom_agent_id").is_some() || args.get("custom_agent_id").is_some() {
        return Err("custom_agent_id is no longer accepted; use assistant_id".to_owned());
    }
    Ok(())
}

async fn resolve_requested_assistant_id(
    service: &Arc<TeamSessionService>,
    request_body: &serde_json::Value,
    args: &serde_json::Value,
    caller_conversation_id: Option<&str>,
) -> Result<String, String> {
    validate_requested_assistant_identity_payload(request_body, args)?;

    if let Some(assistant_id) = extract_assistant_id(request_body).or_else(|| extract_assistant_id(args)) {
        return Ok(assistant_id);
    }

    let Some(conversation_id) = caller_conversation_id else {
        return Err("assistant_id is required when the caller conversation is not assistant-backed".into());
    };

    service
        .lookup_assistant_identity_by_conversation(conversation_id)
        .await
        .map_err(|error| format!("failed to resolve caller assistant identity: {error}"))?
        .ok_or_else(|| "assistant_id is required when the caller conversation is not assistant-backed".into())
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
            warn!(tool = tool_name, team_id = %team_id, error = %err.message, "Guide HTTP: team tool failed");
            if err.domain_code.is_some() || err.details.is_some() {
                let mut data = serde_json::json!({});
                if let Some(domain_code) = err.domain_code {
                    data["domainCode"] = serde_json::json!(domain_code);
                }
                if let Some(details) = err.details {
                    data["details"] = details;
                }
                serde_json::json!({
                    "error": {
                        "message": err.message,
                        "data": data,
                    }
                })
            } else {
                serde_json::json!({"error": err.message})
            }
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
    use std::sync::Arc;
    use std::time::Duration;

    use aionui_db::models::{AssistantDefinitionRow, AssistantOverlayRow, ConversationRow};
    use aionui_db::{
        IAssistantDefinitionRepository, IAssistantOverlayRepository, IConversationRepository, ITeamRepository,
    };
    use tokio::time::timeout;

    use crate::test_utils::workspace_harness::setup_with_assistants_team_repo_and_conversation_repo;

    struct SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow,
    }

    #[async_trait::async_trait]
    impl IAssistantDefinitionRepository for SingleAssistantDefinitionRepo {
        async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, aionui_db::DbError> {
            Ok(vec![self.row.clone()])
        }

        async fn get_by_assistant_id(
            &self,
            assistant_id: &str,
        ) -> Result<Option<AssistantDefinitionRow>, aionui_db::DbError> {
            Ok((self.row.assistant_id == assistant_id).then_some(self.row.clone()))
        }

        async fn get_by_id(&self, definition_id: &str) -> Result<Option<AssistantDefinitionRow>, aionui_db::DbError> {
            Ok((self.row.id == definition_id).then_some(self.row.clone()))
        }

        async fn get_by_source_ref(
            &self,
            _source: &str,
            _source_ref: &str,
        ) -> Result<Option<AssistantDefinitionRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn upsert(
            &self,
            _params: &aionui_db::models::UpsertAssistantDefinitionParams<'_>,
        ) -> Result<AssistantDefinitionRow, aionui_db::DbError> {
            Err(aionui_db::DbError::Init("not implemented".into()))
        }

        async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
    }

    struct SingleAssistantOverlayRepo {
        row: AssistantOverlayRow,
    }

    #[async_trait::async_trait]
    impl IAssistantOverlayRepository for SingleAssistantOverlayRepo {
        async fn get(&self, definition_id: &str) -> Result<Option<AssistantOverlayRow>, aionui_db::DbError> {
            Ok((self.row.assistant_definition_id == definition_id).then_some(self.row.clone()))
        }

        async fn list(&self) -> Result<Vec<AssistantOverlayRow>, aionui_db::DbError> {
            Ok(vec![self.row.clone()])
        }

        async fn upsert(
            &self,
            _params: &aionui_db::models::UpsertAssistantOverlayParams<'_>,
        ) -> Result<AssistantOverlayRow, aionui_db::DbError> {
            Err(aionui_db::DbError::Init("not implemented".into()))
        }

        async fn delete(&self, _definition_id: &str) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
    }

    #[test]
    fn create_team_next_step_tells_solo_agent_to_use_assistant_first_team_tools() {
        let next_step = serde_json::json!({
            "status": "team_created",
            "next_step": "You are now the team Leader. Your team tools (team_spawn_agent, team_send_message, etc.) are now active. \
             First call `team_list_assistants` if you need the real catalog for the confirmed lineup. When calling \
             `team_spawn_agent`, use only `assistant_id` values returned by `team_list_assistants` / the `Available \
             Assistants for Spawning` catalog. Do not use backend names like `claude/codex` as `assistant_id`; for \
             generic vendor teammates, choose the matching catalog entry. Treat any backend/model labels from the earlier \
             planning summary as runtime hints only, and map each teammate to a real catalog `assistant_id` before spawning."
        });
        let next_step = next_step["next_step"].as_str().unwrap();

        assert!(next_step.contains("You are now the team Leader"));
        assert!(next_step.contains("team_spawn_agent"));
        assert!(next_step.contains("team_send_message"));
        assert!(next_step.contains("team_list_assistants"));
        assert!(next_step.contains("assistant_id"));
        assert!(!next_step.contains("End this solo turn now"));
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

    #[test]
    fn extract_assistant_id_ignores_legacy_custom_agent_id() {
        let payload = serde_json::json!({
            "custom_agent_id": "legacy-assistant",
        });
        assert!(extract_assistant_id(&payload).is_none());
    }

    #[test]
    fn validate_requested_assistant_identity_payload_rejects_legacy_custom_agent_id() {
        let err = validate_requested_assistant_identity_payload(
            &serde_json::json!({ "custom_agent_id": "legacy-assistant" }),
            &serde_json::json!({}),
        )
        .expect_err("legacy custom_agent_id should be rejected");

        assert!(err.contains("custom_agent_id"));
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

    #[tokio::test]
    async fn service_backed_list_models_appends_gemini_to_backends() {
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
            row: AssistantDefinitionRow {
                id: "def-guide-models".into(),
                assistant_id: "assistant-models".into(),
                source: "user".into(),
                owner_type: "user".into(),
                source_ref: None,
                source_version: None,
                source_hash: None,
                name: "Models Assistant".into(),
                name_i18n: "{}".into(),
                description: None,
                description_i18n: "{}".into(),
                avatar_type: "emoji".into(),
                avatar_value: Some("🤖".into()),
                agent_id: "claude".into(),
                rule_resource_type: "inline".into(),
                rule_resource_ref: None,
                rule_inline_content: None,
                recommended_prompts: "[]".into(),
                recommended_prompts_i18n: "{}".into(),
                default_model_mode: "auto".into(),
                default_model_value: None,
                default_permission_mode: "auto".into(),
                default_permission_value: None,
                default_skills_mode: "auto".into(),
                default_skill_ids: "[]".into(),
                custom_skill_names: "[]".into(),
                default_disabled_builtin_skill_ids: "[]".into(),
                default_mcps_mode: "auto".into(),
                default_mcp_ids: "[]".into(),
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
            row: AssistantOverlayRow {
                assistant_definition_id: "def-guide-models".into(),
                enabled: true,
                sort_order: 0,
                agent_id_override: None,
                last_used_at: None,
                created_at: 0,
                updated_at: 0,
            },
        });
        let (svc, _team_repo, _task_manager, _conv_repo) =
            setup_with_assistants_team_repo_and_conversation_repo(definition_repo, overlay_repo);

        let server = GuideMcpServer::start().await.expect("start guide server");
        server.set_service(Arc::downgrade(&svc)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header("Authorization", format!("Bearer {}", server.auth_token()))
            .json(&serde_json::json!({
                "tool": "aion_list_models",
                "args": {}
            }))
            .send()
            .await
            .expect("call guide list models");
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value = resp.json().await.expect("guide list models response");
        let result = serde_json::from_str::<serde_json::Value>(body["result"].as_str().expect("result string payload"))
            .expect("parse result payload");
        let backends = result["backends"].as_array().expect("backends array");
        assert!(
            backends.iter().any(|entry| entry["backend"].as_str() == Some("gemini")),
            "service-backed list_models should still advertise gemini",
        );
        assert!(
            result.get("agent_types").is_none(),
            "guide payload should no longer expose legacy agent_types",
        );
    }

    #[tokio::test]
    async fn create_team_uses_assistant_identity_from_caller_conversation() {
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
            row: AssistantDefinitionRow {
                id: "def-guide-lead".into(),
                assistant_id: "assistant-lead".into(),
                source: "user".into(),
                owner_type: "user".into(),
                source_ref: None,
                source_version: None,
                source_hash: None,
                name: "Lead Assistant".into(),
                name_i18n: "{}".into(),
                description: None,
                description_i18n: "{}".into(),
                avatar_type: "emoji".into(),
                avatar_value: Some("🤖".into()),
                agent_id: "claude".into(),
                rule_resource_type: "inline".into(),
                rule_resource_ref: None,
                rule_inline_content: None,
                recommended_prompts: "[]".into(),
                recommended_prompts_i18n: "{}".into(),
                default_model_mode: "auto".into(),
                default_model_value: None,
                default_permission_mode: "auto".into(),
                default_permission_value: None,
                default_skills_mode: "auto".into(),
                default_skill_ids: "[]".into(),
                custom_skill_names: "[]".into(),
                default_disabled_builtin_skill_ids: "[]".into(),
                default_mcps_mode: "auto".into(),
                default_mcp_ids: "[]".into(),
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
            row: AssistantOverlayRow {
                assistant_definition_id: "def-guide-lead".into(),
                enabled: true,
                sort_order: 0,
                agent_id_override: Some("codex".into()),
                last_used_at: None,
                created_at: 0,
                updated_at: 0,
            },
        });
        let (svc, team_repo, _task_manager, conv_repo) =
            setup_with_assistants_team_repo_and_conversation_repo(definition_repo, overlay_repo);

        conv_repo
            .create(&ConversationRow {
                id: "caller-conv".into(),
                user_id: "system_default_user".into(),
                name: "Caller".into(),
                r#type: "acp".into(),
                pinned: false,
                pinned_at: None,
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "assistant_id": "assistant-lead",
                    "workspace": "/tmp/guide-workspace"
                })
                .to_string(),
                model: None,
                status: Some("completed".into()),
                created_at: 0,
                updated_at: 0,
            })
            .await
            .expect("seed caller conversation");

        let server = GuideMcpServer::start().await.expect("start guide server");
        server.set_service(Arc::downgrade(&svc)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header("Authorization", format!("Bearer {}", server.auth_token()))
            .json(&serde_json::json!({
                "tool": "aion_create_team",
                "args": { "summary": "build a review team" },
                "conversation_id": "caller-conv",
                "user_id": "system_default_user"
            }))
            .send()
            .await
            .expect("call guide create team");
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value = resp.json().await.expect("guide create team response");
        let team_id = body["teamId"].as_str().expect("team id in response");
        let next_step = body["next_step"].as_str().expect("next_step in response");
        let team_row = team_repo
            .get_team(team_id)
            .await
            .expect("team lookup")
            .expect("persisted team row");
        let team = crate::types::Team::from_row(&team_row).expect("team row parses");
        let leader = team.agents.first().expect("leader agent exists");

        assert_eq!(leader.assistant_id.as_deref(), Some("assistant-lead"));
        assert_eq!(leader.backend, "codex");
        assert!(next_step.contains("team_list_assistants"));
        assert!(next_step.contains("assistant_id"));
        assert!(next_step.contains("Available Assistants for Spawning"));
        assert!(next_step.contains("claude/codex"));
    }

    #[tokio::test]
    async fn create_team_opens_active_team_run_for_assistant_first_tools() {
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
            row: AssistantDefinitionRow {
                id: "def-guide-teamrun".into(),
                assistant_id: "assistant-teamrun".into(),
                source: "user".into(),
                owner_type: "user".into(),
                source_ref: None,
                source_version: None,
                source_hash: None,
                name: "TeamRun Assistant".into(),
                name_i18n: "{}".into(),
                description: None,
                description_i18n: "{}".into(),
                avatar_type: "emoji".into(),
                avatar_value: Some("🤖".into()),
                agent_id: "claude".into(),
                rule_resource_type: "inline".into(),
                rule_resource_ref: None,
                rule_inline_content: None,
                recommended_prompts: "[]".into(),
                recommended_prompts_i18n: "{}".into(),
                default_model_mode: "auto".into(),
                default_model_value: None,
                default_permission_mode: "auto".into(),
                default_permission_value: None,
                default_skills_mode: "auto".into(),
                default_skill_ids: "[]".into(),
                custom_skill_names: "[]".into(),
                default_disabled_builtin_skill_ids: "[]".into(),
                default_mcps_mode: "auto".into(),
                default_mcp_ids: "[]".into(),
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
            row: AssistantOverlayRow {
                assistant_definition_id: "def-guide-teamrun".into(),
                enabled: true,
                sort_order: 0,
                agent_id_override: Some("claude".into()),
                last_used_at: None,
                created_at: 0,
                updated_at: 0,
            },
        });
        let (svc, _team_repo, _task_manager, conv_repo) =
            setup_with_assistants_team_repo_and_conversation_repo(definition_repo, overlay_repo);

        conv_repo
            .create(&ConversationRow {
                id: "caller-conv-teamrun".into(),
                user_id: "system_default_user".into(),
                name: "Caller".into(),
                r#type: "acp".into(),
                pinned: false,
                pinned_at: None,
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "assistant_id": "assistant-teamrun",
                    "workspace": "/tmp/guide-teamrun-workspace"
                })
                .to_string(),
                model: None,
                status: Some("completed".into()),
                created_at: 0,
                updated_at: 0,
            })
            .await
            .expect("seed caller conversation");

        let server = GuideMcpServer::start().await.expect("start guide server");
        server.set_service(Arc::downgrade(&svc)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header("Authorization", format!("Bearer {}", server.auth_token()))
            .json(&serde_json::json!({
                "tool": "aion_create_team",
                "args": { "summary": "create a debate team" },
                "conversation_id": "caller-conv-teamrun",
                "user_id": "system_default_user"
            }))
            .send()
            .await
            .expect("call guide create team");
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value = resp.json().await.expect("guide create team response");
        let team_id = body["teamId"].as_str().expect("team id in response");

        assert!(is_run_scoped_guide_team_tool("team_spawn_agent"));
        svc.require_active_team_run_for_team_work(team_id)
            .await
            .expect("assistant-first create_team should open a TeamRun before run-scoped tools are used");
    }

    #[tokio::test]
    async fn create_team_next_step_does_not_echo_backend_only_teammate_plan() {
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
            row: AssistantDefinitionRow {
                id: "def-guide-summary".into(),
                assistant_id: "assistant-lead".into(),
                source: "user".into(),
                owner_type: "user".into(),
                source_ref: None,
                source_version: None,
                source_hash: None,
                name: "Lead Assistant".into(),
                name_i18n: "{}".into(),
                description: None,
                description_i18n: "{}".into(),
                avatar_type: "emoji".into(),
                avatar_value: Some("🤖".into()),
                agent_id: "claude".into(),
                rule_resource_type: "inline".into(),
                rule_resource_ref: None,
                rule_inline_content: None,
                recommended_prompts: "[]".into(),
                recommended_prompts_i18n: "{}".into(),
                default_model_mode: "auto".into(),
                default_model_value: None,
                default_permission_mode: "auto".into(),
                default_permission_value: None,
                default_skills_mode: "auto".into(),
                default_skill_ids: "[]".into(),
                custom_skill_names: "[]".into(),
                default_disabled_builtin_skill_ids: "[]".into(),
                default_mcps_mode: "auto".into(),
                default_mcp_ids: "[]".into(),
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
            row: AssistantOverlayRow {
                assistant_definition_id: "def-guide-summary".into(),
                enabled: true,
                sort_order: 0,
                agent_id_override: Some("claude".into()),
                last_used_at: None,
                created_at: 0,
                updated_at: 0,
            },
        });
        let (svc, _team_repo, _task_manager, conv_repo) =
            setup_with_assistants_team_repo_and_conversation_repo(definition_repo, overlay_repo);

        conv_repo
            .create(&ConversationRow {
                id: "caller-conv-summary".into(),
                user_id: "system_default_user".into(),
                name: "Caller".into(),
                r#type: "acp".into(),
                pinned: false,
                pinned_at: None,
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "assistant_id": "assistant-lead",
                    "workspace": "/tmp/guide-workspace"
                })
                .to_string(),
                model: None,
                status: Some("completed".into()),
                created_at: 0,
                updated_at: 0,
            })
            .await
            .expect("seed caller conversation");

        let server = GuideMcpServer::start().await.expect("start guide server");
        server.set_service(Arc::downgrade(&svc)).await;

        let summary = "已确认的团队配置:\n- 正方辩手:gemini(gemini-3.1-pro-preview)\n- 反方辩手:codex(gpt-5.5)\n- 裁判/评委:claude(global.anthropic.claude-sonnet-4-6)";
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header("Authorization", format!("Bearer {}", server.auth_token()))
            .json(&serde_json::json!({
                "tool": "aion_create_team",
                "args": { "summary": summary },
                "conversation_id": "caller-conv-summary",
                "user_id": "system_default_user"
            }))
            .send()
            .await
            .expect("call guide create team");
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value = resp.json().await.expect("guide create team response");
        let next_step = body["next_step"].as_str().expect("next_step in response");

        assert!(next_step.contains("team_list_assistants"));
        assert!(!next_step.contains("正方辩手:gemini("));
        assert!(!next_step.contains("反方辩手:codex("));
        assert!(!next_step.contains("裁判/评委:claude("));
    }

    #[tokio::test]
    async fn create_team_requires_explicit_assistant_id_for_non_assistant_backed_caller() {
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
            row: AssistantDefinitionRow {
                id: "def-guide-non-assistant-caller".into(),
                assistant_id: "assistant-unused".into(),
                source: "user".into(),
                owner_type: "user".into(),
                source_ref: None,
                source_version: None,
                source_hash: None,
                name: "Unused Assistant".into(),
                name_i18n: "{}".into(),
                description: None,
                description_i18n: "{}".into(),
                avatar_type: "emoji".into(),
                avatar_value: Some("🤖".into()),
                agent_id: "claude".into(),
                rule_resource_type: "inline".into(),
                rule_resource_ref: None,
                rule_inline_content: None,
                recommended_prompts: "[]".into(),
                recommended_prompts_i18n: "{}".into(),
                default_model_mode: "auto".into(),
                default_model_value: None,
                default_permission_mode: "auto".into(),
                default_permission_value: None,
                default_skills_mode: "auto".into(),
                default_skill_ids: "[]".into(),
                custom_skill_names: "[]".into(),
                default_disabled_builtin_skill_ids: "[]".into(),
                default_mcps_mode: "auto".into(),
                default_mcp_ids: "[]".into(),
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
            row: AssistantOverlayRow {
                assistant_definition_id: "def-guide-non-assistant-caller".into(),
                enabled: true,
                sort_order: 0,
                agent_id_override: Some("codex".into()),
                last_used_at: None,
                created_at: 0,
                updated_at: 0,
            },
        });
        let (svc, team_repo, _task_manager, conv_repo) =
            setup_with_assistants_team_repo_and_conversation_repo(definition_repo, overlay_repo);

        conv_repo
            .create(&ConversationRow {
                id: "plain-caller-conv".into(),
                user_id: "system_default_user".into(),
                name: "Plain Caller".into(),
                r#type: "chat".into(),
                pinned: false,
                pinned_at: None,
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "workspace": "/tmp/guide-workspace"
                })
                .to_string(),
                model: None,
                status: Some("completed".into()),
                created_at: 0,
                updated_at: 0,
            })
            .await
            .expect("seed non-assistant-backed caller conversation");

        let server = GuideMcpServer::start().await.expect("start guide server");
        server.set_service(Arc::downgrade(&svc)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header("Authorization", format!("Bearer {}", server.auth_token()))
            .json(&serde_json::json!({
                "tool": "aion_create_team",
                "args": { "summary": "build a review team" },
                "conversation_id": "plain-caller-conv",
                "user_id": "system_default_user"
            }))
            .send()
            .await
            .expect("call guide create team for non-assistant-backed caller");
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value = resp.json().await.expect("guide create team error response");

        assert_eq!(
            body["error"]["message"].as_str(),
            Some("assistant_id is required when the caller conversation is not assistant-backed")
        );
        assert_eq!(
            body["error"]["data"]["domainCode"].as_str(),
            Some("TEAM_ASSISTANT_ID_REQUIRED")
        );
        assert_eq!(body["error"]["data"]["details"]["field"].as_str(), Some("assistant_id"));
        assert!(body.get("teamId").is_none());
        assert!(
            team_repo
                .list_teams_by_user("system_default_user")
                .await
                .expect("list teams after rejected create")
                .is_empty()
        );
    }
}
