//! `aioncore mcp-bridge` subcommand: stdio ↔ TCP bridge for the team MCP server.
//!
//! Spawned by the ACP agent CLI as an MCP server with command `aioncore mcp-bridge`.
//! stdio side speaks MCP Content-Length framed JSON-RPC 2.0;
//! TCP side speaks 4-byte big-endian length-prefixed JSON frames against
//! `127.0.0.1:<TEAM_MCP_PORT>` (reusing `aionui_team::mcp::protocol`).
//!
//! On the first `initialize` request from the CLI, the bridge injects
//! `auth_token` and `slot_id` (read from env) into `params` before forwarding
//! to the TCP server; subsequent messages are transparently proxied in both
//! directions. Any unrecoverable error exits non-zero so the ACP CLI marks
//! the MCP server as broken (see docs/teams/mcp.md §4.4 / §4.6).

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use aionui_api_types::TeamMcpStdioConfig;
use aionui_team::mcp::protocol::{read_frame, write_frame};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::commands::cli_error::{CliBoundaryCode, CliBoundaryError, missing_env, parse_required_port};

const SUBCOMMAND: &str = "mcp-bridge";
const CONNECT_ADDR_HOST: &str = "127.0.0.1";
const MCP_STDIO_FRAME_MAX_BYTES: usize = 10 * 1024 * 1024;
const MCP_STDIO_HEADER_LINE_MAX_BYTES: usize = 8 * 1024;
const MCP_STDIO_HEADER_SECTION_MAX_BYTES: usize = 16 * 1024;
const MCP_STDIO_HEADER_MAX_COUNT: usize = 64;

/// Entry point for `aioncore mcp-bridge`. Returns an [`ExitCode`] so the
/// binary surfaces non-zero on any failure (ACP CLI uses that to mark the MCP
/// server as broken).
pub async fn run_mcp_bridge() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // stderr, not tracing: the parent agent CLI captures stderr and
            // shows it to the user when the bridge dies on startup.
            eprintln!("{}", err.stderr_line());
            err.exit_code()
        }
    }
}

async fn run() -> Result<(), CliBoundaryError> {
    let env = BridgeEnv::from_env()?;

    let tcp = TcpStream::connect((CONNECT_ADDR_HOST, env.port)).await.map_err(|_| {
        CliBoundaryError::new(
            CliBoundaryCode::McpTcpConnectFailed,
            SUBCOMMAND,
            "failed to connect to local MCP TCP listener",
        )
        .with_field("host", CONNECT_ADDR_HOST)
        .with_field("port", env.port.to_string())
    })?;
    tcp.set_nodelay(true).ok();
    let (tcp_reader, tcp_writer) = tcp.into_split();

    if std::io::stdin().is_terminal() {
        return Err(CliBoundaryError::new(
            CliBoundaryCode::McpStdinTty,
            SUBCOMMAND,
            "stdin must be provided by an MCP-capable agent CLI",
        ));
    }
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let env_for_stdin = env.clone();
    let stdin_task = tokio::spawn(async move { forward_stdin_to_tcp(stdin, tcp_writer, env_for_stdin).await });
    let tcp_task = tokio::spawn(async move { forward_tcp_to_stdout(tcp_reader, stdout).await });

    // First task to return decides the exit path; we treat clean EOF from
    // either side as "other side closed, we're done".
    tokio::select! {
        res = stdin_task => {
            res.map_err(|_| task_join_error())??;
        }
        res = tcp_task => {
            res.map_err(|_| task_join_error())??;
        }
    }
    Ok(())
}

fn task_join_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpTaskJoinPanic,
        SUBCOMMAND,
        "MCP bridge worker task failed",
    )
}

// ---------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct BridgeEnv {
    port: u16,
    token: String,
    slot_id: String,
}

impl BridgeEnv {
    fn from_env() -> Result<Self, CliBoundaryError> {
        let port_raw = std::env::var(TeamMcpStdioConfig::ENV_PORT)
            .map_err(|_| missing_env(SUBCOMMAND, TeamMcpStdioConfig::ENV_PORT))?;
        let token = std::env::var(TeamMcpStdioConfig::ENV_TOKEN)
            .map_err(|_| missing_env(SUBCOMMAND, TeamMcpStdioConfig::ENV_TOKEN))?;
        let slot_id = std::env::var(TeamMcpStdioConfig::ENV_SLOT_ID)
            .map_err(|_| missing_env(SUBCOMMAND, TeamMcpStdioConfig::ENV_SLOT_ID))?;
        Self::from_values(&port_raw, &token, &slot_id)
    }

