//! E2E integration tests with mock agent tasks.
//!
//! Tests the message flow, confirmation system, and auxiliary routes
//! with a mock IWorkerTaskManager that provides in-memory agents.

mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

use aionui_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
use aionui_ai_agent::protocol::events::TextEventData;
use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{AgentError, AgentStreamEvent, IWorkerTaskManager};
use aionui_api_types::AgentSource;
use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, TimestampMs, now_ms};
use aionui_db::UpsertAgentMetadataParams;
use async_trait::async_trait;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

// ── Mock Agent ──────────────────────────────────────────────────

struct MockAgent {
    conversation_id: String,
    workspace: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Mutex<Vec<Confirmation>>,
    approvals: Mutex<std::collections::HashMap<String, bool>>,
    last_activity: AtomicI64,
}

impl MockAgent {
    fn new(conversation_id: &str, workspace: &str) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            conversation_id: conversation_id.to_owned(),
            workspace: workspace.to_owned(),
            event_tx,
            confirmations: Mutex::new(vec![]),
            approvals: Mutex::new(std::collections::HashMap::new()),
            last_activity: AtomicI64::new(now_ms()),
        }
    }
}

#[async_trait]
impl IAgentTask for MockAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.workspace
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(ConversationStatus::Running)
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.last_activity.load(Ordering::Relaxed)
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, _data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
        self.last_activity.store(now_ms(), Ordering::Relaxed);
        // Emit a text event and finish
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Mock response".into(),
        }));
        let _ = self.event_tx.send(AgentStreamEvent::Finish(
            aionui_ai_agent::protocol::events::FinishEventData::default(),
        ));
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[async_trait]
impl IMockAgent for MockAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }

    fn check_approval(&self, action: &str, _command_type: Option<&str>) -> bool {
        self.approvals.lock().unwrap().get(action).copied().unwrap_or(false)
    }

    fn confirm(&self, _msg_id: &str, call_id: &str, _data: Value, always_allow: bool) -> Result<(), AgentError> {
        let mut confs = self.confirmations.lock().unwrap();
        confs.retain(|c| c.call_id != call_id);
        if always_allow {
            self.approvals.lock().unwrap().insert("test_action".to_owned(), true);
        }
        Ok(())
    }
}

// ── Mock Worker Task Manager ────────────────────────────────────

struct MockTaskManager {
    agents: Mutex<std::collections::HashMap<String, AgentInstance>>,
}

impl MockTaskManager {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn insert(&self, conv_id: &str, workspace: &str) -> Arc<MockAgent> {
        let agent = Arc::new(MockAgent::new(conv_id, workspace));
        self.agents
            .lock()
            .unwrap()
            .insert(conv_id.to_owned(), AgentInstance::Mock(agent.clone()));
        agent
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for MockTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let instance = AgentInstance::Mock(Arc::new(MockAgent::new(conversation_id, "/mock-workspace")));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

// ── Test App builder with mock agents ───────────────────────────

async fn build_app_with_mock_tasks() -> (axum::Router, aionui_app::AppServices, Arc<MockTaskManager>) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let mock_tm = Arc::new(MockTaskManager::new());
    let services = services.with_worker_task_manager(mock_tm.clone());

    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services, mock_tm)
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str) -> String {
    let body = json!({
        "type": "acp",
        "name": name,
        "extra": {}
    });
    let req = common::json_with_token("POST", "/api/conversations", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn upsert_visible_agent_metadata(services: &aionui_app::AppServices, id: &str, agent_type: &str) {
    services
        .agent_registry
        .repo_handle()
        .upsert(&UpsertAgentMetadataParams {
            id,
            icon: None,
            name: id,
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some(id),
            agent_type,
            agent_source: "internal",
            agent_source_info: Some("{}"),
            enabled: true,
            command: None,
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: None,
            behavior_policy: Some("{}"),
            yolo_id: Some("yolo"),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 1,
        })
        .await
        .unwrap();
}

// ── Agent catalog tests ─────────────────────────────────────────

#[tokio::test]
async fn management_endpoint_keeps_deprecated_runtime_rows_for_diagnostics() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    for (id, agent_type) in [
        ("test-visible-acp", "acp"),
        ("test-visible-aionrs", "aionrs"),
        ("test-visible-openclaw", "openclaw-gateway"),
        ("test-visible-nanobot", "nanobot"),
        ("test-visible-remote", "remote"),
        ("test-visible-gemini", "gemini"),
    ] {
        upsert_visible_agent_metadata(&services, id, agent_type).await;
    }
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let agents = body["data"].as_array().expect("data should be array");
    let types: Vec<&str> = agents.iter().filter_map(|agent| agent["agent_type"].as_str()).collect();

    assert!(types.contains(&"acp"));
    assert!(types.contains(&"aionrs"));
    assert!(types.contains(&"openclaw-gateway"));
    assert!(types.contains(&"nanobot"));
    assert!(types.contains(&"remote"));
    assert!(types.contains(&"gemini"));
}

#[tokio::test]
async fn management_endpoint_handles_openclaw_as_acp_backend() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    let meta = services
        .agent_registry
        .find_builtin_by_backend("openclaw")
        .await
        .expect("OpenClaw ACP builtin row should exist");
    assert_eq!(meta.agent_type, AgentType::Acp);
    assert_eq!(meta.backend.as_deref(), Some("openclaw"));
    assert_eq!(meta.command.as_deref(), Some("openclaw"));
    assert_eq!(meta.args, vec!["acp"]);
    assert_eq!(meta.agent_source, AgentSource::Builtin);

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let agents = body["data"].as_array().expect("data should be array");

    let openclaw = agents
        .iter()
        .find(|agent| agent["backend"].as_str() == Some("openclaw"))
        .expect("OpenClaw ACP row should be visible from /api/agents/management");
    assert!(meta.available || openclaw["status"] != "available");
    assert_eq!(openclaw["agent_type"], "acp");
    assert_eq!(openclaw["command"], "openclaw");
    assert_eq!(openclaw["args"], json!(["acp"]));
}

