//! `aioncore mcp-team-stdio` subcommand: MCP stdio server for team tools.
//!
//! Uses the `rmcp` crate (Rust MCP SDK) for protocol handling. Tool calls are
//! forwarded to the TeamMcpServer TCP listener via 4-byte big-endian
//! length-prefixed JSON frames — the same wire protocol used by `mcp-bridge`,
//! but with proper tool registration via rmcp instead of transparent proxying.
//!
//! Each tool call opens a fresh TCP connection, sends an `initialize` frame
//! (injecting auth_token + slot_id), then sends the `tools/call` frame, reads
//! the response, and closes the connection (one-shot mode).

use std::process::ExitCode;

use crate::commands::cli_error::{CliBoundaryCode, CliBoundaryError, missing_env, parse_required_port};
use aionui_api_types::TeamMcpStdioConfig;
use aionui_team::mcp::protocol::{read_frame, write_frame};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;
use tokio::net::TcpStream;

const SUBCOMMAND: &str = "mcp-team-stdio";
const CONNECT_HOST: &str = "127.0.0.1";
const ERR_JSON_SERIALIZE: &str = "failed to serialize MCP JSON frame";
const ERR_TCP_CONNECT: &str = "failed to connect to local MCP TCP listener";
const ERR_TCP_WRITE: &str = "failed to write MCP frame to TCP listener";
const ERR_TCP_READ: &str = "failed to read MCP frame from TCP listener";
const ERR_TOOL_REMOTE: &str = "local team tool returned an error";
const ERR_TOOL_RESPONSE_UNEXPECTED: &str = "unexpected local team tool response";

