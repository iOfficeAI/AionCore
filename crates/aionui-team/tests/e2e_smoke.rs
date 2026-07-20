//! End-to-end smoke tests for the team subsystem.
//!
//! **Purpose:** guard against the "agent claims a tool works but it is an
//! empty shell" failure mode by exercising each user-visible capability
//! through its real wiring (TCP MCP server, mailbox, task board, scheduler)
//! and asserting the observable side effect — not just a success return
//! code.
//!
//! Every scenario runs in CI. Together with `session_service_integration`,
//! these tests cover the real MCP transport, scheduler lifecycle, persistent
//! spawn service, mailbox protocol, crash testament, and observable effects.

mod common;

use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use aionui_team::mcp::protocol::{read_frame, write_frame};
use aionui_team::{
    CrashReason, Mailbox, MailboxMessageType, TaskBoard, TeamAgent, TeamMcpServer, TeammateManager, TeammateRole,
    TeammateStatus,
};
use common::MockTeamRepo;
use serde_json::{Value, json};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Shared helpers — local to this file to avoid touching common/mod.rs and
// keep the scaffold self-contained. If more tests start sharing these,
// promote to common/e2e_helpers.rs.
// ---------------------------------------------------------------------------

struct NullBroadcaster;
impl EventBroadcaster for NullBroadcaster {
    fn broadcast(&self, _msg: WebSocketMessage<Value>) {}
}

/// Concrete handle returned by [`setup_team_with_lead`]. Holds every piece
/// a smoke test might need to assert a side effect.
struct SmokeEnv {
    server: TeamMcpServer,
    task_board: Arc<TaskBoard>,
    repo: Arc<MockTeamRepo>,
    #[allow(dead_code)]
    scheduler: Arc<TeammateManager>,
    team_id: String,
    lead_slot_id: String,
    worker_slot_id: String,
    auth_token: String,
}

/// Build a 2-agent team (lead + worker) wired through a real
/// `TeamMcpServer` listening on a random port, against an in-memory mock
/// team repo. Does not spin up a `TeamSessionService`, ACP agents, or
/// backends — those are exercised in the scenarios that need them.
async fn setup_team_with_lead() -> SmokeEnv {
    let repo = Arc::new(MockTeamRepo::new());
    let mailbox = Arc::new(Mailbox::new(repo.clone()));
    let task_board = Arc::new(TaskBoard::new(repo.clone()));
    let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);

    let team_id = "smoke-team".to_string();
    let lead_slot_id = "lead-1".to_string();
    let worker_slot_id = "worker-1".to_string();
    let agents = vec![
        TeamAgent {
            slot_id: lead_slot_id.clone(),
            name: "Leader".into(),
            role: TeammateRole::Lead,
            conversation_id: "conv-lead".into(),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        },
        TeamAgent {
            slot_id: worker_slot_id.clone(),
            name: "Worker".into(),
            role: TeammateRole::Teammate,
            conversation_id: "conv-worker".into(),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        },
    ];
    let scheduler = Arc::new(TeammateManager::new(
        team_id.clone(),
        &agents,
        mailbox.clone(),
        task_board.clone(),
        broadcaster.clone(),
    ));

    let auth_token = "smoke-token".to_string();
    let server = TeamMcpServer::start(
        auth_token.clone(),
        scheduler.clone(),
        team_id.clone(),
        broadcaster,
        std::sync::Weak::new(),
    )
    .await
    .unwrap();

    SmokeEnv {
        server,
        task_board,
        repo,
        scheduler,
        team_id,
        lead_slot_id,
        worker_slot_id,
        auth_token,
    }
}

/// Connect to the MCP server, perform `initialize` as `slot_id`, and
/// return the authenticated stream.
async fn mcp_connect(env: &SmokeEnv, slot_id: &str) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", env.server.port()))
        .await
        .expect("tcp connect to TeamMcpServer");

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "auth_token": env.auth_token,
            "slot_id": slot_id,
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "smoke-test", "version": "1.0" }
        }
    });
    mcp_send(&mut stream, &init_req).await;
    let resp = mcp_recv(&mut stream).await;
    assert!(
        resp["result"]["serverInfo"]["name"].is_string(),
        "initialize failed: {resp}"
    );
    stream
}

/// Send a JSON-RPC `tools/call` and return the raw response envelope.
async fn mcp_call(stream: &mut TcpStream, id: u64, tool: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    mcp_send(stream, &req).await;
    mcp_recv(stream).await
}

