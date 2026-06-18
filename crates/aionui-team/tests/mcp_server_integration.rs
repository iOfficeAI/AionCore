mod common;

use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use aionui_team::mcp::protocol::{read_frame, write_frame};
use aionui_team::{Mailbox, TaskBoard, TeamAgent, TeamMcpServer, TeammateManager, TeammateRole};
use common::MockTeamRepo;
use serde_json::{Value, json};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

struct RecordingBroadcaster {
    events: std::sync::Mutex<Vec<WebSocketMessage<Value>>>,
}

impl RecordingBroadcaster {
    fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(vec![]),
        }
    }

    fn events(&self) -> Vec<WebSocketMessage<Value>> {
        self.events.lock().unwrap().clone()
    }
}

impl EventBroadcaster for RecordingBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<Value>) {
        self.events.lock().unwrap().push(event);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_agents() -> Vec<TeamAgent> {
    vec![
        TeamAgent {
            slot_id: "lead-1".into(),
            name: "Leader".into(),
            role: TeammateRole::Lead,
            conversation_id: "conv-lead".into(),
            backend: "acp".into(),
            model: "claude".into(),
            custom_agent_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        },
        TeamAgent {
            slot_id: "worker-1".into(),
            name: "Worker".into(),
            role: TeammateRole::Teammate,
            conversation_id: "conv-worker".into(),
            backend: "acp".into(),
            model: "claude".into(),
            custom_agent_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        },
    ]
}

struct TestEnv {
    server: TeamMcpServer,
    _repo: Arc<MockTeamRepo>,
    broadcaster: Arc<RecordingBroadcaster>,
}

async fn setup() -> TestEnv {
    let repo = Arc::new(MockTeamRepo::new());
    let mailbox = Arc::new(Mailbox::new(repo.clone()));
    let task_board = Arc::new(TaskBoard::new(repo.clone()));
    let recorder = Arc::new(RecordingBroadcaster::new());
    let broadcaster: Arc<dyn EventBroadcaster> = recorder.clone();
    let agents = make_agents();
    let scheduler = Arc::new(TeammateManager::new(
        "team-1".into(),
        &agents,
        mailbox,
        task_board,
        broadcaster.clone(),
    ));

    // W5-D29e: standalone MCP server without a live TeamSessionService —
    // the Weak cannot upgrade, so `team_spawn_agent` will surface the
    // service-unavailable error. Non-spawn tools still exercise scheduler
    // flows directly and do not hit this path.
    let server = TeamMcpServer::start(
        "test-token-123".into(),
        scheduler,
        "team-1".into(),
        broadcaster,
        std::sync::Weak::new(),
    )
    .await
    .unwrap();

    TestEnv {
        server,
        _repo: repo,
        broadcaster: recorder,
    }
}

async fn connect_and_init(port: u16, token: &str, slot_id: &str) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "auth_token": token,
            "slot_id": slot_id,
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "1.0" }
        }
    });
    send_request(&mut stream, &init_req).await;
    let resp = read_response(&mut stream).await;
    assert!(resp["result"]["serverInfo"]["name"].is_string());

    stream
}

async fn send_request(stream: &mut TcpStream, request: &Value) {
    let data = serde_json::to_vec(request).unwrap();
    write_frame(stream, &data).await.unwrap();
}

async fn read_response(stream: &mut TcpStream) -> Value {
    let frame = read_frame(stream).await.unwrap();
    serde_json::from_slice(&frame).unwrap()
}

async fn http_rpc(port: u16, slot_id: &str, payload: Value) -> Value {
    http_rpc_with_auth(port, slot_id, Some("test-token-123"), payload).await
}

async fn http_rpc_with_auth(port: u16, slot_id: &str, token: Option<&str>, payload: Value) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = serde_json::to_string(&payload).unwrap();
    let auth_header = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n{auth_header}x-slot-id: {slot_id}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf);
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap()
}

async fn call_tool(stream: &mut TcpStream, id: u64, tool: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": args
        }
    });
    send_request(stream, &req).await;
    read_response(stream).await
}