pub async fn run_team_stdio() -> ExitCode {
    let env = match TeamStdioEnv::from_env() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("{}", err.stderr_line());
            return err.exit_code();
        }
    };

    let server = TeamStdioServer {
        port: env.port,
        token: env.token,
        slot_id: env.slot_id,
    };

    let transport = transport::io::stdio();
    match server.serve(transport).await {
        Ok(peer) => {
            if let Err(_e) = peer.waiting().await {
                let err = CliBoundaryError::new(
                    CliBoundaryCode::McpSessionEndedWithError,
                    SUBCOMMAND,
                    "MCP stdio session ended with an error",
                );
                eprintln!("{}", err.stderr_line());
                err.exit_code()
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(_e) => {
            let err = CliBoundaryError::new(
                CliBoundaryCode::McpStdioServeFailed,
                SUBCOMMAND,
                "failed to start MCP stdio server",
            );
            eprintln!("{}", err.stderr_line());
            err.exit_code()
        }
    }
}

#[derive(Clone, Debug)]
struct TeamStdioEnv {
    port: u16,
    token: String,
    slot_id: String,
}

impl TeamStdioEnv {
    fn from_env() -> Result<Self, CliBoundaryError> {
        let port_raw = std::env::var(TeamMcpStdioConfig::ENV_PORT)
            .map_err(|_| missing_env(SUBCOMMAND, TeamMcpStdioConfig::ENV_PORT))?;
        let token = std::env::var(TeamMcpStdioConfig::ENV_TOKEN)
            .map_err(|_| missing_env(SUBCOMMAND, TeamMcpStdioConfig::ENV_TOKEN))?;
        let slot_id = std::env::var(TeamMcpStdioConfig::ENV_SLOT_ID)
            .map_err(|_| missing_env(SUBCOMMAND, TeamMcpStdioConfig::ENV_SLOT_ID))?;
        Self::from_values(&port_raw, token, slot_id)
    }

    fn from_values(
        port_raw: &str,
        token: impl Into<String>,
        slot_id: impl Into<String>,
    ) -> Result<Self, CliBoundaryError> {
        Ok(Self {
            port: parse_required_port(SUBCOMMAND, TeamMcpStdioConfig::ENV_PORT, port_raw)?,
            token: token.into(),
            slot_id: slot_id.into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TeamStdioServer {
    port: u16,
    token: String,
    slot_id: String,
}

// ---------------------------------------------------------------------------
// Parameter types
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct SendMessageParams {
    /// Target agent slot_id or "*" for broadcast.
    to: String,
    /// Message content.
    message: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SpawnAgentParams {
    /// Agent display name.
    name: String,
    /// AI backend type: "claude" or "codex". Default when omitted.
    #[serde(default)]
    agent_type: Option<String>,
    /// Model override for the new agent.
    #[serde(default)]
    model: Option<String>,
    /// Preset assistant identifier.
    #[serde(default)]
    custom_agent_id: Option<String>,
    /// Legacy backend field (prefer agent_type).
    #[serde(default)]
    backend: Option<String>,
    /// Agent role (default: "teammate").
    #[serde(default)]
    role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TaskCreateParams {
    /// Task subject.
    subject: String,
    /// Task description.
    #[serde(default)]
    description: Option<String>,
    /// Owning agent slot_id.
    #[serde(default)]
    owner: Option<String>,
    /// Task IDs this task depends on.
    #[serde(default)]
    blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TaskUpdateParams {
    /// Task ID to update.
    task_id: String,
    /// New status: pending, in_progress, completed, deleted.
    #[serde(default)]
    status: Option<String>,
    /// New description.
    #[serde(default)]
    description: Option<String>,
    /// New owning agent slot_id.
    #[serde(default)]
    owner: Option<String>,
    /// New dependency list.
    #[serde(default)]
    blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RenameAgentParams {
    /// Agent slot_id to rename.
    slot_id: String,
    /// New display name.
    new_name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ShutdownAgentParams {
    /// Agent slot_id to shut down.
    slot_id: String,
    /// Reason for shutdown.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListModelsParams {
    /// Agent type/backend to query (e.g. "gemini", "claude", "codex"). Shows all when omitted.
    #[serde(default)]
    agent_type: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DescribeAssistantParams {
    /// The preset assistant ID from the "Available Preset Assistants" catalog.
    custom_agent_id: String,
    /// Locale for the description (e.g. "en", "zh"). Default when omitted.
    #[serde(default)]
    locale: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool router
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl TeamStdioServer {
    #[tool(
        name = "team_send_message",
        description = "Send a message to a teammate or broadcast to all (to=\"*\")."
    )]
    async fn send_message(&self, Parameters(params): Parameters<SendMessageParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_send_message",
            &serde_json::json!({ "to": params.to, "message": params.message }),
        )
        .await
    }

    #[tool(
        name = "team_spawn_agent",
        description = "Create a new teammate agent to join the team.\n\nUse this only when one of the following is true:\n- The user explicitly approved the proposed teammate lineup in a previous message\n- The user explicitly instructed you to create a specific teammate immediately\n\nBefore calling this tool in the normal planning flow:\n- Start with one short sentence explaining why additional teammates would help\n- Tell the user which teammate(s) you recommend\n- Present the proposal as a table with: name, responsibility, recommended agent type/backend, and recommended model\n- Include each teammate's responsibility, recommended agent type/backend, and model\n- Ask whether to create them as proposed or change any names, responsibilities, or agent types\n- In that approval question, remind the user that they can later ask you to replace or adjust any teammate if the lineup is not working well\n- Do NOT call this tool in that same turn; wait for explicit approval in a later user message\n\nWhen calling this tool, provide the model parameter if a specific model was recommended and approved.\n\nThe new agent will be created and added to the team. You can then assign tasks and send messages to it."
    )]
    async fn spawn_agent(&self, Parameters(params): Parameters<SpawnAgentParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_spawn_agent",
            &serde_json::json!({
                "name": params.name,
                "agent_type": params.agent_type,
                "model": params.model,
                "custom_agent_id": params.custom_agent_id,
                "backend": params.backend,
                "role": params.role,
            }),
        )
        .await
    }

    #[tool(name = "team_task_create", description = "Create a new task on the team task board.")]
    async fn task_create(&self, Parameters(params): Parameters<TaskCreateParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_task_create",
            &serde_json::json!({
                "subject": params.subject,
                "description": params.description,
                "owner": params.owner,
                "blocked_by": params.blocked_by,
            }),
        )
        .await
    }

    #[tool(
        name = "team_task_update",
        description = "Update an existing task on the team task board."
    )]
    async fn task_update(&self, Parameters(params): Parameters<TaskUpdateParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_task_update",
            &serde_json::json!({
                "task_id": params.task_id,
                "status": params.status,
                "description": params.description,
                "owner": params.owner,
                "blocked_by": params.blocked_by,
            }),
        )
        .await
    }

    #[tool(name = "team_task_list", description = "List all tasks on the team task board.")]
    async fn task_list(&self) -> CallToolResult {
        self.forward_to_tcp("team_task_list", &serde_json::json!({})).await
    }

    #[tool(
        name = "team_members",
        description = "List all team members with their roles and current status."
    )]
    async fn members(&self) -> CallToolResult {
        self.forward_to_tcp("team_members", &serde_json::json!({})).await
    }

    #[tool(name = "team_rename_agent", description = "Rename a team member.")]
    async fn rename_agent(&self, Parameters(params): Parameters<RenameAgentParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_rename_agent",
            &serde_json::json!({ "slot_id": params.slot_id, "new_name": params.new_name }),
        )
        .await
    }

    #[tool(
        name = "team_shutdown_agent",
        description = "Initiate shutdown of a teammate (Lead only). Sends a shutdown_request to the target agent."
    )]
    async fn shutdown_agent(&self, Parameters(params): Parameters<ShutdownAgentParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_shutdown_agent",
            &serde_json::json!({ "slot_id": params.slot_id, "reason": params.reason }),
        )
        .await
    }

    #[tool(
        name = "team_list_models",
        description = "Query available models for team agent types. Returns the real-time model list that matches the frontend model selector.\n\nUse this to:\n- Check what models are available before spawning an agent with a specific model\n- See all available agent types and their models at once\n- Verify a model ID is valid for a given agent type\n\nPass agent_type to query a specific backend, or omit it to see all."
    )]
    async fn list_models(&self, Parameters(params): Parameters<ListModelsParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_list_models",
            &serde_json::json!({ "agent_type": params.agent_type }),
        )
        .await
    }

    #[tool(
        name = "team_describe_assistant",
        description = "Get detailed information about a preset assistant before spawning it as a teammate.\n\nReturns the preset's full description, enabled skills, and example tasks so you can\njudge whether it fits the user's request. Use this when two or more presets look\nrelevant from the one-line catalog in your system prompt.\n\nOnly works on preset assistants listed in \"Available Preset Assistants for Spawning\".\nAfter confirming a match, call team_spawn_agent with the same custom_agent_id."
    )]
    async fn describe_assistant(&self, Parameters(params): Parameters<DescribeAssistantParams>) -> CallToolResult {
        self.forward_to_tcp(
            "team_describe_assistant",
            &serde_json::json!({ "custom_agent_id": params.custom_agent_id, "locale": params.locale }),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// TCP forwarding
// ---------------------------------------------------------------------------

impl TeamStdioServer {
    /// One-shot TCP forward: connect → initialize (with auth) → tools/call → read response → close.
    async fn forward_to_tcp(&self, tool_name: &str, args: &serde_json::Value) -> CallToolResult {
        match self.do_forward(tool_name, args).await {
            Ok(result) => tool_success(result),
            Err(ToolForwardError::Boundary(err)) => {
                eprintln!("{}", err.stderr_line());
                tool_error(err.code(), tool_error_message(err.code()), None, None)
            }
            Err(ToolForwardError::Tool {
                code,
                message,
                upstream_code,
                domain_code,
            }) => tool_error(code, message, upstream_code, domain_code),
        }
    }

    async fn do_forward(&self, tool_name: &str, args: &serde_json::Value) -> Result<String, ToolForwardError> {
        let mut stream = TcpStream::connect((CONNECT_HOST, self.port))
            .await
            .map_err(|_| tcp_connect_error(self.port))?;
        stream.set_nodelay(true).ok();

        // initialize with auth
        let init_frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "auth_token": self.token,
                "slot_id": self.slot_id,
            }
        });
        let init_bytes = serde_json::to_vec(&init_frame).map_err(|_| json_serialize_error())?;
        write_frame(&mut stream, &init_bytes)
            .await
            .map_err(|_| tcp_write_error())?;
        let init_resp = read_frame(&mut stream).await.map_err(|_| tcp_read_error())?;
        drop(init_resp);

        // tools/call
        let call_frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": args,
            }
        });
        let call_bytes = serde_json::to_vec(&call_frame).map_err(|_| json_serialize_error())?;
        write_frame(&mut stream, &call_bytes)
            .await
            .map_err(|_| tcp_write_error())?;
        let resp_bytes = read_frame(&mut stream).await.map_err(|_| tcp_read_error())?;

        let text = String::from_utf8_lossy(&resp_bytes).into_owned();

        parse_tool_response(&text)
    }
}