    fn from_values(port_raw: &str, token: &str, slot_id: &str) -> Result<Self, CliBoundaryError> {
        let port = parse_required_port(SUBCOMMAND, TeamMcpStdioConfig::ENV_PORT, port_raw)?;
        Ok(Self {
            port,
            token: token.to_owned(),
            slot_id: slot_id.to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// stdin → TCP: read MCP Content-Length frames, inject auth on `initialize`, frame to TCP
// ---------------------------------------------------------------------------

async fn forward_stdin_to_tcp<R, W>(stdin: R, mut tcp_writer: W, env: BridgeEnv) -> Result<(), CliBoundaryError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stdin);
    loop {
        let body = match read_mcp_stdio_message(&mut reader).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let mut value: Value = serde_json::from_slice(&body).map_err(|_| {
            CliBoundaryError::new(
                CliBoundaryCode::McpStdinJsonInvalid,
                SUBCOMMAND,
                "stdin MCP frame body is not valid JSON",
            )
        })?;

        if value.get("method").and_then(Value::as_str) == Some("initialize") {
            inject_auth(&mut value, &env);
        }

        let bytes = serde_json::to_vec(&value).map_err(|_| {
            CliBoundaryError::new(
                CliBoundaryCode::McpJsonSerializeFailed,
                SUBCOMMAND,
                "failed to serialize MCP JSON frame",
            )
        })?;
        write_frame(&mut tcp_writer, &bytes).await.map_err(|_| {
            CliBoundaryError::new(
                CliBoundaryCode::McpTcpWriteFailed,
                SUBCOMMAND,
                "failed to write MCP frame to TCP listener",
            )
        })?;
    }
}

/// Read one MCP stdio message (Content-Length framing).
/// Returns None on clean EOF.
async fn read_mcp_stdio_message<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, CliBoundaryError> {
    let mut content_length: Option<usize> = None;
    let mut header_line = Vec::new();
    let mut header_bytes = 0usize;
    let mut header_count = 0usize;
    loop {
        let n = read_bounded_header_line(reader, &mut header_line).await?;
        if n == 0 {
            return if header_bytes == 0 {
                Ok(None) // Clean EOF before the next frame starts.
            } else {
                Err(stdin_frame_invalid())
            };
        }
        header_bytes = header_bytes.checked_add(n).ok_or_else(stdin_frame_invalid)?;
        if header_bytes > MCP_STDIO_HEADER_SECTION_MAX_BYTES {
            return Err(stdin_frame_invalid());
        }
        let trimmed = std::str::from_utf8(&header_line)
            .map_err(|_| stdin_frame_invalid())?
            .trim();
        if trimmed.is_empty() {
            // Empty line = end of headers
            break;
        }
        header_count += 1;
        if header_count > MCP_STDIO_HEADER_MAX_COUNT {
            return Err(stdin_frame_invalid());
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(len_str.trim().parse::<usize>().map_err(|_| stdin_frame_invalid())?);
        }
        // Ignore other headers
    }
    let len = content_length.ok_or_else(stdin_frame_invalid)?;
    if len > MCP_STDIO_FRAME_MAX_BYTES {
        return Err(CliBoundaryError::new(
            CliBoundaryCode::McpFrameTooLarge,
            SUBCOMMAND,
            "MCP stdio frame exceeds configured size limit",
        )
        .with_field("limitBytes", MCP_STDIO_FRAME_MAX_BYTES.to_string()));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.map_err(|_| stdin_read_error())?;
    Ok(Some(body))
}

async fn read_bounded_header_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<usize, CliBoundaryError> {
    line.clear();
    loop {
        let (n, end_of_line) = {
            let available = reader.fill_buf().await.map_err(|_| stdin_read_error())?;
            if available.is_empty() {
                return Ok(line.len());
            }
            let n = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |pos| pos + 1);
            if line.len() + n > MCP_STDIO_HEADER_LINE_MAX_BYTES {
                return Err(stdin_frame_invalid());
            }
            line.extend_from_slice(&available[..n]);
            (n, available[n - 1] == b'\n')
        };
        reader.consume(n);
        if end_of_line {
            return Ok(line.len());
        }
    }
}

fn stdin_frame_invalid() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpStdinFrameInvalid,
        SUBCOMMAND,
        "invalid MCP stdio frame",
    )
}