async fn list_tools(stream: &mut TcpStream, id: u64) -> Vec<String> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list"
    });
    send_request(stream, &req).await;
    let resp = read_response(stream).await;
    resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

fn extract_text(resp: &Value) -> String {
    resp["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
}

fn is_error_response(resp: &Value) -> bool {
    resp["result"]["isError"].as_bool().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests: Connection & Authentication (MC-1, MC-2, MC-3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mc1_correct_token_connects() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });
    send_request(&mut stream, &req).await;
    let resp = read_response(&mut stream).await;
    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 10);

    env.server.stop();
}

#[tokio::test]
async fn mc2_wrong_token_rejected() {
    let env = setup().await;
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", env.server.port()))
        .await
        .unwrap();

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "auth_token": "wrong-token", "slot_id": "s1" }
    });
    send_request(&mut stream, &init_req).await;
    let resp = read_response(&mut stream).await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Authentication failed")
    );

    env.server.stop();
}

#[tokio::test]
async fn mc3_no_token_rejected() {
    let env = setup().await;
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", env.server.port()))
        .await
        .unwrap();

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    send_request(&mut stream, &init_req).await;
    let resp = read_response(&mut stream).await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Authentication failed")
    );

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: tools/list (TTL-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_list_returns_all_10_tools() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let names = list_tools(&mut stream, 10).await;
    assert_eq!(names.len(), 10);

    assert!(names.contains(&"team_send_message".to_owned()));
    assert!(names.contains(&"team_spawn_agent".to_owned()));
    assert!(names.contains(&"team_task_create".to_owned()));
    assert!(names.contains(&"team_task_update".to_owned()));
    assert!(names.contains(&"team_task_list".to_owned()));
    assert!(names.contains(&"team_members".to_owned()));
    assert!(names.contains(&"team_rename_agent".to_owned()));
    assert!(names.contains(&"team_shutdown_agent".to_owned()));
    assert!(names.contains(&"team_list_models".to_owned()));
    assert!(names.contains(&"team_describe_assistant".to_owned()));

    env.server.stop();
}

#[tokio::test]
async fn mcp_tools_list_filters_lead_only_tools() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let names = list_tools(&mut stream, 10).await;

    assert!(!names.contains(&"team_spawn_agent".to_owned()));
    assert!(!names.contains(&"team_rename_agent".to_owned()));
    assert!(!names.contains(&"team_shutdown_agent".to_owned()));
    assert!(names.contains(&"team_send_message".to_owned()));
    assert!(names.contains(&"team_task_create".to_owned()));
    assert!(names.contains(&"team_task_update".to_owned()));
    assert!(names.contains(&"team_task_list".to_owned()));
    assert!(names.contains(&"team_members".to_owned()));
    assert!(names.contains(&"team_list_models".to_owned()));
    assert!(names.contains(&"team_describe_assistant".to_owned()));

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_send_message (TS-1, TS-2, TS-3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ts1_send_message_requires_live_team_run_service() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "worker-1", "message": "Hello worker"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Team service not available"));

    env.server.stop();
}

#[tokio::test]
async fn ts2_broadcast_message_requires_live_team_run_service() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "*", "message": "Attention all"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Team service not available"));

    env.server.stop();
}

#[tokio::test]
async fn ts3_send_message_to_nonexistent_agent() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "nonexistent", "message": "Hello?"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("No agent matches 'nonexistent'"));

    env.server.stop();
}

#[tokio::test]
async fn team_send_message_shutdown_approved_intercepted() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "lead-1", "message": "shutdown_approved"}),
    )
    .await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    let payload: Value = serde_json::from_str(&text).expect("interception payload is JSON");
    assert_eq!(payload["status"], "shutdown_approved_received");

    env.server.stop();
}

#[tokio::test]
async fn team_send_message_shutdown_rejected_intercepted() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "lead-1", "message": "shutdown_rejected: still finishing task"}),
    )
    .await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    assert_eq!(text, "shutdown_rejected: still finishing task");

    env.server.stop();
}