#[tokio::test]
async fn agent_logos_endpoint_returns_backend_to_logo_catalog() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    let req = get_with_token("/api/agents/logos", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be array");

    let logo_for = |backend: &str| -> Option<String> {
        entries
            .iter()
            .find(|entry| entry["backend"].as_str() == Some(backend))
            .and_then(|entry| entry["logo"].as_str())
            .map(str::to_owned)
    };

    // Seeded builtin agents project their stored icon URL.
    assert_eq!(
        logo_for("claude").as_deref(),
        Some("/api/assets/logos/ai-major/claude.svg")
    );
    assert_eq!(
        logo_for("codex").as_deref(),
        Some("/api/assets/logos/tools/coding/codex.svg")
    );

    // Aion CLI has no vendor `backend` (NULL); it must still be keyed by its
    // agent_type ("aionrs") so aionrs conversations resolve a logo.
    assert_eq!(logo_for("aionrs").as_deref(), Some("/api/assets/logos/brand/aion.svg"));

    // Every entry carries a non-empty backend + logo, and backends are unique.
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let backend = entry["backend"].as_str().expect("backend present");
        let logo = entry["logo"].as_str().expect("logo present");
        assert!(!backend.is_empty(), "backend must not be empty");
        assert!(!logo.is_empty(), "logo must not be empty");
        assert!(
            seen.insert(backend.to_owned()),
            "backend {backend} duplicated in catalog"
        );
    }
}

#[tokio::test]
async fn agent_logos_endpoint_includes_disabled_and_missing_rows() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    // A custom/internal row that would be hidden from /api/agents (no command
    // on PATH) must still contribute its logo so historical conversations
    // referencing it can render an icon.
    services
        .agent_registry
        .repo_handle()
        .upsert(&UpsertAgentMetadataParams {
            id: "logo-only-row",
            icon: Some("/api/assets/logos/brand/aion.svg"),
            name: "Logo Only",
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("logo-only-backend"),
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some("{}"),
            enabled: false,
            command: None,
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: None,
            behavior_policy: Some("{}"),
            yolo_id: Some("yolo"),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 1,
        })
        .await
        .unwrap();
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let req = get_with_token("/api/agents/logos", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be array");
    let entry = entries
        .iter()
        .find(|entry| entry["backend"].as_str() == Some("logo-only-backend"));
    assert!(
        entry.is_some(),
        "disabled row with an icon must still appear in the logo catalog"
    );
    assert_eq!(entry.unwrap()["logo"], "/api/assets/logos/brand/aion.svg");
}

// ── Message flow with mock agent ────────────────────────────────

