//! `aioncore mcp-guide-stdio` subcommand: MCP stdio server for team-guide tools.
//!
//! Uses the `rmcp` crate (Rust MCP SDK) for protocol handling, ensuring full
//! compatibility with Claude CLI's MCP client implementation.
//!

// Pre-existing layout: `forward_tool` impl block lives after the test module.
#![allow(clippy::items_after_test_module)]
//! Tool calls are forwarded as HTTP POST to the Guide server running in the main
//! process at `http://127.0.0.1:{AION_MCP_PORT}/tool`.

use std::process::ExitCode;

use crate::commands::error::{CliBoundaryCode, CliBoundaryError, missing_env, parse_required_port};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;

const SUBCOMMAND: &str = "mcp-guide-stdio";
const ENV_PORT: &str = "AION_MCP_PORT";
const ENV_TOKEN: &str = "AION_MCP_TOKEN";
const ENV_BACKEND: &str = "AION_MCP_BACKEND";
const ENV_CONVERSATION_ID: &str = "AION_MCP_CONVERSATION_ID";
const ENV_USER_ID: &str = "AION_MCP_USER_ID";

pub async fn run_team_guide() -> ExitCode {
    let env = match GuideEnv::from_env() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("{}", err.stderr_line());
            return err.exit_code();
        }
    };

    let http_client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let server = GuideServer {
        port: env.port,
        token: env.token,
        backend: env.backend,
        conversation_id: env.conversation_id,
        user_id: env.user_id,
        http_client,
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
struct GuideEnv {
    port: u16,
    token: String,
    backend: String,
    conversation_id: String,
    user_id: String,
}

impl GuideEnv {
    fn from_env() -> Result<Self, CliBoundaryError> {
        let port_raw = std::env::var(ENV_PORT).map_err(|_| missing_env(SUBCOMMAND, ENV_PORT))?;
        let token = std::env::var(ENV_TOKEN).map_err(|_| missing_env(SUBCOMMAND, ENV_TOKEN))?;
        let backend = std::env::var(ENV_BACKEND).unwrap_or_default();
        let conversation_id = std::env::var(ENV_CONVERSATION_ID).unwrap_or_default();
        let user_id = std::env::var(ENV_USER_ID).unwrap_or_default();
        Self::from_values(&port_raw, token, backend, conversation_id, user_id)
    }

    fn from_values(
        port_raw: &str,
        token: impl Into<String>,
        backend: impl Into<String>,
        conversation_id: impl Into<String>,
        user_id: impl Into<String>,
    ) -> Result<Self, CliBoundaryError> {
        Ok(Self {
            port: parse_required_port(SUBCOMMAND, ENV_PORT, port_raw)?,
            token: token.into(),
            backend: backend.into(),
            conversation_id: conversation_id.into(),
            user_id: user_id.into(),
        })
    }
}

#[derive(Clone)]
struct GuideServer {
    port: u16,
    token: String,
    backend: String,
    conversation_id: String,
    user_id: String,
    http_client: reqwest::Client,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateTeamParams {
    /// Task summary or initial instruction to send to the team leader agent.
    summary: String,
    /// Optional team name. When omitted the first few words of summary are used.
    #[serde(default)]
    name: Option<String>,
    /// Absolute path to the project workspace directory.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListModelsParams {
    /// Agent type/backend to query (e.g. "gemini", "claude", "codex"). Shows all when omitted.
    #[serde(default)]
    agent_type: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SendMessageParams {
    /// Target teammate name, or "*" to broadcast to all.
    to: String,
    /// Message content to send.
    message: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SpawnAgentParams {
    /// Name for the new teammate agent.
    name: String,
    /// AI backend type: "claude" or "codex". Default when omitted.
    #[serde(default)]
    agent_type: Option<String>,
    /// Preset assistant identifier.
    #[serde(default)]
    custom_agent_id: Option<String>,
    /// Model override for the new agent.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TaskCreateParams {
    /// Short task title.
    subject: String,
    /// Detailed task description.
    #[serde(default)]
    description: Option<String>,
    /// Teammate name assigned as owner.
    #[serde(default)]
    owner: Option<String>,
    /// Task IDs that must complete before this task can start.
    #[serde(default)]
    blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TaskUpdateParams {
    /// ID of the task to update.
    task_id: String,
    /// New status: pending, in_progress, completed, or deleted.
    #[serde(default)]
    status: Option<String>,
    /// Updated task description.
    #[serde(default)]
    description: Option<String>,
    /// New owner teammate name.
    #[serde(default)]
    owner: Option<String>,
    /// Updated list of blocking task IDs.
    #[serde(default)]
    blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RenameAgentParams {
    /// Slot ID of the team member to rename.
    slot_id: String,
    /// New display name.
    new_name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ShutdownAgentParams {
    /// Slot ID of the teammate to shut down.
    slot_id: String,
    /// Optional reason for shutdown.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TeamListModelsParams {
    /// Agent type to filter models (e.g. "claude", "codex"). Shows all when omitted.
    #[serde(default)]
    agent_type: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DescribeAssistantParams {
    /// Preset assistant identifier to look up.
    custom_agent_id: String,
    /// Locale for the description (e.g. "en", "zh"). Default when omitted.
    #[serde(default)]
    locale: Option<String>,
}

#[tool_router(server_handler)]
impl GuideServer {
    #[tool(
        name = "aion_create_team",
        description = "Create a multi-agent Team. Only call after user explicitly confirms team configuration."
    )]
    async fn create_team(&self, Parameters(params): Parameters<CreateTeamParams>) -> CallToolResult {
        self.forward_tool(
            "aion_create_team",
            &serde_json::json!({
                "summary": params.summary,
                "name": params.name,
                "workspace": params.workspace,
            }),
        )
        .await
    }

    #[tool(
        name = "aion_list_models",
        description = "Query available models for team agent types. Pass agent_type to filter, or omit to see all."
    )]
    async fn list_models(&self, Parameters(params): Parameters<ListModelsParams>) -> CallToolResult {
        self.forward_tool(
            "aion_list_models",
            &serde_json::json!({
                "agent_type": params.agent_type,
            }),
        )
        .await
    }

    #[tool(
        name = "team_send_message",
        description = "Send a message to a teammate or broadcast to all (to=\"*\")."
    )]
    async fn team_send_message(&self, Parameters(params): Parameters<SendMessageParams>) -> CallToolResult {
        self.forward_tool(
            "team_send_message",
            &serde_json::json!({
                "to": params.to,
                "message": params.message,
            }),
        )
        .await
    }

    #[tool(
        name = "team_spawn_agent",
        description = "Create a new teammate agent to join the team. Leader only."
    )]
    async fn team_spawn_agent(&self, Parameters(params): Parameters<SpawnAgentParams>) -> CallToolResult {
        self.forward_tool(
            "team_spawn_agent",
            &serde_json::json!({
                "name": params.name,
                "agent_type": params.agent_type,
                "custom_agent_id": params.custom_agent_id,
                "model": params.model,
            }),
        )
        .await
    }

    #[tool(name = "team_task_create", description = "Create a new task on the team task board.")]
    async fn team_task_create(&self, Parameters(params): Parameters<TaskCreateParams>) -> CallToolResult {
        self.forward_tool(
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
    async fn team_task_update(&self, Parameters(params): Parameters<TaskUpdateParams>) -> CallToolResult {
        self.forward_tool(
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
    async fn team_task_list(&self) -> CallToolResult {
        self.forward_tool("team_task_list", &serde_json::json!({})).await
    }

    #[tool(
        name = "team_members",
        description = "List all team members with their roles and current status."
    )]
    async fn team_members(&self) -> CallToolResult {
        self.forward_tool("team_members", &serde_json::json!({})).await
    }

    #[tool(name = "team_rename_agent", description = "Rename a team member.")]
    async fn team_rename_agent(&self, Parameters(params): Parameters<RenameAgentParams>) -> CallToolResult {
        self.forward_tool(
            "team_rename_agent",
            &serde_json::json!({
                "slot_id": params.slot_id,
                "new_name": params.new_name,
            }),
        )
        .await
    }

    #[tool(
        name = "team_shutdown_agent",
        description = "Initiate graceful shutdown of a teammate. Leader only."
    )]
    async fn team_shutdown_agent(&self, Parameters(params): Parameters<ShutdownAgentParams>) -> CallToolResult {
        self.forward_tool(
            "team_shutdown_agent",
            &serde_json::json!({
                "slot_id": params.slot_id,
                "reason": params.reason,
            }),
        )
        .await
    }

    #[tool(
        name = "team_list_models",
        description = "Query available models for team agent types."
    )]
    async fn team_list_models(&self, Parameters(params): Parameters<TeamListModelsParams>) -> CallToolResult {
        self.forward_tool(
            "team_list_models",
            &serde_json::json!({
                "agent_type": params.agent_type,
            }),
        )
        .await
    }

    #[tool(
        name = "team_describe_assistant",
        description = "Get detailed information about a preset assistant before spawning."
    )]
    async fn team_describe_assistant(&self, Parameters(params): Parameters<DescribeAssistantParams>) -> CallToolResult {
        self.forward_tool(
            "team_describe_assistant",
            &serde_json::json!({
                "custom_agent_id": params.custom_agent_id,
                "locale": params.locale,
            }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn first_text(result: &CallToolResult) -> &str {
        result.content[0].as_text().expect("text content").text.as_str()
    }

    #[test]
    fn all_tool_schemas_have_properties_field() {
        let router = GuideServer::tool_router();
        let tools = router.list_all();
        assert!(!tools.is_empty());
        for tool in &tools {
            assert!(
                tool.input_schema.contains_key("properties"),
                "Tool '{}' schema missing 'properties' field: {:?}. OpenAI API rejects schemas without it.",
                tool.name,
                tool.input_schema,
            );
        }
    }

    #[test]
    fn guide_env_rejects_invalid_port_with_stable_code() {
        let err = GuideEnv::from_values("bad", "tok", "", "", "").unwrap_err();
        assert_eq!(err.code(), crate::commands::error::CliBoundaryCode::McpEnvInvalidPort);
        assert_eq!(err.exit_code(), std::process::ExitCode::from(2));
    }

    #[test]
    fn guide_env_accepts_valid_values() {
        let env = GuideEnv::from_values("4567", "tok", "codex", "conv-1", "user-1").unwrap();
        assert_eq!(env.port, 4567);
        assert_eq!(env.token, "tok");
        assert_eq!(env.backend, "codex");
        assert_eq!(env.conversation_id, "conv-1");
        assert_eq!(env.user_id, "user-1");
    }

    fn guide_server_for_port(port: u16) -> GuideServer {
        GuideServer {
            port,
            token: "test-token".to_owned(),
            backend: "codex".to_owned(),
            conversation_id: "conv-secret-123".to_owned(),
            user_id: "user-secret-456".to_owned(),
            http_client: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn forward_tool_non_2xx_json_error_returns_status_error_without_raw_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "unauthorized for conv-secret-123",
            })))
            .mount(&mock_server)
            .await;

        let server = guide_server_for_port(mock_server.address().port());
        let result = server.forward_tool("team_members", &json!({})).await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "local guide server returned HTTP 401");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_HTTP_STATUS_ERROR"
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("unauthorized"));
        assert!(!serialized.contains("conv-secret-123"));
    }

    #[tokio::test]
    async fn forward_tool_2xx_json_error_returns_remote_error_without_raw_id() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "tool failed for conv-secret-123",
            })))
            .mount(&mock_server)
            .await;

        let server = guide_server_for_port(mock_server.address().port());
        let result = server.forward_tool("team_members", &json!({})).await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "local guide tool returned an error");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_TOOL_REMOTE_ERROR"
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("conv-secret-123"));
    }

    #[tokio::test]
    async fn forward_tool_create_team_object_success_returns_success_text() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "teamId": "team-1",
                "name": "Dev Team",
                "route": "/team/team-1",
                "status": "team_created",
                "next_step": "Team was created and the Team page is open. End this solo turn now.",
            })))
            .mount(&mock_server)
            .await;

        let server = guide_server_for_port(mock_server.address().port());
        let result = server
            .forward_tool("aion_create_team", &json!({"summary": "redacted"}))
            .await;

        assert_ne!(result.is_error, Some(true));
        let text = first_text(&result);
        assert!(text.contains("team_created"));
        assert!(text.contains("team-1"));
        assert!(text.contains("End this solo turn now"));
    }

    #[tokio::test]
    async fn forward_tool_create_team_malformed_object_remains_unexpected() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "Dev Team",
                "route": "/team/team-1",
                "status": "team_created",
                "next_step": "Team was created and the Team page is open. End this solo turn now.",
            })))
            .mount(&mock_server)
            .await;

        let server = guide_server_for_port(mock_server.address().port());
        let result = server
            .forward_tool("aion_create_team", &json!({"summary": "redacted"}))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "unexpected local guide tool response");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_TOOL_RESPONSE_UNEXPECTED"
        );
    }

    #[tokio::test]
    async fn forward_tool_create_team_legacy_result_body_remains_unexpected() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "legacy create-team success",
            })))
            .mount(&mock_server)
            .await;

        let server = guide_server_for_port(mock_server.address().port());
        let result = server
            .forward_tool("aion_create_team", &json!({"summary": "redacted"}))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "unexpected local guide tool response");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_TOOL_RESPONSE_UNEXPECTED"
        );
    }

    #[tokio::test]
    async fn forward_tool_unexpected_2xx_body_returns_unexpected_without_echoing_body() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "teamId": "team-secret-789",
                "route": "/team/team-secret-789",
                "status": "team_created",
                "next_step": "Team was created and the Team page is open. End this solo turn now.",
            })))
            .mount(&mock_server)
            .await;

        let server = guide_server_for_port(mock_server.address().port());
        let result = server.forward_tool("team_members", &json!({})).await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "unexpected local guide tool response");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_TOOL_RESPONSE_UNEXPECTED"
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("team-secret-789"));
        assert!(!serialized.contains("team_created"));
    }

    #[tokio::test]
    async fn forward_tool_response_read_failure_is_not_overwritten_by_later_connect_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                    Content-Type: application/json\r\n\
                    Content-Length: 128\r\n\
                    Connection: close\r\n\
                    \r\n\
                    {\"result\":\"partial",
                )
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let server = guide_server_for_port(port);
        let result = server.forward_tool("team_members", &json!({})).await;
        server_task.await.unwrap();

        assert_eq!(result.is_error, Some(true));
        assert_eq!(first_text(&result), "failed to read local guide server response");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "MCP_HTTP_RESPONSE_READ_FAILED"
        );
    }
}