#[tokio::test]
async fn team_send_message_regular_message_rejects_without_live_team_run_service() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "lead-1", "message": "just a normal update"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Team service not available"));
    assert!(!text.contains("shutdown_approved_received"));
    assert!(!text.contains("shutdown_rejected_received"));

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_spawn_agent (SP-1, SP-2, SP-3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sp1_lead_spawn_requires_live_session_service() {
    // W5-D29e: this standalone test env spins up TeamMcpServer with
    // `Weak::new()` (no live TeamSessionService), so a well-formed Lead
    // spawn now surfaces the service-unavailable error. Real session-level
    // spawn success is covered by `tests/e2e_smoke.rs` scenario 2 and by
    // lib unit tests in `src/session.rs` that wire a TeamSessionService.
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_spawn_agent",
        json!({"name": "Helper", "role": "worker", "backend": "claude"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(
        text.contains("Team service not available"),
        "expected service-unavailable error, got {text:?}"
    );

    env.server.stop();
}

#[tokio::test]
async fn sp2_non_whitelisted_backend_rejected() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_spawn_agent",
        json!({"name": "X", "backend": "malicious"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    // Without a live TeamSessionService the spawn fails at capability check or service access.
    assert!(
        text.contains("not allowed") || text.contains("not available"),
        "unexpected error: {text}"
    );

    env.server.stop();
}

#[tokio::test]
async fn sp3_teammate_cannot_spawn() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_spawn_agent",
        json!({"name": "Helper", "backend": "claude"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Only Lead"));

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_task_create / team_task_list (TTC-1, TTC-2, TTL-1, TTL-2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ttc1_create_basic_task() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_task_create",
        json!({"subject": "Implement feature X"}),
    )
    .await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Implement feature X"));

    env.server.stop();
}

#[tokio::test]
async fn ttc2_create_task_with_dependency() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    call_tool(&mut stream, 2, "team_task_create", json!({"subject": "Task A"})).await;

    let list_resp = call_tool(&mut stream, 3, "team_task_list", json!({})).await;
    let tasks: Vec<Value> = serde_json::from_str(&extract_text(&list_resp)).unwrap();
    let task_a_id = tasks[0]["id"].as_str().unwrap();

    let resp = call_tool(
        &mut stream,
        4,
        "team_task_create",
        json!({"subject": "Task B", "blocked_by": [task_a_id]}),
    )
    .await;

    assert!(!is_error_response(&resp));

    let list_resp2 = call_tool(&mut stream, 5, "team_task_list", json!({})).await;
    let tasks2: Vec<Value> = serde_json::from_str(&extract_text(&list_resp2)).unwrap();
    assert_eq!(tasks2.len(), 2);

    let task_b = tasks2.iter().find(|t| t["subject"] == "Task B").unwrap();
    let blocked_by: Vec<String> = serde_json::from_value(task_b["blocked_by"].clone()).unwrap_or_default();
    assert!(blocked_by.contains(&task_a_id.to_string()));

    env.server.stop();
}

#[tokio::test]
async fn ttl2_task_list_empty() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(&mut stream, 2, "team_task_list", json!({})).await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    let tasks: Vec<Value> = serde_json::from_str(&text).unwrap();
    assert!(tasks.is_empty());

    env.server.stop();
}

#[tokio::test]
async fn ttl1_task_list_after_create() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    call_tool(&mut stream, 2, "team_task_create", json!({"subject": "Task A"})).await;

    let resp = call_tool(&mut stream, 3, "team_task_list", json!({})).await;
    let text = extract_text(&resp);
    let tasks: Vec<Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["subject"], "Task A");

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_task_update (TTU-1, TTU-2, TTU-3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ttu1_update_task_status() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    call_tool(&mut stream, 2, "team_task_create", json!({"subject": "Task A"})).await;

    let list_resp = call_tool(&mut stream, 3, "team_task_list", json!({})).await;
    let tasks: Vec<Value> = serde_json::from_str(&extract_text(&list_resp)).unwrap();
    let task_id = tasks[0]["id"].as_str().unwrap();

    let resp = call_tool(
        &mut stream,
        4,
        "team_task_update",
        json!({"task_id": task_id, "status": "completed"}),
    )
    .await;

    assert!(!is_error_response(&resp));

    let list_resp2 = call_tool(&mut stream, 5, "team_task_list", json!({})).await;
    let tasks2: Vec<Value> = serde_json::from_str(&extract_text(&list_resp2)).unwrap();
    assert_eq!(tasks2[0]["status"], "completed");

    env.server.stop();
}

#[tokio::test]
async fn ttu3_update_nonexistent_task() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_task_update",
        json!({"task_id": "nonexistent-id", "status": "completed"}),
    )
    .await;

    assert!(is_error_response(&resp));

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_members (TM-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tm1_list_all_members() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(&mut stream, 2, "team_members", json!({})).await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    let members: Vec<Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(members.len(), 2);

    let names: Vec<&str> = members.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Leader"));
    assert!(names.contains(&"Worker"));

    // Regression: cold-start agents (including the lead before its first
    // wake) must report an explicit `idle` status — never `null` — so MCP
    // clients do not misread a live teammate as offline.
    for m in &members {
        assert_eq!(
            m["status"].as_str(),
            Some("idle"),
            "team_members must report idle status for cold-start agents, got {:?}",
            m["status"]
        );
    }

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_rename_agent (TRA-1, TRA-2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tra1_rename_existing_agent() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_rename_agent",
        json!({"slot_id": "worker-1", "new_name": "Senior Worker"}),
    )
    .await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("renamed"));

    env.server.stop();
}