fn stdin_read_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpStdinReadFailed,
        SUBCOMMAND,
        "failed to read MCP stdio frame from stdin",
    )
}

fn inject_auth(value: &mut Value, env: &BridgeEnv) {
    let params = value.as_object_mut().and_then(|obj| {
        obj.entry("params")
            .or_insert(Value::Object(Default::default()))
            .as_object_mut()
    });
    if let Some(params) = params {
        params.insert("auth_token".into(), Value::String(env.token.clone()));
        params.insert("slot_id".into(), Value::String(env.slot_id.clone()));
    }
}

// ---------------------------------------------------------------------------
// TCP → stdout: read length-prefixed frames, write MCP Content-Length frames
// ---------------------------------------------------------------------------

async fn forward_tcp_to_stdout<R, W>(mut tcp_reader: R, mut stdout: W) -> Result<(), CliBoundaryError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = match read_frame(&mut tcp_reader).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(_) => {
                return Err(CliBoundaryError::new(
                    CliBoundaryCode::McpTcpReadFailed,
                    SUBCOMMAND,
                    "failed to read MCP frame from TCP listener",
                ));
            }
        };
        // Content-Length framing for stdout (MCP stdio transport)
        let header = format!("Content-Length: {}\r\n\r\n", frame.len());
        stdout
            .write_all(header.as_bytes())
            .await
            .map_err(|_| stdout_write_error())?;
        stdout.write_all(&frame).await.map_err(|_| stdout_write_error())?;
        stdout.flush().await.map_err(|_| stdout_write_error())?;
    }
}

fn stdout_write_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpStdoutWriteFailed,
        SUBCOMMAND,
        "failed to write MCP stdio frame to stdout",
    )
}