impl GuideServer {
    async fn forward_tool(&self, tool_name: &str, args: &serde_json::Value) -> CallToolResult {
        let url = format!("http://127.0.0.1:{}/tool", self.port);
        let body = serde_json::json!({
            "tool": tool_name,
            "args": args,
            "backend": self.backend,
            "conversation_id": self.conversation_id,
            "user_id": self.user_id,
        });

        // Retry up to 3 times with backoff — the Guide HTTP server may not be
        // fully ready immediately after a session resume spawns this process.
        let delays_ms: &[u64] = &[0, 1000, 2000, 3000];
        let mut last_error = None;
        for &delay_ms in delays_ms {
            if delay_ms > 0 {
                let delay = std::time::Duration::from_millis(delay_ms);
                tokio::time::sleep(delay).await;
            }
            match self
                .http_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.token))
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        return tool_error(
                            CliBoundaryCode::McpHttpStatusError,
                            &format!("local guide server returned HTTP {}", status.as_u16()),
                            None,
                            None,
                        );
                    }
                    match resp.text().await {
                        Ok(text) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(result) = parse_tool_success_text(tool_name, &v) {
                                    return tool_success(result);
                                }
                                if v.get("error").is_some() {
                                    return tool_error(
                                        CliBoundaryCode::McpToolRemoteError,
                                        "local guide tool returned an error",
                                        extract_nested_code(&v, &["error", "code"]),
                                        extract_nested_code(&v, &["error", "data", "domainCode"])
                                            .or_else(|| extract_nested_code(&v, &["error", "data", "code"]))
                                            .or_else(|| extract_nested_code(&v, &["error", "data", "errorCode"])),
                                    );
                                }
                            }
                            return tool_error(
                                CliBoundaryCode::McpToolResponseUnexpected,
                                "unexpected local guide tool response",
                                None,
                                None,
                            );
                        }
                        Err(_e) => {
                            let err = CliBoundaryError::new(
                                CliBoundaryCode::McpHttpResponseReadFailed,
                                SUBCOMMAND,
                                "failed to read local guide server response",
                            );
                            eprintln!("{}", err.stderr_line());
                            return tool_error(
                                CliBoundaryCode::McpHttpResponseReadFailed,
                                "failed to read local guide server response",
                                None,
                                None,
                            );
                        }
                    }
                }
                Err(_e) => {
                    let err = CliBoundaryError::new(
                        CliBoundaryCode::McpHttpConnectOrTimeout,
                        SUBCOMMAND,
                        "failed to connect to local guide server or request timed out",
                    );
                    eprintln!("{}", err.stderr_line());
                    last_error = Some((
                        CliBoundaryCode::McpHttpConnectOrTimeout,
                        "failed to connect to local guide server or request timed out",
                    ));
                }
            }
        }
        let (code, message) = last_error.unwrap_or((
            CliBoundaryCode::McpHttpConnectOrTimeout,
            "failed to connect to local guide server or request timed out",
        ));
        tool_error(code, message, None, None)
    }
}

fn tool_success(text: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text)])
}

fn tool_error(
    code: CliBoundaryCode,
    message: &str,
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

    let mut result = CallToolResult::error(vec![Content::text(message.to_owned())]);
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

fn parse_tool_success_text(tool_name: &str, value: &serde_json::Value) -> Option<String> {
    if tool_name == "aion_create_team" {
        return is_create_team_success_body(value).then(|| serde_json::to_string(value).ok())?;
    }

    if let Some(result) = value.get("result").and_then(|result| result.as_str()) {
        return Some(result.to_owned());
    }

    None
}

fn is_create_team_success_body(value: &serde_json::Value) -> bool {
    value.get("status").and_then(|status| status.as_str()) == Some("team_created")
        && value.get("teamId").and_then(|team_id| team_id.as_str()).is_some()
        && value.get("route").and_then(|route| route.as_str()).is_some()
        && value
            .get("next_step")
            .and_then(|next_step| next_step.as_str())
            .is_some()
}