#[tokio::test]
async fn tra2_rename_nonexistent_agent() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_rename_agent",
        json!({"slot_id": "nonexistent", "new_name": "X"}),
    )
    .await;

    assert!(is_error_response(&resp));

    env.server.stop();
}

#[tokio::test]
async fn mcp_non_lead_cannot_rename_agent() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_rename_agent",
        json!({"slot_id": "worker-1", "new_name": "Renamed"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Only Lead"));

    env.server.stop();
}

#[tokio::test]
async fn http_mcp_tools_list_filters_lead_only_tools() {
    let env = setup().await;

    let resp = http_rpc(
        env.server.http_port(),
        "worker-1",
        json!({"jsonrpc": "2.0", "id": 10, "method": "tools/list"}),
    )
    .await;
    let names: Vec<String> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();

    assert!(!names.contains(&"team_spawn_agent".to_owned()));
    assert!(!names.contains(&"team_rename_agent".to_owned()));
    assert!(!names.contains(&"team_shutdown_agent".to_owned()));
    assert!(names.contains(&"team_send_message".to_owned()));

    env.server.stop();
}

#[tokio::test]
async fn http_mcp_non_lead_cannot_rename_agent() {
    let env = setup().await;

    let resp = http_rpc(
        env.server.http_port(),
        "worker-1",
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "team_rename_agent",
                "arguments": {
                    "slot_id": "worker-1",
                    "new_name": "Renamed"
                }
            }
        }),
    )
    .await;

    assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Only Lead"));

    env.server.stop();
}

#[tokio::test]
async fn http_mcp_rejects_missing_auth_token() {
    let env = setup().await;

    let resp = http_rpc_with_auth(
        env.server.http_port(),
        "worker-1",
        None,
        json!({"jsonrpc": "2.0", "id": 12, "method": "tools/list"}),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32600));
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Authentication failed")
    );

    env.server.stop();
}

#[tokio::test]
async fn http_mcp_rejects_invalid_auth_token() {
    let env = setup().await;

    let resp = http_rpc_with_auth(
        env.server.http_port(),
        "worker-1",
        Some("wrong-token"),
        json!({"jsonrpc": "2.0", "id": 13, "method": "tools/list"}),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32600));
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Authentication failed")
    );

    env.server.stop();
}