#[derive(Debug)]
enum ToolForwardError {
    Boundary(CliBoundaryError),
    Tool {
        code: CliBoundaryCode,
        message: &'static str,
        upstream_code: Option<serde_json::Value>,
        domain_code: Option<serde_json::Value>,
    },
}

impl From<CliBoundaryError> for ToolForwardError {
    fn from(error: CliBoundaryError) -> Self {
        Self::Boundary(error)
    }
}

fn parse_tool_response(text: &str) -> Result<String, ToolForwardError> {
    let value = serde_json::from_str::<serde_json::Value>(text).map_err(|_| tool_response_unexpected())?;
    if value.get("error").is_some() {
        return Err(remote_tool_error(
            extract_nested_code(&value, &["error", "code"]),
            extract_nested_code(&value, &["error", "data", "domainCode"])
                .or_else(|| extract_nested_code(&value, &["error", "data", "code"]))
                .or_else(|| extract_nested_code(&value, &["error", "data", "errorCode"])),
        ));
    }
    let result = value.get("result").ok_or_else(tool_response_unexpected)?;
    if let Some(result) = result.as_str() {
        return Ok(result.to_owned());
    }
    if result
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(remote_tool_error(
            extract_nested_code(result, &["structuredContent", "upstreamCode"])
                .or_else(|| extract_nested_code(result, &["upstreamCode"])),
            extract_nested_code(result, &["structuredContent", "domainCode"])
                .or_else(|| extract_nested_code(result, &["structuredContent", "code"]))
                .or_else(|| extract_nested_code(result, &["structuredContent", "errorCode"]))
                .or_else(|| extract_nested_code(result, &["domainCode"]))
                .or_else(|| extract_nested_code(result, &["code"]))
                .or_else(|| extract_nested_code(result, &["errorCode"])),
        ));
    }
    if let Some(content) = result.get("content").and_then(serde_json::Value::as_array) {
        let text_parts: Vec<&str> = content
            .iter()
            .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .collect();
        if !text_parts.is_empty() {
            return Ok(text_parts.join("\n"));
        }
    }
    Err(tool_response_unexpected().into())
}

