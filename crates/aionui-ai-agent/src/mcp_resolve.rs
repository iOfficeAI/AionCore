//! Neutral MCP server resolution (Wave 0c E) — the SINGLE source of truth for
//! turning a conversation's configured MCP servers into the SDK-free
//! [`SessionMcpServer`] shape the clean-slate session stack carries in
//! `SessionConfig.init.mcp_servers`.
//!
//! The legacy per-backend resolvers (`factory::acp::load_user_mcp_servers` →
//! `Vec<agent_client_protocol::McpServer>`, `factory::aionrs::load_user_mcp_servers`
//! → `HashMap<String, McpServerConfig>`) emit SDK/engine-specific types. This
//! module emits the NEUTRAL `aionui_api_types::SessionMcpServer` so the app
//! boundary (`aionui-app`) can convert it once into the crate-local
//! `aionui_session::McpServerSpec`, and each backend serializes that into its own
//! wire shape. Same row-walking + selection + stdio-launch-resolution logic as
//! the legacy ACP path, but vendor-neutral.

use std::sync::Arc;

use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_realtime::EventBroadcaster;
use aionui_runtime::ensure_runtime_command;
use tracing::{info, warn};

/// Resolve a conversation's user-configured MCP servers into neutral
/// [`SessionMcpServer`]s. `selected_ids = Some` → that frozen snapshot defines the
/// session (injected regardless of the row's global `enabled` flag); `None` → all
/// enabled rows. Builtin rows are excluded (guide/team MCP are folded separately
/// by the caller). Stdio launch commands are RESOLVED here (e.g. `npx` → the
/// bundled-node absolute path) so the spec that reaches `open_session` is final —
/// the Wave 0c contract that `McpTransport::Stdio.command` is pre-resolved.
///
/// Best-effort: a repo error, a capability-unsupported transport, or a malformed
/// `transport_config` row is warn-logged and skipped, never fatal. `broadcaster`
/// is accepted for parity with the legacy reporter path (runtime-resolution status
/// reporting) and reserved for that use.
pub async fn resolve_session_mcp_servers(
    repo: &dyn IMcpServerRepository,
    user_id: &str,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    _broadcaster: Arc<dyn EventBroadcaster>,
) -> Vec<SessionMcpServer> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(user_id, ids).await,
        None => repo.list(user_id).await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(conversation_id, error = %err, "mcp_resolve: list() failed; skipping injection");
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for row in rows {
        let selected = selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled);
        if !selected || row.builtin {
            continue;
        }
        match row_to_session_mcp_server(&row).await {
            Ok(server) => servers.push(server),
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "mcp_resolve: failed to convert row; skipping"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "mcp_resolve: resolved user MCP servers"
        );
    }
    servers
}

/// Parse one `McpServerRow` into a neutral `SessionMcpServer`, resolving the stdio
/// launch command. Mirrors `factory::acp::row_to_sdk_mcp_server` but emits the
/// neutral type. Returns an error string when `transport_config` is malformed.
async fn row_to_session_mcp_server(row: &McpServerRow) -> Result<SessionMcpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    let transport = match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env: std::collections::HashMap<String, String> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();

            // Resolve the launch command (npx/bun → bundled path) + fold in the
            // runtime-provided args prefix + env, exactly like the legacy
            // `ensure_stdio_launch`. The resolved form is what the agent spawns.
            let resolved = ensure_runtime_command(command).await.map_err(|e| e.to_string())?;
            let mut final_args: Vec<String> = resolved
                .args_prefix
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            final_args.extend(args);
            for (k, v) in resolved.env {
                env.insert(k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned());
            }
            SessionMcpTransport::Stdio {
                command: resolved.program.to_string_lossy().into_owned(),
                args: final_args,
                env,
            }
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?
                .to_owned();
            SessionMcpTransport::StreamableHttp {
                url,
                headers: parse_headers(value.get("headers")),
            }
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?
                .to_owned();
            SessionMcpTransport::Sse {
                url,
                headers: parse_headers(value.get("headers")),
            }
        }
        other => return Err(format!("unknown transport type: {other}")),
    };

    Ok(SessionMcpServer {
        id: row.id.clone(),
        name: row.name.clone(),
        transport,
    })
}

/// Parse a JSON headers object into a `HashMap` (string values only).
fn parse_headers(value: Option<&serde_json::Value>) -> std::collections::HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}