#[tokio::test]
async fn http_mcp_rejects_lead_slot_spoof_without_valid_auth() {
    let env = setup().await;

    let resp = http_rpc_with_auth(
        env.server.http_port(),
        "lead-1",
        Some("wrong-token"),
        json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "team_rename_agent",
                "arguments": {
                    "slot_id": "worker-1",
                    "new_name": "Spoofed"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32600));
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Authentication failed")
    );

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: team_shutdown_agent (TSA-1, TSA-4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tsa1_lead_shutdown_request_requires_live_team_run_service() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_shutdown_agent",
        json!({"slot_id": "worker-1", "reason": "Task complete"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Team service not available"));

    env.server.stop();
}

#[tokio::test]
async fn tsa4_non_lead_cannot_shutdown() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(&mut stream, 2, "team_shutdown_agent", json!({"slot_id": "lead-1"})).await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Only Lead"));

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: Unknown method / non-initialize first request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_method_returns_error() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "lead-1").await;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "unknown/method"
    });
    send_request(&mut stream, &req).await;
    let resp = read_response(&mut stream).await;
    assert!(resp["error"]["code"].as_i64().unwrap() == -32601);

    env.server.stop();
}

#[tokio::test]
async fn non_initialize_first_request_rejected() {
    let env = setup().await;
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", env.server.port()))
        .await
        .unwrap();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    send_request(&mut stream, &req).await;
    let resp = read_response(&mut stream).await;
    assert!(resp["error"]["message"].as_str().unwrap().contains("initialize"));

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: Server stop (SS-2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ss2_stop_server_closes_listener() {
    let env = setup().await;
    let port = env.server.port();

    let _stream = connect_and_init(port, "test-token-123", "lead-1").await;
    env.server.stop();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let result = TcpStream::connect(format!("127.0.0.1:{port}")).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Tests: stdio bridge config (SB-1, SB-3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sb1_bridge_config_generation() {
    use aionui_team::{TeamMcpStdioConfig, TeamMcpStdioServerSpec};

    let env = setup().await;
    let config = TeamMcpStdioConfig {
        team_id: "team-test".into(),
        port: env.server.port(),
        token: env.server.auth_token().to_string(),
        slot_id: "lead-1".into(),
        binary_path: "/bin/aioncore".into(),
    };

    let spec = TeamMcpStdioServerSpec::from_config("/bin/aioncore", &config);
    let env_map: std::collections::HashMap<_, _> = spec.env.iter().cloned().collect();
    assert_eq!(env_map[TeamMcpStdioConfig::ENV_PORT], env.server.port().to_string());
    assert_eq!(env_map[TeamMcpStdioConfig::ENV_TOKEN], "test-token-123");
    assert_eq!(env_map[TeamMcpStdioConfig::ENV_SLOT_ID], "lead-1");

    env.server.stop();
}

#[tokio::test]
async fn sb3_different_agents_get_different_slot_ids() {
    use aionui_team::{TeamMcpStdioConfig, TeamMcpStdioServerSpec};

    let env = setup().await;
    let port = env.server.port();
    let token = env.server.auth_token().to_string();

    let cfg_lead = TeamMcpStdioConfig {
        team_id: "t".into(),
        port,
        token: token.clone(),
        slot_id: "lead-1".into(),
        binary_path: "/b".into(),
    };
    let cfg_worker = TeamMcpStdioConfig {
        team_id: "t".into(),
        port,
        token,
        slot_id: "worker-1".into(),
        binary_path: "/b".into(),
    };
    let spec_lead = TeamMcpStdioServerSpec::from_config("/b", &cfg_lead);
    let spec_worker = TeamMcpStdioServerSpec::from_config("/b", &cfg_worker);
    let kv_lead: std::collections::HashMap<_, _> = spec_lead.env.iter().cloned().collect();
    let kv_worker: std::collections::HashMap<_, _> = spec_worker.env.iter().cloned().collect();

    assert_eq!(
        kv_lead[TeamMcpStdioConfig::ENV_PORT],
        kv_worker[TeamMcpStdioConfig::ENV_PORT]
    );
    assert_ne!(
        kv_lead[TeamMcpStdioConfig::ENV_SLOT_ID],
        kv_worker[TeamMcpStdioConfig::ENV_SLOT_ID]
    );
}

// ---------------------------------------------------------------------------
// Tests: mcpStatus broadcast (W5-D31b-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_status_tcp_ready_is_broadcast_on_successful_bind() {
    use aionui_api_types::{TeamMcpPhase, TeamMcpStatusPayload};

    let env = setup().await;
    let port = env.server.port();

    let events = env.broadcaster.events();
    let status_events: Vec<_> = events.iter().filter(|e| e.name == "team.mcpStatus").collect();
    assert_eq!(
        status_events.len(),
        1,
        "expected exactly one team.mcpStatus event after bind, got {}",
        status_events.len()
    );

    let payload: TeamMcpStatusPayload = serde_json::from_value(status_events[0].data.clone()).unwrap();
    assert_eq!(payload.team_id, "team-1");
    assert_eq!(payload.slot_id, "");
    assert!(matches!(payload.phase, TeamMcpPhase::TcpReady));
    assert_eq!(payload.port, Some(port));
    assert!(payload.server_count.is_none());
    assert!(payload.error.is_none());

    env.server.stop();

    env.server.stop();
}

// ---------------------------------------------------------------------------
// Tests: W5-D30b — shutdown_rejected detection in team_send_message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tsr1_shutdown_rejected_notifies_lead_and_preserves_agent() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "lead-1", "message": "shutdown_rejected: still working"}),
    )
    .await;

    assert!(!is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(
        text.contains("shutdown_rejected"),
        "response should echo the sentinel, got: {text}"
    );
    assert!(
        text.contains("still working"),
        "response should echo the reason, got: {text}"
    );

    // Leader mailbox contains the notification, worker did not receive a
    // literal copy of the sentinel.
    let state = env._repo.state.lock().unwrap();
    let lead_msgs: Vec<_> = state.messages.iter().filter(|m| m.to_agent_id == "lead-1").collect();
    assert_eq!(lead_msgs.len(), 1, "expected exactly one message to lead");
    assert_eq!(lead_msgs[0].from_agent_id, "worker-1");
    assert!(lead_msgs[0].content.contains("Worker"));
    assert!(lead_msgs[0].content.contains("declined shutdown"));
    assert!(lead_msgs[0].content.contains("still working"));

    let lead_self_msgs: Vec<_> = state
        .messages
        .iter()
        .filter(|m| m.to_agent_id == "lead-1" && m.content == "shutdown_rejected: still working")
        .collect();
    assert!(
        lead_self_msgs.is_empty(),
        "raw sentinel must not be delivered as a normal message"
    );
    drop(state);

    env.server.stop();
}

#[tokio::test]
async fn tsr2_shutdown_rejected_with_whitespace_reason() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "lead-1", "message": "  shutdown_rejected:   need more time  "}),
    )
    .await;

    assert!(!is_error_response(&resp));

    let state = env._repo.state.lock().unwrap();
    let lead_msgs: Vec<_> = state.messages.iter().filter(|m| m.to_agent_id == "lead-1").collect();
    assert_eq!(lead_msgs.len(), 1);
    // Reason is trimmed before inclusion in the notification.
    assert!(lead_msgs[0].content.contains("need more time"));
    assert!(!lead_msgs[0].content.contains("  need more time  "));
    drop(state);

    env.server.stop();
}

#[tokio::test]
async fn tsr3_send_message_without_sentinel_rejects_without_live_team_run_service() {
    let env = setup().await;
    let mut stream = connect_and_init(env.server.port(), "test-token-123", "worker-1").await;

    let resp = call_tool(
        &mut stream,
        2,
        "team_send_message",
        json!({"to": "lead-1", "message": "regular update"}),
    )
    .await;

    assert!(is_error_response(&resp));
    let text = extract_text(&resp);
    assert!(text.contains("Team service not available"));

    // The literal message must not land in the lead mailbox.
    let state = env._repo.state.lock().unwrap();
    assert!(
        state.messages.iter().all(|m| m.content != "regular update"),
        "rejected message should not be delivered: {:?}",
        state.messages
    );
    drop(state);

    env.server.stop();
}