fn json_serialize_error() -> CliBoundaryError {
    CliBoundaryError::new(CliBoundaryCode::McpJsonSerializeFailed, SUBCOMMAND, ERR_JSON_SERIALIZE)
}

fn tcp_connect_error(port: u16) -> CliBoundaryError {
    CliBoundaryError::new(CliBoundaryCode::McpTcpConnectFailed, SUBCOMMAND, ERR_TCP_CONNECT)
        .with_field("host", CONNECT_HOST)
        .with_field("port", port.to_string())
}

fn tcp_write_error() -> CliBoundaryError {
    CliBoundaryError::new(CliBoundaryCode::McpTcpWriteFailed, SUBCOMMAND, ERR_TCP_WRITE)
}

fn tcp_read_error() -> CliBoundaryError {
    CliBoundaryError::new(CliBoundaryCode::McpTcpReadFailed, SUBCOMMAND, ERR_TCP_READ)
}

fn remote_tool_error(
    upstream_code: Option<serde_json::Value>,
    domain_code: Option<serde_json::Value>,
) -> ToolForwardError {
    ToolForwardError::Tool {
        code: CliBoundaryCode::McpToolRemoteError,
        message: ERR_TOOL_REMOTE,
        upstream_code,
        domain_code,
    }
}

fn tool_response_unexpected() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpToolResponseUnexpected,
        SUBCOMMAND,
        ERR_TOOL_RESPONSE_UNEXPECTED,
    )
}

fn tool_success(text: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text)])
}

fn tool_error(
    code: CliBoundaryCode,
    message: &'static str,
    upstream_code: Option<serde_json::Value>,
    domain_code: Option<serde_json::Value>,
) -> CallToolResult {
    let mut structured = serde_json::json!({
        "code": code.as_str(),
        "message": message,
    });
    if let Some(upstream_code) = upstream_code {
        structured["upstreamCode"] = upstream_code;
    }
    if let Some(domain_code) = domain_code {
        structured["domainCode"] = domain_code;
    }

    let mut result = CallToolResult::error(vec![Content::text(message)]);
    result.structured_content = Some(structured);
    result
}