// ---------------------------------------------------------------------------
// Unit tests (integration tests live in tests/mcp_bridge.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env() -> BridgeEnv {
        BridgeEnv {
            port: 1,
            token: "tok".into(),
            slot_id: "slot-a".into(),
        }
    }

    #[test]
    fn bridge_env_rejects_invalid_port_with_stable_code() {
        let err = BridgeEnv::from_values("not-a-port", "tok", "slot-a").unwrap_err();
        assert_eq!(
            err.code(),
            crate::commands::cli_error::CliBoundaryCode::McpEnvInvalidPort
        );
        assert_eq!(err.exit_code(), std::process::ExitCode::from(2));
    }

    #[tokio::test]
    async fn read_mcp_stdio_message_rejects_oversized_content_length() {
        let input = format!("Content-Length: {}\r\n\r\n", MCP_STDIO_FRAME_MAX_BYTES + 1);
        let err = read_mcp_stdio_message(&mut input.as_bytes()).await.unwrap_err();
        assert_eq!(
            err.code(),
            crate::commands::cli_error::CliBoundaryCode::McpFrameTooLarge
        );
    }

    #[tokio::test]
    async fn read_mcp_stdio_message_rejects_invalid_content_length() {
        let input = "Content-Length: nope\r\n\r\n";
        let err = read_mcp_stdio_message(&mut input.as_bytes()).await.unwrap_err();
        assert_eq!(
            err.code(),
            crate::commands::cli_error::CliBoundaryCode::McpStdinFrameInvalid
        );
    }

    #[tokio::test]
    async fn read_mcp_stdio_message_rejects_partial_header_eof() {
        for input in ["Content-Length: 2", "X-Header: value"] {
            let err = read_mcp_stdio_message(&mut input.as_bytes()).await.unwrap_err();
            assert_eq!(
                err.code(),
                crate::commands::cli_error::CliBoundaryCode::McpStdinFrameInvalid
            );
        }
    }

    #[tokio::test]
    async fn read_mcp_stdio_message_rejects_overlong_header_line() {
        let input = format!("X-Header: {}\r\nContent-Length: 2\r\n\r\n{{}}", "a".repeat(16 * 1024));
        let err = read_mcp_stdio_message(&mut input.as_bytes()).await.unwrap_err();
        assert_eq!(
            err.code(),
            crate::commands::cli_error::CliBoundaryCode::McpStdinFrameInvalid
        );
    }

    #[tokio::test]
    async fn read_mcp_stdio_message_rejects_oversized_header_section() {
        let line_a = format!(
            "X-A: {}\r\n",
            "a".repeat(MCP_STDIO_HEADER_LINE_MAX_BYTES - "X-A: \r\n".len())
        );
        let line_b = format!(
            "X-B: {}\r\n",
            "b".repeat(MCP_STDIO_HEADER_LINE_MAX_BYTES - "X-B: \r\n".len())
        );
        let input = format!("{line_a}{line_b}X-C: v\r\nContent-Length: 0\r\n\r\n");

        let err = read_mcp_stdio_message(&mut input.as_bytes()).await.unwrap_err();
        assert_eq!(
            err.code(),
            crate::commands::cli_error::CliBoundaryCode::McpStdinFrameInvalid
        );
    }

    #[tokio::test]
    async fn read_mcp_stdio_message_rejects_too_many_headers() {
        let mut input = String::new();
        for index in 0..=MCP_STDIO_HEADER_MAX_COUNT {
            input.push_str(&format!("X-{index}: v\r\n"));
        }
        input.push_str("Content-Length: 0\r\n\r\n");

        let err = read_mcp_stdio_message(&mut input.as_bytes()).await.unwrap_err();
        assert_eq!(
            err.code(),
            crate::commands::cli_error::CliBoundaryCode::McpStdinFrameInvalid
        );
    }

    #[test]
    fn inject_auth_adds_fields_when_params_missing() {
        let mut v = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        inject_auth(&mut v, &env());
        assert_eq!(v["params"]["auth_token"], "tok");
        assert_eq!(v["params"]["slot_id"], "slot-a");
    }

    #[test]
    fn inject_auth_preserves_existing_params() {
        let mut v = json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params": {"protocolVersion":"2024-11-05","capabilities":{}}
        });
        inject_auth(&mut v, &env());
        assert_eq!(v["params"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["params"]["auth_token"], "tok");
        assert_eq!(v["params"]["slot_id"], "slot-a");
    }

    #[test]
    fn inject_auth_overrides_client_supplied_credentials() {
        // The CLI cannot be trusted to know the bridge's token / slot id,
        // so whatever it sent gets replaced.
        let mut v = json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"auth_token":"stale","slot_id":"wrong"}
        });
        inject_auth(&mut v, &env());
        assert_eq!(v["params"]["auth_token"], "tok");
        assert_eq!(v["params"]["slot_id"], "slot-a");
    }

    #[tokio::test]
    async fn forward_stdin_injects_only_on_initialize() {
        let initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let tools_list = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let input = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            initialize.len(),
            std::str::from_utf8(initialize).unwrap(),
            tools_list.len(),
            std::str::from_utf8(tools_list).unwrap(),
        );
        let mut out = Vec::<u8>::new();
        forward_stdin_to_tcp(input.as_bytes(), &mut out, env()).await.unwrap();

        // Parse two frames back out.
        let mut cursor = std::io::Cursor::new(out);
        let f1 = read_frame(&mut cursor).await.unwrap();
        let f2 = read_frame(&mut cursor).await.unwrap();
        let v1: Value = serde_json::from_slice(&f1).unwrap();
        let v2: Value = serde_json::from_slice(&f2).unwrap();
        assert_eq!(v1["params"]["auth_token"], "tok");
        assert_eq!(v1["params"]["slot_id"], "slot-a");
        assert!(v2.get("params").is_none(), "tools/list params untouched");
    }

    #[tokio::test]
    async fn forward_tcp_writes_content_length_framed_stdout() {
        let payload = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let mut framed = Vec::new();
        write_frame(&mut framed, payload).await.unwrap();

        let mut out = Vec::<u8>::new();
        forward_tcp_to_stdout(&framed[..], &mut out).await.unwrap();

        let mut cursor = std::io::Cursor::new(out);
        let body = read_mcp_stdio_message(&mut cursor).await.unwrap().unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["id"], 1);
    }
}