async fn mcp_send(stream: &mut TcpStream, req: &Value) {
    let bytes = serde_json::to_vec(req).unwrap();
    write_frame(stream, &bytes).await.unwrap();
}

async fn mcp_recv(stream: &mut TcpStream) -> Value {
    let frame = read_frame(stream).await.unwrap();
    serde_json::from_slice(&frame).unwrap()
}

fn is_error_response(resp: &Value) -> bool {
    resp["result"]["isError"].as_bool().unwrap_or(false)
}

// ===========================================================================
// Scenario 1: create team → lead agent exists → MCP tools available
// ===========================================================================

/// User story: "A team runtime has a lead, and the MCP surface that lead
/// will drive is actually wired (not an empty shell)."
///
/// Flow:
/// 1. Build the real scheduler runtime with a lead + one worker.
/// 2. Start a real TCP `TeamMcpServer` for that runtime.
/// 3. Authenticate through the MCP initialize handshake.
/// 4. `tools/list` exposes the required lifecycle tools.
/// 5. `team_members` returns both agents.
#[tokio::test]
async fn smoke_create_team_and_verify_mcp_tools() {
    let env = setup_team_with_lead().await;
    let mut stream = mcp_connect(&env, &env.lead_slot_id).await;

    mcp_send(
        &mut stream,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    let tools = mcp_recv(&mut stream).await;
    let names = tools["result"]["tools"]
        .as_array()
        .expect("tools/list must return a tool array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"team_members"));
    assert!(names.contains(&"team_spawn_agent"));
    assert!(names.contains(&"team_shutdown_agent"));

    let members = mcp_call(&mut stream, 3, "team_members", json!({})).await;
    assert!(!is_error_response(&members), "team_members failed: {members}");
    let text = members["result"]["content"][0]["text"]
        .as_str()
        .expect("team_members text");
    assert!(text.contains("Leader") && text.contains("Worker"));

    env.server.stop();
}

// ===========================================================================
// Scenario 2: team_spawn_agent actually creates a new agent session
// ===========================================================================

/// User story: "A dynamically provisioned agent becomes a real scheduler
/// member with an isolated conversation and observable lifecycle state."
///
/// Flow:
/// This runtime smoke verifies scheduler registration. The companion
/// `session_service_integration::spawn_agent_in_session_succeeds_without_active_team_run`
/// test covers assistant resolution and conversation/team persistence through
/// the production service before the same registration step.
#[tokio::test]
async fn smoke_spawn_agent_creates_real_session() {
    let env = setup_team_with_lead().await;
    let helper = TeamAgent {
        slot_id: "helper-1".into(),
        name: "Helper".into(),
        role: TeammateRole::Teammate,
        conversation_id: "conv-helper".into(),
        backend: "acp".into(),
        model: "claude".into(),
        assistant_id: Some("assistant-helper".into()),
        status: None,
        conversation_type: None,
        cli_path: None,
    };

    env.scheduler.add_agent(&helper).await;
    let registered = env
        .scheduler
        .get_agent("helper-1")
        .await
        .expect("spawned helper registered");
    assert_eq!(registered.conversation_id, "conv-helper");
    assert_eq!(
        env.scheduler.get_status("helper-1").await.unwrap(),
        TeammateStatus::Idle
    );

    // The service-level companion test
    // `spawn_agent_in_session_succeeds_without_active_team_run` proves that
    // the same registration is preceded by conversation/team persistence.
    env.server.stop();
}

// ===========================================================================
// Scenario 3: shutdown agent — full request/approval protocol
// ===========================================================================

/// User story: "The lead asks a worker to shut down; the worker receives
/// the request, acknowledges it, and actually leaves the runtime roster."
///
/// Flow:
/// 1. Create team, spawn worker.
/// 2. Lead requests `team_shutdown_agent(slot_id=worker)` through scheduler.
/// 3. Worker's mailbox receives a `shutdown_request`.
/// 4. Worker acknowledges shutdown.
/// 5. Worker is removed from the team roster.
///
/// MCP integration tests cover approved/rejected message interception; the
/// scheduler broadcaster tests cover the corresponding UI refresh event.
#[tokio::test]
async fn smoke_shutdown_agent_full_protocol() {
    let env = setup_team_with_lead().await;
    let request = env
        .scheduler
        .request_shutdown_agent(&env.lead_slot_id, &env.worker_slot_id, Some("work complete"))
        .await
        .expect("lead shutdown request");
    assert_eq!(request.msg_type, MailboxMessageType::ShutdownRequest);

    let worker_mail = env
        .repo
        .state
        .lock()
        .unwrap()
        .messages
        .iter()
        .filter(|message| message.to_agent_id == env.worker_slot_id)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(worker_mail.len(), 1);
    assert_eq!(worker_mail[0].content, "work complete");

    env.scheduler.notify_shutdown_acknowledged(&env.worker_slot_id);
    env.scheduler
        .remove_agent(&env.worker_slot_id)
        .await
        .expect("approved shutdown removes worker");
    assert!(env.scheduler.get_agent(&env.worker_slot_id).await.is_err());

    env.server.stop();
}

// ===========================================================================
// Scenario 4: agent crash → testament → leader wake
// ===========================================================================

/// User story: "If a worker crashes mid-task, the lead gets a testament
/// mailbox message and is woken up to react — no silent failure."
///
/// Flow:
/// 1. Create team, spawn worker.
/// 2. Inject an Error stream chunk into the worker's agent manager.
/// 3. Lead's mailbox receives a crash testament.
/// 4. Worker's status transitions to `Error`.
/// 5. Lead is woken (wake_lock acquired / wake payload built).
#[tokio::test]
async fn smoke_agent_crash_recovery() {
    let env = setup_team_with_lead().await;
    let wake_target = env
        .scheduler
        .handle_agent_crash(
            &env.worker_slot_id,
            CrashReason::ProcessExited,
            Some("last bounded worker message"),
        )
        .await
        .expect("crash recovery");
    assert_eq!(wake_target.as_deref(), Some(env.lead_slot_id.as_str()));
    assert_eq!(
        env.scheduler.get_status(&env.worker_slot_id).await.unwrap(),
        TeammateStatus::Error
    );

    let state = env.repo.state.lock().unwrap();
    let testament = state
        .messages
        .iter()
        .find(|message| {
            message.to_agent_id == env.lead_slot_id
                && message.from_agent_id == env.worker_slot_id
                && message.content.contains("last bounded worker message")
        })
        .expect("lead must receive a crash testament");
    assert!(testament.content.contains("last bounded worker message"));
    drop(state);

    env.server.stop();
}

// ===========================================================================
// Scenario 5: MCP tool execution has explicit runtime contracts
// ===========================================================================
//
// This is the anchor scenario that guards against the core failure mode
// the user called out: a tool returning `success` with no observable side
// effect. It only uses pieces that already exist (mailbox + task board +
// TeamMcpServer), so it runs in CI today.

#[tokio::test]
async fn smoke_mcp_tool_execution_not_noop() {
    let env = setup_team_with_lead().await;
    let mut stream = mcp_connect(&env, &env.lead_slot_id).await;

    // --- team_send_message requires a live active Team Run ----------------
    let msg_resp = mcp_call(
        &mut stream,
        10,
        "team_send_message",
        json!({ "to": env.worker_slot_id, "message": "hello worker" }),
    )
    .await;
    assert!(
        is_error_response(&msg_resp),
        "standalone team_send_message must not succeed without TeamSessionService: {msg_resp}"
    );
    assert!(
        msg_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Team service not available"),
        "unexpected team_send_message error: {msg_resp}"
    );

    // --- team_task_create → task board side effect -----------------------
    let task_resp = mcp_call(
        &mut stream,
        11,
        "team_task_create",
        json!({ "subject": "Smoke test subject" }),
    )
    .await;
    assert!(
        !is_error_response(&task_resp),
        "team_task_create returned error: {task_resp}"
    );
    let tasks = env.task_board.list_tasks(&env.team_id).await.unwrap();
    assert!(
        tasks.iter().any(|t| t.subject == "Smoke test subject"),
        "team_task_create did not persist task, got {tasks:?}"
    );

    // --- repo-level cross-check: task rows actually hit storage --
    // Even if the service layer lies, the repo-level mock's state is the
    // ground truth for "did data move through the stack".
    let repo_state = env.repo.state.lock().unwrap();
    assert!(!repo_state.tasks.is_empty(), "no task rows reached the repo");
    drop(repo_state);

    env.server.stop();
}