fn extract_nested_code(value: &serde_json::Value, path: &[&str]) -> Option<serde_json::Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    match current {
        serde_json::Value::String(_) | serde_json::Value::Number(_) => Some(current.clone()),
        _ => None,
    }
}

fn tool_error_message(code: CliBoundaryCode) -> &'static str {
    match code {
        CliBoundaryCode::McpJsonSerializeFailed => ERR_JSON_SERIALIZE,
        CliBoundaryCode::McpTcpConnectFailed => ERR_TCP_CONNECT,
        CliBoundaryCode::McpTcpWriteFailed => ERR_TCP_WRITE,
        CliBoundaryCode::McpTcpReadFailed => ERR_TCP_READ,
        CliBoundaryCode::McpToolRemoteError => ERR_TOOL_REMOTE,
        CliBoundaryCode::McpToolResponseUnexpected => ERR_TOOL_RESPONSE_UNEXPECTED,
        _ => "team stdio tool forwarding failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::cli_error::CliBoundaryCode;
    use serde_json::json;
    use tokio::net::TcpListener;

    fn first_text(result: &CallToolResult) -> &str {
        result.content[0].as_text().expect("text content").text.as_str()
    }

    #[test]
    fn team_stdio_env_rejects_invalid_port_with_stable_code() {
        let err = TeamStdioEnv::from_values("bad", "tok", "slot-a").unwrap_err();
        assert_eq!(err.code(), CliBoundaryCode::McpEnvInvalidPort);
        assert_eq!(err.exit_code(), std::process::ExitCode::from(2));
    }

    #[test]
    fn team_stdio_env_accepts_valid_values() {
        let env = TeamStdioEnv::from_values("12345", "tok", "slot-a").unwrap();
        assert_eq!(env.port, 12345);
        assert_eq!(env.token, "tok");
        assert_eq!(env.slot_id, "slot-a");
    }

    #[tokio::test]
    async fn forward_to_tcp_reports_read_failure_after_accept_close() {
        let listener = TcpListener::bind((CONNECT_HOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let server = TeamStdioServer {
            port,
            token: "dummy-token".into(),
            slot_id: "dummy-slot".into(),
        };

        let result = server.forward_to_tcp("team_task_list", &json!({})).await;

        accept_task.await.unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "failed to read MCP frame from TCP listener");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_TCP_READ_FAILED"
        );
    }

    #[tokio::test]
    async fn forward_to_tcp_sanitizes_tool_level_error_result() {
        let listener = TcpListener::bind((CONNECT_HOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _init = read_frame(&mut socket).await.unwrap();
            let init_response = serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {}
            }))
            .unwrap();
            write_frame(&mut socket, &init_response).await.unwrap();

            let _call = read_frame(&mut socket).await.unwrap();
            let tool_response = serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": "upstream failure for conv-secret-123"
                        }
                    ],
                    "isError": true
                }
            }))
            .unwrap();
            write_frame(&mut socket, &tool_response).await.unwrap();
        });
        let server = TeamStdioServer {
            port,
            token: "dummy-token".into(),
            slot_id: "dummy-slot".into(),
        };

        let result = server.forward_to_tcp("team_task_list", &json!({})).await;

        accept_task.await.unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "local team tool returned an error");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_TOOL_REMOTE_ERROR"
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("conv-secret-123"));
    }

    #[test]
    fn parse_tool_response_extracts_content_text() {
        let result = parse_tool_response(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [
                        { "type": "text", "text": "first line" },
                        { "type": "text", "text": "second line" }
                    ],
                    "isError": false
                }
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(result, "first line\nsecond line");
    }

    #[test]
    fn parse_tool_response_sanitizes_top_level_error() {
        let err = parse_tool_response(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": {
                    "code": -32000,
                    "message": "remote failure for conv-secret-123"
                }
            })
            .to_string(),
        )
        .unwrap_err();

        let ToolForwardError::Tool {
            code, upstream_code, ..
        } = err
        else {
            panic!("expected tool error");
        };
        assert_eq!(code, CliBoundaryCode::McpToolRemoteError);
        assert_eq!(upstream_code, Some(json!(-32000)));
    }
}