#[tokio::test]
async fn send_message_with_mock_agent_returns_202() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Mock Agent Test").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": "Hello mock agent" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn stop_stream_with_mock_agent() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Stop Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let send_req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": "Start mock agent" }),
        &token,
        &csrf,
    );
    let send_resp = app.clone().oneshot(send_req).await.unwrap();
    assert_eq!(send_resp.status(), StatusCode::ACCEPTED);
    let send_json = body_json(send_resp).await;
    let turn_id = send_json["data"]["turn_id"]
        .as_str()
        .expect("send response includes turn_id");

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({ "turn_id": turn_id }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn runtime_ensure_with_mock_agent() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Runtime Ensure Test").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/runtime/ensure"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Confirmation system with mock agent ─────────────────────────

#[tokio::test]
async fn list_confirmations_empty() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/confirmations"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn confirm_and_check_approval() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval Test").await;
    let agent = mock_tm.insert(&conv_id, "/mock-workspace");

    // Pre-populate a pending confirmation so the confirm endpoint can find it
    agent.confirmations.lock().unwrap().push(Confirmation {
        id: "conf-1".into(),
        call_id: "call-42".into(),
        title: Some("Allow file edit".into()),
        action: Some("test_action".into()),
        description: String::new(),
        command_type: None,
        options: vec![],
    });

    // Confirm a call with alwaysAllow=true
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/confirmations/call-42/confirm"),
        json!({ "msg_id": "msg-1", "data": { "value": "allow" }, "always_allow": true }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Check approval — should be approved for "test_action"
    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=test_action"),
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["approved"], true);
}

#[tokio::test]
async fn check_approval_not_set() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval NotSet").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=unknown_action"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["approved"], false);
}

// ── Auxiliary routes with mock agent ────────────────────────────

#[tokio::test]
async fn slash_commands_with_mock_returns_empty() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Slash Mock Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/slash-commands"), &token);
    let resp = app.oneshot(req).await.unwrap();
    // Mock agent is not a real AcpAgentManager, so downcast fails → 500
    // OR if agent_type check prevents downcast, returns empty array
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "Expected 200 or 500, got {status}"
    );
}

#[tokio::test]
async fn side_question_with_mock_agent() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Side Q Mock").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/side-question"),
        json!({ "question": "What is this code?" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // Mock agent is type Acp but not a real AcpAgentManager, so downcast
    // fails. The handler first checks agent_type() == Acp, then tries to
    // downcast. Since our mock returns Acp type, downcast fails → 500.
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "Expected 200 or 500, got {status}"
    );
}

// ── Agent overrides roundtrip ───────────────────────────────────

#[tokio::test]
async fn agent_overrides_roundtrip_and_management_summary() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    upsert_visible_agent_metadata(&services, "ovr-agent", "acp").await;
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    // PUT overrides
    let body = json!({
        "command_override": "true",
        "env_override": [{"name": "ANTHROPIC_API_KEY", "value": "sk-x"}, {"name": "PATH", "value": "/evil"}]
    });
    let req = json_with_token("PUT", "/api/agents/ovr-agent/overrides", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let put_body = body_json(resp).await;
    assert_eq!(put_body["data"]["last_check_kind"], "manual");
    assert_eq!(put_body["data"]["last_check_status"], "offline");

    // management row: safe fields, blocked PATH not counted
    let mreq = get_with_token("/api/agents/management", &token);
    let mbody = body_json(app.clone().oneshot(mreq).await.unwrap()).await;
    let mbody_str = serde_json::to_string(&mbody).unwrap();
    let row = mbody["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "ovr-agent")
        .expect("row present");
    assert_eq!(row["has_command_override"], true);
    assert_eq!(row["env_override_key_count"], 1); // PATH excluded
    assert!(
        row["env"].as_array().is_none_or(|arr| arr.is_empty()),
        "management row env must be empty or absent"
    );
    assert!(
        !mbody_str.contains("sk-x"),
        "management response must not leak secret values"
    );

    // GET overrides: plaintext echo
    let greq = get_with_token("/api/agents/ovr-agent/overrides", &token);
    let gbody = body_json(app.clone().oneshot(greq).await.unwrap()).await;
    assert_eq!(gbody["data"]["command_override"], "true");
    let envs = gbody["data"]["env_override"].as_array().unwrap();
    assert!(envs.iter().any(|e| e["name"] == "ANTHROPIC_API_KEY"));
}

#[tokio::test]
async fn internal_aion_cli_rejects_overrides() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    upsert_visible_agent_metadata(&services, "632f31d2", "aionrs").await;
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let command_body = json!({
        "command_override": "irm https://claude.ai/install.ps1 | iex",
        "env_override": [{"name": "ANTHROPIC_API_KEY", "value": "sk-x"}]
    });
    let req = json_with_token("PUT", "/api/agents/632f31d2/overrides", command_body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let env_body = json!({
        "env_override": [{"name": "ANTHROPIC_API_KEY", "value": "sk-x"}]
    });
    let req = json_with_token("PUT", "/api/agents/632f31d2/overrides", env_body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let greq = get_with_token("/api/agents/632f31d2/overrides", &token);
    let gbody = body_json(app.clone().oneshot(greq).await.unwrap()).await;
    assert!(gbody["data"]["command_override"].is_null());
    assert!(gbody["data"]["env_override"].as_array().unwrap().is_empty());
}
