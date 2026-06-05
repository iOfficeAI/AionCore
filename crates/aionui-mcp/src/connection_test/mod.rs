mod protocol;

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    McpConnectionTestErrorCode, McpConnectionTestResult, RuntimeFailureKind, RuntimeResourceKind, RuntimeStatusPayload,
    RuntimeStatusPhase, RuntimeStatusScope, RuntimeStatusScopeKind, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{
    Builder as CmdBuilder, NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressReporter,
    RuntimeCommandProbe, ensure_runtime_command_with_reporter, kill_process_tree, probe_runtime_command,
    resolve_command_path,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::types::McpServerTransport;
use protocol::{
    JsonRpcRequest, JsonRpcResponse, SseEvent, build_http_headers, build_initialize_request,
    build_initialized_notification, build_tools_list_request, error_result, read_sse_events, rpc_error_result,
    run_stdio_protocol, spawn_error_result, success_result, timeout_result, wait_for_endpoint,
    wait_for_jsonrpc_response,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// McpConnectionTestService
// ---------------------------------------------------------------------------

/// Service for testing MCP server connectivity.
///
/// Creates a temporary MCP client, performs the protocol handshake
/// (initialize -> initialized -> tools/list), and returns the tool list
/// or an error.  Supports stdio, HTTP (Streamable HTTP), and SSE transports.
#[derive(Clone)]
pub struct McpConnectionTestService {
    http_client: reqwest::Client,
    timeout: Duration,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl McpConnectionTestService {
    pub fn new(http_client: reqwest::Client, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            http_client,
            timeout: CONNECTION_TIMEOUT,
            broadcaster,
        }
    }

    /// Override the connection test timeout (default: 30s).
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }

    /// Test connectivity to an MCP server.
    ///
    /// Dispatches to the appropriate transport handler.  Always returns
    /// a result (never errors) -- failures are encoded in the struct.
    pub async fn test_connection(&self, name: &str, transport: &McpServerTransport) -> McpConnectionTestResult {
        self.test_connection_with_runtime_scope(name, transport, None).await
    }

    pub async fn test_connection_with_runtime_scope(
        &self,
        name: &str,
        transport: &McpServerTransport,
        runtime_scope_id: Option<&str>,
    ) -> McpConnectionTestResult {
        debug!(name, ?transport, "starting MCP connection test");
        match transport {
            McpServerTransport::Stdio { command, args, env } => {
                self.test_stdio(command, args, env, runtime_scope_id).await
            }
            McpServerTransport::Http { url, headers } => self.test_http(url, headers).await,
            McpServerTransport::Sse { url, headers } => self.test_sse(url, headers).await,
        }
    }

    // -- Stdio transport --------------------------------------------------

    async fn test_stdio(
        &self,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        runtime_scope_id: Option<&str>,
    ) -> McpConnectionTestResult {
        self.test_stdio_inner(command, args, env, runtime_scope_id).await
    }

    async fn test_stdio_inner(
        &self,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        runtime_scope_id: Option<&str>,
    ) -> McpConnectionTestResult {
        let reporter = runtime_scope_id.map(|scope_id| self.runtime_reporter(scope_id.to_owned()));
        let mut cmd = match probe_runtime_command(command) {
            RuntimeCommandProbe::NodeTool { .. } => {
                let resolved = match ensure_runtime_command_with_reporter(command, reporter.as_deref()).await {
                    Ok(resolved) => resolved,
                    Err(error) => return spawn_error_result(command, &runtime_resolution_error(&error.to_string())),
                };
                CmdBuilder::from_resolved(&resolved)
            }
            _ => {
                let program = resolve_stdio_command(command);
                CmdBuilder::new(&program)
            }
        };
        cmd.args(args)
            .envs(env.iter())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return spawn_error_result(command, &e),
        };

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let result = match tokio::time::timeout(self.timeout, run_stdio_protocol(stdin, stdout)).await {
            Ok(r) => r,
            Err(_) => timeout_result(self.timeout),
        };
        if let Err(error) = kill_process_tree(&mut child).await {
            warn!(%error, "failed to clean up MCP stdio connection test process tree");
        }
        result
    }

    // -- HTTP (Streamable HTTP) transport ---------------------------------

    async fn test_http(&self, url: &str, headers: &HashMap<String, String>) -> McpConnectionTestResult {
        match tokio::time::timeout(self.timeout, self.test_http_inner(url, headers)).await {
            Ok(r) => r,
            Err(_) => timeout_result(self.timeout),
        }
    }

    async fn test_http_inner(&self, url: &str, headers: &HashMap<String, String>) -> McpConnectionTestResult {
        let mut req_headers = build_http_headers(headers);
        req_headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().expect("valid header"),
        );
        req_headers.insert(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream".parse().expect("valid header"),
        );

        // 1. initialize
        let init_resp = match self
            .http_post_mcp(url, &req_headers, &build_initialize_request(1))
            .await
        {
            Ok(r) => r,
            Err(result) => return result,
        };
        if let Some(err) = init_resp.rpc.error {
            return rpc_error_result("initialize", &err);
        }

        // Extract session ID for subsequent requests
        if let Some(sid) = init_resp.session_id
            && let Ok(val) = reqwest::header::HeaderValue::from_str(&sid)
        {
            req_headers.insert("mcp-session-id", val);
        }

        // 2. initialized notification (fire-and-forget)
        let _ = self
            .http_client
            .post(url)
            .headers(req_headers.clone())
            .json(&build_initialized_notification())
            .send()
            .await;

        // 3. tools/list
        let tools_resp = match self
            .http_post_mcp(url, &req_headers, &build_tools_list_request(2))
            .await
        {
            Ok(r) => r,
            Err(result) => return result,
        };
        if let Some(err) = tools_resp.rpc.error {
            return rpc_error_result("tools/list", &err);
        }

        success_result(tools_resp.rpc.result)
    }

    /// POST a JSON-RPC message and parse the response.
    ///
    /// Returns `Err(McpConnectionTestResult)` for HTTP-level failures
    /// (connection error, 401, non-success status).
    async fn http_post_mcp(
        &self,
        url: &str,
        headers: &reqwest::header::HeaderMap,
        body: &JsonRpcRequest,
    ) -> Result<HttpMcpResponse, McpConnectionTestResult> {
        let resp = self
            .http_client
            .post(url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await
            .map_err(|e| {
                error_result(
                    McpConnectionTestErrorCode::ConnectionFailed,
                    format!("Connection failed: {e}"),
                    Some(serde_json::json!({ "transport": "http" })),
                )
            })?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(protocol::auth_result(resp.headers()));
        }
        if !resp.status().is_success() {
            return Err(error_result(
                McpConnectionTestErrorCode::HttpError,
                format!("HTTP {} from server", resp.status()),
                Some(serde_json::json!({ "status": resp.status().as_u16() })),
            ));
        }

        let session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let rpc = protocol::parse_http_response(resp).await.map_err(|error| {
            error_result(
                McpConnectionTestErrorCode::ProtocolError,
                error,
                Some(serde_json::json!({ "transport": "http" })),
            )
        })?;
        Ok(HttpMcpResponse { rpc, session_id })
    }

    // -- SSE transport ----------------------------------------------------

    async fn test_sse(&self, url: &str, headers: &HashMap<String, String>) -> McpConnectionTestResult {
        match tokio::time::timeout(self.timeout, self.test_sse_inner(url, headers)).await {
            Ok(r) => r,
            Err(_) => timeout_result(self.timeout),
        }
    }

    async fn test_sse_inner(&self, url: &str, headers: &HashMap<String, String>) -> McpConnectionTestResult {
        let mut req_headers = build_http_headers(headers);

        // 1. Open SSE connection
        let resp = match self
            .http_client
            .get(url)
            .headers(req_headers.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return error_result(
                    McpConnectionTestErrorCode::ConnectionFailed,
                    format!("Connection failed: {e}"),
                    Some(serde_json::json!({ "transport": "sse" })),
                );
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return protocol::auth_result(resp.headers());
        }
        if !resp.status().is_success() {
            return error_result(
                McpConnectionTestErrorCode::HttpError,
                format!("HTTP {} from server", resp.status()),
                Some(serde_json::json!({ "status": resp.status().as_u16() })),
            );
        }

        // 2. Start SSE reader task
        let (event_tx, mut event_rx) = mpsc::channel::<SseEvent>(16);
        let reader_handle = tokio::spawn(read_sse_events(resp, event_tx));

        req_headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().expect("valid header"),
        );

        let result = self.run_sse_protocol(url, &req_headers, &mut event_rx).await;
        reader_handle.abort();
        result
    }

    async fn run_sse_protocol(
        &self,
        base_url: &str,
        headers: &reqwest::header::HeaderMap,
        event_rx: &mut mpsc::Receiver<SseEvent>,
    ) -> McpConnectionTestResult {
        // 3. Wait for endpoint event
        let endpoint = match wait_for_endpoint(event_rx, base_url).await {
            Ok(ep) => ep,
            Err(e) => {
                return error_result(
                    McpConnectionTestErrorCode::ProtocolError,
                    e,
                    Some(serde_json::json!({ "transport": "sse", "stage": "endpoint" })),
                );
            }
        };

        // 4. initialize
        if let Err(e) = self.sse_post(&endpoint, headers, &build_initialize_request(1)).await {
            return error_result(
                McpConnectionTestErrorCode::ProtocolError,
                format!("Failed to send initialize: {e}"),
                Some(serde_json::json!({ "transport": "sse", "stage": "initialize_send" })),
            );
        }
        let init_resp = match wait_for_jsonrpc_response(event_rx).await {
            Ok(r) => r,
            Err(e) => {
                return error_result(
                    McpConnectionTestErrorCode::ProtocolError,
                    format!("initialize response: {e}"),
                    Some(serde_json::json!({ "transport": "sse", "stage": "initialize_response" })),
                );
            }
        };
        if let Some(err) = init_resp.error {
            return rpc_error_result("initialize", &err);
        }

        // 5. initialized notification
        let _ = self
            .sse_post(&endpoint, headers, &build_initialized_notification())
            .await;

        // 6. tools/list
        if let Err(e) = self.sse_post(&endpoint, headers, &build_tools_list_request(2)).await {
            return error_result(
                McpConnectionTestErrorCode::ProtocolError,
                format!("Failed to send tools/list: {e}"),
                Some(serde_json::json!({ "transport": "sse", "stage": "tools_list_send" })),
            );
        }
        let tools_resp = match wait_for_jsonrpc_response(event_rx).await {
            Ok(r) => r,
            Err(e) => {
                return error_result(
                    McpConnectionTestErrorCode::ProtocolError,
                    format!("tools/list response: {e}"),
                    Some(serde_json::json!({ "transport": "sse", "stage": "tools_list_response" })),
                );
            }
        };
        if let Some(err) = tools_resp.error {
            return rpc_error_result("tools/list", &err);
        }

        success_result(tools_resp.result)
    }

    /// POST a JSON-RPC message to an SSE endpoint (fire-and-forget semantics).
    async fn sse_post<T: Serialize>(
        &self,
        endpoint: &str,
        headers: &reqwest::header::HeaderMap,
        body: &T,
    ) -> Result<(), String> {
        self.http_client
            .post(endpoint)
            .headers(headers.clone())
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl McpConnectionTestService {
    fn runtime_reporter(&self, scope_id: String) -> Arc<dyn NodeRuntimeProgressReporter> {
        let broadcaster = self.broadcaster.clone();
        Arc::new(move |update: NodeRuntimeProgress| {
            let payload = RuntimeStatusPayload {
                resource: RuntimeResourceKind::Node,
                resource_id: None,
                scope: RuntimeStatusScope {
                    kind: RuntimeStatusScopeKind::Mcp,
                    id: scope_id.clone(),
                },
                phase: map_phase(update.phase),
                failure_kind: update.failure_kind.map(map_failure_kind),
                message: update.message,
                status_code: update.status_code,
            };
            let payload = serde_json::to_value(payload).expect("runtime status payload should serialize");
            broadcaster.broadcast(WebSocketMessage::new("runtime.statusChanged", payload));
        })
    }
}

fn map_phase(phase: aionui_runtime::NodeRuntimeProgressPhase) -> RuntimeStatusPhase {
    match phase {
        aionui_runtime::NodeRuntimeProgressPhase::WaitingForLock => RuntimeStatusPhase::WaitingForLock,
        aionui_runtime::NodeRuntimeProgressPhase::Downloading => RuntimeStatusPhase::Downloading,
        aionui_runtime::NodeRuntimeProgressPhase::Extracting => RuntimeStatusPhase::Extracting,
        aionui_runtime::NodeRuntimeProgressPhase::Validating => RuntimeStatusPhase::Validating,
        aionui_runtime::NodeRuntimeProgressPhase::Ready => RuntimeStatusPhase::Ready,
        aionui_runtime::NodeRuntimeProgressPhase::Failed => RuntimeStatusPhase::Failed,
    }
}

fn map_failure_kind(kind: NodeRuntimeFailureKind) -> RuntimeFailureKind {
    match kind {
        NodeRuntimeFailureKind::Timeout => RuntimeFailureKind::Timeout,
        NodeRuntimeFailureKind::DownloadFailed => RuntimeFailureKind::DownloadFailed,
        NodeRuntimeFailureKind::HttpStatus => RuntimeFailureKind::HttpStatus,
        NodeRuntimeFailureKind::ValidationFailed => RuntimeFailureKind::ValidationFailed,
        NodeRuntimeFailureKind::UnsupportedPlatform => RuntimeFailureKind::UnsupportedPlatform,
        NodeRuntimeFailureKind::Unknown => RuntimeFailureKind::Unknown,
    }
}

fn resolve_stdio_command(command: &str) -> OsString {
    if !command.is_empty()
        && !command.contains('/')
        && !command.contains('\\')
        && let Some(path) = resolve_command_path(command)
    {
        return path.into_os_string();
    }

    OsString::from(command)
}

fn runtime_resolution_error(message: &str) -> std::io::Error {
    let lower = message.to_ascii_lowercase();
    if lower.contains("not found")
        || lower.contains("unsupported")
        || lower.contains("unavailable")
        || lower.contains("system node")
    {
        std::io::Error::new(std::io::ErrorKind::NotFound, message.to_owned())
    } else {
        std::io::Error::other(message.to_owned())
    }
}

/// Intermediate struct for HTTP transport response parsing.
struct HttpMcpResponse {
    rpc: JsonRpcResponse,
    session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;

    #[test]
    fn service_clone() {
        let svc = McpConnectionTestService::new(reqwest::Client::new(), Arc::new(BroadcastEventBus::new(16)));
        let _cloned = svc.clone();
    }

    #[test]
    fn service_with_timeout() {
        let svc = McpConnectionTestService::new(reqwest::Client::new(), Arc::new(BroadcastEventBus::new(16)))
            .with_timeout(Duration::from_secs(5));
        assert_eq!(svc.timeout, Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_timeout_cleans_up_process_group() {
        let marker_path = std::env::temp_dir().join(format!(
            "aionui-mcp-timeout-pid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let transport = McpServerTransport::Stdio {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s\n' \"$$\" > \"$1\"; sleep 30".into(),
                "mcp-timeout-child".into(),
                marker_path.to_string_lossy().into_owned(),
            ],
            env: HashMap::new(),
        };
        let svc = McpConnectionTestService::new(reqwest::Client::new(), Arc::new(BroadcastEventBus::new(16)))
            .with_timeout(Duration::from_millis(100));

        let result = svc.test_connection("timeout-cleanup", &transport).await;
        assert!(!result.success);
        assert!(
            result.error.as_deref().unwrap_or_default().contains("timed out"),
            "expected timeout result, got {result:?}"
        );

        let pid: i32 = std::fs::read_to_string(&marker_path)
            .expect("stdio child should write its pid")
            .trim()
            .parse()
            .expect("pid marker should be numeric");

        let group_alive = wait_for_process_group_exit(pid, Duration::from_secs(1)).await;
        if group_alive {
            let _ = kill_process_group(pid, libc_sigkill());
        }
        let _ = std::fs::remove_file(marker_path);

        assert!(
            !group_alive,
            "stdio timeout should terminate the spawned process group for pid={pid}"
        );
    }

    #[cfg(unix)]
    async fn wait_for_process_group_exit(pid: i32, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if !is_process_group_alive(pid) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        is_process_group_alive(pid)
    }

    #[cfg(unix)]
    fn is_process_group_alive(pid: i32) -> bool {
        kill_process_group(pid, 0)
    }

    #[cfg(unix)]
    fn kill_process_group(pid: i32, signal: i32) -> bool {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe { kill(-pid, signal) == 0 }
    }

    #[cfg(unix)]
    fn libc_sigkill() -> i32 {
        9
    }
}
